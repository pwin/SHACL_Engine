//! Optional RDFS entailment, materialised before validation.
//!
//! SHACL validates what is *in* the data graph. It resolves `rdfs:subClassOf`
//! when deciding class membership for `sh:class` and `sh:targetClass`, and
//! nothing else: a `sh:targetSubjectsOf ex:parent` will not see a subject that
//! only holds `ex:father`, however plainly `ex:father rdfs:subPropertyOf
//! ex:parent` says it should. Running this first closes that gap, by making
//! the entailed triples real ones.
//!
//! It is deliberately not automatic. Materialising changes what the report
//! says — a `sh:closed` shape starts seeing inferred predicates, and counts
//! move — so it has to be something the caller asked for and can be told
//! apart from what the document actually contained.
//!
//! The rules implemented are the ones that add triples a validator can act on:
//!
//! | Rule | From | Entails |
//! | --- | --- | --- |
//! | rdfs2 | `p rdfs:domain c` and `x p y` | `x rdf:type c` |
//! | rdfs3 | `p rdfs:range c` and `x p y` | `y rdf:type c` |
//! | rdfs5 | `p rdfs:subPropertyOf q`, `q … r` | `p rdfs:subPropertyOf r` |
//! | rdfs7 | `p rdfs:subPropertyOf q` and `x p y` | `x q y` |
//! | rdfs9 | `c rdfs:subClassOf d` and `x rdf:type c` | `x rdf:type d` |
//! | rdfs11 | `c rdfs:subClassOf d`, `d … e` | `c rdfs:subClassOf e` |
//!
//! The axiomatic and reflexive rules (rdfs4, rdfs6, rdfs10, and the RDF/RDFS
//! axioms) are left out on purpose. They entail things like `x rdf:type
//! rdfs:Resource` for every term, which no SHACL shape is improved by and
//! which would inflate a graph several times over for nothing.

use crate::error::{Error, Result};
use crate::model::{Graph, GraphBuilder, TermId, Vocab};

/// How large the closure may grow before it is abandoned, by default.
///
/// The closure of a schema is not linear in its size: `n` classes in a
/// `rdfs:subClassOf` chain entail `n²/2` transitive statements, so a modest
/// document can name a very large graph. Running out of memory partway is a
/// worse answer than refusing, and refusing is honest — the caller asked for
/// something whose cost they may not have known.
pub const DEFAULT_MAX_TRIPLES: usize = 50_000_000;

/// Returns `graph` with its RDFS consequences added.
///
/// Runs to a fixpoint, so chains of any length close: `ex:a rdfs:subClassOf
/// ex:b rdfs:subClassOf ex:c` yields the `ex:a rdfs:subClassOf ex:c` that
/// `rdfs9` then needs to type instances of `ex:a` as `ex:c`.
pub fn rdfs_closure(graph: &Graph, vocab: &Vocab) -> Result<Graph> {
    rdfs_closure_bounded(graph, vocab, DEFAULT_MAX_TRIPLES)
}

/// As [`rdfs_closure`], abandoning the work past `max_triples`.
pub fn rdfs_closure_bounded(graph: &Graph, vocab: &Vocab, max_triples: usize) -> Result<Graph> {
    let mut triples: Vec<[TermId; 3]> = graph.iter().collect();
    triples.sort_unstable();
    triples.dedup();

    // Each round derives from everything known so far and keeps whatever is
    // new. The rule set is small and the schema part of a graph is tiny next
    // to the instance part, so this converges in a handful of rounds.
    loop {
        let before = triples.len();
        let mut derived: Vec<[TermId; 3]> = Vec::new();

        // The schema statements, read once per round rather than per triple.
        let domains: Vec<(TermId, TermId)> = pairs(&triples, vocab.rdfs_domain);
        let ranges: Vec<(TermId, TermId)> = pairs(&triples, vocab.rdfs_range);
        let sub_props: Vec<(TermId, TermId)> = pairs(&triples, vocab.rdfs_subPropertyOf);
        let sub_classes: Vec<(TermId, TermId)> = pairs(&triples, vocab.rdfs_subClassOf);

        for &[s, p, o] in &triples {
            // rdfs2 / rdfs3: a property's domain and range type its ends.
            for &(prop, class) in &domains {
                if p == prop {
                    derived.push([s, vocab.rdf_type, class]);
                }
            }
            for &(prop, class) in &ranges {
                if p == prop {
                    derived.push([o, vocab.rdf_type, class]);
                }
            }
            // rdfs7: a statement also holds of every super-property.
            for &(sub, sup) in &sub_props {
                if p == sub {
                    derived.push([s, sup, o]);
                }
            }
            // rdfs9: an instance of a class is an instance of its superclasses.
            if p == vocab.rdf_type {
                for &(sub, sup) in &sub_classes {
                    if o == sub {
                        derived.push([s, vocab.rdf_type, sup]);
                    }
                }
            }
        }

        // rdfs5 / rdfs11: both hierarchies are transitive.
        for (pairs, predicate) in [
            (&sub_props, vocab.rdfs_subPropertyOf),
            (&sub_classes, vocab.rdfs_subClassOf),
        ] {
            for &(a, b) in pairs {
                for &(c, d) in pairs {
                    if b == c && a != d {
                        derived.push([a, predicate, d]);
                    }
                }
            }
        }

        triples.extend(derived);
        triples.sort_unstable();
        triples.dedup();

        // Checked after deduplicating, so the figure is triples actually held
        // rather than an over-count of what a round proposed.
        if triples.len() > max_triples {
            return Err(Error::Inference(format!(
                "RDFS closure exceeded {max_triples} triples; \
                 the schema entails more than this will materialise"
            )));
        }
        if triples.len() == before {
            break;
        }
    }

    let mut b = GraphBuilder::new();
    for [s, p, o] in triples {
        b.push(s, p, o);
    }
    Ok(b.build())
}

