//! A validation run over an index file must be indistinguishable from one over
//! the document it was built from.
//!
//! The index exists to skip parsing, and skipping work is only safe if it
//! changes nothing observable. That is a stronger claim than "the triples come
//! back": term ids are positions in the store, blank node names are a function
//! of the order labels were first seen, and a report is compared by diffing it
//! against another report. So the property under test is byte-identity of the
//! serialised report, not set-equality of the triples.
//!
//! The corruption tests are here for a different reason. An index file is
//! input like any other, and a library that panics on a truncated file gives a
//! host process nothing to catch.

use oxrdfio::RdfFormat;
use shacl::model::index::{self, SourceDigest};
use shacl::model::{Graph, GraphBuilder, TermStore, Vocab, loader};

/// Data exercising the parts of a store that are easy to lose in a round trip:
/// blank nodes whose numbering must be preserved, every literal shape, and an
/// RDF 1.2 triple term.
const DATA: &str = r#"
@prefix ex: <http://example.org/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:alice a ex:Person ;
    ex:name "Alice" ;
    ex:age 30 ;
    ex:score "1.5"^^xsd:decimal ;
    ex:label "Alice"@en ;
    ex:knows [ a ex:Person ; ex:name "Bob" ] ;
    ex:knows [ a ex:Person ; ex:name "Carol" ; ex:age "not a number" ] .

ex:dave a ex:Person ;
    ex:name "Dave", "David" .

<< ex:alice ex:knows ex:dave >> ex:certainty 0.9 .
"#;

const SHAPES: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:property [ sh:path ex:name ; sh:maxCount 1 ; sh:datatype xsd:string ] ;
    sh:property [ sh:path ex:age ; sh:datatype xsd:integer ] .
"#;

fn parse_into(text: &str, scope: u32, store: &mut TermStore) -> Graph {
    let mut b = GraphBuilder::new();
    loader::parse_str(text, RdfFormat::Turtle, "http://t/", scope, store, &mut b)
        .expect("fixture should parse");
    b.build()
}

/// Loads data and shapes the ordinary way and serialises a report.
fn report_from_source() -> String {
    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let data = parse_into(DATA, 0, &mut store);
    let shapes_graph = parse_into(SHAPES, 1, &mut store);
    let compiled = shacl::shapes::Shapes::compile(&shapes_graph, &store, &vocab)
        .expect("shapes should compile");
    let report = shacl::validate::validate_in(&data, &compiled, &shapes_graph, &mut store, &vocab)
        .expect("validation should run");
    report
        .serialize(
            RdfFormat::NTriples,
            &store,
            &vocab,
            &shapes_graph,
            &compiled,
            &[],
        )
        .expect("report should serialise")
}

/// Writes an index for the data, reads it back, and serialises a report from
/// the restored store.
fn report_via_index() -> String {
    // --- the indexing run: parse once, write the index, then forget it all.
    let bytes = {
        let mut store = TermStore::new();
        // The vocabulary is interned first, exactly as a validating run does,
        // so the ids it occupies are the same on both sides.
        let _ = Vocab::new(&mut store);
        let data = parse_into(DATA, 0, &mut store);
        let mut bytes = Vec::new();
        index::write(&mut bytes, &store, &data, SourceDigest::of(DATA.as_bytes()))
            .expect("index should write");
        bytes
    };

    // --- the validating run: no RDF parser touches the data.
    let (mut store, data) = index::read(&mut &bytes[..], Some(SourceDigest::of(DATA.as_bytes())))
        .expect("index should read");
    // `Vocab::new` interns, and interning is idempotent, so this finds the ids
    // already in the restored store rather than minting new ones.
    let vocab = Vocab::new(&mut store);
    let shapes_graph = parse_into(SHAPES, 1, &mut store);
    let compiled = shacl::shapes::Shapes::compile(&shapes_graph, &store, &vocab)
        .expect("shapes should compile");
    let report = shacl::validate::validate_in(&data, &compiled, &shapes_graph, &mut store, &vocab)
        .expect("validation should run");
    report
        .serialize(
            RdfFormat::NTriples,
            &store,
            &vocab,
            &shapes_graph,
            &compiled,
            &[],
        )
        .expect("report should serialise")
}

#[test]
fn a_report_from_an_index_is_byte_identical_to_one_from_the_source() {
    let from_source = report_from_source();
    let via_index = report_via_index();

    // Sanity: the fixture must actually produce findings, or this test would
    // pass just as well on two empty reports.
    assert!(
        from_source.contains("ValidationResult"),
        "fixture should report violations, got:\n{from_source}"
    );
    assert_eq!(
        from_source, via_index,
        "an index must change nothing observable about a run"
    );
}

