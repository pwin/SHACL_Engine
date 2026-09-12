//! Validation reports: the SHACL result model and its RDF serialisation.

use oxrdf::{Literal, NamedNode, NamedOrBlankNode, Term, Triple};

/// The report graph type, re-exported so a caller can hold one without
/// depending on a separately-versioned `oxrdf` of its own.
pub use oxrdf::Graph as OxGraph;
/// Likewise the formats [`serialize_graph`] accepts.
pub use oxrdfio::{JsonLdProfileSet, RdfFormat};

use crate::model::{Graph, TermId, TermStore, Vocab};

/// One SHACL validation result.
///
/// Ordered — and the field order below is the sort order — so a report can be
/// put into an order that depends on what it says rather than on how it was
/// produced. Focus node first, so everything reported about one node sits
/// together; then the value, path and shape that distinguish two results on
/// the same node. Validation sorts its results before returning them, which
/// is what lets a run split across threads produce the same bytes as one that
/// did not, and the same bytes on a machine with a different number of cores.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
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
    /// True when no result blocks conformance.
    ///
    /// `disallowed` is SHACL 1.2's `sh:conformanceDisallows`: the severities
    /// that break conformance. **Empty means the specification's default:
    /// `sh:Violation`, `sh:Warning` and `sh:Info`.** In SHACL 1.0 those were
    /// the only severities, so `sh:conforms` was simply "no results"; 1.2
    /// adds `sh:Debug` and `sh:Trace` beneath them, which report without
    /// blocking. The test suite pins each edge: `severity-001` puts one
    /// `sh:Warning` result at `conforms false`, `severity-004` one `sh:Debug`
    /// result at `conforms true`.
    ///
    /// `[sh:Violation]` is what pySHACL's `--allow-warnings` selects. It was
    /// this engine's default until 0.3.0, which put a warning-only report on
    /// the wrong side of the one answer a validator gives. The harness did
    /// not notice because it computed the expected report's conformance with
    /// the same default rather than reading the `sh:conforms` the test wrote.
    pub fn conforms(&self, disallowed: &[TermId], vocab: &Vocab) -> bool {
        let default = vocab.default_disallows();
        let disallowed = if disallowed.is_empty() {
            &default[..]
        } else {
            disallowed
        };
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
    /// A complex `sh:resultPath` — `[ sh:inversePath ex:p ]`, a sequence, an
    /// alternative — is a blank node structure, and a report that named it
    /// without carrying it would be useless to anyone reading the report
    /// alone. So the structure comes with it, rendered from `compiled` where
    /// the shape has a compiled path (so it describes the path that was
    /// walked) and copied from `shapes` otherwise. `sh:closed` is the
    /// otherwise: it reports the offending predicate as the path, and its
    /// shape has no path to render.
    pub fn to_oxrdf(
        &self,
        store: &TermStore,
        vocab: &Vocab,
        shapes: &Graph,
        compiled: &crate::shapes::Shapes,
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
            Literal::from(self.conforms(disallowed, vocab)),
        ));
        // A report that judged conformance by a chosen set of severities says
        // so, as SHACL 1.2 asks, so a reader can tell `conforms true` over a
        // warning from `conforms true` over nothing. Absent when the default
        // applied, which is what absent means.
        for &severity in disallowed {
            g.insert(&Triple::new(
                report.clone(),
                iri(vocab.sh_conformanceDisallows),
                iri(severity),
            ));
        }

        for result in &self.results {
            self.write_result(
                result,
                &report,
                vocab.sh_result,
                store,
                vocab,
                shapes,
                compiled,
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
        compiled: &crate::shapes::Shapes,
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
            // A compound path is written into the report *per result*, and
            // per *occurrence* within it. Two things went wrong before. One
            // copy shared across results made every result reporting the
            // same path point at one node, where the specification's reports
            // give each result its own; and copying node-by-node within a
            // result collapsed a path that names the same blank node twice
            // — `sh:path ( _:inv _:inv )` is legal and says "inverse p, then
            // inverse p", which as an expression is two occurrences of one
            // node. Either way the graph was not isomorphic to the expected
            // one, and every complex-path test in a suite compared as graphs
            // failed on the report's shape rather than on what it found.
            // This engine's own harness compares result by result and never
            // saw it; the HOLOS store's harness compares graphs and did.
            let tag = blank_tag(&node);
            let mut n = 0u64;
            let root = match result
                .source_shape
                .and_then(|id| compiled.id_of(id))
                .and_then(|id| compiled.get(id).path.as_ref())
            {
                Some(path) => render_path(path, store, vocab, &tag, &mut n, g),
                None => copy_occurrence(p, shapes, store, g, &tag, &mut n, 0),
            };
            g.insert(&Triple::new(node.clone(), iri(vocab.sh_resultPath), root));
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
                compiled,
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
        compiled: &crate::shapes::Shapes,
        disallowed: &[TermId],
    ) -> crate::Result<String> {
        serialize_graph(
            &self.to_oxrdf(store, vocab, shapes, compiled, disallowed),
            format,
        )
    }
}

