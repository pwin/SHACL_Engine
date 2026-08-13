//! A blank node bound into a SPARQL constraint must be a constant.
//!
//! SPARQL gives a blank node written in a *pattern* the meaning of a variable
//! that cannot be selected: it matches anything. So substituting `$this` with a
//! blank node focus node does not pin the pattern to that node — it unpins it
//! entirely, and the constraint is evaluated against every node the pattern
//! can reach. With N blank node focus nodes sharing a shape, each one matches
//! all N, and the report carries N² results instead of N.
//!
//! Named IRI focus nodes were never affected, which is what let this survive:
//! almost every shape targets IRIs, and the failure needs a target that can
//! reach a blank node — `sh:targetSubjectsOf` of a predicate an anonymous node
//! happens to carry.

use oxrdfio::RdfFormat;
use shacl::model::{Graph, GraphBuilder, TermStore, Vocab, loader};
use shacl::validate::Options;

fn fixture(data: &str, shapes: &str) -> usize {
    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let graph = |text: &str, scope: u32, store: &mut TermStore| -> Graph {
        let mut b = GraphBuilder::new();
        loader::parse_str(text, RdfFormat::Turtle, "http://t/", scope, store, &mut b)
            .expect("test document should parse");
        b.build()
    };
    let data = graph(data, 0, &mut store);
    let shapes = graph(shapes, 1, &mut store);

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
    report.results.len()
}

/// Targets every subject of `rdfs:label`, which a blank node can carry just as
/// a named resource can, and faults on any `ex:p` value at all.
const SHAPES: &str = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:S a sh:NodeShape ;
    sh:targetSubjectsOf <http://www.w3.org/2000/01/rdf-schema#label> ;
    sh:sparql [
        sh:message "has ex:p" ;
        sh:select """SELECT $this ?value WHERE { $this <http://example.org/ns#p> ?value }"""
    ] .
"#;

fn blank_data(n: usize) -> String {
    let mut s = String::from("@prefix ex: <http://example.org/ns#> .\n");
    s.push_str("@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n");
    for i in 0..n {
        s.push_str(&format!("[] rdfs:label \"b{i}\" ; ex:p {i} .\n"));
    }
    s
}

fn named_data(n: usize) -> String {
    let mut s = String::from("@prefix ex: <http://example.org/ns#> .\n");
    s.push_str("@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n");
    for i in 0..n {
        s.push_str(&format!("ex:n{i} rdfs:label \"n{i}\" ; ex:p {i} .\n"));
    }
    s
}

/// The bug, at the sizes it was characterised at: N=2 gave 4, N=3 gave 9,
/// N=4 gave 16.
#[test]
fn blank_node_focus_nodes_do_not_cross_join() {
    for n in [1usize, 2, 3, 4, 8] {
        let got = fixture(&blank_data(n), SHAPES);
        assert_eq!(
            got,
            n,
            "{n} blank node focus nodes should give {n} results, got {got}\
             {}",
            if got == n * n {
                " — the exact N² cross join"
            } else {
                ""
            }
        );
    }
}

/// The control. This path was always correct, and it has to stay that way:
/// a fix that pinned blank nodes by breaking IRIs would pass the test above.
#[test]
fn named_focus_nodes_are_unaffected() {
    for n in [1usize, 2, 3, 4, 8] {
        assert_eq!(fixture(&named_data(n), SHAPES), n);
    }
}

/// A mixed graph, where the two kinds have to be counted together — the fix
/// must not special-case one population.
#[test]
fn a_mixed_graph_counts_each_focus_node_once() {
    let mut data = blank_data(3);
    data.push_str(&named_data(2).replace("@prefix ex: <http://example.org/ns#> .\n", ""));
    assert_eq!(fixture(&data, SHAPES), 5);
}

/// `$currentShape` has the same problem for the same reason, and hits far more
/// shapes: an anonymous property shape — `[ sh:path … ; sh:sparql … ]`, the
/// ordinary way to write one — *is* a blank node, so a constraint that
/// mentions `$currentShape` was matching against every shape in the graph.
#[test]
fn an_anonymous_shape_binds_current_shape_as_a_constant() {
    let shapes = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:S a sh:NodeShape ;
    sh:targetNode ex:a ;
    sh:property [
        sh:path ex:p ;
        sh:sparql [
            sh:message "current shape" ;
            sh:select """SELECT $this ?value WHERE {
                $this <http://example.org/ns#p> ?value .
                GRAPH $shapesGraph {
                    $currentShape <http://www.w3.org/ns/shacl#path> ?path .
                }
            }"""
        ]
    ] .
ex:Other a sh:NodeShape ; sh:property [ sh:path ex:q ] .
ex:Third a sh:NodeShape ; sh:property [ sh:path ex:r ] .
"#;
    let data = r#"
@prefix ex: <http://example.org/ns#> .
ex:a ex:p 1 .
"#;
    // The lookup has to run against the shapes graph, which is the only place
    // `sh:path` exists — in the data graph it matches nothing whether
    // `$currentShape` is a constant or not, and the test would pass without
    // testing anything.
    //
    // One focus node, one value, one shape: one result. Three anonymous
    // property shapes carry `sh:path`, so an unpinned `$currentShape` finds
    // them all and reports three.
    assert_eq!(fixture(data, shapes), 1);
}

/// The size the divergence was found at in the field: 60 blank node focus
/// nodes reported 3600 results. Large enough that a quadratic would be
/// unmistakable, and cheap enough to keep in the suite.
#[test]
fn sixty_blank_focus_nodes_give_sixty_results() {
    assert_eq!(fixture(&blank_data(60), SHAPES), 60);
}

