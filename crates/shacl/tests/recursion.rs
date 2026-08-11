//! Recursive shapes graphs must not take the process down.
//!
//! A stack overflow is not a panic: `catch_unwind` cannot see it, so it kills
//! the host process outright. The Python bindings turn a Rust failure into a
//! Python exception, which makes an overflow the one failure mode they cannot
//! contain — hence tests here rather than just a depth constant.
//!
//! Two shapes are enough to build a cycle, and none of what follows is
//! malformed by any SHACL rule; the spec simply leaves recursion undefined.

use oxrdfio::RdfFormat;
use shacl::model::{Graph, GraphBuilder, TermStore, Vocab, loader};

/// Validates `data` against `shapes`, both given as Turtle.
fn validate(data: &str, shapes: &str) -> shacl::Result<usize> {
    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let graph = |text: &str, scope: u32, store: &mut TermStore| -> Graph {
        let mut b = GraphBuilder::default();
        loader::parse_str(
            text,
            RdfFormat::Turtle,
            "http://example.org/",
            scope,
            store,
            &mut b,
        )
        .expect("test document should parse");
        b.build()
    };
    let d = graph(data, 0, &mut store);
    let s = graph(shapes, 1, &mut store);
    shacl::validate::validate(&d, &s, &mut store, &vocab).map(|r| r.results.len())
}

const CYCLE_DATA: &str = r#"
@prefix ex: <http://example.org/ns#> .
ex:a ex:knows ex:b .
ex:b ex:knows ex:a .
"#;

/// Two property shapes that name each other, over a two-node data cycle.
///
/// `sh:property` used to reach `validate_shape` without consulting the visited
/// set that `sh:node` and the logical constraints already went through, so this
/// recurred until the stack ran out.
#[test]
fn mutually_recursive_property_shapes_terminate() {
    let shapes = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:Q1 a sh:PropertyShape ; sh:path ex:knows ; sh:property ex:Q2 .
ex:Q2 a sh:PropertyShape ; sh:path ex:knows ; sh:property ex:Q1 .
ex:Root a sh:NodeShape ; sh:targetNode ex:a ; sh:property ex:Q1 .
"#;
    assert_eq!(validate(CYCLE_DATA, shapes).unwrap(), 0);
}

/// The same, with the shape naming itself rather than going through a partner.
#[test]
fn a_directly_self_recursive_property_shape_terminates() {
    let shapes = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:Q a sh:PropertyShape ; sh:path ex:knows ; sh:property ex:Q .
ex:Root a sh:NodeShape ; sh:targetNode ex:a ; sh:property ex:Q .
"#;
    assert_eq!(validate(CYCLE_DATA, shapes).unwrap(), 0);
}

/// Breaking the cycle must not cost real results.
///
/// The strongest way to say that: the cyclic shapes graph and the same graph
/// with the back-edge removed must report exactly the same thing. Everything
/// the guard drops is a (shape, node) pair already being validated, so it can
/// only ever remove a repeat — never a finding.
#[test]
fn a_cycle_reports_what_the_same_shapes_without_it_report() {
    let data = r#"
@prefix ex: <http://example.org/ns#> .
ex:a ex:knows ex:b ; ex:age "not a number" .
ex:b ex:knows ex:a ; ex:age "also not a number" .
"#;
    let common = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:Q1 a sh:PropertyShape ;
    sh:path ex:knows ;
    sh:property ex:Q2 ;
    sh:property [ sh:path ex:age ; sh:datatype xsd:integer ] .
ex:Root a sh:NodeShape ; sh:targetNode ex:a ; sh:property ex:Q1 .
"#;
    // ex:Q2 closes the loop in one and dead-ends in the other.
    let cyclic =
        format!("{common}\nex:Q2 a sh:PropertyShape ; sh:path ex:knows ; sh:property ex:Q1 .");
    let acyclic = format!("{common}\nex:Q2 a sh:PropertyShape ; sh:path ex:knows .");

    let n = validate(data, &acyclic).unwrap();
    assert_eq!(validate(data, &cyclic).unwrap(), n);

    // And it is not vacuously equal: the age check hangs off ex:Q1, whose path
    // is ex:knows, so it lands on the value ex:b rather than on ex:a.
    assert_eq!(n, 1);
}

