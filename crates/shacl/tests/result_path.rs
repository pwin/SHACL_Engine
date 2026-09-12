//! `sh:resultPath` must be written the way the SHACL specification's own
//! reports write it: one copy of a compound path per result, and one node per
//! *occurrence* of a sub-expression within it.
//!
//! This is the difference between comparing reports result by result and
//! comparing them as graphs. Every test in this crate's W3C harness compares a
//! fingerprint per result, and under that comparison a report that pointed
//! every result at one shared copy of a path passed. The W3C manifests'
//! expected reports are graphs, and the suite's own comparison is graph
//! isomorphism; under that, every complex-path test failed on the report's
//! shape rather than on what it found. The HOLOS store, which compares as the
//! suite does, found it and carried a fix in its adaptation of this engine.
//! The fix is now here, and so is the comparison that would have caught it.

use oxrdf::dataset::CanonicalizationAlgorithm;
use oxrdf::{Graph as OxGraph, NamedNode, Term};
use oxrdfio::RdfFormat;
use shacl::model::{Graph, GraphBuilder, TermStore, Vocab, loader};
use shacl::shapes::Shapes;

const PREFIX: &str = r#"
@prefix ex: <http://example.org/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
"#;

fn parse(text: &str, scope: u32, store: &mut TermStore) -> Graph {
    let mut b = GraphBuilder::new();
    loader::parse_str(
        &format!("{PREFIX}{text}"),
        RdfFormat::Turtle,
        "http://t/",
        scope,
        store,
        &mut b,
    )
    .expect("fixture should parse");
    b.build()
}

/// Validates and returns the report graph.
fn report_graph(data: &str, shapes: &str) -> OxGraph {
    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let data = parse(data, 0, &mut store);
    let shapes_graph = parse(shapes, 1, &mut store);
    let compiled = Shapes::compile(&shapes_graph, &store, &vocab).expect("shapes should compile");
    let report = shacl::validate::validate_in(&data, &compiled, &shapes_graph, &mut store, &vocab)
        .expect("validation should run");
    // The specification's default severities, which the expected reports
    // below assume: an explicit list would be recorded as
    // `sh:conformanceDisallows` and they carry none.
    report.to_oxrdf(&store, &vocab, &shapes_graph, &compiled, &[])
}

/// An expected report, as the W3C manifests write them.
fn expected_graph(turtle: &str) -> OxGraph {
    let mut g = OxGraph::new();
    for t in oxrdfio::RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri("http://t/")
        .unwrap()
        .for_slice(format!("{PREFIX}{turtle}").as_bytes())
    {
        let q = t.expect("expected report should parse");
        g.insert(&q.into());
    }
    g
}

/// Graph isomorphism — the comparison the W3C suite uses — as a string diff
/// of the canonical forms, so a failure says what differs.
fn assert_isomorphic(mut expected: OxGraph, mut actual: OxGraph, what: &str) {
    expected.canonicalize(CanonicalizationAlgorithm::Unstable);
    actual.canonicalize(CanonicalizationAlgorithm::Unstable);
    let render = |g: &OxGraph| {
        let mut lines: Vec<String> = g.iter().map(|t| t.to_string()).collect();
        lines.sort();
        lines.join("\n")
    };
    let (e, a) = (render(&expected), render(&actual));
    assert!(
        e == a,
        "{what}: the report is not isomorphic to the expected one\n--- expected\n{e}\n--- actual\n{a}"
    );
}

fn sh(local: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("http://www.w3.org/ns/shacl#{local}"))
}

/// The distinct objects of `sh:resultPath` across the report.
fn result_path_nodes(g: &OxGraph) -> Vec<Term> {
    let mut nodes: Vec<Term> = g
        .triples_for_predicate(&sh("resultPath"))
        .map(|t| t.object.into_owned())
        .collect();
    nodes.sort_by_key(|t| t.to_string());
    nodes.dedup();
    nodes
}

