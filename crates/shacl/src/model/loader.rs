//! Reading RDF documents into an indexed [`Graph`].

use std::path::Path;

pub use oxrdfio::RdfFormat;
use oxrdfio::RdfParser;
use rayon::prelude::*;

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

/// Splits Turtle into independently parseable chunks, or `None` if it cannot be
/// done safely.
///
/// Two conditions have to hold. Every directive must precede the first ordinary
/// statement, so that one prologue serves every chunk — Turtle allows `@prefix`
/// anywhere, and a redefinition partway through would change the meaning of
/// later chunks. And the document must contain no labelled blank node, because
/// a label is document-scoped: `_:a` in two chunks is one node, and parsing
/// them separately would split it. Anonymous blank nodes, from `[ ]` or from
/// collection syntax, are confined to the statement that introduces them and so
/// are safe.
fn turtle_chunks(text: &str, want: usize) -> Option<Vec<(usize, usize)>> {
    if want < 2 || text.len() < 1 << 20 {
        return None;
    }
    if text.contains("_:") {
        return None;
    }

    let bytes = text.as_bytes();
    let mut boundaries = Vec::new();
    let mut prologue_end = None;
    let mut depth = 0i32;
    let mut i = 0;
    // Tracks whether the statement being scanned began with a directive.
    let mut stmt_start = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'<' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'>' {
                    i += 1;
                }
                i += 1;
            }
            b'"' | b'\'' => {
                let quote = bytes[i];
                let long = bytes[i..].starts_with(&[quote; 3]);
                let delim: &[u8] = if long { &[quote, quote, quote] } else { &[quote] };
                i += delim.len();
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i..].starts_with(delim) {
                        i += delim.len();
                        break;
                    }
                    i += 1;
                }
            }
            b'[' | b'(' => {
                depth += 1;
                i += 1;
            }
            b']' | b')' => {
                depth -= 1;
                i += 1;
            }
            b'.' if depth == 0 => {
                // A statement terminator is followed by whitespace or EOF,
                // which is what separates it from the `.` in a decimal.
                let ends = bytes
                    .get(i + 1)
                    .is_none_or(|c| c.is_ascii_whitespace());
                if ends {
                    let stmt = text[stmt_start..i].trim_start();
                    let directive = stmt.starts_with('@')
                        || stmt.get(..6).is_some_and(|s| s.eq_ignore_ascii_case("prefix"))
                        || stmt.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("base"));
                    if directive {
                        // A directive after the prologue closed means one
                        // shared prologue is not enough.
                        if prologue_end.is_some() {
                            return None;
                        }
                    } else if prologue_end.is_none() {
                        prologue_end = Some(stmt_start);
                    }
                    boundaries.push(i + 1);
                    stmt_start = i + 1;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }

    let prologue_end = prologue_end?;
    let body = &boundaries[..];
    if body.len() < want * 2 {
        return None;
    }

    // Cut at statement boundaries nearest to even splits of the body.
    let start = prologue_end;
    let span = text.len() - start;
    let mut chunks = Vec::with_capacity(want);
    let mut prev = start;
    for k in 1..want {
        let target = start + span * k / want;
        let cut = match body.binary_search(&target) {
            Ok(x) => body[x],
            Err(x) if x < body.len() => body[x],
            Err(_) => continue,
        };
        if cut > prev {
            chunks.push((prev, cut));
            prev = cut;
        }
    }
    chunks.push((prev, text.len()));
    (chunks.len() > 1).then_some(chunks)
}

/// Parses Turtle across several threads, falling back to a single pass when the
/// document cannot be split safely.
///
/// Only the parse runs in parallel. Interning is left sequential: it accounts
/// for about a tenth of load against the parser's four fifths, and keeping one
/// shared term store avoids having to merge per-thread stores and renumber
/// every term afterwards.
pub fn parse_turtle_parallel(
    text: &str,
    base: &str,
    scope: u32,
    store: &mut TermStore,
    builder: &mut GraphBuilder,
) -> Result<()> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let Some(chunks) = turtle_chunks(text, threads) else {
        return parse_str(text, RdfFormat::Turtle, base, scope, store, builder);
    };
    let prologue = &text[..chunks[0].0];

    let parsed: Vec<Result<Vec<oxrdf::Triple>>> = chunks
        .par_iter()
        .map(|&(from, to)| {
            let source = format!("{prologue}{}", &text[from..to]);
            let parser = oxttl::TurtleParser::new()
                .with_base_iri(base)
                .map_err(|e| Error::Parse(format!("invalid base IRI {base}: {e}")))?;
            let mut out = Vec::new();
            for triple in parser.for_slice(source.as_bytes()) {
                out.push(triple.map_err(|e| Error::Parse(format!("{base}: {e}")))?);
            }
            Ok(out)
        })
        .collect();

    for (i, chunk) in parsed.into_iter().enumerate() {
        // Each chunk is parsed independently, so each restarts its generated
        // blank node labels from `_:b0`. Interning them all under one scope
        // would merge unrelated nodes, so the chunk index goes into the high
        // bits of the scope. Document scopes are small, so this cannot collide
        // with another document's.
        let chunk_scope = scope | ((i as u32 + 1) << 16);
        for t in chunk? {
            let s = store.intern_oxrdf(oxrdf::TermRef::from(t.subject.as_ref()), chunk_scope);
            let p = store.named_node(t.predicate.as_str());
            let o = store.intern_oxrdf(t.object.as_ref(), chunk_scope);
            builder.push(s, p, o);
        }
    }
    Ok(())
}