/// A shape cycle walked over distinct data nodes is not truncated at the first
/// repeated *shape*, only at a repeated (shape, node) pair.
#[test]
fn a_shape_cycle_over_a_chain_visits_every_node() {
    let shapes = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:Q a sh:PropertyShape ;
    sh:path ex:knows ;
    sh:property ex:Q ;
    sh:property [ sh:path ex:age ; sh:datatype xsd:integer ] .
ex:Root a sh:NodeShape ; sh:targetNode ex:a1 ; sh:property ex:Q .
"#;
    let data = r#"
@prefix ex: <http://example.org/ns#> .
ex:a1 ex:knows ex:a2 . ex:a2 ex:knows ex:a3 . ex:a3 ex:knows ex:a4 .
ex:a2 ex:age "x" . ex:a3 ex:age "y" . ex:a4 ex:age "z" .
"#;
    // a2, a3 and a4 are each reached once; a1 is never a value of ex:knows.
    assert_eq!(validate(data, shapes).unwrap(), 3);
}

/// Distinct pairs do not bound the descent on their own — their number is the
/// product of shapes and nodes — so a long enough chain must hit the depth cap
/// and say so, rather than overflowing or quietly returning a short report.
#[test]
fn a_chain_deeper_than_the_limit_is_an_error_not_a_crash() {
    let shapes = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:Q a sh:PropertyShape ; sh:path ex:knows ; sh:property ex:Q .
ex:Root a sh:NodeShape ; sh:targetNode ex:a0 ; sh:property ex:Q .
"#;
    let mut data = String::from("@prefix ex: <http://example.org/ns#> .\n");
    for i in 0..500 {
        data.push_str(&format!("ex:a{i} ex:knows ex:a{}.\n", i + 1));
    }
    match validate(&data, shapes) {
        Err(shacl::Error::Recursion(m)) => assert!(m.contains("48"), "unexpected message: {m}"),
        other => panic!("expected a recursion error, got {other:?}"),
    }
}

/// The README documents the limit and quotes the error. Both are the kind of
/// prose that goes stale silently — a reader who trusts a stale number debugs
/// the wrong thing — so the number is asserted against the message the engine
/// actually produces rather than maintained by hand.
#[test]
fn the_readme_quotes_the_real_recursion_error() {
    let shapes = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:Q a sh:PropertyShape ; sh:path ex:knows ; sh:property ex:Q .
ex:Root a sh:NodeShape ; sh:targetNode ex:a0 ; sh:property ex:Q .
"#;
    let mut data = String::from("@prefix ex: <http://example.org/ns#> .\n");
    for i in 0..500 {
        data.push_str(&format!("ex:a{i} ex:knows ex:a{}.\n", i + 1));
    }
    let Err(err) = validate(&data, shapes) else {
        panic!("expected a recursion error");
    };

    let readme = std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../README.md"),
    )
    .expect("README.md should be readable");

    // `Display` prefixes "recursion limit exceeded:", which is what a user
    // sees, so the README is checked against the whole rendered line.
    let line = err.to_string();
    assert!(
        readme.contains(&line),
        "README should quote the real error, which is:\n{line}"
    );

    let depth: usize = line
        .split_whitespace()
        .find_map(|w| w.parse().ok())
        .expect("the message should name the depth");
    assert!(
        readme.contains(&format!("**{depth} levels of nesting**")),
        "README should say {depth} levels; update it when MAX_DEPTH moves"
    );
}

/// `sh:memberShape` recursed without the guard too.
#[test]
fn a_recursive_member_shape_terminates() {
    let shapes = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:Q a sh:PropertyShape ; sh:path ex:list ; sh:memberShape ex:M .
ex:M a sh:NodeShape ; sh:property ex:Q .
ex:Root a sh:NodeShape ; sh:targetNode ex:a ; sh:property ex:Q .
"#;
    let data = r#"
@prefix ex: <http://example.org/ns#> .
ex:a ex:list ( ex:a ) .
"#;
    // Terminating at all is the point; the count only pins the current answer.
    assert!(validate(data, shapes).is_ok());
}
