//! Validation reports: the SHACL result model and its RDF serialisation.

use oxrdf::{Graph as OxGraph, Literal, NamedNode, NamedOrBlankNode, Term, Triple};

use crate::model::{Graph, TermId, TermStore, Vocab};

/// One SHACL validation result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationResult {
    pub focus_node: TermId,
    /// The offending value, absent for constraints that fault the focus node
    /// itself (`sh:minCount`, `sh:closed`).
    pub value: Option<TermId>,
    /// The `sh:path` node from the shapes graph, serialised structurally so
    /// complex paths round-trip.
    pub path: Option<TermId>,
    pub source_shape: Option<TermId>,
    pub source_constraint: Option<TermId>,
    pub source_constraint_component: TermId,
    pub severity: TermId,
    pub messages: Vec<TermId>,
    /// Nested results from `sh:node`-style constraints.
    pub details: Vec<ValidationResult>,
}

impl ValidationResult {
    /// A result with only the mandatory fields set.
    pub fn new(focus_node: TermId, component: TermId, severity: TermId) -> Self {
        Self {
            focus_node,
            value: None,
            path: None,
            source_shape: None,
            source_constraint: None,
            source_constraint_component: component,
            severity,
            messages: Vec::new(),
            details: Vec::new(),
        }
    }

    pub fn with_value(mut self, value: TermId) -> Self {
        self.value = Some(value);
        self
    }

    pub fn with_path(mut self, path: Option<TermId>) -> Self {
        self.path = path;
        self
    }

    pub fn with_source_shape(mut self, shape: TermId) -> Self {
        self.source_shape = Some(shape);
        self
    }
}

/// The outcome of validating a data graph against a shapes graph.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    pub results: Vec<ValidationResult>,
}

impl ValidationReport {
    /// True when no result has a severity that blocks conformance.
    ///
    /// `disallowed` lists the severities that break conformance — by default
    /// just `sh:Violation`, but SHACL 1.2's `sh:conformanceDisallows` lets a
    /// caller widen it.
    pub fn conforms(&self, disallowed: &[TermId]) -> bool {
        !self
            .results
            .iter()
            .any(|r| disallowed.contains(&r.severity))
    }

    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    /// Serialises the report into an `oxrdf` graph.
    ///
    /// `shapes` is consulted to copy complex `sh:path` structures across; a
    /// report referring to `[ sh:inversePath ex:p ]` is only comparable to the
    /// expected one if those triples come with it.
    pub fn to_oxrdf(
        &self,
        store: &TermStore,
        vocab: &Vocab,
        shapes: &Graph,
        disallowed: &[TermId],
    ) -> OxGraph {
        let mut g = OxGraph::new();
        let mut next_bnode = 0u64;
        let report = fresh_bnode(&mut next_bnode);

        let iri =
            |t: TermId| -> NamedNode { NamedNode::new_unchecked(store.iri(t).unwrap_or_default()) };

        g.insert(&Triple::new(
            report.clone(),
            iri(vocab.rdf_type),
            iri(vocab.sh_ValidationReport),
        ));
        g.insert(&Triple::new(
            report.clone(),
            iri(vocab.sh_conforms),
            Literal::from(self.conforms(disallowed)),
        ));

        for result in &self.results {
            let node = self.write_result(
                result,
                &report,
                store,
                vocab,
                shapes,
                &mut g,
                &mut next_bnode,
            );
            let _ = node;
        }
        g
    }

    #[allow(clippy::too_many_arguments)]
    fn write_result(
        &self,
        result: &ValidationResult,
        parent: &NamedOrBlankNode,
        store: &TermStore,
        vocab: &Vocab,
        shapes: &Graph,
        g: &mut OxGraph,
        next: &mut u64,
    ) -> NamedOrBlankNode {
        let iri =
            |t: TermId| -> NamedNode { NamedNode::new_unchecked(store.iri(t).unwrap_or_default()) };
        let node = fresh_bnode(next);

        g.insert(&Triple::new(
            parent.clone(),
            iri(vocab.sh_result),
            node.clone(),
        ));
        g.insert(&Triple::new(
            node.clone(),
            iri(vocab.rdf_type),
            iri(vocab.sh_ValidationResult),
        ));
        g.insert(&Triple::new(
            node.clone(),
            iri(vocab.sh_focusNode),
            store.to_oxrdf(result.focus_node),
        ));
        g.insert(&Triple::new(
            node.clone(),
            iri(vocab.sh_resultSeverity),
            store.to_oxrdf(result.severity),
        ));
        g.insert(&Triple::new(
            node.clone(),
            iri(vocab.sh_sourceConstraintComponent),
            store.to_oxrdf(result.source_constraint_component),
        ));

        if let Some(v) = result.value {
            g.insert(&Triple::new(
                node.clone(),
                iri(vocab.sh_value),
                store.to_oxrdf(v),
            ));
        }
        if let Some(s) = result.source_shape {
            g.insert(&Triple::new(
                node.clone(),
                iri(vocab.sh_sourceShape),
                store.to_oxrdf(s),
            ));
        }
        if let Some(c) = result.source_constraint {
            g.insert(&Triple::new(
                node.clone(),
                iri(vocab.sh_sourceConstraint),
                store.to_oxrdf(c),
            ));
        }
        if let Some(p) = result.path {
            g.insert(&Triple::new(
                node.clone(),
                iri(vocab.sh_resultPath),
                store.to_oxrdf(p),
            ));
            copy_subtree(p, shapes, store, g);
        }
        for &m in &result.messages {
            g.insert(&Triple::new(
                node.clone(),
                iri(vocab.sh_resultMessage),
                store.to_oxrdf(m),
            ));
        }
        for detail in &result.details {
            self.write_result(detail, &node, store, vocab, shapes, g, next);
        }
        node
    }
}

