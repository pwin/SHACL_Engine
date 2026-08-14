//! SHACL-AF rules.
//!
//! There is no W3C test suite for these — `data-shapes-test-suite` carries
//! `core` and `sparql` only — so these tests are the specification read
//! closely, and `benchmarks/`-style agreement with pySHACL is checked
//! separately. Each one pins a sentence of the spec rather than just a happy
//! path, because the parts that are easy to get wrong (ordering, visibility,
//! the cross product) are exactly the parts nothing else would catch.

use oxrdfio::RdfFormat;
use shacl::model::{Graph, GraphBuilder, TermStore, Vocab, loader};

const PREFIX: &str = r#"
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
"#;

struct F {
    store: TermStore,
    vocab: Vocab,
    data: Graph,
    shapes_graph: Graph,
    shapes: shacl::shapes::Shapes,
}

fn fixture(data: &str, shapes: &str) -> F {
    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let graph = |text: &str, scope: u32, store: &mut TermStore| -> Graph {
        let mut b = GraphBuilder::new();
        loader::parse_str(
            &format!("{PREFIX}{text}"),
            RdfFormat::Turtle,
            "http://t/",
            scope,
            store,
            &mut b,
        )
        .expect("should parse");
        b.build()
    };
    let data = graph(data, 0, &mut store);
    let shapes_graph = graph(shapes, 1, &mut store);
    let shapes = shacl::shapes::Shapes::compile(&shapes_graph, &store, &vocab)
        .expect("shapes should compile");
    F {
        store,
        vocab,
        data,
        shapes_graph,
        shapes,
    }
}

impl F {
    fn apply(&mut self) -> Graph {
        shacl::rules::apply(
            &self.data,
            &self.shapes,
            &self.shapes_graph,
            &mut self.store,
            &self.vocab,
        )
        .expect("rules should run")
    }

    fn iterate(&mut self, rounds: usize) -> shacl::Result<Graph> {
        shacl::rules::apply_iterated(
            &self.data,
            &self.shapes,
            &self.shapes_graph,
            &mut self.store,
            &self.vocab,
            rounds,
        )
    }

    /// Whether `s p o` is in `g`, by IRI.
    fn has(&mut self, g: &Graph, s: &str, p: &str, o: &str) -> bool {
        let iri = |x: &str, store: &mut TermStore| store.named_node(x);
        let (s, p, o) = (
            iri(s, &mut self.store),
            iri(p, &mut self.store),
            iri(o, &mut self.store),
        );
        g.contains(s, p, o)
    }
}

/// The basic shape of a triple rule: three node expressions, evaluated per
/// focus node.
#[test]
fn a_triple_rule_infers_from_its_focus_node() {
    let mut f = fixture(
        "ex:alice a ex:Person .",
        r#"
ex:S a sh:NodeShape ; sh:targetClass ex:Person ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ;
        sh:predicate rdf:type ;
        sh:object ex:Agent ] .
"#,
    );
    let out = f.apply();
    assert!(
        f.has(
            &out,
            "http://example.org/alice",
            RDF_TYPE,
            "http://example.org/Agent"
        ),
        "the rule should have typed alice as an Agent"
    );
    // The data it started from is still there.
    assert!(f.has(
        &out,
        "http://example.org/alice",
        RDF_TYPE,
        "http://example.org/Person"
    ));
}

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// "For each combination of members s of S, p of P and o of O, infer a
/// triple." A path expression yielding several values must give several
/// triples, not one.
#[test]
fn the_inferred_triples_are_the_cross_product() {
    let mut f = fixture(
        "ex:a ex:knows ex:b, ex:c, ex:d .",
        r#"
ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:knows ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ;
        sh:predicate ex:contact ;
        sh:object [ sh:path ex:knows ] ] .
"#,
    );
    let out = f.apply();
    for who in ["b", "c", "d"] {
        assert!(
            f.has(
                &out,
                "http://example.org/a",
                "http://example.org/contact",
                &format!("http://example.org/{who}")
            ),
            "ex:contact ex:{who} should have been inferred"
        );
    }
}

