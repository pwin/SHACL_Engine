//! A SHACL-SPARQL constraint that compares `xsd:date` values fires.
//!
//! Until 0.3.7 the engine built `spareval` without its `sep-0002` feature, which is what gives
//! xsd:date, xsd:time and the duration types values. A comparison between two dates was a
//! type error, the FILTER dropped the row, and the constraint reported nothing: an expired
//! certificate passed validation silently. `sh:lessThan` and the range components were never
//! affected, because the engine compares those itself rather than through the evaluator.

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

const DATA: &str = r#"
@prefix ex:  <http://example.org/ns#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:current a ex:Certificate ; ex:validUntil "2027-03-31"^^xsd:date .
ex:expired a ex:Certificate ; ex:validUntil "2026-06-30"^^xsd:date .
"#;

#[test]
fn an_expired_date_is_reported() {
    let shapes = r#"
        @prefix ex: <http://example.org/ns#> .
        @prefix sh: <http://www.w3.org/ns/shacl#> .
        ex:CertificateShape a sh:NodeShape ;
            sh:targetClass ex:Certificate ;
            sh:sparql [
                sh:message "expired on {?until}" ;
                sh:select """
                    SELECT $this ?until WHERE {
                        $this <http://example.org/ns#validUntil> ?until .
                        FILTER (?until < "2026-11-10"^^<http://www.w3.org/2001/XMLSchema#date>)
                    }"""
            ] .
    "#;
    assert_eq!(messages(DATA, shapes), ["expired on 2026-06-30"]);
}

#[test]
fn a_gap_between_dates_is_a_duration() {
    let shapes = r#"
        @prefix ex: <http://example.org/ns#> .
        @prefix sh: <http://www.w3.org/ns/shacl#> .
        ex:CertificateShape a sh:NodeShape ;
            sh:targetClass ex:Certificate ;
            sh:sparql [
                sh:message "valid for {?left} more" ;
                sh:select """
                    SELECT $this ?left WHERE {
                        $this <http://example.org/ns#validUntil> ?until .
                        BIND (?until - "2026-11-10"^^<http://www.w3.org/2001/XMLSchema#date> AS ?left)
                        FILTER (?left < "P365D"^^<http://www.w3.org/2001/XMLSchema#dayTimeDuration>
                                && ?left > "P0D"^^<http://www.w3.org/2001/XMLSchema#dayTimeDuration>)
                    }"""
            ] .
    "#;
    assert_eq!(messages(DATA, shapes), ["valid for P141D more"]);
}
