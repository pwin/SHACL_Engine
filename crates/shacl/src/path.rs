//! SHACL property paths: compilation from RDF, and evaluation over a graph.
//!
//! Paths are compiled once, when the shapes graph is read, into a tree of
//! [`Path`] nodes holding interned predicates. Evaluation then never touches the
//! shapes graph again — it only probes the data graph's indexes.

use crate::error::{Error, Result};
use crate::model::{Graph, TermId, TermStore, Vocab};

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
    pub fn compile(
        node: TermId,
        shapes: &Graph,
        store: &TermStore,
        vocab: &Vocab,
    ) -> Result<Self> {
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
            return Err(Error::Shape("a literal is not a valid property path".into()));
        }

        let sub = |n: TermId| Self::compile_at(n, shapes, store, vocab, depth + 1);

        if let Some(inner) = shapes.object(node, vocab.sh_inversePath) {
            return Ok(Path::Inverse(Box::new(sub(inner)?)));
        }
        if let Some(head) = shapes.object(node, vocab.sh_alternativePath) {
            let items = shapes
                .list(head, vocab)
                .ok_or_else(|| Error::Shape("sh:alternativePath is not a well-formed list".into()))?;
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

    /// True if this is a bare predicate path, which the validator special-cases.
    #[inline]
    pub fn as_predicate(&self) -> Option<TermId> {
        match self {
            Path::Predicate(p) => Some(*p),
            _ => None,
        }
    }

    /// Appends the value nodes reachable from `focus` to `out`, deduplicated.
    ///
    /// SHACL treats value nodes as a set; `out` is left in first-reached order,
    /// which keeps results deterministic for a given graph.
    pub fn eval(&self, focus: TermId, data: &Graph, out: &mut Vec<TermId>) {
        let start = out.len();
        self.eval_into(focus, data, out, start, false);
    }

    /// Evaluates the path backwards: the nodes from which `node` is reachable.
    pub fn eval_inverse(&self, node: TermId, data: &Graph, out: &mut Vec<TermId>) {
        let start = out.len();
        self.eval_into(node, data, out, start, true);
    }

    /// Core traversal. `reverse` flips edge direction throughout, which is how
    /// `sh:inversePath` over a compound path is handled without a second
    /// evaluator.
    fn eval_into(
        &self,
        focus: TermId,
        data: &Graph,
        out: &mut Vec<TermId>,
        dedup_from: usize,
        reverse: bool,
    ) {
        match self {
            Path::Predicate(p) => {
                if reverse {
                    for s in data.subjects(*p, focus) {
                        push_unique(out, dedup_from, s);
                    }
                } else {
                    for o in data.objects(focus, *p) {
                        push_unique(out, dedup_from, o);
                    }
                }
            }
            Path::Inverse(inner) => inner.eval_into(focus, data, out, dedup_from, !reverse),
            Path::Sequence(steps) => {
                // Walk the steps in order, carrying the frontier between them.
                // Reversed, the sequence itself must also be walked backwards.
                let mut frontier = vec![focus];
                let mut next = Vec::new();
                let ordered: Box<dyn Iterator<Item = &Path>> = if reverse {
                    Box::new(steps.iter().rev())
                } else {
                    Box::new(steps.iter())
                };
                for step in ordered {
                    next.clear();
                    for &node in &frontier {
                        let base = next.len();
                        let _ = base;
                        step.eval_into(node, data, &mut next, 0, reverse);
                    }
                    dedup_in_place(&mut next);
                    std::mem::swap(&mut frontier, &mut next);
                }
                for node in frontier {
                    push_unique(out, dedup_from, node);
                }
            }
            Path::Alternative(alts) => {
                for alt in alts {
                    alt.eval_into(focus, data, out, dedup_from, reverse);
                }
            }
            Path::ZeroOrMore(inner) => {
                push_unique(out, dedup_from, focus);
                inner.closure(focus, data, out, dedup_from, reverse);
            }
            Path::OneOrMore(inner) => {
                inner.closure(focus, data, out, dedup_from, reverse);
            }
            Path::ZeroOrOne(inner) => {
                push_unique(out, dedup_from, focus);
                inner.eval_into(focus, data, out, dedup_from, reverse);
            }
        }
    }

    /// Transitive closure of `self` from `focus`, excluding `focus` unless it is
    /// genuinely reachable from itself via a cycle.
    fn closure(
        &self,
        focus: TermId,
        data: &Graph,
        out: &mut Vec<TermId>,
        dedup_from: usize,
        reverse: bool,
    ) {
        // `seen` tracks expansion, kept separate from `out` because `out` may
        // already contain unrelated results from a sibling alternative.
        let mut seen: Vec<TermId> = Vec::new();
        let mut queue = vec![focus];
        let mut step = Vec::new();

        while let Some(node) = queue.pop() {
            step.clear();
            self.eval_into(node, data, &mut step, 0, reverse);
            for &next in &step {
                if seen.contains(&next) {
                    continue;
                }
                seen.push(next);
                queue.push(next);
                push_unique(out, dedup_from, next);
            }
        }
    }
}

