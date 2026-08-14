//! SHACL-AF rules: inferring triples before validating.
//!
//! Rules are the one part of SHACL that *changes* the graph rather than
//! reporting on it, so they are kept out of the validation path entirely.
//! Nothing here runs unless a caller asks for it, and what comes back is a new
//! graph rather than a mutation of the one passed in — a report is only
//! meaningful against a known input, and silently expanding the caller's data
//! would make it impossible to tell which triples were asserted and which were
//! derived.
//!
//! The execution order is the specification's, and its shape matters:
//!
//! > for each shape S in the shapes graph, ordered by execution order { for
//! > each non-deactivated rule R in the shape, ordered by execution order {
//! > for each target node T of S that conforms to all conditions of R {
//! > execute R using T as focus node } } }
//!
//! Two consequences worth stating because they surprise people. It is a single
//! pass: the spec explicitly declines to say what happens "if the same rule
//! needs to be applied multiple times after other rules have fired", so a rule
//! set that computes a transitive closure will not converge in one run — see
//! [`apply_iterated`]. And triples inferred at the same `sh:order` are not
//! visible to each other, which is what lets rules at one level be evaluated in
//! any order without changing the answer.

use crate::error::{Error, Result};
use crate::model::{Graph, GraphBuilder, TermId, TermStore, Vocab};
use crate::nodeexpr;
use crate::shapes::{RuleKind, Shapes};
use crate::validate;

/// The ceiling on inferred triples, mirroring [`crate::inference`]. A rule
/// whose object expression builds fresh terms can generate without bound, and
/// running out of memory is a worse answer than an error.
pub const DEFAULT_MAX_TRIPLES: usize = 50_000_000;

/// Applies every rule in `shapes` once, returning the data graph plus whatever
/// was inferred.
///
/// One pass, as the specification defines. Use [`apply_iterated`] when the rule
/// set is meant to reach a fixpoint.
pub fn apply(
    data: &Graph,
    shapes: &Shapes,
    shapes_graph: &Graph,
    store: &mut TermStore,
    vocab: &Vocab,
) -> Result<Graph> {
    apply_bounded(
        data,
        shapes,
        shapes_graph,
        store,
        vocab,
        DEFAULT_MAX_TRIPLES,
    )
}

/// [`apply`], with a cap on the size of the result.
pub fn apply_bounded(
    data: &Graph,
    shapes: &Shapes,
    shapes_graph: &Graph,
    store: &mut TermStore,
    vocab: &Vocab,
    max_triples: usize,
) -> Result<Graph> {
    // Shape/rule pairs in execution order. Flattened first so ordering is over
    // the pairs rather than nested, which is what makes the `sh:order`
    // boundaries below straightforward to find.
    let mut work: Vec<(usize, f64, f64)> = Vec::new();
    for (i, shape) in shapes.iter().enumerate() {
        if shape.deactivated {
            continue;
        }
        for (j, rule) in shape.rules.iter().enumerate() {
            if rule.deactivated {
                continue;
            }
            let _ = j;
            work.push((i, shape.order, rule.order));
        }
    }
    if work.is_empty() {
        return Ok(data.clone());
    }
    // Total order on (shape order, rule order). `f64` has no `Ord` because of
    // NaN; `sh:order` is a number in a shapes graph and a NaN there is
    // meaningless, so treating it as equal to everything is as good an answer
    // as any and better than panicking.
    work.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut current = data.clone();
    let mut level = (work[0].1, work[0].2);
    let mut pending: Vec<[TermId; 3]> = Vec::new();

    for (shape_index, shape_order, rule_order) in work {
        // Crossing an order boundary makes everything inferred so far visible.
        // Within a level it stays pending, which is the specification's
        // "rules at the same order cannot see each other's inferences".
        if (shape_order, rule_order) != level {
            current = merge(&current, &pending, max_triples)?;
            pending.clear();
            level = (shape_order, rule_order);
        }

        let shape_id = shapes.id_at(shape_index);
        let focus =
            validate::focus_nodes_of(shape_id, &current, shapes, shapes_graph, store, vocab)?;
        let shape = shapes.get(shape_id);

        for rule in &shape.rules {
            if rule.deactivated {
                continue;
            }
            if (shape.order, rule.order) != (shape_order, rule_order) {
                continue;
            }
            for &node in &focus {
                if !conforms_to_all(&rule.conditions, node, &current, shapes, store, vocab)? {
                    continue;
                }
                fire(
                    rule,
                    node,
                    &current,
                    shapes,
                    shapes_graph,
                    store,
                    vocab,
                    &mut pending,
                )?;
            }
        }
    }
    merge(&current, &pending, max_triples)
}

