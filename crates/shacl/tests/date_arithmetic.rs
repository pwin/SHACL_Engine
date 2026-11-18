//! A SPARQL constraint adds a duration to a dateTime for every value, in either order.
//!
//! 0.3.7 turned on spareval's `sep-0002`, and a dateTime plus a duration was then unbound for
//! some values and not others: sparopt's optimizer orders the operands of `+` by hash, and
//! spareval took a duration only after the dateTime, so whenever the duration hashed lower the
//! sum was unbound and a constraint built on it never fired. XPath's operator mapping defines
//! `+` with the duration on either side, and the copy in `vendor/spareval` accepts both. These
//! tests compute each sum inside a constraint and compare it with `oxsdatatypes` directly.

use oxrdfio::RdfFormat;
use oxsdatatypes::{DateTime, Duration};
use shacl::model::{Graph, GraphBuilder, TermStore, Vocab, loader};
use shacl::validate::Options;
use std::str::FromStr;

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

const ONE_NODE: &str = "@prefix ex: <http://example.org/> . ex:e a ex:Thing .";

/// The sum a constraint computes, or `None` when it is unbound and the constraint is silent.
fn sum_in_a_constraint(expression: &str) -> Option<String> {
    let shapes = format!(
        r#"
@prefix ex: <http://example.org/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:S a sh:NodeShape ;
    sh:targetClass ex:Thing ;
    sh:sparql [
        sh:message "{{?s}}" ;
        sh:select """
            SELECT $this ?s WHERE {{
                BIND({expression} AS ?s)
                FILTER(BOUND(?s))
            }}"""
    ] .
"#
    );
    messages(ONE_NODE, &shapes).into_iter().next()
}

const DATE_TIMES: [&str; 6] = [
    "2025-03-08T00:00:00Z",
    "2024-01-01T12:00:00Z",
    "2026-11-18T09:30:00",
    "1999-12-31T23:59:59Z",
    "2025-07-15T00:00:00",
    "2030-02-28T10:00:00Z",
];

const DURATIONS: [(&str, &str); 5] = [
    ("P1D", "dayTimeDuration"),
    ("PT1H", "dayTimeDuration"),
    ("P1M", "yearMonthDuration"),
    ("P2Y", "yearMonthDuration"),
    ("P1DT2H", "duration"),
];

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

#[test]
fn a_duration_adds_to_a_date_time_in_either_order() {
    let mut unbound = Vec::new();
    for dt in DATE_TIMES {
        for (dur, ty) in DURATIONS {
            let expected = DateTime::from_str(dt)
                .expect("dateTime")
                .checked_add_duration(Duration::from_str(dur).expect("duration"))
                .expect("in range")
                .to_string();
            let lit_dt = format!("\"{dt}\"^^<{XSD}dateTime>");
            let lit_dur = format!("\"{dur}\"^^<{XSD}{ty}>");
            for expression in [
                format!("{lit_dt} + {lit_dur}"),
                format!("{lit_dur} + {lit_dt}"),
            ] {
                match sum_in_a_constraint(&expression) {
                    Some(got) => assert_eq!(got, expected, "{expression}"),
                    None => unbound.push(expression),
                }
            }
        }
    }
    assert!(unbound.is_empty(), "unbound: {unbound:#?}");
}

#[test]
fn a_renewal_constraint_fires() {
    // Renewal falls due two years after issue: P2Y was among the durations whose sums were
    // lost, so before 0.3.8 this certificate could pass however late it was.
    let shapes = format!(
        r#"
@prefix ex: <http://example.org/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:RenewalShape a sh:NodeShape ;
    sh:targetClass ex:Certificate ;
    sh:sparql [
        sh:message "renewal was due on {{?due}}" ;
        sh:select """
            SELECT $this ?due WHERE {{
                $this <http://example.org/issued> ?issued .
                BIND(?issued + "P2Y"^^<{XSD}yearMonthDuration> AS ?due)
                FILTER(?due < "2026-11-18T00:00:00Z"^^<{XSD}dateTime>)
            }}"""
    ] .
"#
    );
    let data = r#"
        @prefix ex: <http://example.org/> .
        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
        ex:old a ex:Certificate ; ex:issued "2024-01-01T12:00:00Z"^^xsd:dateTime .
        ex:new a ex:Certificate ; ex:issued "2025-06-30T09:00:00Z"^^xsd:dateTime .
    "#;
    assert_eq!(
        messages(data, &shapes),
        ["renewal was due on 2026-01-01T12:00:00Z"]
    );
}