/// Reads an RDF file into `builder`.
///
/// Turtle takes the parallel path, which decides for itself whether the
/// document can be split and falls back to one pass when it cannot.
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
    if format == RdfFormat::Turtle {
        return parse_turtle_parallel(&text, &base, scope, store, builder);
    }
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

    /// Builds a document big enough to clear the chunker's size threshold.
    fn big_turtle(extra: &str) -> String {
        let mut s = String::from("@prefix ex: <http://ex/> .\n");
        s.push_str(extra);
        for i in 0..40_000 {
            s.push_str(&format!("ex:s{i} ex:p \"v{i}\" ; ex:q ex:o{i} .\n"));
        }
        s
    }

    /// Asserts both load paths produce the same graph.
    ///
    /// Blank node labels legitimately differ — each chunk restarts its
    /// generated labels, and the scope prefix keeps them apart — so the graphs
    /// are compared up to isomorphism rather than by term text.
    fn parse_both(text: &str) {
        let canonical = |g: Graph, s: &TermStore| {
            let mut og = oxrdf::Graph::new();
            for [a, b, c] in g.iter() {
                let subject = match s.to_oxrdf(a) {
                    oxrdf::Term::NamedNode(n) => oxrdf::NamedOrBlankNode::NamedNode(n),
                    oxrdf::Term::BlankNode(b) => oxrdf::NamedOrBlankNode::BlankNode(b),
                    other => panic!("unexpected subject {other}"),
                };
                let predicate = match s.to_oxrdf(b) {
                    oxrdf::Term::NamedNode(n) => n,
                    other => panic!("unexpected predicate {other}"),
                };
                og.insert(&oxrdf::Triple::new(subject, predicate, s.to_oxrdf(c)));
            }
            og.canonicalize(oxrdf::dataset::CanonicalizationAlgorithm::Unstable);
            let mut lines: Vec<String> = og.iter().map(|t| t.to_string()).collect();
            lines.sort();
            lines
        };

        let mut seq_store = TermStore::new();
        let mut seq = GraphBuilder::new();
        parse_str(text, RdfFormat::Turtle, "http://b/", 0, &mut seq_store, &mut seq).unwrap();
        let a = canonical(seq.build(), &seq_store);

        let mut par_store = TermStore::new();
        let mut par = GraphBuilder::new();
        parse_turtle_parallel(text, "http://b/", 0, &mut par_store, &mut par).unwrap();
        let b = canonical(par.build(), &par_store);

        assert_eq!(a.len(), b.len(), "triple count differs");
        assert_eq!(a, b, "parallel parse disagreed with sequential");
    }

    #[test]
    fn parallel_parse_agrees_with_sequential() {
        parse_both(&big_turtle(""));
    }

    #[test]
    fn parallel_parse_handles_anonymous_blank_nodes() {
        // `[ ]` and collections are confined to one statement, so chunking is
        // still safe even though blank nodes are involved.
        let mut s = String::from("@prefix ex: <http://ex/> .\n");
        for i in 0..40_000 {
            s.push_str(&format!(
                "ex:s{i} ex:p [ ex:inner \"v{i}\" ] ; ex:list ( \"a{i}\" \"b{i}\" ) .\n"
            ));
        }
        parse_both(&s);
    }

    #[test]
    fn chunking_declines_on_labelled_blank_nodes() {
        // A label is document-scoped, so `_:shared` in two chunks is one node
        // and must not be split.
        let text = big_turtle("ex:a ex:p _:shared .\nex:b ex:p _:shared .\n");
        assert!(turtle_chunks(&text, 8).is_none());
        parse_both(&text);
    }

    #[test]
    fn chunking_declines_when_a_directive_follows_the_prologue() {
        let mut text = big_turtle("");
        text.push_str("@prefix late: <http://late/> .\nlate:a late:b late:c .\n");
        assert!(turtle_chunks(&text, 8).is_none());
        parse_both(&text);
    }

    #[test]
    fn chunking_declines_on_small_documents() {
        assert!(turtle_chunks("@prefix ex: <http://ex/> . ex:a ex:b ex:c .", 8).is_none());
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
