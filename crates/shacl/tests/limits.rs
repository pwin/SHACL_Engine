//! Stopping early must never change the answer.
//!
//! A cap exists to save work, so it is a real early exit rather than a trimmed
//! report. That makes it dangerous in one specific way: if the run stops on a
//! result that does not break conformance, a violation further along is never
//! reached, and the report says the graph conforms when it does not. Shapes
//! are evaluated in whatever order they compiled in, so which kind of result
//! turns up first says nothing about what is in the graph — this is the
//! ordinary case, not an edge one.

use oxrdfio::RdfFormat;
use shacl::model::{Graph, GraphBuilder, TermStore, Vocab, loader};
use shacl::validate::Options;

/// A warning-severity shape and a violation-severity one, over two nodes.
///
/// `ex:WarnShape` is declared first so it is reached first.
const SHAPES: &str = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:WarnShape a sh:NodeShape ; sh:targetNode ex:a ;
    sh:property [ sh:path ex:w ; sh:minCount 1 ; sh:severity sh:Warning ] .
ex:ViolShape a sh:NodeShape ; sh:targetNode ex:b ;
    sh:property [ sh:path ex:v ; sh:minCount 1 ; sh:severity sh:Violation ] .
"#;

const DATA: &str = r#"
@prefix ex: <http://example.org/ns#> .
ex:a ex:x 1 .
ex:b ex:y 2 .
"#;

struct F {
    store: TermStore,
    vocab: Vocab,
    data: Graph,
    shapes: Graph,
}

fn fixture(data: &str, shapes: &str) -> F {
    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let graph = |text: &str, scope: u32, store: &mut TermStore| {
        let mut b = GraphBuilder::new();
        loader::parse_str(text, RdfFormat::Turtle, "http://t/", scope, store, &mut b)
            .expect("test document should parse");
        b.build()
    };
    let data = graph(data, 0, &mut store);
    let shapes = graph(shapes, 1, &mut store);
    F {
        store,
        vocab,
        data,
        shapes,
    }
}

impl F {
    fn run(&mut self, options: Options) -> shacl::report::ValidationReport {
        let compiled = shacl::shapes::Shapes::compile(&self.shapes, &self.store, &self.vocab)
            .expect("shapes should compile");
        shacl::validate::validate_in_with(
            &self.data,
            &compiled,
            &self.shapes,
            &mut self.store,
            &self.vocab,
            options,
        )
        .expect("validation should succeed")
    }
}

/// Stopping at the first *result* would stop at the warning and never reach
/// the violation, reporting a graph that does not conform as one that does.
#[test]
fn a_cap_does_not_stop_on_a_result_that_does_not_block() {
    let mut f = fixture(DATA, SHAPES);
    let violation = vec![f.vocab.sh_Violation];

    let full = f.run(Options::default());
    assert!(!full.conforms(&violation), "the graph does not conform");

    let capped = f.run(Options::first_blocking(violation.clone()));
    assert!(
        !capped.conforms(&violation),
        "stopping early must not turn a non-conforming graph into a conforming one"
    );
}

/// The same, stated as the invariant rather than the example: a cap may change
/// how much is reported, never whether the graph conforms.
#[test]
fn conformance_is_the_same_capped_or_not() {
    for (data, shapes) in [
        (DATA, SHAPES),
        // Nothing wrong at all: the cap must never be reached, so the whole
        // graph is validated and the answer stays "conforms".
        (
            "@prefix ex: <http://example.org/ns#> . ex:a ex:w 1 . ex:b ex:v 2 .",
            SHAPES,
        ),
        // Only a warning: still conforming by default, and the cap must not
        // fire on it.
        (
            "@prefix ex: <http://example.org/ns#> . ex:b ex:v 2 .",
            SHAPES,
        ),
    ] {
        let mut f = fixture(data, shapes);
        let blocking = vec![f.vocab.sh_Violation];
        let full = f.run(Options::default()).conforms(&blocking);
        let capped = f
            .run(Options::first_blocking(blocking.clone()))
            .conforms(&blocking);
        assert_eq!(full, capped, "data: {data}");
    }
}

/// Widening what counts as blocking widens what the cap stops on.
#[test]
fn the_cap_follows_the_severities_it_is_given() {
    let mut f = fixture(DATA, SHAPES);
    let strict = vec![f.vocab.sh_Violation, f.vocab.sh_Warning];

    let capped = f.run(Options::first_blocking(strict.clone()));
    assert!(!capped.conforms(&strict));
    // The warning alone settles it now, so one result is enough.
    assert_eq!(capped.results.len(), 1);
}

/// A cap still does its job when everything blocks, which is the usual case:
/// `sh:Violation` is the default severity.
#[test]
fn a_cap_stops_early_when_every_result_blocks() {
    let shapes = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:S a sh:NodeShape ; sh:targetNode ex:a, ex:b, ex:c, ex:d ;
    sh:property [ sh:path ex:needed ; sh:minCount 1 ] .
"#;
    let data = "@prefix ex: <http://example.org/ns#> . ex:a ex:x 1 . ex:b ex:x 1 . \
                ex:c ex:x 1 . ex:d ex:x 1 .";
    let mut f = fixture(data, shapes);
    let blocking = vec![f.vocab.sh_Violation];

    assert_eq!(f.run(Options::default()).results.len(), 4);
    assert_eq!(
        f.run(Options::first_blocking(blocking.clone()))
            .results
            .len(),
        1
    );
    assert_eq!(
        f.run(Options {
            max_results: Some(2),
            blocking: Some(blocking),
        })
        .results
        .len(),
        2
    );
}

/// Trimming to the cap must not drop the result that breaks conformance.
///
/// The warning is reported before the violation, so a blind `truncate(1)`
/// would keep the warning and discard the violation — leaving a report whose
/// own `sh:conforms` contradicted the graph it came from.
#[test]
fn trimming_keeps_the_result_that_settles_conformance() {
    let mut f = fixture(DATA, SHAPES);
    let blocking = vec![f.vocab.sh_Violation];
    let report = f.run(Options::first_blocking(blocking.clone()));

    assert!(
        report
            .results
            .iter()
            .any(|r| blocking.contains(&r.severity)),
        "the blocking result survived the trim"
    );
    assert!(!report.conforms(&blocking));
}
