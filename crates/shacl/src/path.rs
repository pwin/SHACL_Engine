//! SHACL property paths: compilation from RDF, and evaluation over a graph.
//!
//! Paths are compiled once, when the shapes graph is read, into a tree of
//! [`Path`] nodes holding interned predicates. Evaluation then never touches the
//! shapes graph again — it only probes the data graph's indexes.

use crate::error::{Error, Result};
use crate::model::{Graph, TermId, TermStore, Vocab};
use crate::valueset::{self, ValueSets};

/// A compiled SHACL property path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Path {
    /// A single predicate: the overwhelmingly common case.
    Predicate(TermId),
    Inverse(Box<Path>),
    /// Two or more paths traversed in order.
    Sequence(Vec<Path>),
    /// Two or more paths whose results are unioned.
    Alternative(Vec<Path>),
    ZeroOrMore(Box<Path>),
    OneOrMore(Box<Path>),
    ZeroOrOne(Box<Path>),
}

impl Path {
    /// Compiles the path expression rooted at `node` in `shapes`.
    ///
    /// `depth` bounds recursion so a shapes graph containing a cyclic blank node
    /// structure is rejected rather than overflowing the stack.
    pub fn compile(node: TermId, shapes: &Graph, store: &TermStore, vocab: &Vocab) -> Result<Self> {
        Self::compile_at(node, shapes, store, vocab, 0)
    }

    fn compile_at(
        node: TermId,
        shapes: &Graph,
        store: &TermStore,
        vocab: &Vocab,
        depth: u32,
    ) -> Result<Self> {
        const MAX_DEPTH: u32 = 64;
        if depth > MAX_DEPTH {
            return Err(Error::Shape("property path nested too deeply".into()));
        }

        // An IRI is always a predicate path, never a structure to descend into.
        if store.is_iri(node) {
            return Ok(Path::Predicate(node));
        }
        if store.is_literal(node) {
            return Err(Error::Shape(
                "a literal is not a valid property path".into(),
            ));
        }

        let sub = |n: TermId| Self::compile_at(n, shapes, store, vocab, depth + 1);

        // The list form is tested first. A blank node can carry both
        // `rdf:first` and, say, `sh:inversePath`; the suite pins the sequence
        // reading as the winner in that case.
        if shapes.object(node, vocab.rdf_first).is_some() {
            let items = shapes
                .list(node, vocab)
                .ok_or_else(|| Error::Shape("blank node is not a valid property path".into()))?;
            if items.len() < 2 {
                return Err(Error::Shape(
                    "a sequence path needs at least two steps".into(),
                ));
            }
            let steps = items.into_iter().map(sub).collect::<Result<Vec<_>>>()?;
            return Ok(Path::Sequence(steps));
        }

        if let Some(inner) = shapes.object(node, vocab.sh_inversePath) {
            return Ok(Path::Inverse(Box::new(sub(inner)?)));
        }
        if let Some(head) = shapes.object(node, vocab.sh_alternativePath) {
            let items = shapes.list(head, vocab).ok_or_else(|| {
                Error::Shape("sh:alternativePath is not a well-formed list".into())
            })?;
            if items.len() < 2 {
                return Err(Error::Shape(
                    "sh:alternativePath needs at least two alternatives".into(),
                ));
            }
            let alts = items.into_iter().map(sub).collect::<Result<Vec<_>>>()?;
            return Ok(Path::Alternative(alts));
        }
        if let Some(inner) = shapes.object(node, vocab.sh_zeroOrMorePath) {
            return Ok(Path::ZeroOrMore(Box::new(sub(inner)?)));
        }
        if let Some(inner) = shapes.object(node, vocab.sh_oneOrMorePath) {
            return Ok(Path::OneOrMore(Box::new(sub(inner)?)));
        }
        if let Some(inner) = shapes.object(node, vocab.sh_zeroOrOnePath) {
            return Ok(Path::ZeroOrOne(Box::new(sub(inner)?)));
        }

        // Otherwise the blank node must head an RDF list: a sequence path.
        let items = shapes
            .list(node, vocab)
            .ok_or_else(|| Error::Shape("blank node is not a valid property path".into()))?;
        if items.len() < 2 {
            return Err(Error::Shape(
                "a sequence path needs at least two steps".into(),
            ));
        }
        let steps = items.into_iter().map(sub).collect::<Result<Vec<_>>>()?;
        Ok(Path::Sequence(steps))
    }