#[test]
fn the_index_survives_a_round_trip_intact() {
    let mut store = TermStore::new();
    let _ = Vocab::new(&mut store);
    let data = parse_into(DATA, 0, &mut store);

    let mut bytes = Vec::new();
    index::write(&mut bytes, &store, &data, SourceDigest(0)).expect("index should write");
    let (restored, graph) = index::read(&mut &bytes[..], None).expect("index should read");

    assert_eq!(graph.len(), data.len(), "triple count");
    assert_eq!(
        graph.iter().collect::<Vec<_>>(),
        data.iter().collect::<Vec<_>>(),
        "triples, in order"
    );
    // Every term must render the same from the restored store, which is what
    // makes the ids in those triples mean the same thing.
    for i in 0..data.len() {
        for t in data.iter().nth(i).expect("row") {
            assert_eq!(
                restored.to_oxrdf(t),
                store.to_oxrdf(t),
                "term {} renders differently after a round trip",
                t.as_raw()
            );
        }
    }
}

#[test]
fn interning_into_a_restored_store_finds_what_is_already_there() {
    let mut store = TermStore::new();
    let _ = Vocab::new(&mut store);
    let data = parse_into(DATA, 0, &mut store);
    let alice = store.named_node("http://example.org/alice");
    let blanks = store.blank_node(0, "b0");

    let mut bytes = Vec::new();
    index::write(&mut bytes, &store, &data, SourceDigest(0)).expect("index should write");
    let (mut restored, _) = index::read(&mut &bytes[..], None).expect("index should read");

    assert_eq!(
        restored.named_node("http://example.org/alice"),
        alice,
        "an IRI already in the index must not be interned a second time"
    );
    assert_eq!(
        restored.blank_node(0, "b0"),
        blanks,
        "blank node numbering must survive, or reports would rename nodes"
    );
    // And a term that was *not* in the source still interns cleanly, which is
    // what the shapes graph does after an index is loaded.
    let fresh = restored.named_node("http://example.org/not-in-the-data");
    assert_ne!(fresh, alice);
    assert_eq!(
        restored.iri(fresh),
        Some("http://example.org/not-in-the-data")
    );
}

#[test]
fn an_index_from_a_different_document_is_refused() {
    let mut store = TermStore::new();
    let _ = Vocab::new(&mut store);
    let data = parse_into(DATA, 0, &mut store);
    let mut bytes = Vec::new();
    index::write(&mut bytes, &store, &data, SourceDigest::of(DATA.as_bytes()))
        .expect("index should write");

    let Err(err) = index::read(&mut &bytes[..], Some(SourceDigest::of(b"different bytes"))) else {
        panic!("a stale index must be refused");
    };
    assert!(
        err.to_string().contains("different document"),
        "the error should say the index is stale, got: {err}"
    );
}

#[test]
fn a_truncated_index_is_an_error_not_a_panic() {
    let mut store = TermStore::new();
    let _ = Vocab::new(&mut store);
    let data = parse_into(DATA, 0, &mut store);
    let mut bytes = Vec::new();
    index::write(&mut bytes, &store, &data, SourceDigest(0)).expect("index should write");

    // Every prefix of a valid file. Each one is a plausible result of an
    // interrupted write, and none may panic.
    for cut in 0..bytes.len() {
        let err = index::read(&mut &bytes[..cut], None);
        assert!(
            err.is_err(),
            "a file truncated to {cut} of {} bytes was accepted",
            bytes.len()
        );
    }
}

#[test]
fn a_corrupt_index_is_an_error_not_a_panic() {
    let mut store = TermStore::new();
    let _ = Vocab::new(&mut store);
    let data = parse_into(DATA, 0, &mut store);
    let mut bytes = Vec::new();
    index::write(&mut bytes, &store, &data, SourceDigest(0)).expect("index should write");

    // Flip a byte at a time and read it back. Most corruptions are caught;
    // some land in a value that happens to stay in range, which is fine. The
    // claim under test is only that none of them panics.
    for i in (0..bytes.len()).step_by(7) {
        let mut damaged = bytes.clone();
        damaged[i] = damaged[i].wrapping_add(0x5b);
        let _ = index::read(&mut &damaged[..], None);
    }
}

#[test]
fn a_file_that_is_not_an_index_is_rejected_by_name() {
    let Err(err) = index::read(&mut &b"@prefix ex: <http://example.org/> ."[..], None) else {
        panic!("Turtle is not an index file");
    };
    assert!(
        err.to_string().contains("not a SHACL index file"),
        "pointing a reader at the wrong file should say so, got: {err}"
    );
}