/// Applies the rules repeatedly until nothing new is inferred.
///
/// Outside the specification, which defines a single pass and says nothing
/// about repeating it. It is offered because the obvious uses of rules —
/// transitive closure, subclass propagation — need more than one pass to be
/// worth anything, and because a caller who wants that should be able to ask
/// for it rather than run `apply` in a loop and guess at termination.
///
/// Bounded twice over: by the triple ceiling, and by `max_rounds`, because a
/// rule that mints a new term each round has no fixpoint to reach.
pub fn apply_iterated(
    data: &Graph,
    shapes: &Shapes,
    shapes_graph: &Graph,
    store: &mut TermStore,
    vocab: &Vocab,
    max_rounds: usize,
) -> Result<Graph> {
    let mut current = data.clone();
    for round in 0..max_rounds {
        let next = apply_bounded(
            &current,
            shapes,
            shapes_graph,
            store,
            vocab,
            DEFAULT_MAX_TRIPLES,
        )?;
        if next.len() == current.len() {
            return Ok(next);
        }
        current = next;
        if round + 1 == max_rounds {
            return Err(Error::Inference(format!(
                "rules did not reach a fixpoint in {max_rounds} rounds; \
                 the rule set may infer new terms without end"
            )));
        }
    }
    Ok(current)
}

/// Runs one rule for one focus node, appending what it infers.
#[allow(clippy::too_many_arguments)]
fn fire(
    rule: &crate::shapes::Rule,
    focus: TermId,
    data: &Graph,
    shapes: &Shapes,
    shapes_graph: &Graph,
    store: &mut TermStore,
    vocab: &Vocab,
    out: &mut Vec<[TermId; 3]>,
) -> Result<()> {
    match &rule.kind {
        RuleKind::Triple {
            subject,
            predicate,
            object,
        } => {
            let ctx = nodeexpr::Ctx {
                data,
                exprs: shapes_graph,
                vocab,
                shnex: &nodeexpr::Shnex::new(store),
                shapes: Some(shapes),
                vars: &[],
            };
            let subjects = nodeexpr::eval(*subject, Some(focus), &ctx, store)?;
            let predicates = nodeexpr::eval(*predicate, Some(focus), &ctx, store)?;
            let objects = nodeexpr::eval(*object, Some(focus), &ctx, store)?;

            // "For each combination of members s of S, p of P and o of O,
            // infer a triple." A predicate that is not an IRI cannot appear in
            // one, so it is dropped rather than producing an ill-formed graph.
            for &s in &subjects {
                for &p in &predicates {
                    if !store.is_iri(p) {
                        continue;
                    }
                    for &o in &objects {
                        out.push([s, p, o]);
                    }
                }
            }
            Ok(())
        }
        // Deferred from compilation: this rule would have run, so the error
        // that stopped it being built is raised now rather than skipped.
        RuleKind::Broken(why) => Err(Error::Shape(format!(
            "rule on {} cannot run: {why}",
            store.to_oxrdf(rule.node)
        ))),
        RuleKind::Sparql(q) => {
            let this = crate::sparql::to_term(focus, store);
            let triples = crate::sparql::run_construct(&q.query, &[("this", this)], data, store)?;
            for t in triples {
                let s = store.intern_oxrdf(t.subject.as_ref().into(), crate::model::scope::DATA);
                let p = store.named_node(t.predicate.as_str());
                let o = store.intern_oxrdf(t.object.as_ref(), crate::model::scope::DATA);
                out.push([s, p, o]);
            }
            Ok(())
        }
    }
}

fn conforms_to_all(
    conditions: &[TermId],
    node: TermId,
    data: &Graph,
    shapes: &Shapes,
    store: &mut TermStore,
    vocab: &Vocab,
) -> Result<bool> {
    for &shape_node in conditions {
        if !validate::node_conforms(node, shape_node, data, shapes, store, vocab)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Builds a graph from `base` plus `extra`, refusing to exceed `max_triples`.
fn merge(base: &Graph, extra: &[[TermId; 3]], max_triples: usize) -> Result<Graph> {
    if extra.is_empty() {
        return Ok(base.clone());
    }
    if base.len() + extra.len() > max_triples {
        return Err(Error::Inference(format!(
            "rules would infer more than {max_triples} triples; \
             raise the limit or narrow the rules"
        )));
    }
    let mut b = GraphBuilder::new();
    for [s, p, o] in base.iter() {
        b.push(s, p, o);
    }
    for &[s, p, o] in extra {
        b.push(s, p, o);
    }
    Ok(b.build())
}
