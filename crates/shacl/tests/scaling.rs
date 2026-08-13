//! Validation must stay linear in the number of focus nodes.
//!
//! The recursion guard keeps a visited set of (shape, focus node) pairs, and a
//! node shape pushes one pair per focus node — so the set is as large as the
//! data, and anything that *searches* it once per node is quadratic. That is
//! not a recursive edge case: it is what every `sh:NodeShape` with an
//! `sh:property` does over a large target set, which is to say the ordinary
//! case. It reached a release as a `Vec::contains`, costing 44 seconds of
//! validation at 100k instances against 0.6 seconds of loading.
//!
//! Nothing else in the suite would have caught it. The recursion tests use
//! deep, narrow graphs where the visited set stays small, and the conformance
//! suite's documents are tiny. This measures the shape of the curve instead of
//! a wall time, so it means the same thing on a slow machine as on a fast one,
//! and in a debug build as in a release one.

use std::time::Instant;

use oxrdfio::RdfFormat;
use shacl::model::{Graph, GraphBuilder, TermStore, Vocab, loader};
use shacl::validate::Options;

const SHAPES: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:datatype xsd:string ] ;
    sh:property [ sh:path ex:age  ; sh:maxCount 1 ; sh:datatype xsd:integer ] .
"#;

/// One instance in ten fails both property shapes, so results are produced
/// rather than the run short-circuiting through an all-conforming graph.
fn people(n: usize) -> String {
    let mut s = String::from("@prefix ex: <http://example.org/> .\n");
    for i in 0..n {
        let age = if i % 10 == 0 {
            "\"old\"".to_string()
        } else {
            (i % 90).to_string()
        };
        s.push_str(&format!("ex:p{i} a ex:Person ; ex:name \"P{i}\" ; ex:age {age} .\n"));
    }
    s
}

fn validate_ms(n: usize) -> (u128, usize) {
    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let graph = |text: &str, scope: u32, store: &mut TermStore| -> Graph {
        let mut b = GraphBuilder::new();
        loader::parse_str(text, RdfFormat::Turtle, "http://t/", scope, store, &mut b)
            .expect("should parse");
        b.build()
    };
    let data = graph(&people(n), 0, &mut store);
    let shapes = graph(SHAPES, 1, &mut store);
    let compiled = shacl::shapes::Shapes::compile(&shapes, &store, &vocab).expect("should compile");

    // Loading is deliberately outside the measurement: it was never the
    // problem, and including it would mask the term being measured.
    let t = Instant::now();
    let report = shacl::validate::validate_in_with(
        &data,
        &compiled,
        &shapes,
        &mut store,
        &vocab,
        Options::default(),
    )
    .expect("should validate");
    (t.elapsed().as_millis(), report.results.len())
}

#[test]
fn validation_does_not_grow_quadratically_in_focus_nodes() {
    // Warm up so the first measurement does not carry one-off costs.
    let _ = validate_ms(2_000);

    let (small_ms, small_results) = validate_ms(5_000);
    let (large_ms, large_results) = validate_ms(20_000);

    // One instance in ten carries a non-integer age, and nothing else here is
    // wrong, so it is one finding per bad instance.
    assert_eq!(small_results, 500, "the fixture should produce findings");
    assert_eq!(large_results, 2_000);

    // Four times the data. Linear would be ~4x, quadratic ~16x. The bound is
    // deliberately loose — this is a shape test, not a benchmark, and a loaded
    // CI runner should not fail it — but 16x cannot hide under 8x.
    let small_ms = small_ms.max(1);
    let ratio = large_ms as f64 / small_ms as f64;
    assert!(
        ratio < 8.0,
        "4x the focus nodes took {ratio:.1}x the time \
         ({small_ms}ms -> {large_ms}ms); linear is ~4x, quadratic ~16x"
    );
}
