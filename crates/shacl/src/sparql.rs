//! SPARQL-based constraints.
//!
//! Two pieces: an adapter that lets a SPARQL engine query the interned graph
//! directly, and pre-binding.
//!
//! Pre-binding is the subtle half. SHACL requires `$this` to be *substituted*
//! into the query, not joined onto it. The difference is observable: a query
//! whose body is `{ FILTER(false) } UNION { FILTER($this = ex:X) }` must see
//! `$this` bound inside both branches, which a `VALUES` clause joined at the
//! top would not deliver, since the union's branches are evaluated
//! independently before the join. `FILTER(bound($this))` likewise rules out
//! textual substitution, because `bound(<iri>)` is not even legal syntax.
//! SEP-0007 substitution, which the evaluator implements natively, has exactly
//! the semantics SHACL asks for.

use std::cell::RefCell;
use std::convert::Infallible;

use hashbrown::HashMap;
use oxrdf::{Term, TermRef, Variable};
use spareval::{QueryEvaluator, QueryResults, QueryableDataset};
use spargebra::{Query, SparqlParser};

use crate::error::{Error, Result};
use crate::model::{Graph, TermId, TermStore, Vocab};

/// Lets the SPARQL evaluator read the interned graph without materialising it.
///
/// `InternalTerm` is [`TermId`], so pattern matching runs straight against the
/// sorted indexes with no term conversion in the loop. Terms the evaluator
/// computes rather than reads — the output of `CONCAT`, say — cannot be in the
/// shared store, so they are parked in a side table addressed by ids above the
/// store's range.
pub struct DataAdapter<'a> {
    graph: &'a Graph,
    store: &'a TermStore,
    /// The shapes graph, reachable as a named graph so a constraint can write
    /// `GRAPH $shapesGraph { … }`. Absent unless the caller supplied it.
    shapes: Option<&'a Graph>,
    /// The id [`SHAPES_GRAPH_IRI`] internalises to, resolved once so the
    /// pattern match is an integer comparison.
    shapes_name: Option<TermId>,
    /// Terms minted during evaluation, addressed as `store.len() + index`.
    extra: RefCell<Vec<Term>>,
}

/// The IRI that names the shapes graph inside a SPARQL constraint.
///
/// SHACL says `$shapesGraph` is bound to an IRI standing for the shapes graph
/// but does not say which, so any stable one will do: what matters is that the
/// same IRI is bound to the variable and used as the graph name, so that
/// `GRAPH $shapesGraph { … }` finds it.
pub const SHAPES_GRAPH_IRI: &str = "urn:x-shacl:shapes-graph";

impl<'a> DataAdapter<'a> {
    pub fn new(graph: &'a Graph, store: &'a TermStore) -> Self {
        Self {
            graph,
            store,
            shapes: None,
            shapes_name: None,
            extra: RefCell::new(Vec::new()),
        }
    }

    /// Exposes `shapes` as the named graph [`SHAPES_GRAPH_IRI`].
    pub fn with_shapes(mut self, shapes: &'a Graph) -> Self {
        let name = Term::from(oxrdf::NamedNode::new_unchecked(SHAPES_GRAPH_IRI));
        // Resolve through the same path the evaluator will, so the id here and
        // the id a `GRAPH` pattern arrives with are the same one.
        let id = self
            .store
            .get_term(name.as_ref())
            .unwrap_or_else(|| self.intern_external(name));
        self.shapes = Some(shapes);
        self.shapes_name = Some(id);
        self
    }

    fn intern_external(&self, term: Term) -> TermId {
        let mut extra = self.extra.borrow_mut();
        if let Some(i) = extra.iter().position(|t| *t == term) {
            return TermId::from_raw(self.store.len() as u32 + i as u32);
        }
        extra.push(term);
        TermId::from_raw(self.store.len() as u32 + extra.len() as u32 - 1)
    }
}

/// Resolves a triple pattern against one graph.
///
/// Picks the index the bound components can seek on, so a pattern with a known
/// subject or object never degenerates into a full scan.
fn match_pattern(
    graph: &Graph,
    subject: Option<&TermId>,
    predicate: Option<&TermId>,
    object: Option<&TermId>,
) -> Vec<[TermId; 3]> {
    match (subject, predicate, object) {
        (Some(&s), Some(&p), Some(&o)) => {
            if graph.contains(s, p, o) {
                vec![[s, p, o]]
            } else {
                Vec::new()
            }
        }
        (Some(&s), Some(&p), None) => graph.objects(s, p).map(|o| [s, p, o]).collect(),
        (None, Some(&p), Some(&o)) => graph.subjects(p, o).map(|s| [s, p, o]).collect(),
        (Some(&s), None, None) => graph.predicate_objects(s).map(|(p, o)| [s, p, o]).collect(),
        (None, None, Some(&o)) => graph
            .subject_predicates(o)
            .map(|(s, p)| [s, p, o])
            .collect(),
        (None, Some(&p), None) => graph.iter().filter(|r| r[1] == p).collect(),
        (Some(&s), None, Some(&o)) => graph
            .predicate_objects(s)
            .filter(|&(_, x)| x == o)
            .map(|(p, _)| [s, p, o])
            .collect(),
        (None, None, None) => graph.iter().collect(),
    }
}

