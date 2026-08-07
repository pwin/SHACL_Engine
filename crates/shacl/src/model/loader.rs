//! Reading RDF documents into an indexed [`Graph`].

use std::path::Path;

use oxrdfio::{RdfFormat, RdfParser};

use super::graph::{Graph, GraphBuilder};
use super::term::TermStore;
use crate::error::{Error, Result};

/// Guesses an RDF syntax from a file extension.
pub fn format_from_path(path: &Path) -> Option<RdfFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "ttl" | "turtle" => RdfFormat::Turtle,
        "nt" => RdfFormat::NTriples,
        "nq" => RdfFormat::NQuads,
        "trig" => RdfFormat::TriG,
        "rdf" | "xml" | "owl" => RdfFormat::RdfXml,
        _ => return None,
    })
}

/// Converts a filesystem path into the `file:` IRI used as the document's base.
///
/// The test suite relies on this: a test file refers to itself as `<>`, so the
/// base IRI is what makes `sht:dataGraph <>` resolve to the right document.
pub fn path_to_base_iri(path: &Path) -> Result<String> {
    let abs = path
        .canonicalize()
        .map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    let s = abs.to_string_lossy();
    // Windows canonicalisation yields a `\\?\C:\...` prefix; strip it and
    // normalise separators so the IRI is portable.
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).replace('\\', "/");
    Ok(if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    })
}

/// Parses `text` into `builder`, interning terms into `store`.
///
/// `scope` isolates this document's blank node labels from every other
/// document's; see [`TermStore::blank_node`].
pub fn parse_str(
    text: &str,
    format: RdfFormat,
    base: &str,
    scope: u32,
    store: &mut TermStore,
    builder: &mut GraphBuilder,
) -> Result<()> {
    let parser = RdfParser::from_format(format)
        .with_base_iri(base)
        .map_err(|e| Error::Parse(format!("invalid base IRI {base}: {e}")))?
        // Test documents are Turtle with a `<>` self-reference; without this
        // the parser rejects relative IRIs outright.
        .with_default_graph(oxrdf::GraphName::DefaultGraph);

    for quad in parser.for_slice(text.as_bytes()) {
        let quad = quad.map_err(|e| Error::Parse(format!("{base}: {e}")))?;
        let s = store.intern_oxrdf(quad.subject.as_ref().into(), scope);
        let p = store.named_node(quad.predicate.as_str());
        let o = store.intern_oxrdf(quad.object.as_ref(), scope);
        builder.push(s, p, o);
    }
    Ok(())
}

/// Reads an RDF file into `builder`.
pub fn parse_file(
    path: &Path,
    scope: u32,
    store: &mut TermStore,
    builder: &mut GraphBuilder,
) -> Result<()> {
    let format = format_from_path(path)
        .ok_or_else(|| Error::Parse(format!("unknown RDF syntax for {}", path.display())))?;
    let base = path_to_base_iri(path)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    parse_str(&text, format, &base, scope, store, builder)
}

/// Reads a single file into a standalone graph.
pub fn load_file(path: &Path, scope: u32, store: &mut TermStore) -> Result<Graph> {
    let mut builder = GraphBuilder::new();
    parse_file(path, scope, store, &mut builder)?;
    Ok(builder.build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::vocab::Vocab;

    #[test]
    fn parses_turtle_and_resolves_the_empty_iri_against_the_base() {
        let mut store = TermStore::new();
        let v = Vocab::new(&mut store);
        let mut b = GraphBuilder::new();
        parse_str(
            "@prefix ex: <http://ex/> . <> a ex:Doc . ex:s ex:p 1 .",
            RdfFormat::Turtle,
            "http://base/doc.ttl",
            0,
            &mut store,
            &mut b,
        )
        .unwrap();
        let g = b.build();

        let doc = store.get_named_node("http://base/doc.ttl").expect("<> resolved");
        let ty = store.get_named_node("http://ex/Doc").unwrap();
        assert!(g.contains(doc, v.rdf_type, ty));
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn detects_formats_by_extension() {
        assert_eq!(
            format_from_path(Path::new("a/b.ttl")),
            Some(RdfFormat::Turtle)
        );
        assert_eq!(format_from_path(Path::new("a.rdf")), Some(RdfFormat::RdfXml));
        assert_eq!(format_from_path(Path::new("a.txt")), None);
        assert_eq!(format_from_path(Path::new("noext")), None);
    }

    #[test]
    fn reports_syntax_errors_rather_than_panicking() {
        let mut store = TermStore::new();
        let mut b = GraphBuilder::new();
        let err = parse_str(
            "this is not turtle @@@",
            RdfFormat::Turtle,
            "http://base/",
            0,
            &mut store,
            &mut b,
        );
        assert!(matches!(err, Err(Error::Parse(_))));
    }
}