/// A validation report read back out of RDF, plus the severities its author
/// declared as blocking conformance.
#[derive(Debug, Clone)]
pub struct ParsedReport {
    pub report: ValidationReport,
    pub conforms: bool,
    /// `sh:conformanceDisallows` values, defaulting to `[sh:Violation]`.
    pub disallowed: Vec<TermId>,
}

impl ValidationReport {
    /// Reads the `sh:ValidationReport` rooted at `node`.
    ///
    /// Used by the test harness to load expected reports, so that expected and
    /// actual travel through exactly the same representation before comparison.
    pub fn parse(node: TermId, g: &Graph, store: &TermStore, vocab: &Vocab) -> ParsedReport {
        let conforms = g
            .object(node, vocab.sh_conforms)
            .and_then(|t| store.lexical_form(t).map(|s| s == "true"))
            .unwrap_or(true);

        let mut disallowed: Vec<TermId> = g.objects(node, vocab.sh_conformanceDisallows).collect();
        if disallowed.is_empty() {
            disallowed.push(vocab.sh_Violation);
        }

        let results = g
            .objects(node, vocab.sh_result)
            .map(|r| parse_result(r, g, store, vocab, 0))
            .collect();

        ParsedReport {
            report: ValidationReport { results },
            conforms,
            disallowed,
        }
    }
}

fn parse_result(
    node: TermId,
    g: &Graph,
    _store: &TermStore,
    vocab: &Vocab,
    depth: u32,
) -> ValidationResult {
    ValidationResult {
        focus_node: g.object(node, vocab.sh_focusNode).unwrap_or(node),
        value: g.object(node, vocab.sh_value),
        path: g.object(node, vocab.sh_resultPath),
        source_shape: g.object(node, vocab.sh_sourceShape),
        source_constraint: g.object(node, vocab.sh_sourceConstraint),
        source_constraint_component: g
            .object(node, vocab.sh_sourceConstraintComponent)
            .unwrap_or(node),
        severity: g
            .object(node, vocab.sh_resultSeverity)
            .unwrap_or(vocab.sh_Violation),
        messages: g.objects(node, vocab.sh_resultMessage).collect(),
        // `sh:detail` can nest arbitrarily; bound it so a cyclic expected
        // report in a hand-written test cannot hang the harness.
        details: if depth < 32 {
            g.objects(node, vocab.sh_detail)
                .map(|d| parse_result(d, g, _store, vocab, depth + 1))
                .collect()
        } else {
            Vec::new()
        },
    }
}

fn fresh_bnode(next: &mut u64) -> NamedOrBlankNode {
    let n = *next;
    *next += 1;
    NamedOrBlankNode::BlankNode(oxrdf::BlankNode::new_unchecked(format!("r{n}")))
}