impl<'a> QueryableDataset<'a> for &'a DataAdapter<'a> {
    type InternalTerm = TermId;
    type Error = Infallible;

    fn internal_quads_for_pattern(
        &self,
        subject: Option<&TermId>,
        predicate: Option<&TermId>,
        object: Option<&TermId>,
        graph_name: Option<Option<&TermId>>,
    ) -> impl Iterator<Item = std::result::Result<spareval::InternalQuad<TermId>, Infallible>> + use<'a>
    {
        // Which graphs the pattern may draw from. `None` is "any graph", which
        // is what `GRAPH ?g { … }` asks for and so spans both.
        let mut sources: Vec<(Option<TermId>, &Graph)> = Vec::new();
        match graph_name {
            Some(None) => sources.push((None, self.graph)),
            Some(Some(&g)) => {
                if self.shapes_name == Some(g)
                    && let Some(shapes) = self.shapes
                {
                    sources.push((Some(g), shapes));
                }
            }
            None => {
                sources.push((None, self.graph));
                if let (Some(shapes), Some(name)) = (self.shapes, self.shapes_name) {
                    sources.push((Some(name), shapes));
                }
            }
        }

        let mut rows: Vec<(Option<TermId>, [TermId; 3])> = Vec::new();
        for (name, graph) in sources {
            rows.extend(
                match_pattern(graph, subject, predicate, object)
                    .into_iter()
                    .map(|r| (name, r)),
            );
        }
        rows.into_iter().map(|(g, [s, p, o])| {
            Ok(spareval::InternalQuad {
                subject: s,
                predicate: p,
                object: o,
                graph_name: g,
            })
        })
    }

    fn internalize_term(&self, term: Term) -> std::result::Result<TermId, Infallible> {
        // Anything the store rendered resolves to the handle it already has —
        // a pre-bound focus node, or a term handed back in a solution. That
        // covers blank nodes and RDF 1.2 triple terms, neither of which
        // `get_term` will resolve, and both of which reach here.
        //
        // Only a term the store never produced is parked in the side table.
        Ok(self
            .store
            .resolve_rendered(term.as_ref())
            .unwrap_or_else(|| self.intern_external(term)))
    }

    fn externalize_term(&self, term: TermId) -> std::result::Result<Term, Infallible> {
        let raw = term.as_raw() as usize;
        Ok(if raw < self.store.len() {
            self.store.to_oxrdf(term)
        } else {
            self.extra.borrow()[raw - self.store.len()].clone()
        })
    }
}

/// A compiled SPARQL constraint.
#[derive(Debug, Clone)]
pub struct SparqlConstraint {
    pub query: Query,
    /// True for `sh:ask` validators, which fault when the query answers false.
    pub is_ask: bool,
    /// The `sh:SPARQLConstraint` node, reported as `sh:sourceConstraint`.
    pub source: TermId,
    pub message: Vec<TermId>,
    pub severity: Option<TermId>,
}

/// Reads the `sh:prefixes` declarations reachable from `node` into SPARQL
/// `PREFIX` lines.
///
/// `sh:prefixes` points at an owl:Ontology-ish node carrying `sh:declare`, and
/// the declarations are prepended to the query text before parsing.
pub fn prefix_header(node: TermId, shapes: &Graph, store: &TermStore, vocab: &Vocab) -> String {
    let mut header = String::new();
    let mut seen = Vec::new();
    let mut queue: Vec<TermId> = shapes.objects(node, vocab.sh_prefixes).collect();

    // With no `sh:prefixes` of its own, fall back to every prefix the shapes
    // graph declares. SHACL says a query's prefixes come from `sh:prefixes`,
    // but a graph that declares them at the top — `ex: a sh:ShapesGraph ;
    // sh:declare [ … ]` — and writes `ex:` in a rule is a shape people
    // actually write, and the W3C 1.2 suite contains one. Refusing it means
    // refusing the whole shapes graph over a prefix that is right there.
    if queue.is_empty() {
        queue.extend(shapes.subjects_of(vocab.sh_declare));
    }

    while let Some(owner) = queue.pop() {
        if seen.contains(&owner) {
            continue;
        }
        seen.push(owner);
        // `owl:imports` chains let one prefix set build on another.
        queue.extend(shapes.objects(owner, vocab.owl_imports));

        for decl in shapes.objects(owner, vocab.sh_declare) {
            let prefix = shapes
                .object(decl, vocab.sh_prefix)
                .and_then(|t| store.lexical_form(t));
            let namespace = shapes
                .object(decl, vocab.sh_namespace)
                .and_then(|t| store.lexical_form(t));
            if let (Some(p), Some(ns)) = (prefix, namespace) {
                header.push_str(&format!("PREFIX {p}: <{ns}>\n"));
            }
        }
    }
    header
}