    /// Renders the path as SPARQL property path syntax.
    ///
    /// SHACL substitutes `$PATH` into a SPARQL constraint textually rather than
    /// binding it as a variable, because a compound path is syntax, not a term:
    /// `$this $PATH ?value` with an inverse path has to become `^<p>`, which no
    /// variable binding could express.
    pub fn to_sparql(&self, store: &TermStore) -> String {
        match self {
            Path::Predicate(p) => format!("<{}>", store.iri(*p).unwrap_or_default()),
            Path::Inverse(inner) => format!("^{}", inner.to_sparql(store)),
            Path::Sequence(steps) => format!(
                "({})",
                steps
                    .iter()
                    .map(|s| s.to_sparql(store))
                    .collect::<Vec<_>>()
                    .join("/")
            ),
            Path::Alternative(alts) => format!(
                "({})",
                alts.iter()
                    .map(|a| a.to_sparql(store))
                    .collect::<Vec<_>>()
                    .join("|")
            ),
            Path::ZeroOrMore(inner) => format!("({})*", inner.to_sparql(store)),
            Path::OneOrMore(inner) => format!("({})+", inner.to_sparql(store)),
            Path::ZeroOrOne(inner) => format!("({})?", inner.to_sparql(store)),
        }
    }

    /// True if this is a bare predicate path, which the validator special-cases.
    #[inline]
    pub fn as_predicate(&self) -> Option<TermId> {
        match self {
            Path::Predicate(p) => Some(*p),
            _ => None,
        }
    }

    /// Evaluates this path from every focus node, producing the focus→values
    /// relation.
    ///
    /// This is the engine's primary entry point. Rows stay indexed by the
    /// originating focus node so per-row aggregates such as `sh:minCount`, and
    /// the focus node every violation must name, survive a compound path.
    pub fn eval_sets(&self, focus: &[TermId], data: &Graph) -> ValueSets {
        self.eval_sets_dir(focus, data, false)
    }

    fn eval_sets_dir(&self, focus: &[TermId], data: &Graph, reverse: bool) -> ValueSets {
        // The traversal state is a list of `(origin, reached)` pairs rather
        // than one focus node at a time. Keeping the origin index alongside
        // each reached node is what lets the whole frontier be sorted before
        // the next step without losing track of which focus node it belongs
        // to — and sorting the frontier is the entire point, since probing the
        // index in sorted order is several times cheaper than probing it at
        // random once the graph outgrows cache.
        let pairs: Pairs = focus
            .iter()
            .enumerate()
            .map(|(i, &n)| (i as u32, n))
            .collect();
        let mut reached = Pairs::new();
        self.walk(&pairs, data, reverse, &mut reached);
        normalise(&mut reached);

        // `reached` is sorted by origin, so grouping into rows is a merge.
        let mut b = valueset::Builder::with_capacity(focus.len());
        let mut i = 0;
        for (origin, &f) in focus.iter().enumerate() {
            b.start_row(f);
            while i < reached.len() && reached[i].0 == origin as u32 {
                b.push_value(reached[i].1);
                i += 1;
            }
            b.end_row();
        }
        b.finish()
    }

    /// Appends the value nodes reachable from `focus` to `out`, deduplicated.
    pub fn eval(&self, focus: TermId, data: &Graph, out: &mut Vec<TermId>) {
        let sets = self.eval_sets_dir(std::slice::from_ref(&focus), data, false);
        out.extend(sets.row(0).values.iter().copied());
    }

    /// Evaluates the path backwards: the nodes from which `node` is reachable.
    pub fn eval_inverse(&self, node: TermId, data: &Graph, out: &mut Vec<TermId>) {
        let sets = self.eval_sets_dir(std::slice::from_ref(&node), data, true);
        out.extend(sets.row(0).values.iter().copied());
    }