/// `sh:condition`: the rule fires only on focus nodes conforming to *all* of
/// them.
#[test]
fn a_condition_gates_which_focus_nodes_fire() {
    let mut f = fixture(
        "ex:ok a ex:Person ; ex:age 40 . ex:no a ex:Person .",
        r#"
ex:HasAge a sh:NodeShape ; sh:property [ sh:path ex:age ; sh:minCount 1 ] .
ex:S a sh:NodeShape ; sh:targetClass ex:Person ;
    sh:rule [ a sh:TripleRule ;
        sh:condition ex:HasAge ;
        sh:subject sh:this ;
        sh:predicate rdf:type ;
        sh:object ex:Adult ] .
"#,
    );
    let out = f.apply();
    assert!(
        f.has(
            &out,
            "http://example.org/ok",
            RDF_TYPE,
            "http://example.org/Adult"
        ),
        "the node meeting the condition should be typed"
    );
    assert!(
        !f.has(
            &out,
            "http://example.org/no",
            RDF_TYPE,
            "http://example.org/Adult"
        ),
        "the node failing the condition must not be"
    );
}

/// `sh:deactivated true` means the rules engine ignores it.
#[test]
fn a_deactivated_rule_does_nothing() {
    let mut f = fixture(
        "ex:a a ex:Person .",
        r#"
ex:S a sh:NodeShape ; sh:targetClass ex:Person ;
    sh:rule [ a sh:TripleRule ; sh:deactivated true ;
        sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:Agent ] .
"#,
    );
    let before = f.data.len();
    let out = f.apply();
    assert_eq!(out.len(), before, "a deactivated rule inferred something");
}

/// A rule at a later `sh:order` sees what an earlier one inferred; two at the
/// same order do not see each other. This is the specification's visibility
/// rule, and the reason a rule set cannot depend on evaluation order within a
/// level.
#[test]
fn order_decides_what_a_rule_can_see() {
    let shapes = r#"
ex:First a sh:NodeShape ; sh:targetClass ex:Thing ; sh:order 1 ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:Step1 ] .

# Order 2: runs after, and targets what the first rule produced.
ex:Second a sh:NodeShape ; sh:targetClass ex:Step1 ; sh:order 2 ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:Step2 ] .

# Also order 1, targeting ex:Step1 — which is invisible at this level.
ex:SameLevel a sh:NodeShape ; sh:targetClass ex:Step1 ; sh:order 1 ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:TooSoon ] .
"#;
    let mut f = fixture("ex:a a ex:Thing .", shapes);
    let out = f.apply();

    assert!(f.has(
        &out,
        "http://example.org/a",
        RDF_TYPE,
        "http://example.org/Step1"
    ));
    assert!(
        f.has(
            &out,
            "http://example.org/a",
            RDF_TYPE,
            "http://example.org/Step2"
        ),
        "a later order must see what an earlier one inferred"
    );
    assert!(
        !f.has(
            &out,
            "http://example.org/a",
            RDF_TYPE,
            "http://example.org/TooSoon"
        ),
        "a rule must not see an inference made at its own order"
    );
}

/// `sh:SPARQLRule`, whose `sh:construct` builds the triples directly.
#[test]
fn a_sparql_rule_constructs_triples() {
    let mut f = fixture(
        "ex:a ex:width 3 ; ex:height 4 . ex:b ex:width 5 ; ex:height 6 .",
        r#"
ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:width ;
    sh:rule [ a sh:SPARQLRule ; sh:construct """
        CONSTRUCT { $this <http://example.org/area> ?a }
        WHERE {
          $this <http://example.org/width> ?w ; <http://example.org/height> ?h .
          BIND(?w * ?h AS ?a)
        }""" ] .
"#,
    );
    let out = f.apply();
    let (s, p) = (
        f.store.named_node("http://example.org/a"),
        f.store.named_node("http://example.org/area"),
    );
    let areas: Vec<_> = out.objects(s, p).collect();
    // Two nodes, so a query whose $this was never substituted would bind
    // ?this to both and give each node the other's area as well. One value
    // here is the assertion that pre-binding actually happened.
    assert_eq!(areas.len(), 1, "ex:a should have exactly one area");
    assert_eq!(f.store.lexical_form(areas[0]), Some("12"), "3 * 4 is 12");

    let b = f.store.named_node("http://example.org/b");
    let b_areas: Vec<_> = out.objects(b, p).collect();
    assert_eq!(b_areas.len(), 1);
    assert_eq!(f.store.lexical_form(b_areas[0]), Some("30"), "5 * 6 is 30");
}

