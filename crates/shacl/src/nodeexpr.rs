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
    var = "var",
    flat_map = "flatMap",
    find_first = "findFirst",
    conforms_to_shape = "conformsToShape",
    filter_shape = "filterShape",
    nodes_matching = "nodesMatching",
    match_all = "matchAll",
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
    /// The compiled shapes that shape-valued operators test against. `None`
    /// makes every shape vacuously satisfied.
    pub shapes: Option<&'a crate::shapes::Shapes>,
    /// Variable bindings visible to `shnex:var`, beyond `focusNode`.
    pub vars: &'a [(String, TermId)],
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
    let v = ctx.vocab;

    // SHACL-AF's focus node expression. Checked before the constant rule
    // below, because `sh:this` is an IRI and would otherwise stand for itself.
    if node == v.sh_this {
        return Ok(focus.into_iter().collect());
    }

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
        // Sequence concatenation, not string concatenation — that is
        // `sparql:concat`, in the other namespace.
        let parts = g
            .list(list, ctx.vocab)
            .ok_or_else(|| Error::Shape("shnex:concat needs a list".into()))?;
        let mut out = Vec::new();
        for part in parts {
            out.extend(sub!(part, focus));
        }
        return Ok(out);
    }

    if let Some(inner) = g.object(node, s.sum) {
        let values = sub!(inner, focus);
        // The result's datatype follows the inputs: summing decimals yields a
        // decimal even when the total happens to be whole.
        let all_integers = values
            .iter()
            .all(|&v| store.datatype(v) == Some(ctx.vocab.xsd_integer));
        let total: f64 = values.iter().filter_map(|&v| number(v, store)).sum();
        return Ok(vec![if all_integers {
            int_literal(total as i64, store)
        } else {
            decimal_literal(total, store)
        }]);
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

    if let Some(list) = g.object(node, s.intersection) {
        // A list of sequences, all intersected — not one sequence intersected
        // with `shnex:nodes`.
        let members = g
            .list(list, ctx.vocab)
            .ok_or_else(|| Error::Shape("shnex:intersection needs a list".into()))?;
        let mut acc: Option<Vec<TermId>> = None;
        for m in members {
            let vs = sub!(m, focus);
            acc = Some(match acc {
                None => vs,
                Some(prev) => prev.into_iter().filter(|x| vs.contains(x)).collect(),
            });
        }
        // An intersection is a set: duplicates in the operands must not
        // survive into the result.
        let mut out = acc.unwrap_or_default();
        let mut seen = Vec::new();
        out.retain(|v| {
            let fresh = !seen.contains(v);
            if fresh {
                seen.push(*v);
            }
            fresh
        });
        return Ok(out);
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

    if let Some(key_expr) = g.object(node, s.order_by) {
        // `shnex:orderBy` names a *key* expression evaluated per node, not the
        // nodes themselves. A node with no key sorts first.
        let descending = g
            .object(node, s.desc)
            .and_then(|d| store.lexical_form(d))
            .is_some_and(|t| t == "true");
        let values = operand!();
        let mut keyed = Vec::with_capacity(values.len());
        for v in values {
            let key = sub!(key_expr, Some(v)).into_iter().next();
            keyed.push((key, v));
        }
        keyed.sort_by(|(a, _), (b, _)| {
            let o = match (a, b) {
                (Some(x), Some(y)) => crate::datatypes::compare(*x, *y, store, ctx.vocab)
                    .unwrap_or(std::cmp::Ordering::Equal),
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            };
            if descending { o.reverse() } else { o }
        });
        return Ok(keyed.into_iter().map(|(_, v)| v).collect());
    }

    if let Some(name) = g.object(node, s.var) {
        let name = store.lexical_form(name).unwrap_or_default().to_string();
        // `focusNode` is always in scope; everything else comes from the
        // surrounding bindings.
        if name == "focusNode" {
            return Ok(focus.into_iter().collect());
        }
        return Ok(ctx
            .vars
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t)
            .into_iter()
            .collect());
    }

    if let Some(body) = g.object(node, s.flat_map) {
        // Each input node becomes the focus for one evaluation of the body,
        // and the results are concatenated.
        let inputs = operand!();
        let mut out = Vec::new();
        for input in inputs {
            out.extend(sub!(body, Some(input)));
        }
        return Ok(out);
    }

    // --- shape-valued operators
    if let Some(args) = g.object(node, s.conforms_to_shape) {
        let pair = g
            .list(args, ctx.vocab)
            .ok_or_else(|| Error::Shape("shnex:conformsToShape needs a list".into()))?;
        let [node_expr, shape_expr] = pair.as_slice() else {
            return Err(Error::Shape(
                "shnex:conformsToShape takes a node and a shape".into(),
            ));
        };
        let nodes = sub!(*node_expr, focus);
        let shapes = sub!(*shape_expr, focus);
        let (Some(&n), Some(&sh)) = (nodes.first(), shapes.first()) else {
            return Ok(Vec::new());
        };
        let ok = conforms(n, sh, ctx, store)?;
        return Ok(vec![bool_literal(ok, store)]);
    }

    if let Some(shape) = g.object(node, s.filter_shape) {
        let inputs = operand!();
        let mut out = Vec::new();
        for n in inputs {
            if conforms(n, shape, ctx, store)? {
                out.push(n);
            }
        }
        return Ok(out);
    }

    if let Some(shape) = g.object(node, s.find_first) {
        let inputs = operand!();
        for n in inputs {
            if conforms(n, shape, ctx, store)? {
                return Ok(vec![n]);
            }
        }
        return Ok(Vec::new());
    }

    if let Some(shape) = g.object(node, s.match_all) {
        let inputs = operand!();
        let mut all = true;
        for n in inputs {
            if !conforms(n, shape, ctx, store)? {
                all = false;
                break;
            }
        }
        return Ok(vec![bool_literal(all, store)]);
    }

    if let Some(shape) = g.object(node, s.nodes_matching) {
        // No operand: this ranges over every node in the data graph.
        let mut out = Vec::new();
        for n in all_nodes(ctx.data) {
            if conforms(n, shape, ctx, store)? {
                out.push(n);
            }
        }
        return Ok(out);
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

        // Two functions exist precisely to inspect absence, so they cannot go
        // through the generic path, which treats a missing argument as making
        // the whole call undefined.
        match func.as_str() {
            "bound" => {
                let bound = values.first().is_some_and(Option::is_some);
                return Ok(vec![bool_literal(bound, store)]);
            }
            "coalesce" => {
                return Ok(values.into_iter().flatten().take(1).collect());
            }
            // The term tests are answered from the store rather than by SPARQL.
            // They are trivial, and one of their arguments may be a blank node,
            // which has no expression form: `isBLANK(_:b0)` does not parse.
            "isBlank" | "isIRI" | "isURI" | "isLiteral" => {
                let Some(Some(v)) = values.first() else {
                    return Ok(Vec::new());
                };
                let kind = store.kind(*v);
                let yes = match func.as_str() {
                    "isBlank" => kind == crate::model::TermKind::Blank,
                    "isLiteral" => kind == crate::model::TermKind::Literal,
                    _ => kind == crate::model::TermKind::Iri,
                };
                return Ok(vec![bool_literal(yes, store)]);
            }
            // SHACL names these but SPARQL has no such functions. `hasLang`
            // asks whether a literal carries a given language range;
            // `hasLangdir` takes one argument and asks only whether a base
            // direction is present at all.
            "hasLang" => {
                let (Some(Some(v)), Some(Some(want))) = (values.first(), values.get(1)) else {
                    return Ok(Vec::new());
                };
                let tag = store.language(*v).unwrap_or_default().to_string();
                let want = store.lexical_form(*want).unwrap_or_default().to_string();
                let yes = crate::datatypes::language_matches(&tag, &want);
                return Ok(vec![bool_literal(yes, store)]);
            }
            "hasLangdir" => {
                let Some(Some(v)) = values.first() else {
                    return Ok(Vec::new());
                };
                let yes = store.direction(*v).is_some();
                return Ok(vec![bool_literal(yes, store)]);
            }
            _ => {}
        }
        return eval_sparql_call(&func, &values, store, ctx.vocab);
    }

    // A blank node heading an RDF list is a sequence of expressions, evaluated
    // and concatenated. `()` is `rdf:nil`, an IRI, so it is a constant and
    // never reaches here.
    if g.object(node, ctx.vocab.rdf_first).is_some()
        && let Some(items) = g.list(node, ctx.vocab)
    {
        let mut out = Vec::new();
        for item in items {
            out.extend(sub!(item, focus));
        }
        return Ok(out);
    }

    // ---------------------------------------------- SHACL-AF node expressions
    //
    // A smaller algebra than the SHACL 1.2 one above, in the `sh:` namespace
    // rather than `shnex:`, and used by SHACL-AF rules. Handled here rather
    // than in a separate evaluator so a rule can nest either flavour.

    /// The operand of an AF expression: `sh:nodes`, defaulting to the focus.
    macro_rules! af_nodes {
        ($required:expr) => {
            match g.object(node, v.sh_nodes) {
                Some(n) => sub!(n, focus),
                None if $required => {
                    return Err(Error::Shape(
                        "sh:filterShape node expression needs sh:nodes".into(),
                    ));
                }
                None => focus.into_iter().collect::<Vec<_>>(),
            }
        };
    }

    // Path expression: the values of `sh:path` reached from `sh:nodes`.
    if let Some(path_node) = g.object(node, v.sh_path) {
        let starts = af_nodes!(false);
        let path = Path::compile(path_node, g, store, v)?;
        let mut out = Vec::new();
        for start in starts {
            path.eval(start, ctx.data, &mut out);
        }
        out.sort_unstable();
        out.dedup();
        return Ok(out);
    }

    // Filter shape: those of `sh:nodes` that conform to `sh:filterShape`.
    if let Some(shape_node) = g.object(node, v.sh_filterShape) {
        let candidates = af_nodes!(true);
        let Some(shapes) = ctx.shapes else {
            // No compiled shapes to test against, so every node is vacuously
            // conforming — the same reading the rest of this module takes.
            return Ok(candidates);
        };
        let mut out = Vec::new();
        for n in candidates {
            if crate::validate::node_conforms(n, shape_node, ctx.data, shapes, store, v)? {
                out.push(n);
            }
        }
        return Ok(out);
    }

    // Union and intersection over a list of expressions.
    if let Some(list) = g.object(node, v.sh_union)
        && let Some(items) = g.list(list, v)
    {
        let mut out = Vec::new();
        for item in items {
            out.extend(sub!(item, focus));
        }
        out.sort_unstable();
        out.dedup();
        return Ok(out);
    }
    if let Some(list) = g.object(node, v.sh_intersection)
        && let Some(items) = g.list(list, v)
    {
        let mut sets = items
            .into_iter()
            .map(|item| eval_at(item, focus, ctx, store, depth + 1));
        let Some(first) = sets.next() else {
            return Ok(Vec::new());
        };
        let mut out = first?;
        for rest in sets {
            let rest = rest?;
            out.retain(|n| rest.contains(n));
        }
        out.sort_unstable();
        out.dedup();
        return Ok(out);
    }

    // A blank node with no triples at all denotes the empty sequence.
    if !g.has_subject(node) {
        return Ok(Vec::new());
    }

    Err(Error::Shape("unsupported node expression".into()))
}

