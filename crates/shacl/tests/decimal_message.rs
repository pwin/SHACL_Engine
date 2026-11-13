//! A SHACL-SPARQL message is filled from a decimal product that needs rounding.
//!
//! The report that prompted this: a message printed `{?ratio}` as written. The
//! constraint computed `?ratio` as the product of two decimals with more than 18
//! fractional digits between them, the registry `oxsdatatypes` refused any
//! product it could not hold exactly, and the error left `?ratio` unbound. The
//! constraint still fired, so the failure showed only as a placeholder in the
//! text. `vendor/oxsdatatypes` rounds the product to 18 digits, half to even, as
//! XPath F&O §4.2 allows, and the message carries it.

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
    report
        .results
        .iter()
        .flat_map(|r| r.messages.iter())
        .map(|&m| store.lexical_form(m).expect("a literal").to_owned())
        .collect()
}

const SHAPES: &str = r#"
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:OrderShape a sh:NodeShape ;
    sh:targetClass ex:Order ;
    sh:sparql [
        sh:message "ratio is {?ratio}" ;
        sh:select """
            SELECT $this ?ratio WHERE {
                $this <http://example.org/ns#quantity> ?q ; <http://example.org/ns#rate> ?r .
                BIND(?q * ?r AS ?ratio)
            }"""
    ] .
"#;

#[test]
fn the_message_carries_the_rounded_product() {
    // The exact product is 41.0583333333333333329.
    let data = r#"
        @prefix ex: <http://example.org/ns#> .
        ex:order a ex:Order ; ex:quantity 1.3 ; ex:rate 31.583333333333333333 .
    "#;
    assert_eq!(messages(data, SHAPES), ["ratio is 41.058333333333333333"]);
}

#[test]
fn a_quotient_with_a_large_dividend_is_rounded_too() {
    let shapes = SHAPES.replace("?q * ?r", "?q / ?r");
    let data = r#"
        @prefix ex: <http://example.org/ns#> .
        ex:order a ex:Order ; ex:quantity 1000 ; ex:rate 31.583333333333333333 .
    "#;
    assert_eq!(messages(data, &shapes), ["ratio is 31.662269129287598945"]);
}