/// Writes an already-built report graph in `format`.
///
/// Split from [`ValidationReport::serialize`] because the graph is
/// self-contained — it holds terms, not handles into a `TermStore` — so a
/// caller that has to outlive the store can keep the graph and choose a format
/// later. The Python bindings do exactly that.
pub fn serialize_graph(graph: &OxGraph, format: oxrdfio::RdfFormat) -> crate::Result<String> {
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

    // `OxGraph` is hash-backed, so `iter()` hands the triples back in an order
    // that changes from one process to the next. Two identical validation runs
    // would then print byte-different reports, which defeats diffing one
    // against another — the main reason to want the report as a graph at all.
    //
    // Sorted by N-Triples text rather than by term, because `Triple` has no
    // `Ord`; the key is cached so it is built once per triple rather than once
    // per comparison. Report size is bounded by the result count, so this is
    // paid on something small.
    let mut triples: Vec<_> = graph.iter().collect();
    triples.sort_by_cached_key(|t| t.to_string());

    let mut out = Vec::new();
    let mut writer = serializer.for_writer(&mut out);
    for triple in triples {
        writer
            .serialize_triple(triple)
            .map_err(|e| crate::Error::Io(e.to_string()))?;
    }
    writer
        .finish()
        .map_err(|e| crate::Error::Io(e.to_string()))?;
    String::from_utf8(out).map_err(|e| crate::Error::Io(e.to_string()))
}

/// A validation report read back out of RDF, plus the severities its author
/// declared as blocking conformance.
#[derive(Debug, Clone)]
pub struct ParsedReport {
    pub report: ValidationReport,
    pub conforms: bool,
    /// `sh:conformanceDisallows` values. Empty when the report declares
    /// none, which is the specification's default: every result blocks.
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

