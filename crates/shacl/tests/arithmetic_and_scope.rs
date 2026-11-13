//! Arithmetic and filter scope that `spargebra` 0.4.6 got wrong, through a SPARQL constraint.
//!
//! `spargebra` 0.4.6 grouped a chain of `-` or `/` from the right, so `10 - 2 - 3` was
//! `10 - (2 - 3)`, 11, and a signed literal straight after an operand took the operator
//! before it, so `1 -2*3 -4` was -1. And it moved the filter of `OPTIONAL { { P FILTER(f) } }`
//! out into the `OPTIONAL`'s own condition, where it could see the variables bound outside the
//! nested group. All three are wrong answers rather than errors, so a constraint built on them
//! fires on the wrong nodes or says the wrong thing. `spargebra` 0.4.8, which `spareval` 0.2.9
//! brings with it, parses all three as written.

use oxrdfio::RdfFormat;
use shacl::model::{Graph, GraphBuilder, TermStore, Vocab, loader};
use shacl::validate::Options;

fn messages(data: &str, shapes: &str) -> Vec<String> {
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
    let mut out: Vec<String> = report
        .results
        .iter()
        .flat_map(|r| r.messages.iter())
        .map(|&m| store.lexical_form(m).expect("a literal").to_owned())
        .collect();
    out.sort();
    out
}

fn computed(expression: &str) -> String {
    format!(
        r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:ThingShape a sh:NodeShape ;
    sh:targetClass ex:Thing ;
    sh:sparql [
        sh:message "value is {{?v}}" ;
        sh:select """
            SELECT $this ?v WHERE {{
                $this <http://example.org/ns#n> ?n .
                BIND({expression} AS ?v)
            }}"""
    ] .
"#
    )
}

const ONE_THING: &str = r#"
    @prefix ex: <http://example.org/ns#> .
    ex:thing a ex:Thing ; ex:n 10 .
"#;

#[test]
fn a_chain_of_minus_groups_from_the_left() {
    // 0.4.6: 11.
    assert_eq!(messages(ONE_THING, &computed("?n - 2 - 3")), ["value is 5"]);
}

#[test]
fn a_signed_literal_after_an_operand_does_not_take_the_operator_before_it() {
    // 0.4.6: -1, as `1 - (2*3 - 4)`.
    assert_eq!(messages(ONE_THING, &computed("1 -2*3 -4")), ["value is -9"]);
}

#[test]
fn a_filter_in_a_nested_optional_group_does_not_see_the_outer_variables() {
    // Inside the nested group ?title is not in scope, so the filter is an error there and no
    // book gets a price: the constraint fires on all three. 0.4.6 gave TITLE 2 its price, so
    // it fired on two.
    let shapes = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:BookShape a sh:NodeShape ;
    sh:targetClass ex:Book ;
    sh:sparql [
        sh:message "no price for {?title}" ;
        sh:select """
            SELECT $this ?title WHERE {
                $this <http://example.org/ns#title> ?title .
                OPTIONAL { { $this <http://example.org/ns#price> ?price FILTER(?title = "TITLE 2") } }
                FILTER(!BOUND(?price))
            }"""
    ] .
"#;
    let data = r#"
        @prefix ex: <http://example.org/ns#> .
        ex:book1 a ex:Book ; ex:title "TITLE 1" ; ex:price 10 .
        ex:book2 a ex:Book ; ex:title "TITLE 2" ; ex:price 20 .
        ex:book3 a ex:Book ; ex:title "TITLE 3" .
    "#;
    assert_eq!(
        messages(data, shapes),
        [
            "no price for TITLE 1",
            "no price for TITLE 2",
            "no price for TITLE 3"
        ]
    );
}