/// A single pass is what the specification defines, so a transitive rule does
/// *not* close in one run. This pins the behaviour rather than wishing it
/// away — the alternative is a reader assuming closure and getting silence.
#[test]
fn one_pass_does_not_compute_a_transitive_closure() {
    let shapes = r#"
ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:sub ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ;
        sh:predicate ex:sub ;
        sh:object [ sh:path ( ex:sub ex:sub ) ] ] .
"#;
    let mut f = fixture(
        "ex:a ex:sub ex:b . ex:b ex:sub ex:c . ex:c ex:sub ex:d .",
        shapes,
    );

    let once = f.apply();
    assert!(
        f.has(
            &once,
            "http://example.org/a",
            "http://example.org/sub",
            "http://example.org/c"
        ),
        "one pass reaches two hops"
    );
    assert!(
        !f.has(
            &once,
            "http://example.org/a",
            "http://example.org/sub",
            "http://example.org/d"
        ),
        "and no further"
    );

    let closed = f.iterate(10).expect("should reach a fixpoint");
    assert!(
        f.has(
            &closed,
            "http://example.org/a",
            "http://example.org/sub",
            "http://example.org/d"
        ),
        "iterating to a fixpoint closes the chain"
    );
}

/// A rule set with no fixpoint has to stop with an error rather than run until
/// the machine gives out.
#[test]
fn a_rule_set_that_never_settles_is_an_error() {
    // Each round appends to the string, so every round infers something new.
    let shapes = r#"
ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:label ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ;
        sh:predicate ex:label ;
        sh:object [ sh:path ex:label ] ] .
ex:T a sh:NodeShape ; sh:targetSubjectsOf ex:label ;
    sh:rule [ a sh:SPARQLRule ; sh:construct """
        CONSTRUCT { $this <http://example.org/label> ?longer }
        WHERE { $this <http://example.org/label> ?l .
                BIND(CONCAT(STR(?l), "x") AS ?longer) }""" ] .
"#;
    let mut f = fixture(r#"ex:a ex:label "seed" ."#, shapes);
    let err = f.iterate(5).unwrap_err();
    assert!(
        format!("{err}").contains("fixpoint"),
        "expected a fixpoint error, got: {err}"
    );
}

/// Rules never touch the graph they were given. A report is only meaningful
/// against a known input, so the caller keeps theirs.
#[test]
fn the_input_graph_is_left_alone() {
    let mut f = fixture(
        "ex:a a ex:Person .",
        r#"
ex:S a sh:NodeShape ; sh:targetClass ex:Person ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:Agent ] .
"#,
    );
    let before = f.data.len();
    let out = f.apply();
    assert_eq!(f.data.len(), before, "the input graph was modified");
    assert!(out.len() > before, "the output should have grown");
}

/// A shapes graph with no rules is the overwhelming majority, and must cost
/// nothing and change nothing.
#[test]
fn a_shapes_graph_without_rules_infers_nothing() {
    let mut f = fixture(
        "ex:a a ex:Person .",
        "ex:S a sh:NodeShape ; sh:targetClass ex:Person ; sh:property [ sh:path ex:name ] .",
    );
    assert!(!f.shapes.has_rules());
    let before = f.data.len();
    assert_eq!(f.apply().len(), before);
}

