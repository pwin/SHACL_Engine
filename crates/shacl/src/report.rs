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
            self.write_result(
                result,
                &report,
                vocab.sh_result,
                store,
                vocab,
                shapes,
                &mut g,
                &mut next_bnode,
            );
        }
        g
    }

    /// `link` is the predicate joining `parent` to this result: `sh:result`
    /// from the report itself, `sh:detail` from an enclosing result. They are
    /// not interchangeable — a result is not a report, so hanging a nested one
    /// off `sh:result` puts it outside the vocabulary's intended usage, and
    /// this crate's own reader looks for `sh:detail` and would drop it.
    #[allow(clippy::too_many_arguments)]
    fn write_result(
        &self,
        result: &ValidationResult,
        parent: &NamedOrBlankNode,
        link: TermId,
        store: &TermStore,
        vocab: &Vocab,
        shapes: &Graph,
        g: &mut OxGraph,
        next: &mut u64,
    ) -> NamedOrBlankNode {
        let iri =
            |t: TermId| -> NamedNode { NamedNode::new_unchecked(store.iri(t).unwrap_or_default()) };
        let node = fresh_bnode(next);

        g.insert(&Triple::new(parent.clone(), iri(link), node.clone()));
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
            self.write_result(
                detail,
                &node,
                vocab.sh_detail,
                store,
                vocab,
                shapes,
                g,
                next,
            );
        }
        node
    }
}

