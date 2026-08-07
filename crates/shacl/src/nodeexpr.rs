//! SHACL 1.2 node expressions.
//!
//! A node expression maps a focus node to a *sequence* of nodes. Sequence
//! rather than set matters: `shnex:orderBy` and `shnex:limit` are meaningless
//! over a set and `shnex:count` counts duplicates, so this evaluator returns a
//! `Vec` and deduplicates only where an operator asks it to.
//!
//! The store is threaded mutably because operators such as `shnex:count` and
//! `shnex:concat` compute terms that need not occur anywhere in the graph.

use crate::error::{Error, Result};
use crate::model::vocab::XSD;
use crate::model::{Graph, TermId, TermStore, Vocab};
use crate::path::Path;

macro_rules! shnex_vocab {
    ($ns:literal { $( $field:ident = $local:literal ),* $(,)? }) => {
        /// The `shnex:` namespace.
        pub const SHNEX: &str = $ns;

        /// Interned handles for the node expression vocabulary.
        #[derive(Debug, Clone)]
        pub struct Shnex { $( pub $field: TermId, )* }

        impl Shnex {
            pub fn new(store: &mut TermStore) -> Self {
                Self { $( $field: store.named_node(concat!($ns, $local)), )* }
            }
        }
    };
}

shnex_vocab! {
    "http://www.w3.org/ns/shacl-node-expr#" {
    nodes = "nodes",
    path_values = "pathValues",
    focus_node = "focusNode",
    count = "count",
    distinct = "distinct",
    exists = "exists",
    if_ = "if",
    then = "then",
    else_ = "else",
    concat = "concat",
    sum = "sum",
    min = "min",
    max = "max",
    limit = "limit",
    offset = "offset",
    intersection = "intersection",
    remove = "remove",
    instances_of = "instancesOf",
    order_by = "orderBy",
    desc = "desc",
    }
}

/// The graphs and vocabulary an expression evaluates against.
pub struct Ctx<'a> {
    pub data: &'a Graph,
    /// Where the expression itself is written. Not always the data graph: a
    /// shapes graph carries its own expressions.
    pub exprs: &'a Graph,
    pub vocab: &'a Vocab,
    pub shnex: &'a Shnex,
}

/// Evaluates the node expression at `node` for `focus`.
pub fn eval(
    node: TermId,
    focus: Option<TermId>,
    ctx: &Ctx<'_>,
    store: &mut TermStore,
) -> Result<Vec<TermId>> {
    eval_at(node, focus, ctx, store, 0)
}