        let disallowed: Vec<TermId> = g.objects(node, vocab.sh_conformanceDisallows).collect();

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

/// A label unique to one result, used to scope its copy of a property path.
fn blank_tag(node: &NamedOrBlankNode) -> String {
    match node {
        NamedOrBlankNode::BlankNode(b) => b.as_str().to_owned(),
        NamedOrBlankNode::NamedNode(n) => n.as_str().to_owned(),
    }
}

/// Relabels a blank node into one result's scope, leaving anything else alone.
fn scoped(term: Term, tag: &str) -> Term {
    match term {
        Term::BlankNode(b) => Term::BlankNode(oxrdf::BlankNode::new_unchecked(format!(
            "{tag}_{}",
            b.as_str()
        ))),
        other => other,
    }
}

/// A blank node in one result's path, numbered within that result.
fn path_bnode(tag: &str, next: &mut u64) -> oxrdf::BlankNode {
    let b = oxrdf::BlankNode::new_unchecked(format!("{tag}_p{}", *next));
    *next += 1;
    b
}

/// Renders a compiled path into the report in SHACL's RDF form.
///
/// Blank nodes are numbered per *occurrence* within the result, so a path
/// that mentions the same sub-expression twice arrives as two occurrences —
/// which is what the expression says.
fn render_path(
    path: &crate::path::Path,
    store: &TermStore,
    vocab: &Vocab,
    tag: &str,
    next: &mut u64,
    g: &mut OxGraph,
) -> Term {
    use crate::path::Path;
    let iri =
        |t: TermId| -> NamedNode { NamedNode::new_unchecked(store.iri(t).unwrap_or_default()) };
    let wrap = |inner: &Path, pred: TermId, next: &mut u64, g: &mut OxGraph| -> Term {
        let object = render_path(inner, store, vocab, tag, next, g);
        let here = path_bnode(tag, next);
        g.insert(&Triple::new(here.clone(), iri(pred), object));
        here.into()
    };
    match path {
        Path::Predicate(p) => Term::NamedNode(iri(*p)),
        Path::Inverse(inner) => wrap(inner, vocab.sh_inversePath, next, g),
        Path::ZeroOrMore(inner) => wrap(inner, vocab.sh_zeroOrMorePath, next, g),
        Path::OneOrMore(inner) => wrap(inner, vocab.sh_oneOrMorePath, next, g),
        Path::ZeroOrOne(inner) => wrap(inner, vocab.sh_zeroOrOnePath, next, g),
        Path::Sequence(parts) => render_list(parts, store, vocab, tag, next, g),
        Path::Alternative(parts) => {
            let object = render_list(parts, store, vocab, tag, next, g);
            let here = path_bnode(tag, next);
            g.insert(&Triple::new(
                here.clone(),
                iri(vocab.sh_alternativePath),
                object,
            ));
            here.into()
        }
    }
}

/// Renders a list of paths as an RDF list, built back to front.
fn render_list(
    parts: &[crate::path::Path],
    store: &TermStore,
    vocab: &Vocab,
    tag: &str,
    next: &mut u64,
    g: &mut OxGraph,
) -> Term {
    let iri =
        |t: TermId| -> NamedNode { NamedNode::new_unchecked(store.iri(t).unwrap_or_default()) };
    let mut tail = Term::NamedNode(iri(vocab.rdf_nil));
    for part in parts.iter().rev() {
        let value = render_path(part, store, vocab, tag, next, g);
        let cell = path_bnode(tag, next);
        g.insert(&Triple::new(cell.clone(), iri(vocab.rdf_first), value));
        g.insert(&Triple::new(cell.clone(), iri(vocab.rdf_rest), tail));
        tail = cell.into();
    }
    tail
}

/// Copies one *occurrence* of a path node into the report, and everything
/// below it.
///
/// Deliberately a tree copy. The source is a path *expression*, and an
/// expression that mentions the same sub-expression twice has two occurrences
/// of it even where the shapes graph stores one node. A copier keyed on the
/// source node writes it once, and a reader cannot recover the path from the
/// result.
///
/// `depth` is a cycle guard. A well-formed path expression is finite and
/// acyclic, but a shapes graph is data and may say otherwise; a copier that
/// trusted it would not return.
fn copy_occurrence(
    node: TermId,
    src: &Graph,
    store: &TermStore,
    dst: &mut OxGraph,
    tag: &str,
    next: &mut u64,
    depth: u32,
) -> Term {
    const MAX_DEPTH: u32 = 64;
    if !store.is_blank(node) || depth >= MAX_DEPTH {
        return scoped(store.to_oxrdf(node), tag);
    }
    let here = path_bnode(tag, next);
    for (p, o) in src.predicate_objects(node) {
        let object = copy_occurrence(o, src, store, dst, tag, next, depth + 1);
        dst.insert(&Triple::new(
            NamedOrBlankNode::BlankNode(here.clone()),
            NamedNode::new_unchecked(store.iri(p).unwrap_or_default()),
            object,
        ));
    }
    here.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GraphBuilder, loader};
    use oxrdf::dataset::CanonicalizationAlgorithm;
    use oxrdfio::RdfFormat;