/// Rejects queries SHACL declares incompatible with pre-binding.
///
/// The spec rules out `VALUES`, `MINUS` and `SERVICE` outright, and forbids
/// re-binding a pre-bound variable with `BIND`. These are failures rather than
/// violations: the shape cannot be evaluated at all, so answering either way
/// would be a guess.
fn reject_unsupported(query: &Query, bindings: &[(&str, Term)]) -> Result<()> {
    let pattern = match query {
        Query::Select { pattern, .. }
        | Query::Ask { pattern, .. }
        | Query::Construct { pattern, .. } => pattern,
        _ => return Ok(()),
    };
    let names: Vec<&str> = bindings.iter().map(|(n, _)| *n).collect();
    if let Some(what) = unsupported_in(pattern, &names) {
        return Err(Error::Sparql(format!(
            "{what} cannot be combined with SHACL pre-binding"
        )));
    }
    Ok(())
}

fn unsupported_in(p: &spargebra::algebra::GraphPattern, prebound: &[&str]) -> Option<&'static str> {
    use spargebra::algebra::GraphPattern as G;
    let recurse = |x: &G| unsupported_in(x, prebound);
    match p {
        G::Values { .. } => Some("VALUES"),
        G::Minus { .. } => Some("MINUS"),
        G::Service { .. } => Some("SERVICE"),
        G::Extend {
            inner, variable, ..
        } => {
            if prebound.contains(&variable.as_str()) {
                return Some("re-binding a pre-bound variable");
            }
            recurse(inner)
        }
        G::Join { left, right } | G::Union { left, right } | G::LeftJoin { left, right, .. } => {
            recurse(left).or_else(|| recurse(right))
        }
        G::Filter { inner, .. }
        | G::Graph { inner, .. }
        | G::OrderBy { inner, .. }
        | G::Project { inner, .. }
        | G::Distinct { inner }
        | G::Reduced { inner }
        | G::Slice { inner, .. }
        | G::Group { inner, .. } => recurse(inner),
        _ => None,
    }
}

/// Pre-bound variables and the terms they stand for.
type Binding<'a> = Vec<(&'a str, Term)>;

/// Whether `name` appears anywhere the substitution would reach.
///
/// Runs the same fold with a probe that rewrites nothing, so the answer cannot
/// drift from the positions [`substitute`] actually visits — including the
/// projection, where a variable can appear and nowhere else.
fn mentions(query: &Query, name: &str) -> bool {
    let seen = std::cell::Cell::new(false);
    let probe = |v: &Variable| -> Option<Term> {
        if v.as_str() == name {
            seen.set(true);
        }
        None
    };
    match query {
        Query::Select { pattern, .. }
        | Query::Ask { pattern, .. }
        | Query::Construct { pattern, .. } => {
            fold_pattern(pattern, &probe);
        }
        _ => {}
    }
    seen.get()
}

/// Adds `names` to a `SELECT`'s projection if they are not already there.
///
/// The evaluator will only substitute a variable that its projection produces,
/// so a blank node bound to `$currentShape` — which most constraints never
/// project, and many never mention outside a `GRAPH` block — is rejected
/// outright without this. An `ASK` needs no such help: it collects variables
/// from the whole pattern, having no projection to collect them from.
///
/// Only the outermost projection is touched. A subquery's is a separate scope,
/// and widening it would change which of its bindings escape.
fn ensure_projected(query: &Query, names: &[&str]) -> Query {
    use spargebra::algebra::GraphPattern as G;

    fn widen(
        p: &spargebra::algebra::GraphPattern,
        names: &[&str],
    ) -> spargebra::algebra::GraphPattern {
        match p {
            G::Project { inner, variables } => {
                let mut variables = variables.clone();
                for name in names {
                    let v = Variable::new_unchecked(*name);
                    if !variables.contains(&v) {
                        variables.push(v);
                    }
                }
                G::Project {
                    inner: inner.clone(),
                    variables,
                }
            }
            // The wrappers a projection can sit under. Descend through them and
            // nothing else: past here the tree is the query body, where a
            // `Project` would belong to a subquery.
            G::Distinct { inner } => G::Distinct {
                inner: Box::new(widen(inner, names)),
            },
            G::Reduced { inner } => G::Reduced {
                inner: Box::new(widen(inner, names)),
            },
            G::OrderBy { inner, expression } => G::OrderBy {
                inner: Box::new(widen(inner, names)),
                expression: expression.clone(),
            },
            G::Slice {
                inner,
                start,
                length,
            } => G::Slice {
                inner: Box::new(widen(inner, names)),
                start: *start,
                length: *length,
            },
            other => other.clone(),
        }
    }

    match query {
        Query::Select {
            dataset,
            pattern,
            base_iri,
        } => Query::Select {
            dataset: dataset.clone(),
            pattern: widen(pattern, names),
            base_iri: base_iri.clone(),
        },
        other => other.clone(),
    }
}