    /// Advances every pair one whole path, batched.
    fn walk(&self, input: &Pairs, data: &Graph, reverse: bool, out: &mut Pairs) {
        match self {
            Path::Predicate(p) => probe(input, *p, data, reverse, out),
            Path::Inverse(inner) => inner.walk(input, data, !reverse, out),
            Path::Sequence(steps) => {
                let mut frontier = input.to_vec();
                let mut next = Pairs::new();
                let ordered: Box<dyn Iterator<Item = &Path>> = if reverse {
                    Box::new(steps.iter().rev())
                } else {
                    Box::new(steps.iter())
                };
                for step in ordered {
                    next.clear();
                    step.walk(&frontier, data, reverse, &mut next);
                    // Normalising between steps collapses the duplicate work
                    // that two origins reaching the same node would otherwise
                    // cause the next step to repeat.
                    normalise(&mut next);
                    std::mem::swap(&mut frontier, &mut next);
                    if frontier.is_empty() {
                        return;
                    }
                }
                out.extend_from_slice(&frontier);
            }
            Path::Alternative(alts) => {
                for alt in alts {
                    alt.walk(input, data, reverse, out);
                }
            }
            Path::ZeroOrMore(inner) => {
                out.extend_from_slice(input);
                inner.closure(input, data, reverse, out);
            }
            Path::OneOrMore(inner) => inner.closure(input, data, reverse, out),
            Path::ZeroOrOne(inner) => {
                out.extend_from_slice(input);
                inner.walk(input, data, reverse, out);
            }
        }
    }

    /// Transitive closure over the whole pair list at once.
    ///
    /// One fixpoint serves every origin, so overlapping traversals are walked
    /// once rather than once per focus node, and the round count is bounded by
    /// the graph's depth.
    fn closure(&self, input: &Pairs, data: &Graph, reverse: bool, out: &mut Pairs) {
        let mut seen = Pairs::new();
        let mut frontier = input.to_vec();
        let mut next = Pairs::new();

        while !frontier.is_empty() {
            next.clear();
            self.walk(&frontier, data, reverse, &mut next);
            normalise(&mut next);
            // Dropping pairs already expanded is what terminates on a cycle.
            next.retain(|p| seen.binary_search(p).is_err());
            if next.is_empty() {
                return;
            }
            out.extend_from_slice(&next);
            seen.extend_from_slice(&next);
            normalise(&mut seen);
            std::mem::swap(&mut frontier, &mut next);
        }
    }
}

/// `(origin index, node reached)`. The origin is an index into the focus slice
/// rather than the focus term itself, so a focus node appearing twice — as it
/// does when `sh:property` feeds values from two parents — stays two rows.
type Pairs = Vec<(u32, TermId)>;

#[inline]
fn normalise(pairs: &mut Pairs) {
    pairs.sort_unstable();
    pairs.dedup();
}