/// Copies the blank-node subtree rooted at `root` from `src` into `dst`.
///
/// Only blank nodes are followed, so this cannot walk out into the rest of the
/// shapes graph, and a cycle is bounded by the visited set.
fn copy_subtree(root: TermId, src: &Graph, store: &TermStore, dst: &mut OxGraph) {
    if !store.is_blank(root) {
        return;
    }
    let mut seen = vec![root];
    let mut queue = vec![root];
    while let Some(node) = queue.pop() {
        for (p, o) in src.predicate_objects(node) {
            let subject = match store.to_oxrdf(node) {
                Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b),
                Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n),
                _ => continue,
            };
            dst.insert(&Triple::new(
                subject,
                NamedNode::new_unchecked(store.iri(p).unwrap_or_default()),
                store.to_oxrdf(o),
            ));
            if store.is_blank(o) && !seen.contains(&o) {
                seen.push(o);
                queue.push(o);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GraphBuilder, loader};
    use oxrdf::dataset::CanonicalizationAlgorithm;
    use oxrdfio::RdfFormat;

    fn fixture(turtle: &str) -> (TermStore, Vocab, Graph) {
        let mut store = TermStore::new();
        let vocab = Vocab::new(&mut store);
        let mut b = GraphBuilder::new();
        loader::parse_str(
            turtle,
            RdfFormat::Turtle,
            "http://t/",
            1,
            &mut store,
            &mut b,
        )
        .unwrap();
        (store, vocab, b.build())
    }

    fn canonical(mut g: OxGraph) -> String {
        g.canonicalize(CanonicalizationAlgorithm::Unstable);
        let mut lines: Vec<String> = g.iter().map(|t| t.to_string()).collect();
        lines.sort();
        lines.join("\n")
    }

    #[test]
    fn conformance_depends_on_the_disallowed_severities() {
        let (mut store, vocab, _) = fixture("");
        let focus = store.named_node("http://ex/a");
        let report = ValidationReport {
            results: vec![ValidationResult::new(
                focus,
                vocab.sh_DatatypeConstraintComponent,
                vocab.sh_Warning,
            )],
        };

        assert!(
            report.conforms(&[vocab.sh_Violation]),
            "a warning alone does not break conformance"
        );
        assert!(!report.conforms(&[vocab.sh_Violation, vocab.sh_Warning]));
    }

    #[test]
    fn empty_report_serialises_as_conforming() {
        let (store, vocab, shapes) = fixture("");
        let g =
            ValidationReport::default().to_oxrdf(&store, &vocab, &shapes, &[vocab.sh_Violation]);
        let text = canonical(g);
        assert!(text.contains("#ValidationReport"));
        assert!(text.contains(r#""true"^^<http://www.w3.org/2001/XMLSchema#boolean>"#));
        assert!(!text.contains("#result>"));
    }

    #[test]
    fn serialises_a_result_with_all_its_fields() {
        let (mut store, vocab, shapes) = fixture("");
        let focus = store.named_node("http://ex/bob");
        let value = store.literal("x", "http://www.w3.org/2001/XMLSchema#string", None);
        let shape = store.named_node("http://ex/S");
        let path = store.named_node("http://ex/age");

        let report = ValidationReport {
            results: vec![
                ValidationResult::new(
                    focus,
                    vocab.sh_DatatypeConstraintComponent,
                    vocab.sh_Violation,
                )
                .with_value(value)
                .with_path(Some(path))
                .with_source_shape(shape),
            ],
        };
        let text = canonical(report.to_oxrdf(&store, &vocab, &shapes, &[vocab.sh_Violation]));

        assert!(text.contains(r#""false"^^<http://www.w3.org/2001/XMLSchema#boolean>"#));
        assert!(text.contains("<http://ex/bob>"));
        assert!(text.contains("<http://ex/age>"));
        assert!(text.contains("<http://ex/S>"));
        assert!(text.contains("#DatatypeConstraintComponent"));
    }

    #[test]
    fn complex_result_paths_carry_their_triples_along() {
        // The report must be self-contained: an inverse path is a blank node in
        // the shapes graph, useless in a report without its structure.
        let (mut store, vocab, shapes) = fixture(
            "@prefix sh: <http://www.w3.org/ns/shacl#> . @prefix ex: <http://ex/> .
             ex:S sh:path [ sh:inversePath ex:parent ] .",
        );
        let s = store.named_node("http://ex/S");
        let path = shapes.object(s, vocab.sh_path).unwrap();
        let focus = store.named_node("http://ex/a");

        let report = ValidationReport {
            results: vec![
                ValidationResult::new(
                    focus,
                    vocab.sh_MinCountConstraintComponent,
                    vocab.sh_Violation,
                )
                .with_path(Some(path)),
            ],
        };
        let text = canonical(report.to_oxrdf(&store, &vocab, &shapes, &[vocab.sh_Violation]));

        assert!(
            text.contains("#inversePath"),
            "path structure was not copied"
        );
        assert!(text.contains("<http://ex/parent>"));
    }

    #[test]
    fn nested_details_are_serialised() {
        let (mut store, vocab, shapes) = fixture("");
        let focus = store.named_node("http://ex/a");
        let inner = ValidationResult::new(
            focus,
            vocab.sh_DatatypeConstraintComponent,
            vocab.sh_Violation,
        );
        let mut outer =
            ValidationResult::new(focus, vocab.sh_NodeConstraintComponent, vocab.sh_Violation);
        outer.details.push(inner);

        let report = ValidationReport {
            results: vec![outer],
        };
        let text = canonical(report.to_oxrdf(&store, &vocab, &shapes, &[vocab.sh_Violation]));
        assert!(text.contains("#NodeConstraintComponent"));
        assert!(text.contains("#DatatypeConstraintComponent"));
    }
}