/// Whether `node` conforms to the shape declared at `shape`.
fn conforms(node: TermId, shape: TermId, ctx: &Ctx<'_>, store: &mut TermStore) -> Result<bool> {
    match ctx.shapes {
        Some(shapes) => {
            crate::validate::node_conforms(node, shape, ctx.data, shapes, store, ctx.vocab)
        }
        // Without compiled shapes there is nothing to violate.
        None => Ok(true),
    }
}

/// Every distinct term appearing anywhere in `g`, which is the universe
/// `shnex:nodesMatching` ranges over.
fn all_nodes(g: &Graph) -> Vec<TermId> {
    let mut out = Vec::with_capacity(g.len() * 2);
    for [s, _, o] in g.iter() {
        out.push(s);
        out.push(o);
    }
    out.sort_unstable();
    out.dedup();
    out
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
    vocab: &Vocab,
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
            )));
        }
        None => format!("{}({})", sparql_function_name(func), rendered.join(", ")),
    };

    // An empty WHERE yields exactly one solution, so the expression is
    // evaluated once with nothing bound.
    let text = format!("SELECT ({expr} AS ?r) WHERE {{}}");
    let query = crate::sparql::parse_query("", &text)
        .map_err(|e| Error::Sparql(format!("{e} in generated query: {text}")))?;
    let empty = crate::model::GraphBuilder::new().build();
    let rows = crate::sparql::run(&query, &[], &empty, store)?;

    // A function that errors binds nothing, which is an empty sequence rather
    // than a failure — `sparql:bound` of an unbound variable relies on this.
    let Some(term) = rows.first().and_then(|r| r.get("r")).cloned() else {
        return Ok(Vec::new());
    };
    let interned = store.intern_oxrdf(term.as_ref(), crate::model::scope::SPARQL);
    Ok(vec![if canonicalises_decimals(func) {
        canonicalise(interned, store, vocab)
    } else {
        interned
    }])
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
///
/// The type-test predicates are the awkward ones: SPARQL capitalises their
/// suffixes, so `isBlank` has to be emitted as `isBLANK`.
fn sparql_function_name(func: &str) -> &str {
    match func {
        "encode" => "ENCODE_FOR_URI",
        "uri" => "IRI",
        "sameValue" => "sameTerm",
        "isBlank" => "isBLANK",
        "isLiteral" => "isLITERAL",
        "isNumeric" => "isNUMERIC",
        "isTriple" => "isTRIPLE",
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

/// An `xsd:decimal` in canonical form, which always carries a fraction digit:
/// the canonical spelling of four is `4.0`, not `4`.
fn decimal_literal(n: f64, store: &mut TermStore) -> TermId {
    let mut lex = n.to_string();
    if !lex.contains('.') {
        lex.push_str(".0");
    }
    store.literal(&lex, &format!("{XSD}decimal"), None)
}

/// Whether a function's result should be rewritten into canonical decimal form.
///
/// Only arithmetic. The date and time accessors return a component of their
/// input and keep its lexical form — `SECONDS` of `…T00:00:00` is `"00"`, and
/// respelling that as `"0.0"` would be wrong even though the value is equal.
fn canonicalises_decimals(func: &str) -> bool {
    matches!(
        func,
        "abs"
            | "ceil"
            | "floor"
            | "round"
            | "divide"
            | "multiply"
            | "plus"
            | "subtract"
            | "unary-minus"
            | "unary-plus"
    )
}

/// Rewrites an `xsd:decimal` into canonical form.
///
/// SPARQL arithmetic yields decimals spelled without a fraction digit, which
/// are the same value but a different term.
fn canonicalise(t: TermId, store: &mut TermStore, vocab: &Vocab) -> TermId {
    if store.datatype(t) != Some(vocab.xsd_decimal) {
        return t;
    }
    let Some(lex) = store.lexical_form(t) else {
        return t;
    };
    if lex.contains('.') {
        return t;
    }
    let lex = format!("{lex}.0");
    store.literal(&lex, &format!("{XSD}decimal"), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GraphBuilder, loader};
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
                shapes: None,
                vars: &[],
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
        assert_eq!(
            f.eval(Some("http://ex/TestNode")).unwrap(),
            vec!["test node"]
        );
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
    fn var_reads_the_focus_node_and_bound_variables() {
        let mut f = F::new("ex:E ex:expr [ shnex:var \"focusNode\" ] .");
        assert_eq!(f.eval(Some("http://ex/A")).unwrap(), vec!["http://ex/A"]);

        let mut f = F::new("ex:E ex:expr [ shnex:var \"absent\" ] .");
        assert!(f.eval(Some("http://ex/A")).unwrap().is_empty());
    }

    #[test]
    fn flat_map_rebinds_the_focus_node_per_input() {
        // Each input becomes the focus for one evaluation of the body, which
        // is what makes shnex:var "focusNode" inside it useful.
        let mut f = F::new(
            "ex:E ex:expr [ shnex:nodes ( ex:A ex:B ) ;
                            shnex:flatMap [ shnex:pathValues ex:p ] ] .
             ex:A ex:p \"a\" . ex:B ex:p \"b\" .",
        );
        assert_eq!(f.eval(None).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn sparql_calls_reach_the_function_library() {
        let mut f = F::new(
            "@prefix sparql: <http://www.w3.org/ns/sparql#> .
             ex:E ex:expr [ sparql:abs ( -42 ) ] .",
        );
        assert_eq!(f.eval(None).unwrap(), vec!["42"]);
    }

    #[test]
    fn sparql_operators_are_emitted_as_syntax() {
        // `greater-than` is named like a function but has no call form.
        let mut f = F::new(
            "@prefix sparql: <http://www.w3.org/ns/sparql#> .
             ex:E ex:expr [ sparql:greater-than ( 10 5 ) ] .",
        );
        assert_eq!(f.eval(None).unwrap(), vec!["true"]);
    }

    #[test]
    fn a_sparql_call_with_no_argument_value_is_empty() {
        let mut f = F::new(
            "@prefix sparql: <http://www.w3.org/ns/sparql#> .
             ex:E ex:expr [ sparql:abs ( [ shnex:var \"absent\" ] ) ] .",
        );
        assert!(f.eval(None).unwrap().is_empty());
    }

    #[test]
    fn rejects_an_unknown_operator() {
        let mut f = F::new("ex:E ex:expr [ ex:notAnOperator true ] .");
        assert!(matches!(f.eval(None), Err(Error::Shape(_))));
    }
}