fn eval_at(
    node: TermId,
    focus: Option<TermId>,
    ctx: &Ctx<'_>,
    store: &mut TermStore,
    depth: u32,
) -> Result<Vec<TermId>> {
    const MAX_DEPTH: u32 = 64;
    if depth > MAX_DEPTH {
        return Err(Error::Shape("node expression nested too deeply".into()));
    }
    let g = ctx.exprs;
    let s = ctx.shnex;

    // Anything that is not a blank node carrying an operator is a constant
    // standing for itself.
    if !store.is_blank(node) {
        return Ok(vec![node]);
    }

    macro_rules! sub {
        ($n:expr, $f:expr) => {
            eval_at($n, $f, ctx, store, depth + 1)?
        };
    }
    /// The operand, which defaults to the focus node when `shnex:nodes` is
    /// absent.
    macro_rules! operand {
        () => {
            match g.object(node, s.nodes) {
                Some(n) => sub!(n, focus),
                None => focus.into_iter().collect::<Vec<_>>(),
            }
        };
    }

    if let Some(path_node) = g.object(node, s.path_values) {
        let starts = match g.object(node, s.focus_node) {
            Some(f) => sub!(f, focus),
            None => focus.into_iter().collect::<Vec<_>>(),
        };
        let path = Path::compile(path_node, g, store, ctx.vocab)?;
        let mut out = Vec::new();
        for start in starts {
            path.eval(start, ctx.data, &mut out);
        }
        return Ok(out);
    }

    if let Some(inner) = g.object(node, s.count) {
        let n = sub!(inner, focus).len();
        return Ok(vec![int_literal(n as i64, store)]);
    }

    if let Some(inner) = g.object(node, s.distinct) {
        let mut values = sub!(inner, focus);
        let mut seen = Vec::new();
        values.retain(|v| {
            let fresh = !seen.contains(v);
            if fresh {
                seen.push(*v);
            }
            fresh
        });
        return Ok(values);
    }

    if let Some(inner) = g.object(node, s.exists) {
        let any = !sub!(inner, focus).is_empty();
        return Ok(vec![bool_literal(any, store)]);
    }

    if let Some(cond) = g.object(node, s.if_) {
        let test = sub!(cond, focus);
        let truthy = test
            .first()
            .is_some_and(|&t| store.lexical_form(t) == Some("true"));
        let branch = if truthy { s.then } else { s.else_ };
        return Ok(match g.object(node, branch) {
            Some(b) => sub!(b, focus),
            None => Vec::new(),
        });
    }

    if let Some(list) = g.object(node, s.concat) {
        let parts = g
            .list(list, ctx.vocab)
            .ok_or_else(|| Error::Shape("shnex:concat needs a list".into()))?;
        let mut text = String::new();
        for part in parts {
            for v in sub!(part, focus) {
                text.push_str(store.lexical_form(v).unwrap_or_default());
            }
        }
        return Ok(vec![store.literal(&text, &format!("{XSD}string"), None)]);
    }

    if let Some(inner) = g.object(node, s.sum) {
        let values = sub!(inner, focus);
        let total: f64 = values.iter().filter_map(|&v| number(v, store)).sum();
        return Ok(vec![number_literal(total, store)]);
    }

    for (op, want_min) in [(s.min, true), (s.max, false)] {
        if let Some(inner) = g.object(node, op) {
            let values = sub!(inner, focus);
            let best = values.iter().copied().reduce(|a, b| {
                match (number(a, store), number(b, store)) {
                    // Non-numeric operands cannot be ordered, so the first
                    // value simply wins rather than the comparison erroring.
                    (Some(x), Some(y)) if (x < y) == want_min => a,
                    (Some(_), Some(_)) => b,
                    _ => a,
                }
            });
            return Ok(best.into_iter().collect());
        }
    }

    if let Some(n) = g.object(node, s.limit) {
        let k = integer(n, store);
        let mut values = operand!();
        if let Some(k) = k {
            values.truncate(k.max(0) as usize);
        }
        return Ok(values);
    }
    if let Some(n) = g.object(node, s.offset) {
        let k = integer(n, store).unwrap_or(0).max(0) as usize;
        let values = operand!();
        return Ok(values.into_iter().skip(k).collect());
    }

    if let Some(other) = g.object(node, s.intersection) {
        let a = operand!();
        let b = sub!(other, focus);
        return Ok(a.into_iter().filter(|x| b.contains(x)).collect());
    }
    if let Some(other) = g.object(node, s.remove) {
        let a = operand!();
        let b = sub!(other, focus);
        return Ok(a.into_iter().filter(|x| !b.contains(x)).collect());
    }

    if let Some(class_expr) = g.object(node, s.instances_of) {
        let classes = sub!(class_expr, focus);
        let mut out = Vec::new();
        for c in classes {
            for sub_class in subclasses(ctx, c) {
                out.extend(ctx.data.subjects(ctx.vocab.rdf_type, sub_class));
            }
        }
        out.sort_unstable();
        out.dedup();
        return Ok(out);
    }

    if g.object(node, s.order_by).is_some() {
        let descending = g
            .object(node, s.desc)
            .and_then(|d| store.lexical_form(d))
            .is_some_and(|t| t == "true");
        let mut values = operand!();
        values.sort_by(|&a, &b| {
            let o = crate::datatypes::compare(a, b, store, ctx.vocab)
                .unwrap_or(std::cmp::Ordering::Equal);
            if descending { o.reverse() } else { o }
        });
        return Ok(values);
    }

    // `sparql:someFunction ( a b )` exposes the SPARQL function library as a
    // node expression. Rather than reimplementing sixty-odd builtins, the call
    // is rebuilt as a SPARQL expression and handed to the evaluator.
    if let Some((func, args_list)) = sparql_call(node, ctx, store) {
        let args = g
            .list(args_list, ctx.vocab)
            .ok_or_else(|| Error::Shape("a sparql: call needs a list of arguments".into()))?;
        let mut values = Vec::new();
        for arg in args {
            // Each argument is itself a node expression. Only its first value
            // participates: SPARQL functions take terms, not sequences.
            values.push(sub!(arg, focus).into_iter().next());
        }
        return eval_sparql_call(&func, &values, store);
    }

    // A blank node heading an RDF list is a sequence of expressions, evaluated
    // and concatenated. `()` is `rdf:nil`, an IRI, so it is a constant and
    // never reaches here.
    if g.object(node, ctx.vocab.rdf_first).is_some() {
        if let Some(items) = g.list(node, ctx.vocab) {
            let mut out = Vec::new();
            for item in items {
                out.extend(sub!(item, focus));
            }
            return Ok(out);
        }
    }

    // A blank node with no triples at all denotes the empty sequence.
    if !g.has_subject(node) {
        return Ok(Vec::new());
    }

    Err(Error::Shape("unsupported node expression".into()))
}