/// Substitutes pre-bound variables into the query algebra.
///
/// Doing this here rather than handing the bindings to the evaluator matters
/// for two reasons. The evaluator only substitutes variables that appear in the
/// `SELECT` projection, which rules out `ASK` validators and component
/// parameters entirely. And SHACL's pre-binding means the variable *is* bound,
/// so `FILTER(bound($this))` must pass — whereas any substitution leaves `BOUND`
/// applied to a constant, which answers false. Those calls are folded to `true`
/// in the same pass.
///
/// Replacing the variable throughout the algebra also gives the union
/// behaviour SHACL requires for free: a body of
/// `{ FILTER(false) } UNION { FILTER($this = ex:X) }` has the constant in both
/// branches, where a `VALUES` clause joined outside would reach neither.
fn substitute(query: &Query, bindings: &[(&str, Term)]) -> Query {
    let lookup = |v: &Variable| -> Option<Term> {
        bindings
            .iter()
            .find(|(n, _)| *n == v.as_str())
            .map(|(_, t)| t.clone())
    };
    match query {
        Query::Select {
            dataset,
            pattern,
            base_iri,
        } => Query::Select {
            dataset: dataset.clone(),
            pattern: fold_pattern(pattern, &lookup),
            base_iri: base_iri.clone(),
        },
        Query::Ask {
            dataset,
            pattern,
            base_iri,
        } => Query::Ask {
            dataset: dataset.clone(),
            pattern: fold_pattern(pattern, &lookup),
            base_iri: base_iri.clone(),
        },
        // A SHACL-AF `sh:SPARQLRule` is a CONSTRUCT, and `$this` appears in
        // both halves of it: the template says what to infer, the body says
        // when. Substituting only the body would leave `?this` in the template
        // unbound; substituting neither — which this did before rules existed
        // — turns the whole query into "for every node", which quietly gives
        // the right answer whenever the data happens to hold exactly one
        // match, and the wrong one as soon as it holds two.
        Query::Construct {
            template,
            dataset,
            pattern,
            base_iri,
        } => Query::Construct {
            template: template
                .iter()
                .map(|t| spargebra::term::TriplePattern {
                    subject: fold_term_pattern(&t.subject, &lookup),
                    predicate: fold_named_node_pattern(&t.predicate, &lookup),
                    object: fold_term_pattern(&t.object, &lookup),
                })
                .collect(),
            dataset: dataset.clone(),
            pattern: fold_pattern(pattern, &lookup),
            base_iri: base_iri.clone(),
        },
        other => other.clone(),
    }
}

/// Substitutes a term into a triple-pattern position.
fn fold_term_pattern(
    t: &spargebra::term::TermPattern,
    pre: &dyn Fn(&Variable) -> Option<Term>,
) -> spargebra::term::TermPattern {
    use spargebra::term::TermPattern as T;
    match t {
        T::Variable(v) => match pre(v) {
            Some(Term::NamedNode(n)) => T::NamedNode(n),
            // Never `T::BlankNode(b)`: SPARQL reads a blank node in a pattern
            // as a variable that cannot be selected, so it would match every
            // node instead of this one. `run_in` routes blank nodes to the
            // evaluator's own substitution and never sends one here; leaving
            // the variable in place is the safe reading of a caller that does.
            Some(Term::BlankNode(_)) => t.clone(),
            Some(Term::Literal(l)) => T::Literal(l),
            _ => t.clone(),
        },
        other => other.clone(),
    }
}

/// Substitutes into a predicate position, which only accepts IRIs.
fn fold_named_node_pattern(
    n: &spargebra::term::NamedNodePattern,
    pre: &dyn Fn(&Variable) -> Option<Term>,
) -> spargebra::term::NamedNodePattern {
    use spargebra::term::NamedNodePattern as N;
    match n {
        N::Variable(v) => match pre(v) {
            Some(Term::NamedNode(node)) => N::NamedNode(node),
            _ => n.clone(),
        },
        other => other.clone(),
    }
}