#[inline]
fn push_unique(out: &mut Vec<TermId>, from: usize, value: TermId) {
    if !out[from..].contains(&value) {
        out.push(value);
    }
}

fn dedup_in_place(v: &mut Vec<TermId>) {
    let mut i = 0;
    while i < v.len() {
        if v[..i].contains(&v[i]) {
            v.remove(i);
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{loader, GraphBuilder};
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
            loader::parse_str(turtle, RdfFormat::Turtle, "http://t/", 0, &mut store, &mut b)
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

        fn eval(&mut self, path: &Path, focus: &str) -> Vec<String> {
            let f = self.iri(focus);
            let mut out = Vec::new();
            path.eval(f, &self.graph, &mut out);
            out.iter()
                .map(|&t| self.store.lexical_form(t).unwrap_or("?").to_string())
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
        assert_eq!(f.eval(&p, "http://ex/a"), vec!["http://ex/b", "http://ex/c"]);
        assert!(f.eval(&p, "http://ex/none").is_empty());
    }

    #[test]
    fn evaluates_an_inverse_path() {
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path [ sh:inversePath ex:p ] . ex:a ex:p ex:b . ex:c ex:p ex:b ."
        ));
        let p = f.path().unwrap();
        assert_eq!(p, Path::Inverse(Box::new(Path::Predicate(f.iri("http://ex/p")))));
        assert_eq!(f.eval(&p, "http://ex/b"), vec!["http://ex/a", "http://ex/c"]);
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
        assert_eq!(f.eval(&p, "http://ex/a"), vec!["http://ex/b", "http://ex/c"]);
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
        assert_eq!(f.eval(&p, "http://ex/a"), vec!["http://ex/b", "http://ex/c"]);
    }

    #[test]
    fn zero_or_one_takes_at_most_one_step() {
        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path [ sh:zeroOrOnePath ex:p ] . ex:a ex:p ex:b . ex:b ex:p ex:c ."
        ));
        let p = f.path().unwrap();
        assert_eq!(f.eval(&p, "http://ex/a"), vec!["http://ex/a", "http://ex/b"]);
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
    fn rejects_malformed_paths() {
        let mut f = Fixture::new(&format!("{PREFIX} ex:S sh:path 42 ."));
        assert!(matches!(f.path(), Err(Error::Shape(_))), "literal path");

        let mut f = Fixture::new(&format!(
            "{PREFIX} ex:S sh:path [ sh:alternativePath ( ex:p ) ] ."
        ));
        assert!(matches!(f.path(), Err(Error::Shape(_))), "one alternative");

        let mut f = Fixture::new(&format!("{PREFIX} ex:S sh:path [ ex:bogus true ] ."));
        assert!(matches!(f.path(), Err(Error::Shape(_))), "not a path at all");
    }
}