impl ValidationReport {
    /// Serialises the report as RDF, in the form the SHACL specification
    /// defines: a `sh:ValidationReport` carrying `sh:conforms` and one
    /// `sh:ValidationResult` per violation.
    ///
    /// This, rather than any human-readable rendering, is what a SHACL
    /// processor is meant to hand back — it is a graph, so it can be queried,
    /// diffed and fed to another tool.
    pub fn serialize(
        &self,
        format: oxrdfio::RdfFormat,
        store: &TermStore,
        vocab: &Vocab,
        shapes: &Graph,
        disallowed: &[TermId],
    ) -> crate::Result<String> {
        let graph = self.to_oxrdf(store, vocab, shapes, disallowed);
        let mut serializer = oxrdfio::RdfSerializer::from_format(format);
        // Prefixes are cosmetic in N-Triples but make Turtle readable, which
        // is the whole reason someone would pick it.
        for (prefix, iri) in [
            ("sh", crate::model::vocab::SH),
            ("rdf", crate::model::vocab::RDF),
            ("rdfs", crate::model::vocab::RDFS),
            ("xsd", crate::model::vocab::XSD),
        ] {
            serializer = serializer
                .with_prefix(prefix, iri)
                .map_err(|e| crate::Error::Io(format!("bad prefix {prefix}: {e}")))?;
        }

        let mut out = Vec::new();
        let mut writer = serializer.for_writer(&mut out);
        for triple in graph.iter() {
            writer
                .serialize_triple(triple)
                .map_err(|e| crate::Error::Io(e.to_string()))?;
        }
        writer
            .finish()
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        String::from_utf8(out).map_err(|e| crate::Error::Io(e.to_string()))
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
    fn serialises_a_report_that_parses_back_as_shacl() {
        let (mut store, vocab, shapes) = fixture("");
        let focus = store.named_node("http://ex/bob");
        let value = store.literal("old", "http://www.w3.org/2001/XMLSchema#string", None);
        let report = ValidationReport {
            results: vec![
                ValidationResult::new(
                    focus,
                    vocab.sh_DatatypeConstraintComponent,
                    vocab.sh_Violation,
                )
                .with_value(value),
            ],
        };

        let turtle = report
            .serialize(
                oxrdfio::RdfFormat::Turtle,
                &store,
                &vocab,
                &shapes,
                &[vocab.sh_Violation],
            )
            .expect("serialises");

        // Prefixed rather than expanded: the point of choosing Turtle.
        assert!(turtle.contains("sh:ValidationReport"), "{turtle}");
        assert!(turtle.contains("sh:conforms false"), "{turtle}");
        assert!(
            turtle.contains("sh:DatatypeConstraintComponent"),
            "{turtle}"
        );

        // Round-trip it: reading the report back must yield the same report,
        // which is what makes it usable by another tool rather than merely
        // printable.
        let mut store2 = TermStore::new();
        let vocab2 = Vocab::new(&mut store2);
        let mut b = GraphBuilder::new();
        loader::parse_str(
            &turtle,
            RdfFormat::Turtle,
            "http://r/",
            0,
            &mut store2,
            &mut b,
        )
        .expect("the report is well-formed RDF");
        let g = b.build();

        let root = g
            .subjects(vocab2.rdf_type, vocab2.sh_ValidationReport)
            .next()
            .expect("a sh:ValidationReport node");
        let parsed = ValidationReport::parse(root, &g, &store2, &vocab2);
        assert!(!parsed.conforms);
        assert_eq!(parsed.report.results.len(), 1);
        assert_eq!(
            parsed.report.results[0].source_constraint_component,
            vocab2.sh_DatatypeConstraintComponent
        );
    }

    #[test]
    fn an_empty_report_still_serialises_as_a_report() {
        let (store, vocab, shapes) = fixture("");
        let turtle = ValidationReport::default()
            .serialize(
                oxrdfio::RdfFormat::Turtle,
                &store,
                &vocab,
                &shapes,
                &[vocab.sh_Violation],
            )
            .unwrap();
        assert!(turtle.contains("sh:conforms true"), "{turtle}");
        assert!(!turtle.contains("sh:result "), "{turtle}");
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

    /// A nested result hangs off `sh:detail`, never `sh:result`.
    ///
    /// Checking that both component names appear somewhere in the output, as
    /// the test above does, passes either way — the predicate joining them is
    /// the thing that was wrong, so it has to be asserted directly.
    #[test]
    fn a_nested_result_is_linked_by_sh_detail() {
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

        let g = report.to_oxrdf(&store, &vocab, &shapes, &[vocab.sh_Violation]);
        let p = |t: TermId| NamedNode::new_unchecked(store.iri(t).unwrap_or_default());

        // Exactly one sh:result — from the report to the outer result — and
        // exactly one sh:detail, from the outer result to the inner one.
        assert_eq!(g.triples_for_predicate(&p(vocab.sh_result)).count(), 1);
        let details: Vec<_> = g.triples_for_predicate(&p(vocab.sh_detail)).collect();
        assert_eq!(details.len(), 1);

        // And the detail hangs off the *result*, not the report: the object of
        // sh:result is the subject of sh:detail.
        let outer_node = g
            .triples_for_predicate(&p(vocab.sh_result))
            .next()
            .expect("a sh:result triple")
            .object;
        assert_eq!(outer_node.to_string(), details[0].subject.to_string());
    }

    /// The nested result must survive a serialise-then-reparse round trip.
    ///
    /// This is what the predicate bug actually cost: the reader looks for
    /// `sh:detail`, so a detail written as `sh:result` came back attached to
    /// nothing and was silently lost.
    #[test]
    fn nested_details_survive_a_round_trip() {
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

        let text = report
            .serialize(
                oxrdfio::RdfFormat::Turtle,
                &store,
                &vocab,
                &shapes,
                &[vocab.sh_Violation],
            )
            .expect("the report should serialise");

        let mut store2 = TermStore::new();
        let vocab2 = Vocab::new(&mut store2);
        let mut b = crate::model::GraphBuilder::default();
        crate::model::loader::parse_str(
            &text,
            oxrdfio::RdfFormat::Turtle,
            "http://ex/",
            0,
            &mut store2,
            &mut b,
        )
        .expect("the serialised report should parse");
        let g = b.build();
        let root = g
            .subjects(vocab2.rdf_type, vocab2.sh_ValidationReport)
            .next()
            .expect("the round-tripped graph should hold a report");
        let back = ValidationReport::parse(root, &g, &store2, &vocab2).report;

        assert_eq!(back.results.len(), 1, "one top-level result");
        assert_eq!(back.results[0].details.len(), 1, "its detail came back");
        assert_eq!(
            back.results[0].details[0].source_constraint_component,
            store2.named_node("http://www.w3.org/ns/shacl#DatatypeConstraintComponent")
        );
    }
}