fn fold_pattern(
    p: &spargebra::algebra::GraphPattern,
    pre: &dyn Fn(&Variable) -> Option<Term>,
) -> spargebra::algebra::GraphPattern {
    use spargebra::algebra::GraphPattern as G;
    let sub = |x: &G| Box::new(fold_pattern(x, pre));
    match p {
        G::Bgp { patterns } => G::Bgp {
            patterns: patterns
                .iter()
                .map(|t| spargebra::term::TriplePattern {
                    subject: fold_term_pattern(&t.subject, pre),
                    predicate: fold_named_node_pattern(&t.predicate, pre),
                    object: fold_term_pattern(&t.object, pre),
                })
                .collect(),
        },
        G::Path {
            subject,
            path,
            object,
        } => G::Path {
            subject: fold_term_pattern(subject, pre),
            path: path.clone(),
            object: fold_term_pattern(object, pre),
        },
        // A substituted variable is no longer produced by the pattern, so it
        // must leave the projection too or the evaluator will reject it.
        G::Project { inner, variables } => G::Project {
            inner: sub(inner),
            variables: variables
                .iter()
                .filter(|v| pre(v).is_none())
                .cloned()
                .collect(),
        },
        G::Join { left, right } => G::Join {
            left: sub(left),
            right: sub(right),
        },
        G::LeftJoin {
            left,
            right,
            expression,
        } => G::LeftJoin {
            left: sub(left),
            right: sub(right),
            expression: expression.as_ref().map(|e| fold_expr(e, pre)),
        },
        G::Filter { expr, inner } => G::Filter {
            expr: fold_expr(expr, pre),
            inner: sub(inner),
        },
        G::Union { left, right } => G::Union {
            left: sub(left),
            right: sub(right),
        },
        G::Graph { name, inner } => G::Graph {
            name: name.clone(),
            inner: sub(inner),
        },
        G::Extend {
            inner,
            variable,
            expression,
        } => G::Extend {
            inner: sub(inner),
            variable: variable.clone(),
            expression: fold_expr(expression, pre),
        },
        G::Minus { left, right } => G::Minus {
            left: sub(left),
            right: sub(right),
        },
        G::OrderBy { inner, expression } => G::OrderBy {
            inner: sub(inner),
            expression: expression.clone(),
        },
        G::Distinct { inner } => G::Distinct { inner: sub(inner) },
        G::Reduced { inner } => G::Reduced { inner: sub(inner) },
        G::Slice {
            inner,
            start,
            length,
        } => G::Slice {
            inner: sub(inner),
            start: *start,
            length: *length,
        },
        G::Group {
            inner,
            variables,
            aggregates,
        } => G::Group {
            inner: sub(inner),
            variables: variables.clone(),
            aggregates: aggregates.clone(),
        },
        G::Service {
            name,
            inner,
            silent,
        } => G::Service {
            name: name.clone(),
            inner: sub(inner),
            silent: *silent,
        },
        // Leaves, plus any variant added behind a feature flag: nothing to
        // rewrite, since none of them can contain a BOUND call.
        other => other.clone(),
    }
}

fn fold_expr(
    e: &spargebra::algebra::Expression,
    pre: &dyn Fn(&Variable) -> Option<Term>,
) -> spargebra::algebra::Expression {
    use spargebra::algebra::Expression as E;
    let sub = |x: &E| Box::new(fold_expr(x, pre));
    match e {
        // A pre-bound variable is bound by definition.
        E::Bound(v) if pre(v).is_some() => E::Literal(oxrdf::Literal::from(true)),
        E::Variable(v) => match pre(v) {
            Some(Term::NamedNode(n)) => E::NamedNode(n),
            Some(Term::Literal(l)) => E::Literal(l),
            // A blank node has no expression form; leave it to evaluate as
            // unbound rather than silently changing its meaning.
            _ => e.clone(),
        },
        E::Or(a, b) => E::Or(sub(a), sub(b)),
        E::And(a, b) => E::And(sub(a), sub(b)),
        E::Equal(a, b) => E::Equal(sub(a), sub(b)),
        E::SameTerm(a, b) => E::SameTerm(sub(a), sub(b)),
        E::Greater(a, b) => E::Greater(sub(a), sub(b)),
        E::GreaterOrEqual(a, b) => E::GreaterOrEqual(sub(a), sub(b)),
        E::Less(a, b) => E::Less(sub(a), sub(b)),
        E::LessOrEqual(a, b) => E::LessOrEqual(sub(a), sub(b)),
        E::In(a, list) => E::In(sub(a), list.iter().map(|x| fold_expr(x, pre)).collect()),
        E::Add(a, b) => E::Add(sub(a), sub(b)),
        E::Subtract(a, b) => E::Subtract(sub(a), sub(b)),
        E::Multiply(a, b) => E::Multiply(sub(a), sub(b)),
        E::Divide(a, b) => E::Divide(sub(a), sub(b)),
        E::UnaryPlus(a) => E::UnaryPlus(sub(a)),
        E::UnaryMinus(a) => E::UnaryMinus(sub(a)),
        E::Not(a) => E::Not(sub(a)),
        E::Exists(p) => E::Exists(Box::new(fold_pattern(p, pre))),
        E::If(a, b, c) => E::If(sub(a), sub(b), sub(c)),
        E::Coalesce(list) => E::Coalesce(list.iter().map(|x| fold_expr(x, pre)).collect()),
        E::FunctionCall(f, args) => {
            E::FunctionCall(f.clone(), args.iter().map(|x| fold_expr(x, pre)).collect())
        }
        other => other.clone(),
    }
}