/// Every `(subject, object)` of `predicate`.
fn pairs(triples: &[[TermId; 3]], predicate: TermId) -> Vec<(TermId, TermId)> {
    triples
        .iter()
        .filter(|t| t[1] == predicate)
        .map(|t| (t[0], t[2]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TermStore, loader};
    use oxrdfio::RdfFormat;

    fn closure(turtle: &str) -> (TermStore, Vocab, Graph) {
        let mut store = TermStore::new();
        let vocab = Vocab::new(&mut store);
        let mut b = GraphBuilder::new();
        loader::parse_str(
            turtle,
            RdfFormat::Turtle,
            "http://t/",
            0,
            &mut store,
            &mut b,
        )
        .unwrap();
        let g = rdfs_closure(&b.build(), &vocab).expect("closure should fit the default bound");
        (store, vocab, g)
    }

    /// Asserts a triple is present, naming it readably when it is not.
    fn has(g: &Graph, store: &mut TermStore, s: &str, p: &str, o: &str) -> bool {
        let (s, p, o) = (
            store.named_node(s),
            store.named_node(p),
            store.named_node(o),
        );
        g.contains(s, p, o)
    }

    const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

    #[test]
    fn subclass_membership_reaches_every_ancestor() {
        let (mut store, _, g) = closure(
            "@prefix ex: <http://ex/> .
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
             ex:Employee rdfs:subClassOf ex:Person .
             ex:Person rdfs:subClassOf ex:Agent .
             ex:grace a ex:Employee .",
        );
        // rdfs9 through a transitive closure built by rdfs11.
        assert!(has(
            &g,
            &mut store,
            "http://ex/grace",
            RDF_TYPE,
            "http://ex/Person"
        ));
        assert!(has(
            &g,
            &mut store,
            "http://ex/grace",
            RDF_TYPE,
            "http://ex/Agent"
        ));
        assert!(has(
            &g,
            &mut store,
            "http://ex/Employee",
            "http://www.w3.org/2000/01/rdf-schema#subClassOf",
            "http://ex/Agent"
        ));
    }

    #[test]
    fn a_statement_holds_of_every_super_property() {
        let (mut store, _, g) = closure(
            "@prefix ex: <http://ex/> .
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
             ex:father rdfs:subPropertyOf ex:parent .
             ex:parent rdfs:subPropertyOf ex:ancestor .
             ex:a ex:father ex:b .",
        );
        assert!(has(
            &g,
            &mut store,
            "http://ex/a",
            "http://ex/parent",
            "http://ex/b"
        ));
        assert!(has(
            &g,
            &mut store,
            "http://ex/a",
            "http://ex/ancestor",
            "http://ex/b"
        ));
    }

    #[test]
    fn domain_and_range_type_the_ends_of_a_statement() {
        let (mut store, _, g) = closure(
            "@prefix ex: <http://ex/> .
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
             ex:worksFor rdfs:domain ex:Person ; rdfs:range ex:Company .
             ex:ada ex:worksFor ex:acme .",
        );
        assert!(has(
            &g,
            &mut store,
            "http://ex/ada",
            RDF_TYPE,
            "http://ex/Person"
        ));
        assert!(has(
            &g,
            &mut store,
            "http://ex/acme",
            RDF_TYPE,
            "http://ex/Company"
        ));
    }

    /// Inference derived through another inference must also land: rdfs7 makes
    /// the `ex:parent` statement, whose domain then types the subject.
    #[test]
    fn rules_feed_each_other_to_a_fixpoint() {
        let (mut store, _, g) = closure(
            "@prefix ex: <http://ex/> .
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
             ex:father rdfs:subPropertyOf ex:parent .
             ex:parent rdfs:domain ex:Person .
             ex:a ex:father ex:b .",
        );
        assert!(has(
            &g,
            &mut store,
            "http://ex/a",
            RDF_TYPE,
            "http://ex/Person"
        ));
    }

    /// A cycle must terminate rather than deriving forever.
    #[test]
    fn a_cyclic_hierarchy_terminates() {
        let (mut store, _, g) = closure(
            "@prefix ex: <http://ex/> .
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
             ex:A rdfs:subClassOf ex:B . ex:B rdfs:subClassOf ex:A .
             ex:x a ex:A .",
        );
        assert!(has(&g, &mut store, "http://ex/x", RDF_TYPE, "http://ex/B"));
    }

    /// Nothing is invented from a graph with no schema in it.
    #[test]
    fn a_graph_without_a_schema_is_unchanged() {
        let text = "@prefix ex: <http://ex/> . ex:a ex:p ex:b . ex:b ex:q 1 .";
        let (_, _, g) = closure(text);
        assert_eq!(g.len(), 2, "no axiomatic or reflexive triples added");
    }

    /// The closure is not linear in the input: `n` classes in a chain entail
    /// `n²/2` transitive statements. Past the bound it must say so rather than
    /// run the machine out of memory.
    #[test]
    fn an_expensive_closure_is_refused_rather_than_attempted() {
        let mut store = TermStore::new();
        let vocab = Vocab::new(&mut store);
        let chain = |n: usize, store: &mut TermStore| {
            let mut text = String::from(
                "@prefix ex: <http://ex/> .\n\
                 @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n",
            );
            for i in 0..n {
                text.push_str(&format!("ex:C{i} rdfs:subClassOf ex:C{}.\n", i + 1));
            }
            let mut b = GraphBuilder::new();
            loader::parse_str(&text, RdfFormat::Turtle, "http://t/", 0, store, &mut b).unwrap();
            b.build()
        };

        // 40 links entail ~800 statements, so a generous bound finishes and a
        // tight one does not. Kept short deliberately: transitive closure is
        // quadratic in the number of pairs, so a long chain would make this
        // test the slowest thing in the suite for no extra confidence.
        let small = chain(40, &mut store);
        assert!(rdfs_closure_bounded(&small, &vocab, 1_000_000).is_ok());
        match rdfs_closure_bounded(&small, &vocab, 100) {
            Err(crate::Error::Inference(m)) => assert!(m.contains("100"), "message: {m}"),
            other => panic!("expected an inference limit error, got {other:?}"),
        }

        // A chain long enough to be genuinely expensive is refused early
        // rather than worked through — which is the point of the bound.
        let large = chain(2_000, &mut store);
        assert!(matches!(
            rdfs_closure_bounded(&large, &vocab, 10_000),
            Err(crate::Error::Inference(_))
        ));
    }

    /// Inference and `sh:closed` interact, and the direction is surprising
    /// enough to pin: materialising adds predicates, and a closed shape then
    /// objects to them. This is the reason inference is opt-in, so it is worth
    /// a test rather than only a sentence in the docs.
    #[test]
    fn inference_can_make_a_closed_shape_fail() {
        let shapes_text = "@prefix ex: <http://ex/> .
             @prefix sh: <http://www.w3.org/ns/shacl#> .
             ex:S a sh:NodeShape ;
                 sh:targetNode ex:a ;
                 sh:closed true ;
                 sh:property [ sh:path ex:father ] .";
        let data_text = "@prefix ex: <http://ex/> .
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
             ex:father rdfs:subPropertyOf ex:parent .
             ex:a ex:father ex:b .";

        let mut store = TermStore::new();
        let vocab = Vocab::new(&mut store);
        let parse = |text: &str, scope: u32, store: &mut TermStore| {
            let mut b = GraphBuilder::new();
            loader::parse_str(text, RdfFormat::Turtle, "http://t/", scope, store, &mut b).unwrap();
            b.build()
        };
        let data = parse(data_text, 0, &mut store);
        let shapes_graph = parse(shapes_text, 1, &mut store);
        let compiled = crate::shapes::Shapes::compile(&shapes_graph, &store, &vocab).unwrap();

        let plain =
            crate::validate::validate_in(&data, &compiled, &shapes_graph, &mut store, &vocab)
                .unwrap();
        assert!(
            plain.conforms(&[vocab.sh_Violation]),
            "ex:a holds only ex:father, which the shape permits"
        );

        // rdfs7 adds `ex:a ex:parent ex:b`, which `sh:closed` has no place for.
        let inferred = rdfs_closure(&data, &vocab).unwrap();
        let after =
            crate::validate::validate_in(&inferred, &compiled, &shapes_graph, &mut store, &vocab)
                .unwrap();
        assert!(
            !after.conforms(&[vocab.sh_Violation]),
            "the inferred predicate is not among those the shape allows"
        );
        assert!(
            after
                .results
                .iter()
                .any(|r| r.source_constraint_component == vocab.sh_ClosedConstraintComponent),
            "and it is the closedness that objects"
        );
    }
}