/// End to end: rules run, then validation sees what they inferred.
///
/// The expected answer is pySHACL's, taken from running the same two files
/// through it with `-a`. An independent implementation agreeing is worth more
/// than my reading of the spec agreeing with itself, and there is no W3C test
/// suite for SHACL-AF to defer to — `data-shapes-test-suite` covers core and
/// SPARQL only.
#[test]
fn rules_then_validation_matches_pyshacl() {
    let shapes = r#"
ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:Agent ] ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ; sh:predicate ex:contact ; sh:object [ sh:path ex:knows ] ] .
ex:AgentShape a sh:NodeShape ;
    sh:targetClass ex:Agent ;
    sh:property [ sh:path ex:name ; sh:minCount 1 ;
                  sh:message "an Agent needs a name" ] .
"#;
    let data = r#"
ex:alice a ex:Person ; ex:name "Alice" ; ex:knows ex:bob, ex:carol .
ex:bob   a ex:Person .
"#;
    let mut f = fixture(data, shapes);
    let expanded = f.apply();

    // The rule fired for both people, so both are Agents.
    assert!(f.has(
        &expanded,
        "http://example.org/bob",
        RDF_TYPE,
        "http://example.org/Agent"
    ));
    assert!(f.has(
        &expanded,
        "http://example.org/alice",
        RDF_TYPE,
        "http://example.org/Agent"
    ));

    let report = shacl::validate::validate_in(
        &expanded,
        &f.shapes,
        &f.shapes_graph,
        &mut f.store,
        &f.vocab,
    )
    .expect("validation should succeed");

    // pySHACL 0.40.1: Conforms False, Results (1), Focus Node ex:bob.
    assert_eq!(report.results.len(), 1, "pySHACL reports exactly one");
    let bob = f.store.named_node("http://example.org/bob");
    assert_eq!(report.results[0].focus_node, bob);
}

// ------------------------------------------------------------ negation as failure
//
// SHACL-AF has no negation operator of its own, and needs none: a rule fires
// on absence through `sh:condition [ sh:not S ]`, or inside a SPARQL rule
// through `FILTER NOT EXISTS`. Both are tested here because both are the
// answer people reach for, and because negation is what makes a rule set
// non-monotonic — see `negation_makes_iteration_order_dependent`.

/// `sh:condition [ sh:not S ]`: fire only where the node fails a shape.
#[test]
fn negation_as_failure_via_a_condition() {
    let mut f = fixture(
        r#"ex:named a ex:Person ; ex:name "N" . ex:anon a ex:Person ."#,
        r#"
ex:HasName a sh:NodeShape ; sh:property [ sh:path ex:name ; sh:minCount 1 ] .
ex:S a sh:NodeShape ; sh:targetClass ex:Person ;
    sh:rule [ a sh:TripleRule ;
        sh:condition [ sh:not ex:HasName ] ;
        sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:Anonymous ] .
"#,
    );
    let out = f.apply();
    assert!(
        f.has(
            &out,
            "http://example.org/anon",
            RDF_TYPE,
            "http://example.org/Anonymous"
        ),
        "the node lacking a name should have been marked"
    );
    assert!(
        !f.has(
            &out,
            "http://example.org/named",
            RDF_TYPE,
            "http://example.org/Anonymous"
        ),
        "the node with a name must not be"
    );
}

/// The same, inside a SPARQL rule, where SPARQL's own `NOT EXISTS` does it.
#[test]
fn negation_as_failure_via_sparql_not_exists() {
    let mut f = fixture(
        r#"ex:named a ex:Person ; ex:name "N" . ex:anon a ex:Person ."#,
        r#"
ex:S a sh:NodeShape ; sh:targetClass ex:Person ;
    sh:rule [ a sh:SPARQLRule ; sh:construct """
        CONSTRUCT { $this a <http://example.org/Anonymous> }
        WHERE { FILTER NOT EXISTS { $this <http://example.org/name> ?n } }""" ] .
"#,
    );
    let out = f.apply();
    assert!(f.has(
        &out,
        "http://example.org/anon",
        RDF_TYPE,
        "http://example.org/Anonymous"
    ));
    assert!(!f.has(
        &out,
        "http://example.org/named",
        RDF_TYPE,
        "http://example.org/Anonymous"
    ));
}