    fn fixture(turtle: &str) -> (TermStore, Vocab, Graph, crate::shapes::Shapes) {
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
        let graph = b.build();
        let compiled = crate::shapes::Shapes::compile(&graph, &store, &vocab).unwrap();
        (store, vocab, graph, compiled)
    }

    fn canonical(mut g: OxGraph) -> String {
        g.canonicalize(CanonicalizationAlgorithm::Unstable);
        let mut lines: Vec<String> = g.iter().map(|t| t.to_string()).collect();
        lines.sort();
        lines.join("\n")
    }

    #[test]
    fn conformance_depends_on_the_disallowed_severities() {
        let (mut store, vocab, _, _) = fixture("");
        let focus = store.named_node("http://ex/a");
        let report = ValidationReport {
            results: vec![ValidationResult::new(
                focus,
                vocab.sh_DatatypeConstraintComponent,
                vocab.sh_Warning,
            )],
        };

        assert!(
            report.conforms(&[vocab.sh_Violation], &vocab),
            "a warning alone does not break conformance when only violations are disallowed"
        );
        assert!(!report.conforms(&[vocab.sh_Violation, vocab.sh_Warning], &vocab));
        assert!(
            !report.conforms(&[], &vocab),
            "under the specification's default a warning does break conformance"
        );
    }

    #[test]
    fn empty_report_serialises_as_conforming() {
        let (store, vocab, shapes, compiled) = fixture("");
        let g = ValidationReport::default().to_oxrdf(
            &store,
            &vocab,
            &shapes,
            &compiled,
            &[vocab.sh_Violation],
        );
        let text = canonical(g);
        assert!(text.contains("#ValidationReport"));
        assert!(text.contains(r#""true"^^<http://www.w3.org/2001/XMLSchema#boolean>"#));
        assert!(!text.contains("#result>"));
    }

    #[test]
    fn serialises_a_result_with_all_its_fields() {
        let (mut store, vocab, shapes, compiled) = fixture("");
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
        let text =
            canonical(report.to_oxrdf(&store, &vocab, &shapes, &compiled, &[vocab.sh_Violation]));

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
        let (mut store, vocab, shapes, compiled) = fixture(
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
        let text =
            canonical(report.to_oxrdf(&store, &vocab, &shapes, &compiled, &[vocab.sh_Violation]));

        assert!(
            text.contains("#inversePath"),
            "path structure was not copied"
        );
        assert!(text.contains("<http://ex/parent>"));
    }

    #[test]
    fn serialises_a_report_that_parses_back_as_shacl() {
        let (mut store, vocab, shapes, compiled) = fixture("");
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
                &compiled,
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
        let (store, vocab, shapes, compiled) = fixture("");
        let turtle = ValidationReport::default()
            .serialize(
                oxrdfio::RdfFormat::Turtle,
                &store,
                &vocab,
                &shapes,
                &compiled,
                &[vocab.sh_Violation],
            )
            .unwrap();
        assert!(turtle.contains("sh:conforms true"), "{turtle}");
        assert!(!turtle.contains("sh:result "), "{turtle}");
    }

    #[test]
    fn nested_details_are_serialised() {
        let (mut store, vocab, shapes, compiled) = fixture("");
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
        let text =
            canonical(report.to_oxrdf(&store, &vocab, &shapes, &compiled, &[vocab.sh_Violation]));
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
        let (mut store, vocab, shapes, compiled) = fixture("");
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

        let g = report.to_oxrdf(&store, &vocab, &shapes, &compiled, &[vocab.sh_Violation]);
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
        let (mut store, vocab, shapes, compiled) = fixture("");
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
                &compiled,
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