/// The `sparql:` namespace, whose predicates name SPARQL functions.
const SPARQL_NS: &str = "http://www.w3.org/ns/sparql#";

/// Finds a `sparql:` function call on `node`, as `(local name, argument list)`.
fn sparql_call(node: TermId, ctx: &Ctx<'_>, store: &TermStore) -> Option<(String, TermId)> {
    ctx.exprs.predicate_objects(node).find_map(|(p, o)| {
        let iri = store.iri(p)?;
        let local = iri.strip_prefix(SPARQL_NS)?;
        Some((local.to_string(), o))
    })
}

/// Rebuilds a `sparql:` call as SPARQL expression text and evaluates it.
///
/// The argument terms are written in N-Triples form, which is a subset of
/// SPARQL term syntax, so no separate serialiser is needed.
fn eval_sparql_call(
    func: &str,
    args: &[Option<TermId>],
    store: &mut TermStore,
) -> Result<Vec<TermId>> {
    // An argument that produced no value makes the whole call undefined.
    let mut rendered = Vec::with_capacity(args.len());
    for a in args {
        match a {
            Some(t) => rendered.push(store.to_oxrdf(*t).to_string()),
            None => return Ok(Vec::new()),
        }
    }

    let expr = match sparql_operator(func) {
        // Infix and prefix operators have no call syntax in SPARQL.
        Some(Operator::Infix(op)) if rendered.len() == 2 => {
            format!("({} {} {})", rendered[0], op, rendered[1])
        }
        Some(Operator::Prefix(op)) if rendered.len() == 1 => {
            format!("({}{})", op, rendered[0])
        }
        Some(_) => {
            return Err(Error::Shape(format!(
                "sparql:{func} was given {} arguments",
                rendered.len()
            )))
        }
        None => format!("{}({})", sparql_function_name(func), rendered.join(", ")),
    };

    // An empty WHERE yields exactly one solution, so the expression is
    // evaluated once with nothing bound.
    let query = crate::sparql::parse_query("", &format!("SELECT ({expr} AS ?r) WHERE {{}}"))?;
    let empty = crate::model::GraphBuilder::new().build();
    let rows = crate::sparql::run(&query, &[], &empty, store)?;

    // A function that errors binds nothing, which is an empty sequence rather
    // than a failure — `sparql:bound` of an unbound variable relies on this.
    let Some(term) = rows.first().and_then(|r| r.get("r")).cloned() else {
        return Ok(Vec::new());
    };
    Ok(vec![store.intern_oxrdf(term.as_ref(), crate::model::scope::SPARQL)])
}