/// Negation is not monotonic, and iterating a rule set that uses it is where
/// that stops being an abstraction.
///
/// Here one rule marks nodes with no `ex:name`, and a second gives them one.
/// A single pass — what the specification defines — is well defined: the mark
/// is made against the graph as it was. Iterating is not: round two sees the
/// name that round one added, so the conclusion drawn in round one is one the
/// final graph no longer supports. Rules only *add* triples, so the mark
/// stays; it is simply no longer true.
///
/// SHACL 1.2 Rules answers this with stratification. SHACL-AF does not, so
/// this test exists to pin the behaviour and warn in the one place a reader
/// would look.
#[test]
fn negation_makes_iteration_order_dependent() {
    let shapes = r#"
ex:HasName a sh:NodeShape ; sh:property [ sh:path ex:name ; sh:minCount 1 ] .
ex:Mark a sh:NodeShape ; sh:targetClass ex:Person ; sh:order 1 ;
    sh:rule [ a sh:TripleRule ;
        sh:condition [ sh:not ex:HasName ] ;
        sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:Unnamed ] .
ex:Fill a sh:NodeShape ; sh:targetClass ex:Person ; sh:order 2 ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ; sh:predicate ex:name ; sh:object "given" ] .
"#;
    let mut f = fixture("ex:a a ex:Person .", shapes);

    let once = f.apply();
    assert!(
        f.has(
            &once,
            "http://example.org/a",
            RDF_TYPE,
            "http://example.org/Unnamed"
        ),
        "one pass marks it: at that moment it had no name"
    );

    // Iterating reaches a fixpoint rather than oscillating — rules only add —
    // but the mark now sits on a node that does have a name.
    let closed = f.iterate(5).expect("adding triples always settles");
    assert!(f.has(
        &closed,
        "http://example.org/a",
        RDF_TYPE,
        "http://example.org/Unnamed"
    ));
    let (s, p) = (
        f.store.named_node("http://example.org/a"),
        f.store.named_node("http://example.org/name"),
    );
    assert_eq!(
        closed.objects(s, p).count(),
        1,
        "the conclusion drawn from absence outlives the absence"
    );
}

/// SHACL-AF function expressions are not supported, and say so rather than
/// inferring nothing. Pinned because "quietly produced no triples" is the
/// failure mode that makes a missing feature look like a working one.
#[test]
fn an_af_function_expression_is_refused() {
    let mut f = fixture(
        "ex:a a ex:Person .",
        r#"
ex:S a sh:NodeShape ; sh:targetClass ex:Person ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ;
        sh:predicate ex:label ;
        sh:object [ ex:someFunction ( sh:this ) ] ] .
"#,
    );
    let err = shacl::rules::apply(&f.data, &f.shapes, &f.shapes_graph, &mut f.store, &f.vocab)
        .unwrap_err();
    assert!(
        format!("{err}").contains("node expression"),
        "expected a node expression error, got: {err}"
    );
}

/// A rule fires on its shape's *target nodes*. A shape with no target has
/// none, so its rules never run — including a property shape nested inside
/// another shape, which is where people naturally reach for `sh:rule`.
#[test]
fn a_rule_on_an_untargeted_shape_never_fires() {
    let mut f = fixture(
        "ex:a a ex:Person ; ex:name \"A\" .",
        r#"
ex:S a sh:NodeShape ; sh:targetClass ex:Person ;
    sh:property [
        sh:path ex:name ;
        # A rule here has no targets of its own, so it never fires.
        sh:rule [ a sh:TripleRule ;
            sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:Named ] ] .
"#,
    );
    let out = f.apply();
    assert!(
        !f.has(
            &out,
            "http://example.org/a",
            RDF_TYPE,
            "http://example.org/Named"
        ),
        "a rule on a shape with no target must not fire"
    );
}