/// Parses a SPARQL query, prepending `header` and normalising `$var` to `?var`.
pub fn parse_query(header: &str, text: &str) -> Result<Query> {
    let full = format!("{header}{text}");
    SparqlParser::new()
        .parse_query(&full)
        .map_err(|e| Error::Sparql(format!("{e}")))
}

/// Runs `query` with the given pre-bound variables, returning each solution as
/// a map from variable name to term.
///
/// Substitution is SEP-0007, which is what makes `$this` visible inside union
/// branches and to `bound()`.
pub fn run(
    query: &Query,
    bindings: &[(&str, Term)],
    graph: &Graph,
    store: &TermStore,
) -> Result<Vec<HashMap<String, Term>>> {
    run_in(query, bindings, graph, store, None)
}

/// As [`run`], with the shapes graph reachable as `GRAPH $shapesGraph`.
///
/// Separate because most callers have no shapes graph to offer — node
/// expressions evaluate against an empty one — and passing `None` everywhere
/// would say less than the name does.
pub fn run_in(
    query: &Query,
    bindings: &[(&str, Term)],
    graph: &Graph,
    store: &TermStore,
    shapes: Option<&Graph>,
) -> Result<Vec<HashMap<String, Term>>> {
    let adapter = match shapes {
        Some(s) => DataAdapter::new(graph, store).with_shapes(s),
        None => DataAdapter::new(graph, store),
    };
    match evaluate(query, bindings, &adapter)? {
        QueryResults::Solutions(solutions) => {
            let mut out = Vec::new();
            for solution in solutions {
                let solution = solution.map_err(|e| Error::Sparql(format!("{e}")))?;
                let mut row = HashMap::new();
                for (var, term) in solution.iter() {
                    row.insert(var.as_str().to_string(), term.clone());
                }
                out.push(row);
            }
            Ok(out)
        }
        QueryResults::Boolean(b) => {
            // An ASK answering true yields one empty solution, false none, so
            // callers can treat both query forms uniformly.
            Ok(if b { vec![HashMap::new()] } else { Vec::new() })
        }
        QueryResults::Graph(_) => Err(Error::Sparql(
            "CONSTRUCT is not valid for a SPARQL constraint".into(),
        )),
    }
}

/// Prepares and runs `query` against `adapter`, applying SHACL pre-binding.
///
/// The adapter is the caller's because the results borrow it: a solution
/// iterator reads the dataset lazily, so the dataset has to outlive it.
fn evaluate<'a>(
    query: &Query,
    bindings: &[(&str, Term)],
    adapter: &'a DataAdapter<'a>,
) -> Result<QueryResults<'a>> {
    reject_unsupported(query, bindings)?;

    // Blank nodes take the evaluator's own substitution rather than the
    // algebra rewrite; everything else takes the rewrite. See [`substitute`].
    let (blank, ground): (Binding, Binding) = bindings
        .iter()
        .cloned()
        .partition(|(_, t)| matches!(t, Term::BlankNode(_)));

    let evaluator = QueryEvaluator::new();
    let substituted = substitute(query, &ground);

    // A variable the query never mentions cannot be substituted — the
    // evaluator rejects it rather than ignoring it — and binding it would mean
    // nothing anyway. `$currentShape` is the ordinary case: most constraints
    // do not refer to it.
    let wanted: Vec<&str> = blank
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| mentions(&substituted, name))
        .collect();
    let substituted = ensure_projected(&substituted, &wanted);

    let prepared = wanted
        .iter()
        .fold(evaluator.prepare(&substituted), |q, name| {
            let term = blank
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, t)| t.clone())
                .expect("wanted is drawn from blank");
            q.substitute_variable(Variable::new_unchecked(*name), term)
        });

    prepared
        .execute(adapter)
        .map_err(|e| Error::Sparql(format!("{e}")))
}

/// Runs a `CONSTRUCT` query, returning the triples it builds.
///
/// This is what a SHACL-AF `sh:SPARQLRule` is: the constructed triples are the
/// inference. Pre-binding works exactly as it does for a constraint, so a rule
/// body can use `$this` and mean the focus node.
pub fn run_construct(
    query: &Query,
    bindings: &[(&str, Term)],
    graph: &Graph,
    store: &TermStore,
) -> Result<Vec<oxrdf::Triple>> {
    let adapter = DataAdapter::new(graph, store);
    match evaluate(query, bindings, &adapter)? {
        QueryResults::Graph(triples) => triples
            .map(|t| t.map_err(|e| Error::Sparql(format!("{e}"))))
            .collect(),
        _ => Err(Error::Sparql(
            "sh:construct must be a CONSTRUCT query, not SELECT or ASK".into(),
        )),
    }
}

