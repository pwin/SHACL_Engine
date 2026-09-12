//! Validating across threads must change how long a report takes and nothing
//! about what it says.
//!
//! That claim has two halves and each needs its own test. The first is that a
//! run which *does* split produces the sequential report byte for byte — not
//! the same set of results, the same document — on data large enough that the
//! parallel path is actually taken. The second is that the shapes graphs which
//! would break that property are the ones that decline to split. A recursive
//! property shape is the sharp case: the sequential run's second level sees
//! every value already on the stack and stops, where a piece's second level
//! walks on into the next piece's nodes until the depth limit. If the gate ever
//! let that through, the parallel run would not merely differ — it would fail.

use oxrdfio::RdfFormat;
use shacl::model::{Graph, GraphBuilder, TermStore, Vocab, loader};
use shacl::shapes::Shapes;
use shacl::validate::{self, Options};

/// Comfortably past the threshold below which the engine stays sequential.
const INSTANCES: usize = 12_000;

const PREFIX: &str = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
"#;

/// A chain of people, every third one with a bad age, every fifth without a
/// name — enough variety that a mis-ordered or missing result would show.
fn data() -> String {
    let mut s = String::from(PREFIX);
    for i in 0..INSTANCES {
        s.push_str(&format!(
            "ex:p{i} a ex:Person ; ex:knows ex:p{} ",
            (i + 1) % INSTANCES
        ));
        if i % 5 != 0 {
            s.push_str(&format!("; ex:name \"person {i}\" "));
        }
        if i % 3 == 0 {
            s.push_str("; ex:age \"old\" ");
        } else {
            s.push_str(&format!("; ex:age {} ", i % 90));
        }
        s.push_str(".\n");
    }
    s
}

struct Loaded {
    store: TermStore,
    vocab: Vocab,
    data: Graph,
    shapes_graph: Graph,
    shapes: Shapes,
}

fn load(shapes_ttl: &str) -> Loaded {
    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let parse = |text: &str, scope: u32, store: &mut TermStore| -> Graph {
        let mut b = GraphBuilder::new();
        loader::parse_str(text, RdfFormat::Turtle, "http://t/", scope, store, &mut b)
            .expect("fixture should parse");
        b.build()
    };
    let data = parse(&data(), 0, &mut store);
    let shapes_graph = parse(&format!("{PREFIX}{shapes_ttl}"), 1, &mut store);
    let shapes = Shapes::compile(&shapes_graph, &store, &vocab).expect("shapes should compile");
    Loaded {
        store,
        vocab,
        data,
        shapes_graph,
        shapes,
    }
}

/// The serialised report under a given thread count.
fn report(l: &mut Loaded, threads: usize) -> String {
    let options = Options {
        threads,
        ..Options::default()
    };
    let r = validate::validate_in_with(
        &l.data,
        &l.shapes,
        &l.shapes_graph,
        &mut l.store,
        &l.vocab,
        options,
    )
    .expect("validation should run");
    r.serialize(
        RdfFormat::NTriples,
        &l.store,
        &l.vocab,
        &l.shapes_graph,
        &[],
    )
    .expect("report should serialise")
}

/// Asserts two reports equal, naming the first line that differs rather than
/// printing both documents — a report here is tens of megabytes.
fn assert_same_report(expected: &str, actual: &str, what: &str) {
    if expected == actual {
        return;
    }
    let (el, al): (Vec<_>, Vec<_>) = (expected.lines().collect(), actual.lines().collect());
    let at = el
        .iter()
        .zip(&al)
        .position(|(a, b)| a != b)
        .unwrap_or(el.len().min(al.len()));
    panic!(
        "{what}: reports differ ({} vs {} lines); first difference at line {at}:
  expected: {}
  actual:   {}",
        el.len(),
        al.len(),
        el.get(at).unwrap_or(&"<end>"),
        al.get(at).unwrap_or(&"<end>"),
    );
}

const CORE: &str = r#"
ex:PersonShape a sh:NodeShape ;
  sh:targetClass ex:Person ;
  sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:datatype xsd:string ] ;
  sh:property [ sh:path ex:age ; sh:datatype xsd:integer ; sh:maxInclusive 150 ] ;
  sh:property [ sh:path ex:knows ; sh:class ex:Person ; sh:node ex:Named ] ;
  sh:property [ sh:path ( ex:knows ex:name ) ; sh:minLength 3 ] .