/// A blank node handed back as the offending value reaches the report as
/// itself.
///
/// `BIND($this AS ?value)` is the case that exposes however the engine carries
/// a pre-bound blank node internally: `sh:value` has to name the node the data
/// holds. It was silently absent before — `from_term` could not resolve a
/// blank node — and a release later it was briefly an engine-internal IRI.
/// Both were invisible without asking for the value back.
#[test]
fn a_blank_node_value_reaches_the_report_as_itself() {
    let shapes = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:S a sh:NodeShape ;
    sh:targetSubjectsOf <http://www.w3.org/2000/01/rdf-schema#label> ;
    sh:sparql [
        sh:message "reports itself" ;
        sh:select """SELECT $this ?value WHERE {
            $this <http://example.org/ns#p> ?ignored .
            BIND($this AS ?value)
        }"""
    ] .
"#;
    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let graph = |text: &str, scope: u32, store: &mut TermStore| -> Graph {
        let mut b = GraphBuilder::new();
        loader::parse_str(text, RdfFormat::Turtle, "http://t/", scope, store, &mut b).unwrap();
        b.build()
    };
    let data = graph(&blank_data(2), 0, &mut store);
    let shapes_g = graph(shapes, 1, &mut store);
    let compiled = shacl::shapes::Shapes::compile(&shapes_g, &store, &vocab).unwrap();
    let report = shacl::validate::validate_in_with(
        &data,
        &compiled,
        &shapes_g,
        &mut store,
        &vocab,
        Options::default(),
    )
    .unwrap();

    assert_eq!(report.results.len(), 2);
    let rendered = report
        .serialize(
            RdfFormat::NTriples,
            &store,
            &vocab,
            &shapes_g,
            &[vocab.sh_Violation],
        )
        .unwrap();
    assert!(
        !rendered.contains("urn:x-shacl:bnode:"),
        "the stand-in IRI leaked into the report:\n{rendered}"
    );
    // And `sh:value` is present and is a blank node — not merely absent,
    // which is how this looked before the round trip closed.
    assert!(rendered.contains("shacl#value> _:"), "{rendered}");
}

/// `FILTER(isIRI($this))` must still exclude a blank node focus node.
///
/// This is how a shape says "only named classes" — anonymous class and
/// property expressions (`owl:unionOf`, `owl:intersectionOf`, the object of
/// `owl:inverseOf`) are blank nodes, and real ontologies carry them in bulk.
/// Reporting on them is a false positive on valid input.
#[test]
fn is_iri_still_excludes_a_blank_node_focus() {
    let shapes = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:S a sh:NodeShape ;
    sh:targetSubjectsOf <http://www.w3.org/2000/01/rdf-schema#label> ;
    sh:sparql [
        sh:message "named only" ;
        sh:select """SELECT $this ?value WHERE {
            $this <http://example.org/ns#p> ?value .
            FILTER(isIRI($this))
        }"""
    ] .
"#;
    // Three blank and two named focus nodes; only the named two qualify.
    let mut data = blank_data(3);
    data.push_str(&named_data(2).replace("@prefix ex: <http://example.org/ns#> .\n", ""));
    assert_eq!(
        fixture(&data, shapes),
        2,
        "isIRI($this) let blank nodes through"
    );
}

/// The other half: a blank node focus node must report as a blank node.
#[test]
fn is_blank_recognises_a_blank_node_focus() {
    let shapes = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:S a sh:NodeShape ;
    sh:targetSubjectsOf <http://www.w3.org/2000/01/rdf-schema#label> ;
    sh:sparql [
        sh:message "anonymous only" ;
        sh:select """SELECT $this ?value WHERE {
            $this <http://example.org/ns#p> ?value .
            FILTER(isBlank($this))
        }"""
    ] .
"#;
    let mut data = blank_data(3);
    data.push_str(&named_data(2).replace("@prefix ex: <http://example.org/ns#> .\n", ""));
    assert_eq!(
        fixture(&data, shapes),
        3,
        "isBlank($this) did not see the blank nodes"
    );
}

/// SPARQL's term inspection must tell the truth about a pre-bound blank node.
///
/// This is the property the N² fix originally broke: carrying a blank node
/// into the query as an IRI made it a constant — which fixed the cross-join —
/// but `isIRI($this)` then answered true, and shapes that use exactly that to
/// exclude anonymous class expressions started reporting on them. A count is
/// not enough to catch it, so each predicate is pinned on both populations.
#[test]
fn term_inspection_tells_the_truth_about_a_blank_focus() {
    let shape = |filter: &str| {
        format!(
            r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:S a sh:NodeShape ;
    sh:targetSubjectsOf <http://www.w3.org/2000/01/rdf-schema#label> ;
    sh:sparql [
        sh:message "m" ;
        sh:select """SELECT $this ?value WHERE {{
            $this <http://example.org/ns#p> ?value .
            FILTER({filter})
        }}"""
    ] .
"#
        )
    };

    let mut data = blank_data(3);
    data.push_str(&named_data(2).replace("@prefix ex: <http://example.org/ns#> .\n", ""));

    // 3 blank, 2 named.
    for (filter, expected) in [
        ("isIRI($this)", 2),
        ("isBlank($this)", 3),
        ("isLiteral($this)", 0),
        ("!isBlank($this)", 2),
        ("sameTerm($this, $this)", 5),
        // Pre-binding means the variable *is* bound, whichever kind of term it
        // holds — this is why substitution cannot be textual.
        ("bound($this)", 5),
    ] {
        assert_eq!(
            fixture(&data, &shape(filter)),
            expected,
            "FILTER({filter}) gave the wrong population"
        );
    }
}