impl SparqlConstraint {
    /// Compiles the `sh:construct` query of a SHACL-AF SPARQL rule.
    ///
    /// Parsed here rather than at inference time so a malformed query is a
    /// shapes-graph error, reported once, instead of a failure that only
    /// appears when the rule happens to fire.
    pub fn compile_construct(
        text: &str,
        node: TermId,
        shapes: &Graph,
        store: &TermStore,
        vocab: &Vocab,
    ) -> Result<Self> {
        let header = prefix_header(node, shapes, store, vocab);
        let query = parse_query(&header, text)?;
        if !matches!(query, Query::Construct { .. }) {
            return Err(Error::Shape(
                "sh:construct must be a CONSTRUCT query".into(),
            ));
        }
        Ok(Self {
            query,
            is_ask: false,
            source: node,
            message: Vec::new(),
            severity: None,
        })
    }
}

/// An empty solution, used to stand for the single failure an unsatisfied
/// `sh:ask` produces.
pub fn empty_solution() -> HashMap<String, Term> {
    HashMap::new()
}

/// True if the parsed query is an `ASK`.
pub fn is_ask(query: &Query) -> bool {
    matches!(query, Query::Ask { .. })
}

/// Converts an interned term to `oxrdf` for pre-binding.
///
/// A blank node goes in as itself. It must not reach the algebra rewrite —
/// see [`substitute`] — but the evaluator's own substitution takes it
/// faithfully, and [`QueryableDataset::internalize_term`] resolves it back to
/// the handle it already has.
pub fn to_term(t: TermId, store: &TermStore) -> Term {
    store.to_oxrdf(t)
}

