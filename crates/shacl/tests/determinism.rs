//! The RDF report is byte-for-byte reproducible.
//!
//! The README makes this promise so that two reports can be diffed and only
//! real differences show up. It is not free: it needs the triples written in a
//! fixed order and blank nodes numbered in first-seen order rather than
//! carrying the parser's random names for anonymous `[ … ]` nodes. Both are
//! easy to lose — a `HashMap` iterated somewhere in the middle would do it —
//! and losing them produces a report that is still *correct*, so nothing else
//! here would fail.
//!
//! The expected bytes are written out in full rather than compared against a
//! second run. A round-trip test only proves one process agrees with itself,
//! which random blank node labels already do; pinning the text is what makes
//! this fail on the machine where the answer differs. CI runs it on Linux,
//! Windows and 64-bit ARM macOS, so agreement there is the cross-platform
//! claim actually being checked rather than assumed.

use oxrdfio::RdfFormat;
use shacl::model::{Graph, GraphBuilder, TermStore, Vocab, loader};
use shacl::validate::Options;

/// Anonymous blank nodes in the shapes, and two shapes over two focus nodes,
/// so the report contains several results whose relative order has to be
/// pinned down as well as their content.
const SHAPES: &str = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:PersonShape a sh:NodeShape ; sh:targetClass ex:Person ;
    sh:property [ sh:path ex:age ; sh:datatype <http://www.w3.org/2001/XMLSchema#integer> ] ;
    sh:property [ sh:path ex:name ; sh:minCount 1 ] .
"#;

/// `_:labelled` is written with a label on purpose: the report must not carry
/// it through, since a blank node label is local syntax rather than identity.
const DATA: &str = r#"
@prefix ex: <http://example.org/ns#> .
ex:alice a ex:Person ; ex:age "old" .
ex:bob a ex:Person ; ex:age 40 .
_:labelled a ex:Person ; ex:age "young" .
"#;

fn report(format: RdfFormat) -> String {
    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let graph = |text: &str, scope: u32, store: &mut TermStore| {
        let mut b = GraphBuilder::new();
        loader::parse_str(text, RdfFormat::Turtle, "http://t/", scope, store, &mut b)
            .expect("test document should parse");
        b.build()
    };
    let data: Graph = graph(DATA, 0, &mut store);
    let shapes: Graph = graph(SHAPES, 1, &mut store);

    let compiled =
        shacl::shapes::Shapes::compile(&shapes, &store, &vocab).expect("shapes should compile");
    let report = shacl::validate::validate_in_with(
        &data,
        &compiled,
        &shapes,
        &mut store,
        &vocab,
        Options::default(),
    )
    .expect("validation should succeed");

    report
        .serialize(
            format,
            &store,
            &vocab,
            &shapes,
            &compiled,
            &[vocab.sh_Violation],
        )
        .expect("report should serialise")
}

/// N-Triples rather than Turtle: it has no prefixes, no nesting and one triple
/// per line, so a diff points at the triple that changed instead of at a
/// reflowed block.
#[test]
fn the_report_is_exactly_these_bytes() {
    // `_:r0` is the report, `_:r1`..`_:r5` the results, numbered in the order
    // they were produced. `_:0_b0` is the data graph's labelled blank node and
    // `_:1_b1` a shape's anonymous one — the scope prefix is what keeps two
    // documents' labels apart, and it appears here so that a change to the
    // scoping scheme shows up as a failure rather than as a surprise later.
    let expected = "\
_:r0 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://www.w3.org/ns/shacl#ValidationReport> .
_:r0 <http://www.w3.org/ns/shacl#conformanceDisallows> <http://www.w3.org/ns/shacl#Violation> .
_:r0 <http://www.w3.org/ns/shacl#conforms> \"false\"^^<http://www.w3.org/2001/XMLSchema#boolean> .
_:r0 <http://www.w3.org/ns/shacl#result> _:r1 .
_:r0 <http://www.w3.org/ns/shacl#result> _:r2 .
_:r0 <http://www.w3.org/ns/shacl#result> _:r3 .
_:r0 <http://www.w3.org/ns/shacl#result> _:r4 .
_:r0 <http://www.w3.org/ns/shacl#result> _:r5 .
_:r1 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://www.w3.org/ns/shacl#ValidationResult> .
_:r1 <http://www.w3.org/ns/shacl#focusNode> <http://example.org/ns#alice> .
_:r1 <http://www.w3.org/ns/shacl#resultPath> <http://example.org/ns#name> .
_:r1 <http://www.w3.org/ns/shacl#resultSeverity> <http://www.w3.org/ns/shacl#Violation> .
_:r1 <http://www.w3.org/ns/shacl#sourceConstraintComponent> <http://www.w3.org/ns/shacl#MinCountConstraintComponent> .
_:r1 <http://www.w3.org/ns/shacl#sourceShape> _:1_b2 .
_:r2 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://www.w3.org/ns/shacl#ValidationResult> .
_:r2 <http://www.w3.org/ns/shacl#focusNode> <http://example.org/ns#alice> .
_:r2 <http://www.w3.org/ns/shacl#resultPath> <http://example.org/ns#age> .
_:r2 <http://www.w3.org/ns/shacl#resultSeverity> <http://www.w3.org/ns/shacl#Violation> .
_:r2 <http://www.w3.org/ns/shacl#sourceConstraintComponent> <http://www.w3.org/ns/shacl#DatatypeConstraintComponent> .
_:r2 <http://www.w3.org/ns/shacl#sourceShape> _:1_b1 .
_:r2 <http://www.w3.org/ns/shacl#value> \"old\" .
_:r3 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://www.w3.org/ns/shacl#ValidationResult> .
_:r3 <http://www.w3.org/ns/shacl#focusNode> <http://example.org/ns#bob> .
_:r3 <http://www.w3.org/ns/shacl#resultPath> <http://example.org/ns#name> .
_:r3 <http://www.w3.org/ns/shacl#resultSeverity> <http://www.w3.org/ns/shacl#Violation> .
_:r3 <http://www.w3.org/ns/shacl#sourceConstraintComponent> <http://www.w3.org/ns/shacl#MinCountConstraintComponent> .
_:r3 <http://www.w3.org/ns/shacl#sourceShape> _:1_b2 .
_:r4 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://www.w3.org/ns/shacl#ValidationResult> .
_:r4 <http://www.w3.org/ns/shacl#focusNode> _:0_b0 .
_:r4 <http://www.w3.org/ns/shacl#resultPath> <http://example.org/ns#name> .
_:r4 <http://www.w3.org/ns/shacl#resultSeverity> <http://www.w3.org/ns/shacl#Violation> .
_:r4 <http://www.w3.org/ns/shacl#sourceConstraintComponent> <http://www.w3.org/ns/shacl#MinCountConstraintComponent> .
_:r4 <http://www.w3.org/ns/shacl#sourceShape> _:1_b2 .
_:r5 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://www.w3.org/ns/shacl#ValidationResult> .
_:r5 <http://www.w3.org/ns/shacl#focusNode> _:0_b0 .
_:r5 <http://www.w3.org/ns/shacl#resultPath> <http://example.org/ns#age> .
_:r5 <http://www.w3.org/ns/shacl#resultSeverity> <http://www.w3.org/ns/shacl#Violation> .
_:r5 <http://www.w3.org/ns/shacl#sourceConstraintComponent> <http://www.w3.org/ns/shacl#DatatypeConstraintComponent> .
_:r5 <http://www.w3.org/ns/shacl#sourceShape> _:1_b1 .
_:r5 <http://www.w3.org/ns/shacl#value> \"young\" .
";
    let actual = report(RdfFormat::NTriples);
    // To re-pin after a change accepted deliberately — see the README on what
    // is and is not promised between releases:
    //   SHACL_PRINT_REPORT=1 cargo test -p shacl --test determinism -- --nocapture
    if std::env::var_os("SHACL_PRINT_REPORT").is_some() {
        eprintln!("{actual}");
    }
    // Compared as sets of lines first: an ordering change and a content change
    // are different bugs, and the assertion should say which one happened.
    // A reordering renumbers the `_:rN` labels too, so it shows up here as
    // well; the message names the first assertion that failed.
    let a: std::collections::BTreeSet<_> = actual.lines().collect();
    let e: std::collections::BTreeSet<_> = expected.lines().collect();
    assert_eq!(a, e, "the report's content changed");
    assert_eq!(actual, expected, "the report's ordering changed");
}

/// Two runs in one process, which catches anything seeded per-run — a hasher,
/// an address, a clock — that the pinned bytes above would also catch but only
/// on the run that happened to differ.
#[test]
fn two_runs_agree() {
    for format in [RdfFormat::NTriples, RdfFormat::Turtle, RdfFormat::RdfXml] {
        assert_eq!(report(format), report(format), "{format:?} is not stable");
    }
}

/// A labelled blank node in the source must not reach the report: RDF treats
/// the label as local syntax, and carrying it would tie the output to a
/// spelling the input was free to choose.
#[test]
fn source_blank_node_labels_do_not_survive() {
    let out = report(RdfFormat::NTriples);
    assert!(
        !out.contains("labelled"),
        "the source's own blank node label reached the report:\n{out}"
    );
}