#[test]
fn each_result_carries_its_own_copy_of_a_compound_path() {
    // Two focus nodes fail the same inverse-path constraint.
    let data = r#"
ex:alice ex:name "Alice" .
ex:bob ex:name "Bob" .
"#;
    let shapes = r#"
ex:Shape a sh:NodeShape ;
  sh:targetNode ex:alice, ex:bob ;
  sh:property [ sh:path [ sh:inversePath ex:knows ] ; sh:minCount 1 ] .
"#;
    let g = report_graph(data, shapes);

    // The specification's report for this has two results and two path nodes.
    let paths = result_path_nodes(&g);
    assert_eq!(
        paths.len(),
        2,
        "two results must not share one sh:resultPath node; got {paths:?}"
    );

    let expected = expected_graph(
        r#"
[] a sh:ValidationReport ;
   sh:conforms false ;
   sh:result [
     a sh:ValidationResult ;
     sh:focusNode ex:alice ;
     sh:resultPath [ sh:inversePath ex:knows ] ;
     sh:resultSeverity sh:Violation ;
     sh:sourceConstraintComponent sh:MinCountConstraintComponent ;
     sh:sourceShape _:ps ;
   ] ;
   sh:result [
     a sh:ValidationResult ;
     sh:focusNode ex:bob ;
     sh:resultPath [ sh:inversePath ex:knows ] ;
     sh:resultSeverity sh:Violation ;
     sh:sourceConstraintComponent sh:MinCountConstraintComponent ;
     sh:sourceShape _:ps ;
   ] .
"#,
    );
    assert_isomorphic(expected, g, "a shared inverse path");
}

#[test]
fn a_sequence_path_is_copied_whole_per_result() {
    let data = r#"
ex:alice ex:knows ex:carol .
ex:bob ex:knows ex:dave .
ex:carol ex:name 1 .
ex:dave ex:name 2 .
"#;
    let shapes = r#"
ex:Shape a sh:NodeShape ;
  sh:targetNode ex:alice, ex:bob ;
  sh:property [ sh:path ( ex:knows ex:name ) ; sh:datatype xsd:string ] .
"#;
    let g = report_graph(data, shapes);
    assert_eq!(result_path_nodes(&g).len(), 2, "one list head per result");

    let expected = expected_graph(
        r#"
[] a sh:ValidationReport ;
   sh:conforms false ;
   sh:result [
     a sh:ValidationResult ;
     sh:focusNode ex:alice ;
     sh:resultPath ( ex:knows ex:name ) ;
     sh:value 1 ;
     sh:resultSeverity sh:Violation ;
     sh:sourceConstraintComponent sh:DatatypeConstraintComponent ;
     sh:sourceShape _:ps ;
   ] ;
   sh:result [
     a sh:ValidationResult ;
     sh:focusNode ex:bob ;
     sh:resultPath ( ex:knows ex:name ) ;
     sh:value 2 ;
     sh:resultSeverity sh:Violation ;
     sh:sourceConstraintComponent sh:DatatypeConstraintComponent ;
     sh:sourceShape _:ps ;
   ] .
"#,
    );
    assert_isomorphic(expected, g, "a shared sequence path");
}

#[test]
fn a_path_naming_one_node_twice_is_written_as_two_occurrences() {
    // `( _:inv _:inv )` — inverse knows, then inverse knows again. As an
    // expression it has two steps; the shapes graph happens to store them as
    // one node. The report must say two.
    let data = r#"
ex:c ex:knows ex:b .
ex:b ex:knows ex:a .
ex:a ex:name "A" .
"#;
    let shapes = r#"
_:inv sh:inversePath ex:knows .
ex:Shape a sh:NodeShape ;
  sh:targetNode ex:a ;
  sh:property [ sh:path ( _:inv _:inv ) ; sh:maxCount 0 ] .
"#;
    let g = report_graph(data, shapes);

    // Two list cells, each with its own inverse-path node: four blank nodes
    // under the path, not three.
    let expected = expected_graph(
        r#"
[] a sh:ValidationReport ;
   sh:conforms false ;
   sh:result [
     a sh:ValidationResult ;
     sh:focusNode ex:a ;
     sh:resultPath ( [ sh:inversePath ex:knows ] [ sh:inversePath ex:knows ] ) ;
     sh:resultSeverity sh:Violation ;
     sh:sourceConstraintComponent sh:MaxCountConstraintComponent ;
     sh:sourceShape _:ps ;
   ] .
"#,
    );
    assert_isomorphic(expected, g, "a repeated sub-expression");
}

#[test]
fn sh_closed_reports_the_predicate_as_the_path() {
    // `sh:closed` has no compiled path; the offending predicate is reported
    // as the path directly, and there is nothing to copy.
    let data = r#"
ex:alice ex:name "Alice" ; ex:age 3 .
"#;
    let shapes = r#"
ex:Shape a sh:NodeShape ;
  sh:targetNode ex:alice ;
  sh:closed true ;
  sh:property [ sh:path ex:name ] .
"#;
    let g = report_graph(data, shapes);
    let paths = result_path_nodes(&g);
    assert_eq!(
        paths,
        vec![Term::NamedNode(NamedNode::new_unchecked(
            "http://example.org/age"
        ))]
    );
}