enum Operator {
    Infix(&'static str),
    Prefix(&'static str),
}

/// Operator aliases. SHACL names these as functions, but SPARQL only has
/// syntax for them.
fn sparql_operator(func: &str) -> Option<Operator> {
    Some(match func {
        "greater-than" => Operator::Infix(">"),
        "greater-than-or-equal" => Operator::Infix(">="),
        "less-than" => Operator::Infix("<"),
        "less-than-or-equal" => Operator::Infix("<="),
        "equals" => Operator::Infix("="),
        "not-equals" => Operator::Infix("!="),
        "plus" => Operator::Infix("+"),
        "subtract" => Operator::Infix("-"),
        "multiply" => Operator::Infix("*"),
        "divide" => Operator::Infix("/"),
        "logical-and" => Operator::Infix("&&"),
        "logical-or" => Operator::Infix("||"),
        "unary-minus" => Operator::Prefix("-"),
        "unary-plus" => Operator::Prefix("+"),
        "logical-not" => Operator::Prefix("!"),
        _ => return None,
    })
}

/// The SPARQL spelling of a function whose SHACL name differs.
fn sparql_function_name(func: &str) -> &str {
    match func {
        "encode" => "ENCODE_FOR_URI",
        "uri" => "IRI",
        "sameValue" => "sameTerm",
        other => other,
    }
}

fn subclasses(ctx: &Ctx<'_>, class: TermId) -> Vec<TermId> {
    let mut seen = vec![class];
    let mut queue = vec![class];
    while let Some(c) = queue.pop() {
        for sub in ctx.data.subjects(ctx.vocab.rdfs_subClassOf, c) {
            if !seen.contains(&sub) {
                seen.push(sub);
                queue.push(sub);
            }
        }
    }
    seen
}

/// Both operands as numbers, or `None` if either is not numeric.
fn number(t: TermId, store: &TermStore) -> Option<f64> {
    store.lexical_form(t)?.parse().ok()
}

fn integer(t: TermId, store: &TermStore) -> Option<i64> {
    store.lexical_form(t)?.parse().ok()
}

fn int_literal(n: i64, store: &mut TermStore) -> TermId {
    store.literal(&n.to_string(), &format!("{XSD}integer"), None)
}

fn bool_literal(b: bool, store: &mut TermStore) -> TermId {
    store.literal(
        if b { "true" } else { "false" },
        &format!("{XSD}boolean"),
        None,
    )
}

fn number_literal(n: f64, store: &mut TermStore) -> TermId {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        int_literal(n as i64, store)
    } else {
        store.literal(&n.to_string(), &format!("{XSD}decimal"), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{loader, GraphBuilder};
    use oxrdfio::RdfFormat;

    const PREFIX: &str = "@prefix shnex: <http://www.w3.org/ns/shacl-node-expr#> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
        @prefix ex: <http://ex/> . ";

    struct F {
        store: TermStore,
        vocab: Vocab,
        shnex: Shnex,
        graph: Graph,
    }

    impl F {
        fn new(turtle: &str) -> Self {
            let mut store = TermStore::new();
            let vocab = Vocab::new(&mut store);
            let shnex = Shnex::new(&mut store);
            let mut b = GraphBuilder::new();
            loader::parse_str(
                &format!("{PREFIX}{turtle}"),
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
                shnex,
                graph: b.build(),
            }
        }

        /// Evaluates the expression at `ex:E`'s `ex:expr`, for an optional focus.
        fn eval(&mut self, focus: Option<&str>) -> Result<Vec<String>> {
            let e = self.store.named_node("http://ex/E");
            let p = self.store.named_node("http://ex/expr");
            let node = self.graph.object(e, p).expect("ex:E ex:expr");
            let focus = focus.map(|f| self.store.named_node(f));
            let ctx = Ctx {
                data: &self.graph,
                exprs: &self.graph,
                vocab: &self.vocab,
                shnex: &self.shnex,
            };
            let out = eval(node, focus, &ctx, &mut self.store)?;
            Ok(out
                .iter()
                .map(|&t| self.store.lexical_form(t).unwrap_or("?").to_string())
                .collect())
        }
    }

    #[test]
    fn a_constant_stands_for_itself() {
        let mut f = F::new("ex:E ex:expr ex:Something .");
        assert_eq!(f.eval(None).unwrap(), vec!["http://ex/Something"]);

        let mut f = F::new("ex:E ex:expr 42 .");
        assert_eq!(f.eval(None).unwrap(), vec!["42"]);
    }

    #[test]
    fn path_values_walks_from_the_focus_node() {
        let mut f = F::new(
            "ex:E ex:expr [ shnex:pathValues rdfs:label ] .
             ex:TestNode rdfs:label \"test node\" .",
        );
        assert_eq!(f.eval(Some("http://ex/TestNode")).unwrap(), vec!["test node"]);
        assert!(f.eval(Some("http://ex/Absent")).unwrap().is_empty());
    }

    #[test]
    fn count_returns_a_computed_literal() {
        // The result is not a term anywhere in the graph, which is why the
        // evaluator needs a mutable store.
        let mut f = F::new(
            "ex:E ex:expr [ shnex:count [ shnex:pathValues ex:p ] ] .
             ex:A ex:p ex:x, ex:y, ex:z .",
        );
        assert_eq!(f.eval(Some("http://ex/A")).unwrap(), vec!["3"]);
        assert_eq!(f.eval(Some("http://ex/None")).unwrap(), vec!["0"]);
    }

    #[test]
    fn distinct_preserves_order_of_first_appearance() {
        let mut f = F::new(
            "ex:E ex:expr [ shnex:distinct [ shnex:pathValues ex:p ] ] .
             ex:A ex:p ex:x, ex:y .",
        );
        let got = f.eval(Some("http://ex/A")).unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn exists_reports_a_boolean() {
        let mut f = F::new(
            "ex:E ex:expr [ shnex:exists [ shnex:pathValues ex:p ] ] .
             ex:A ex:p ex:x .",
        );
        assert_eq!(f.eval(Some("http://ex/A")).unwrap(), vec!["true"]);
        assert_eq!(f.eval(Some("http://ex/B")).unwrap(), vec!["false"]);
    }

    #[test]
    fn if_selects_a_branch() {
        let mut f = F::new(
            "ex:E ex:expr [ shnex:if [ shnex:exists [ shnex:pathValues ex:p ] ] ;
                            shnex:then \"yes\" ; shnex:else \"no\" ] .
             ex:A ex:p ex:x .",
        );
        assert_eq!(f.eval(Some("http://ex/A")).unwrap(), vec!["yes"]);
        assert_eq!(f.eval(Some("http://ex/B")).unwrap(), vec!["no"]);
    }

    #[test]
    fn limit_and_offset_slice_the_sequence() {
        let data = "ex:A ex:p 1, 2, 3 .";
        let mut f = F::new(&format!(
            "ex:E ex:expr [ shnex:limit 2 ; shnex:nodes [ shnex:pathValues ex:p ] ] . {data}"
        ));
        assert_eq!(f.eval(Some("http://ex/A")).unwrap().len(), 2);

        let mut f = F::new(&format!(
            "ex:E ex:expr [ shnex:offset 2 ; shnex:nodes [ shnex:pathValues ex:p ] ] . {data}"
        ));
        assert_eq!(f.eval(Some("http://ex/A")).unwrap().len(), 1);
    }

    #[test]
    fn rejects_an_unknown_operator() {
        let mut f = F::new("ex:E ex:expr [ ex:notAnOperator true ] .");
        assert!(matches!(f.eval(None), Err(Error::Shape(_))));
    }
}