ex:Named a sh:NodeShape ;
  sh:property [ sh:path ex:name ; sh:minCount 1 ] .
"#;

#[test]
fn a_split_run_produces_the_sequential_report_byte_for_byte() {
    let mut l = load(CORE);
    assert!(
        l.shapes.is_focus_separable(),
        "the fixture must be one that splits, or this tests nothing"
    );
    let sequential = report(&mut l, 1);
    assert!(
        sequential.matches("ValidationResult").count() > 1000,
        "the fixture must produce many results for ordering to matter"
    );
    for threads in [2, 3, 8] {
        let parallel = report(&mut l, threads);
        assert_same_report(&sequential, &parallel, &format!("{threads} threads"));
    }
}

#[test]
fn the_default_is_to_split() {
    // `Options::default()` asks for every core; the point of this test is
    // that a caller who never heard of the option still gets the same report
    // as one who forced it sequential.
    let mut l = load(CORE);
    let default = {
        let r = validate::validate_in(&l.data, &l.shapes, &l.shapes_graph, &mut l.store, &l.vocab)
            .expect("validation should run");
        r.serialize(
            RdfFormat::NTriples,
            &l.store,
            &l.vocab,
            &l.shapes_graph,
            &[],
        )
        .expect("report should serialise")
    };
    assert_same_report(&report(&mut l, 1), &default, "the default thread count");
}

// ------------------------------------------------ the graphs that must decline

/// A property shape that reaches itself. Splitting this would not just differ
/// from the sequential run, it would hit the depth limit — see the module doc.
const RECURSIVE: &str = r#"
ex:Q a sh:PropertyShape ;
  sh:path ex:knows ;
  sh:property ex:Q ;
  sh:property [ sh:path ex:age ; sh:datatype xsd:integer ] .
ex:PersonShape a sh:NodeShape ; sh:targetClass ex:Person ; sh:property ex:Q .
"#;

/// A cycle through `sh:node` rather than `sh:property`, two shapes long.
const MUTUAL: &str = r#"
ex:A a sh:NodeShape ; sh:targetClass ex:Person ;
  sh:property [ sh:path ex:knows ; sh:node ex:B ] .
ex:B a sh:NodeShape ;
  sh:property [ sh:path ex:knows ; sh:node ex:A ] .
"#;

/// Uniqueness is a property of the focus set as a whole.
const CROSS_NODE: &str = r#"
ex:PersonShape a sh:NodeShape ; sh:targetClass ex:Person ;
  sh:uniqueValuesFor ( ex:name ) .
"#;

/// A query can mint a term the data never held.
const SPARQL: &str = r#"
ex:PersonShape a sh:NodeShape ; sh:targetClass ex:Person ;
  sh:sparql [ sh:select "SELECT $this WHERE { $this <http://example.org/ns#age> ?a . FILTER(!isLiteral(?a)) }" ] .
"#;

#[test]
fn shapes_graphs_that_cannot_split_decline_to() {
    for (name, ttl) in [
        ("a self-recursive property shape", RECURSIVE),
        ("a mutual sh:node cycle", MUTUAL),
        ("sh:uniqueValuesFor", CROSS_NODE),
        ("sh:sparql", SPARQL),
    ] {
        let l = load(ttl);
        assert!(
            !l.shapes.is_focus_separable(),
            "{name} must not be treated as separable"
        );
    }
    let l = load(CORE);
    assert!(
        l.shapes.is_focus_separable(),
        "the plain Core graph must be"
    );
}

#[test]
fn a_recursive_graph_still_gives_one_answer_under_every_thread_count() {
    // The recursive graph runs sequentially whatever is asked for, and this
    // pins that it neither errors nor drifts — the failure mode the gate
    // exists to prevent, checked from the outside.
    let mut l = load(RECURSIVE);
    let one = report(&mut l, 1);
    let eight = report(&mut l, 8);
    assert_same_report(&one, &eight, "a recursive graph under 8 threads");
    assert!(one.matches("ValidationResult").count() > 1000);
}
