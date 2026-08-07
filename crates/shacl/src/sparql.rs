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
    /// Terms minted during evaluation, addressed as `store.len() + index`.
    extra: RefCell<Vec<Term>>,
}

impl<'a> DataAdapter<'a> {
    pub fn new(graph: &'a Graph, store: &'a TermStore) -> Self {
        Self {
            graph,
            store,
            extra: RefCell::new(Vec::new()),
        }
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

impl<'a> QueryableDataset<'a> for &'a DataAdapter<'a> {
    type InternalTerm = TermId;
    type Error = Infallible;

    fn internal_quads_for_pattern(
        &self,
        subject: Option<&TermId>,
        predicate: Option<&TermId>,
        object: Option<&TermId>,
        _graph_name: Option<Option<&TermId>>,
    ) -> impl Iterator<Item = std::result::Result<spareval::InternalQuad<TermId>, Infallible>> + use<'a>
    {
        // Pick the index that the bound components can seek on, so a pattern
        // with a known subject or object never degenerates into a full scan.
        let rows: Vec<[TermId; 3]> = match (subject, predicate, object) {
            (Some(&s), Some(&p), Some(&o)) => {
                if self.graph.contains(s, p, o) {
                    vec![[s, p, o]]
                } else {
                    Vec::new()
                }
            }
            (Some(&s), Some(&p), None) => {
                self.graph.objects(s, p).map(|o| [s, p, o]).collect()
            }
            (None, Some(&p), Some(&o)) => {
                self.graph.subjects(p, o).map(|s| [s, p, o]).collect()
            }
            (Some(&s), None, None) => self
                .graph
                .predicate_objects(s)
                .map(|(p, o)| [s, p, o])
                .collect(),
            (None, None, Some(&o)) => self
                .graph
                .subject_predicates(o)
                .map(|(s, p)| [s, p, o])
                .collect(),
            (None, Some(&p), None) => self
                .graph
                .iter()
                .filter(|r| r[1] == p)
                .collect(),
            (Some(&s), None, Some(&o)) => self
                .graph
                .predicate_objects(s)
                .filter(|&(_, x)| x == o)
                .map(|(p, _)| [s, p, o])
                .collect(),
            (None, None, None) => self.graph.iter().collect(),
        };
        rows.into_iter().map(|[s, p, o]| {
            Ok(spareval::InternalQuad {
                subject: s,
                predicate: p,
                object: o,
                graph_name: None,
            })
        })
    }

    fn internalize_term(&self, term: Term) -> std::result::Result<TermId, Infallible> {
        Ok(self
            .store
            .get_term(term.as_ref())
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
            Some(Term::BlankNode(b)) => T::BlankNode(b),
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
    let adapter = DataAdapter::new(graph, store);
    let evaluator = QueryEvaluator::new();
    let substituted = substitute(query, bindings);
    let prepared = evaluator.prepare(&substituted);

    match prepared
        .execute(&adapter)
        .map_err(|e| Error::Sparql(format!("{e}")))?
    {
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
pub fn to_term(t: TermId, store: &TermStore) -> Term {
    store.to_oxrdf(t)
}

/// Resolves a term produced by SPARQL back into the store, if it is present.
pub fn from_term(term: TermRef<'_>, store: &TermStore) -> Option<TermId> {
    store.get_term(term)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{loader, GraphBuilder, Vocab};
    use oxrdfio::RdfFormat;

    fn fixture(turtle: &str) -> (TermStore, Vocab, Graph) {
        let mut store = TermStore::new();
        let vocab = Vocab::new(&mut store);
        let mut b = GraphBuilder::new();
        loader::parse_str(turtle, RdfFormat::Turtle, "http://t/", 0, &mut store, &mut b).unwrap();
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
        assert_eq!(run(&yes, &[("this", this.clone())], &g, &store).unwrap().len(), 1);

        let no = parse_query("", "ASK { $this <http://ex/nope> ?o }").unwrap();
        assert_eq!(run(&no, &[("this", this)], &g, &store).unwrap().len(), 0);
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