/// Resolves a term produced by SPARQL back into the store, if it is present.
///
/// The stand-in IRI is decoded here as well as on the way in, because a
/// constraint can hand a pre-bound term straight back — `BIND($this AS
/// ?value)` is the obvious way — and the report has to name the blank node the
/// data holds rather than the spelling the engine used to carry it through the
/// algebra. Without this the value resolves to nothing and `sh:value` is
/// quietly absent.
pub fn from_term(term: TermRef<'_>, store: &TermStore) -> Option<TermId> {
    // One inverse for every kind, rather than a match here that has to be kept
    // in step with the renderer. Blank nodes and triple terms both need more
    // than `get_term` offers, and both have been silently dropped before.
    store.resolve_rendered(term)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GraphBuilder, Vocab, loader};
    use oxrdfio::RdfFormat;

    fn fixture(turtle: &str) -> (TermStore, Vocab, Graph) {
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
        (store, vocab, b.build())
    }

    const DATA: &str = "@prefix ex: <http://ex/> .
        ex:a ex:p ex:b ; ex:q 1 .
        ex:b ex:p ex:c .
        ex:x ex:p ex:y .";

    #[test]
    fn evaluates_a_basic_pattern_against_the_interned_graph() {
        let (store, _, g) = fixture(DATA);
        let q = parse_query("", "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }").unwrap();
        let rows = run(&q, &[], &g, &store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["o"].to_string(), "<http://ex/b>");
    }

    #[test]
    fn substitutes_this_into_the_pattern() {
        let (mut store, _, g) = fixture(DATA);
        let a = store.named_node("http://ex/a");
        let q = parse_query("", "SELECT $this ?o WHERE { $this <http://ex/p> ?o }").unwrap();

        let rows = run(&q, &[("this", to_term(a, &store))], &g, &store).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["o"].to_string(), "<http://ex/b>");
    }

    #[test]
    fn pre_binding_reaches_inside_a_union() {
        // The property a top-level VALUES join would not provide: both union
        // branches must see $this bound.
        let (mut store, _, g) = fixture(DATA);
        let a = store.named_node("http://ex/a");
        let q = parse_query(
            "",
            "SELECT $this WHERE {
                { FILTER (false) } UNION { FILTER ($this = <http://ex/a>) }
            }",
        )
        .unwrap();

        let rows = run(&q, &[("this", to_term(a, &store))], &g, &store).unwrap();
        assert_eq!(rows.len(), 1, "the second branch must match");
    }

    #[test]
    fn pre_bound_variables_are_bound_for_bound() {
        // `bound($this)` is why textual substitution cannot work: it would
        // produce `bound(<http://ex/a>)`, which does not parse.
        let (mut store, _, g) = fixture(DATA);
        let a = store.named_node("http://ex/a");
        let q = parse_query("", "SELECT $this WHERE { FILTER (bound($this)) }").unwrap();

        let rows = run(&q, &[("this", to_term(a, &store))], &g, &store).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn ask_queries_report_as_one_or_zero_solutions() {
        let (mut store, _, g) = fixture(DATA);
        let a = store.named_node("http://ex/a");
        let this = to_term(a, &store);

        let yes = parse_query("", "ASK { $this <http://ex/p> ?o }").unwrap();
        assert!(is_ask(&yes));
        assert_eq!(
            run(&yes, &[("this", this.clone())], &g, &store)
                .unwrap()
                .len(),
            1
        );

        let no = parse_query("", "ASK { $this <http://ex/nope> ?o }").unwrap();
        assert_eq!(run(&no, &[("this", this)], &g, &store).unwrap().len(), 0);
    }

    /// The shapes graph is reachable as a named graph, and the data graph is
    /// still the default one — a constraint must not see shapes triples
    /// unless it asks for them by name.
    #[test]
    fn the_shapes_graph_is_queryable_by_name() {
        let (mut store, _, data) = fixture(DATA);
        let mut b = GraphBuilder::new();
        loader::parse_str(
            "@prefix ex: <http://ex/> . ex:S ex:property 42 .",
            RdfFormat::Turtle,
            "http://t/",
            1,
            &mut store,
            &mut b,
        )
        .unwrap();
        let shapes = b.build();

        let graph_iri = Term::from(oxrdf::NamedNode::new_unchecked(SHAPES_GRAPH_IRI));
        let q = parse_query(
            "",
            "SELECT ?s WHERE { GRAPH $shapesGraph { ?s <http://ex/property> 42 } }",
        )
        .unwrap();
        let rows = run_in(
            &q,
            &[("shapesGraph", graph_iri.clone())],
            &data,
            &store,
            Some(&shapes),
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"].to_string(), "<http://ex/S>");

        // The default graph is the data graph, and holds none of that.
        let default_only =
            parse_query("", "SELECT ?s WHERE { ?s <http://ex/property> 42 }").unwrap();
        assert!(
            run_in(&default_only, &[], &data, &store, Some(&shapes))
                .unwrap()
                .is_empty(),
            "shapes triples must not leak into the default graph"
        );

        // Without a shapes graph the named graph is simply empty, rather than
        // falling back to the data.
        assert!(
            run_in(&q, &[("shapesGraph", graph_iri)], &data, &store, None)
                .unwrap()
                .is_empty()
        );
    }

    /// `bound($shapesGraph)` and `bound($currentShape)` must both hold, which
    /// is the whole reason pre-binding is substitution rather than a join.
    #[test]
    fn shapes_graph_and_current_shape_are_bound() {
        let (mut store, _, data) = fixture(DATA);
        let shape = store.named_node("http://ex/S");
        let mut b = GraphBuilder::new();
        loader::parse_str(
            "@prefix ex: <http://ex/> . ex:S ex:property 42 .",
            RdfFormat::Turtle,
            "http://t/",
            1,
            &mut store,
            &mut b,
        )
        .unwrap();
        let shapes = b.build();

        let q = parse_query(
            "",
            "SELECT $this WHERE {
                FILTER bound($shapesGraph) .
                GRAPH $shapesGraph {
                    FILTER bound($currentShape) .
                    $currentShape <http://ex/property> 42 .
                }
            }",
        )
        .unwrap();
        let a = store.named_node("http://ex/a");
        let rows = run_in(
            &q,
            &[
                ("this", to_term(a, &store)),
                ("currentShape", to_term(shape, &store)),
                (
                    "shapesGraph",
                    Term::from(oxrdf::NamedNode::new_unchecked(SHAPES_GRAPH_IRI)),
                ),
            ],
            &data,
            &store,
            Some(&shapes),
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn builds_prefix_headers_from_sh_declare() {
        let (mut store, vocab, g) = fixture(
            "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://ex/> .
             ex:S sh:prefixes ex:onto .
             ex:onto sh:declare [ sh:prefix \"ex\" ; sh:namespace \"http://ex/\" ] .
             ex:a ex:p ex:b .",
        );
        let s = store.named_node("http://ex/S");
        let header = prefix_header(s, &g, &store, &vocab);
        assert_eq!(header, "PREFIX ex: <http://ex/>\n");

        // And the header actually makes the prefix usable.
        let q = parse_query(&header, "SELECT ?o WHERE { ex:a ex:p ?o }").unwrap();
        assert_eq!(run(&q, &[], &g, &store).unwrap().len(), 1);
    }

    #[test]
    fn computed_terms_survive_the_round_trip() {
        // CONCAT produces a literal that is not in the store; the adapter must
        // still be able to hand it back.
        let (store, _, g) = fixture(DATA);
        let q = parse_query("", "SELECT ?s WHERE { BIND(CONCAT(\"a\", \"b\") AS ?s) }").unwrap();
        let rows = run(&q, &[], &g, &store).unwrap();
        assert_eq!(rows[0]["s"].to_string(), "\"ab\"");
    }
}