/// Advances every pair one predicate, probing the index in sorted node order.
///
/// This is where the batching pays. The frontier is re-sorted by the node being
/// probed, so the index is walked forwards rather than jumped around, and all
/// origins that reached the same node share a single probe instead of each
/// repeating it.
fn probe(input: &Pairs, p: TermId, data: &Graph, reverse: bool, out: &mut Pairs) {
    // Sorted by node first, so equal nodes are adjacent.
    let mut by_node: Vec<(TermId, u32)> = input.iter().map(|&(o, n)| (n, o)).collect();
    by_node.sort_unstable();

    let mut i = 0;
    while i < by_node.len() {
        let node = by_node[i].0;
        let start = i;
        while i < by_node.len() && by_node[i].0 == node {
            i += 1;
        }
        let origins = &by_node[start..i];

        if reverse {
            for t in data.subjects(p, node) {
                out.extend(origins.iter().map(|&(_, o)| (o, t)));
            }
        } else {
            for t in data.objects(node, p) {
                out.extend(origins.iter().map(|&(_, o)| (o, t)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GraphBuilder, loader};
    use oxrdfio::RdfFormat;

    struct Fixture {
        store: TermStore,
        vocab: Vocab,
        graph: Graph,
    }

    impl Fixture {
        fn new(turtle: &str) -> Self {
            let mut store = TermStore::new();
            let vocab = Vocab::new(&mut store);
            let mut b = GraphBuilder::new();
            loader::parse_str(
                turtle,
                RdfFormat::Turtle,
                "http://t/",
                0,
                &mut store,
                &mut b,
            )
            .unwrap();
            Self {
                store,
                vocab,
                graph: b.build(),
            }
        }

        fn iri(&mut self, s: &str) -> TermId {
            self.store.named_node(s)
        }

        /// Compiles the path that `<http://ex/S> sh:path ?p` points at.
        fn path(&mut self) -> Result<Path> {
            let s = self.iri("http://ex/S");
            let node = self.graph.object(s, self.vocab.sh_path).expect("sh:path");
            Path::compile(node, &self.graph, &self.store, &self.vocab)
        }

        /// Value nodes as sorted strings. Value nodes are a set, so the
        /// evaluator makes no ordering promise.
        fn eval(&mut self, path: &Path, focus: &str) -> Vec<String> {
            let f = self.iri(focus);
            let mut out = Vec::new();
            path.eval(f, &self.graph, &mut out);
            let mut v: Vec<String> = out
                .iter()
                .map(|&t| self.store.lexical_form(t).unwrap_or("?").to_string())
                .collect();
            v.sort();
            v
        }

        /// The full focus→values relation, as sorted strings per row.
        fn eval_sets(&mut self, path: &Path, focus: &[&str]) -> Vec<(String, Vec<String>)> {
            let ids: Vec<TermId> = focus.iter().map(|f| self.store.named_node(f)).collect();
            let sets = path.eval_sets(&ids, &self.graph);
            sets.rows()
                .map(|r| {
                    let mut vs: Vec<String> = r
                        .values
                        .iter()
                        .map(|&t| self.store.lexical_form(t).unwrap_or("?").to_string())
                        .collect();
                    vs.sort();
                    (
                        self.store.lexical_form(r.focus).unwrap_or("?").to_string(),
                        vs,
                    )
                })
                .collect()
        }
    }

    const PREFIX: &str = "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://ex/> . ";

    #[test]
    fn compiles_and_evaluates_a_predicate_path() {
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path ex:p . ex:a ex:p ex:b, ex:c . ex:x ex:p ex:y ."
        ));
        let p = f.path().unwrap();
        assert!(p.as_predicate().is_some());
        assert_eq!(
            f.eval(&p, "http://ex/a"),
            vec!["http://ex/b", "http://ex/c"]
        );
        assert!(f.eval(&p, "http://ex/none").is_empty());
    }

    #[test]
    fn evaluates_an_inverse_path() {
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path [ sh:inversePath ex:p ] . ex:a ex:p ex:b . ex:c ex:p ex:b ."
        ));
        let p = f.path().unwrap();
        assert_eq!(
            p,
            Path::Inverse(Box::new(Path::Predicate(f.iri("http://ex/p"))))
        );
        assert_eq!(
            f.eval(&p, "http://ex/b"),
            vec!["http://ex/a", "http://ex/c"]
        );
    }

    #[test]
    fn evaluates_a_sequence_path() {
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path ( ex:p ex:q ) . ex:a ex:p ex:m . ex:m ex:q ex:z . ex:a ex:q ex:wrong ."
        ));
        let p = f.path().unwrap();
        assert_eq!(f.eval(&p, "http://ex/a"), vec!["http://ex/z"]);
    }

    #[test]
    fn evaluates_an_alternative_path() {
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path [ sh:alternativePath ( ex:p ex:q ) ] . ex:a ex:p ex:b ; ex:q ex:c ."
        ));
        let p = f.path().unwrap();
        assert_eq!(
            f.eval(&p, "http://ex/a"),
            vec!["http://ex/b", "http://ex/c"]
        );
    }

    #[test]
    fn zero_or_more_includes_the_focus_node() {
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path [ sh:zeroOrMorePath ex:p ] . ex:a ex:p ex:b . ex:b ex:p ex:c ."
        ));
        let p = f.path().unwrap();
        assert_eq!(
            f.eval(&p, "http://ex/a"),
            vec!["http://ex/a", "http://ex/b", "http://ex/c"]
        );
    }

    #[test]
    fn one_or_more_excludes_the_focus_node() {
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path [ sh:oneOrMorePath ex:p ] . ex:a ex:p ex:b . ex:b ex:p ex:c ."
        ));
        let p = f.path().unwrap();
        assert_eq!(
            f.eval(&p, "http://ex/a"),
            vec!["http://ex/b", "http://ex/c"]
        );
    }

    #[test]
    fn zero_or_one_takes_at_most_one_step() {
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path [ sh:zeroOrOnePath ex:p ] . ex:a ex:p ex:b . ex:b ex:p ex:c ."
        ));
        let p = f.path().unwrap();
        assert_eq!(
            f.eval(&p, "http://ex/a"),
            vec!["http://ex/a", "http://ex/b"]
        );
    }

    #[test]
    fn closure_terminates_on_a_cycle() {
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path [ sh:oneOrMorePath ex:p ] . ex:a ex:p ex:b . ex:b ex:p ex:a ."
        ));
        let p = f.path().unwrap();
        let mut got = f.eval(&p, "http://ex/a");
        got.sort();
        assert_eq!(got, vec!["http://ex/a", "http://ex/b"]);
    }

    #[test]
    fn inverse_of_a_sequence_walks_backwards() {
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path [ sh:inversePath ( ex:p ex:q ) ] . ex:a ex:p ex:m . ex:m ex:q ex:z ."
        ));
        let p = f.path().unwrap();
        assert_eq!(f.eval(&p, "http://ex/z"), vec!["http://ex/a"]);
    }

    #[test]
    fn eval_sets_keeps_values_attributed_to_their_focus_node() {
        // The property that makes this usable for sh:minCount and for naming a
        // violation's focus node: rows must not merge.
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path ex:p .
             ex:a ex:p ex:x, ex:y . ex:b ex:p ex:y . ex:c ex:q ex:z ."
        ));
        let p = f.path().unwrap();
        let rows = f.eval_sets(&p, &["http://ex/a", "http://ex/b", "http://ex/c"]);

        assert_eq!(
            rows,
            vec![
                (
                    "http://ex/a".into(),
                    vec!["http://ex/x".into(), "http://ex/y".into()]
                ),
                ("http://ex/b".into(), vec!["http://ex/y".into()]),
                ("http://ex/c".into(), vec![]),
            ]
        );
    }

    #[test]
    fn eval_sets_shares_a_closure_across_focus_nodes() {
        // a -> b -> c -> d, and b is also a focus node. Both rows must be
        // complete even though the traversals overlap.
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path [ sh:oneOrMorePath ex:p ] .
             ex:a ex:p ex:b . ex:b ex:p ex:c . ex:c ex:p ex:d ."
        ));
        let p = f.path().unwrap();
        let rows = f.eval_sets(&p, &["http://ex/a", "http://ex/b"]);

        assert_eq!(rows[0].1, vec!["http://ex/b", "http://ex/c", "http://ex/d"]);
        assert_eq!(rows[1].1, vec!["http://ex/c", "http://ex/d"]);
    }

    #[test]
    fn eval_sets_on_no_focus_nodes_is_empty() {
        let mut f = Fixture::new(&format!("{PREFIX} ex:S sh:path ex:p . ex:a ex:p ex:b ."));
        let p = f.path().unwrap();
        assert!(f.eval_sets(&p, &[]).is_empty());
    }

    #[test]
    fn the_list_reading_wins_when_a_node_is_both_a_list_and_a_path_operator() {
        // A blank node carrying rdf:first *and* sh:inversePath is ambiguous;
        // the suite pins the sequence reading as correct.
        let mut f = Fixture::new(&format!(
            "{PREFIX} @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
             ex:S sh:path [ rdf:first ex:p ; rdf:rest ( ex:q ) ; sh:inversePath ex:p ] .
             ex:a ex:p ex:m . ex:m ex:q ex:z ."
        ));
        let p = f.path().unwrap();
        assert!(
            matches!(p, Path::Sequence(_)),
            "expected a sequence, got {p:?}"
        );
        assert_eq!(f.eval(&p, "http://ex/a"), vec!["http://ex/z"]);
    }

    #[test]
    fn rejects_malformed_paths() {
        let mut f = Fixture::new(&format!("{PREFIX} ex:S sh:path 42 ."));
        assert!(matches!(f.path(), Err(Error::Shape(_))), "literal path");

        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path [ sh:alternativePath ( ex:p ) ] ."
        ));
        assert!(matches!(f.path(), Err(Error::Shape(_))), "one alternative");

        let mut f = Fixture::new(&format!("{PREFIX} ex:S sh:path [ ex:bogus true ] ."));
        assert!(
            matches!(f.path(), Err(Error::Shape(_))),
            "not a path at all"
        );
    }
}
