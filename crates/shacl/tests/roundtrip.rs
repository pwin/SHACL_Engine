//! Every term must survive a round trip through every boundary it crosses.
//!
//! Terms live as `TermId` inside the engine and as text everywhere else, so
//! each boundary has a renderer and a resolver that have to agree. They are
//! written in different modules, by different reasoning, and nothing but a
//! test holds them together.
//!
//! This is not hypothetical tidiness. Four shipped defects came from exactly
//! this drift, all involving blank nodes: a pre-bound blank node became a
//! SPARQL *variable* and cross-joined N focus nodes into N-squared results; the
//! stand-in that fixed it made `isIRI` answer true for a blank node; a blank
//! node arriving back from a solution resolved to nothing, so `sh:value` was
//! silently absent; and the JavaScript surface published the store's internal
//! `1:b1` where the report said `_:1_b1`.
//!
//! Each was found by a test aimed at something else. A round trip asserted
//! over every term kind is the check that would have caught them first.

use shacl::model::{TermId, TermStore, Vocab};
use shacl::sparql;

/// One of every kind of term the engine can hold.
fn every_kind(store: &mut TermStore) -> Vec<(&'static str, TermId)> {
    let xsd = "http://www.w3.org/2001/XMLSchema#";
    const RDF_LANG_STRING: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString";
    let s = store.named_node("http://example.org/s");
    let p = store.named_node("http://example.org/p");
    let o = store.named_node("http://example.org/o");
    vec![
        ("iri", store.named_node("http://example.org/thing")),
        // A percent-encoded IRI, because the renderer must not re-encode.
        (
            "iri with escapes",
            store.named_node("http://example.org/a%20b#c"),
        ),
        ("blank, scope 0", store.blank_node(0, "b0")),
        ("blank, scope 1", store.blank_node(1, "b0")),
        // Scope is part of identity: the same label in two documents is two
        // nodes, and the rendering has to keep them apart.
        ("blank, high scope", store.blank_node(1024, "b7")),
        (
            "blank, label with underscore",
            store.blank_node(0, "has_underscore"),
        ),
        (
            "plain literal",
            store.literal("hello", &format!("{xsd}string"), None),
        ),
        (
            "integer literal",
            store.literal("42", &format!("{xsd}integer"), None),
        ),
        // Literals whose lexical form looks like other syntax.
        (
            "literal with quotes",
            store.literal("say \"hi\"", &format!("{xsd}string"), None),
        ),
        (
            "literal with newline",
            store.literal("a\nb", &format!("{xsd}string"), None),
        ),
        (
            "literal that looks like a bnode",
            store.literal("_:b0", &format!("{xsd}string"), None),
        ),
        (
            "literal that looks like an IRI",
            store.literal("http://example.org/x", &format!("{xsd}string"), None),
        ),
        (
            "empty literal",
            store.literal("", &format!("{xsd}string"), None),
        ),
        (
            "language literal",
            store.literal("bonjour", RDF_LANG_STRING, Some("fr")),
        ),
        ("triple term", store.triple_term(s, p, o)),
    ]
}

/// The store's own boundary: `to_oxrdf` out, `get_term` back.
///
/// Blank nodes take `blank_node_from_output_label`, because `get_term` refuses
/// them by design — an externally-supplied label names nothing here. That
/// exception is the whole reason this needs testing: the inverse lives in a
/// different function from the renderer.
#[test]
fn every_term_survives_the_store_boundary() {
    let mut store = TermStore::new();
    let _ = Vocab::new(&mut store);
    for (what, id) in every_kind(&mut store) {
        let rendered = store.to_oxrdf(id);
        let back = store.resolve_rendered(rendered.as_ref());
        assert_eq!(back, Some(id), "{what} did not survive the store boundary");
    }
}

/// The SPARQL boundary: `to_term` in, `from_term` out.
///
/// This is where the pre-binding defects lived. A term handed to the evaluator
/// and handed back must be the same term, whatever its kind.
#[test]
fn every_term_survives_the_sparql_boundary() {
    let mut store = TermStore::new();
    let _ = Vocab::new(&mut store);
    for (what, id) in every_kind(&mut store) {
        let out = sparql::to_term(id, &store);
        let back = sparql::from_term(out.as_ref(), &store);
        assert_eq!(back, Some(id), "{what} did not survive the SPARQL boundary");
    }
}

/// Two terms that differ must not render alike.
///
/// A round trip alone would be satisfied by a renderer that maps everything to
/// one string and a resolver that guesses. Distinctness is the other half, and
/// it is the half that scope prefixes exist for.
#[test]
fn distinct_terms_render_distinctly() {
    let mut store = TermStore::new();
    let _ = Vocab::new(&mut store);
    let kinds = every_kind(&mut store);

    let mut seen: Vec<(String, &str)> = Vec::new();
    for (what, id) in &kinds {
        let text = store.to_oxrdf(*id).to_string();
        if let Some((_, other)) = seen.iter().find(|(t, _)| *t == text) {
            panic!("{what} and {other} both render as {text}");
        }
        seen.push((text, what));
    }
}
