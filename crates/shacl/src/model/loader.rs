//! Reading RDF documents into an indexed [`Graph`].

use std::io::{BufReader, Read};

use flate2::read::GzDecoder;
use std::path::{Path, PathBuf};

pub use oxrdfio::RdfFormat;
use oxrdfio::RdfParser;
#[cfg(feature = "parallel")]
use rayon::prelude::*;

use super::graph::{Graph, GraphBuilder};
use super::term::TermStore;
use crate::error::{Error, Result};

/// Whether `path` names a gzip-compressed document.
///
/// Decided by the name alone. Sniffing the magic bytes would be more robust
/// but would mean opening the file to answer a question the caller asks about
/// a path, sometimes before deciding whether to open it at all.
pub fn is_gzipped(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("gz"))
}

/// Guesses an RDF syntax from a file extension.
///
/// `.gz` is a wrapper rather than a syntax, so it is stripped first and the
/// answer comes from what is underneath: `data.ttl.gz` is Turtle.
pub fn format_from_path(path: &Path) -> Option<RdfFormat> {
    let stem;
    let path = if is_gzipped(path) {
        stem = PathBuf::from(path.file_stem()?);
        &stem
    } else {
        path
    };
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "ttl" | "turtle" => RdfFormat::Turtle,
        "nt" => RdfFormat::NTriples,
        "nq" => RdfFormat::NQuads,
        "trig" => RdfFormat::TriG,
        "rdf" | "xml" | "owl" => RdfFormat::RdfXml,
        // JSON-LD was already an output format; not reading it back was an
        // asymmetry rather than a decision.
        "jsonld" | "json" => RdfFormat::JsonLd {
            profile: oxrdfio::JsonLdProfileSet::empty(),
        },
        "n3" => RdfFormat::N3,
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
    Ok(file_iri(&abs.to_string_lossy()))
}

/// Builds a `file:` IRI from an absolute path, as a pure function of the text.
///
/// Separated from [`path_to_base_iri`] so both platforms' path shapes can be
/// tested anywhere — `canonicalize` only ever produces the host's own.
fn file_iri(path: &str) -> String {
    // Windows canonicalisation yields a `\\?\C:\...` prefix; strip it and
    // normalise separators so the IRI is portable.
    let s = path
        .strip_prefix(r"\\?\")
        .unwrap_or(path)
        .replace('\\', "/");
    let encoded = encode_iri_path(&s);
    // A Unix path already starts with the separator that follows `file://`;
    // a Windows one starts at the drive letter and needs it added.
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

/// Percent-encodes the characters a path may contain but an IRI may not.
///
/// Without this, any path containing a space — `My Documents`, `Program
/// Files` — produced an IRI the parser rejected outright, so the file could
/// not be validated at all. `#` and `?` are worse than invalid: they are
/// legal, and would silently truncate the IRI into a fragment or query.
///
/// Non-ASCII is left alone. That is the point of an IRI as against a URI, and
/// it keeps accented path names readable.
fn encode_iri_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            // `%` first: it introduces an escape, so a literal one must become
            // `%25` or the result is ambiguous.
            '%' => out.push_str("%25"),
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '[' => out.push_str("%5B"),
            ']' => out.push_str("%5D"),
            '"' => out.push_str("%22"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '^' => out.push_str("%5E"),
            '`' => out.push_str("%60"),
            '{' => out.push_str("%7B"),
            '|' => out.push_str("%7C"),
            '}' => out.push_str("%7D"),
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {
                out.push_str(&format!("%{:02X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
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

/// Parses `reader` into `builder`, without requiring the document to sit in
/// memory as one buffer first.
///
/// `oxrdfio` reads just enough of `reader` to recognise the next token and
/// compacts what it has already consumed, so a document far larger than
/// memory can still be validated — the only limit is a single pathologically
/// long token, which the parser refuses past a fixed buffer size rather than
/// growing without bound.
///
/// This is what a genuinely unbounded source needs: standard input, or a file
/// too large to read into one `String`. A file whose format is Turtle still
/// prefers [`parse_turtle_parallel`] when the whole document is available up
/// front — splitting it across threads needs random access to find safe cut
/// points, which a stream cannot offer — so that path deliberately keeps
/// reading the document whole rather than switching to this one.
/// Tells a malformed document apart from a reader that failed mid-stream.
///
/// A streaming parse surfaces both through one iterator, and calling the
/// second one a parse error sends the reader off to inspect a document that is
/// perfectly well-formed. A download stopped at its size cap is the case that
/// matters: reported as a parse error it reads as corrupt RDF, and the actual
/// cause — a limit, with a flag to raise it — never reaches the person who
/// could act on it.
fn classify(base: &str, e: oxrdfio::RdfParseError) -> Error {
    match e {
        oxrdfio::RdfParseError::Io(e) => Error::Io(format!("{base}: {e}")),
        e => Error::Parse(format!("{base}: {e}")),
    }
}

pub fn parse_reader(
    reader: impl Read,
    format: RdfFormat,
    base: &str,
    scope: u32,
    store: &mut TermStore,
    builder: &mut GraphBuilder,
) -> Result<()> {
    let parser = RdfParser::from_format(format)
        .with_base_iri(base)
        .map_err(|e| Error::Parse(format!("invalid base IRI {base}: {e}")))?
        .with_default_graph(oxrdf::GraphName::DefaultGraph);

    for quad in parser.for_reader(reader) {
        let quad = quad.map_err(|e| classify(base, e))?;
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
///
/// Only the chunked (`parallel`) parse calls this, but the chunk-boundary tests
/// below exercise it directly in either build, hence `test` in the gate.
#[cfg(any(feature = "parallel", test))]
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
                let delim: &[u8] = if long {
                    &[quote, quote, quote]
                } else {
                    &[quote]
                };
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
                let ends = bytes.get(i + 1).is_none_or(|c| c.is_ascii_whitespace());
                if ends {
                    let stmt = text[stmt_start..i].trim_start();
                    let directive = stmt.starts_with('@')
                        || stmt
                            .get(..6)
                            .is_some_and(|s| s.eq_ignore_ascii_case("prefix"))
                        || stmt
                            .get(..4)
                            .is_some_and(|s| s.eq_ignore_ascii_case("base"));
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
///
/// Without the `parallel` feature there are no worker threads to split across,
/// so this becomes the sequential whole-document parse -- the same path already
/// taken whenever a document cannot be chunked safely. The name is kept in both
/// builds so callers need no `cfg` of their own.
pub fn parse_turtle_parallel(
    text: &str,
    base: &str,
    scope: u32,
    store: &mut TermStore,
    builder: &mut GraphBuilder,
) -> Result<()> {
    #[cfg(not(feature = "parallel"))]
    return parse_str(text, RdfFormat::Turtle, base, scope, store, builder);

    #[cfg(feature = "parallel")]
    parse_turtle_chunked(text, base, scope, store, builder)
}

/// The chunked parse itself, split out so a `parallel`-off build carries no
/// chunking/re-scoping code it could never reach.
#[cfg(feature = "parallel")]
fn parse_turtle_chunked(
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
/// Turtle takes the parallel path, which needs the whole document up front to
/// find safe chunk boundaries and so still reads it into one buffer. Every
/// other format was already a single sequential pass with nothing to
/// parallelise, so it streams instead: memory is bounded to one read buffer
/// rather than the whole document, and parsing starts before the read
/// finishes.
pub fn parse_file(
    path: &Path,
    scope: u32,
    store: &mut TermStore,
    builder: &mut GraphBuilder,
) -> Result<()> {
    let format = format_from_path(path)
        .ok_or_else(|| Error::Parse(format!("unknown RDF syntax for {}", path.display())))?;
    let base = path_to_base_iri(path)?;
    let gz = is_gzipped(path);
    let open =
        || std::fs::File::open(path).map_err(|e| Error::Io(format!("{}: {e}", path.display())));

    if format == RdfFormat::Turtle {
        // Turtle keeps the whole-document path so it can still be split across
        // threads, which needs random access to find safe chunk boundaries.
        // Decompressing first costs a pass but keeps that win.
        let text = if gz {
            let mut s = String::new();
            GzDecoder::new(BufReader::new(open()?))
                .read_to_string(&mut s)
                .map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
            s
        } else {
            std::fs::read_to_string(path)
                .map_err(|e| Error::Io(format!("{}: {e}", path.display())))?
        };
        return parse_turtle_parallel(&text, &base, scope, store, builder);
    }

    let file = BufReader::new(open()?);
    if gz {
        parse_reader(GzDecoder::new(file), format, &base, scope, store, builder)
    } else {
        parse_reader(file, format, &base, scope, store, builder)
    }
}

/// Reads a single file into a standalone graph.
pub fn load_file(path: &Path, scope: u32, store: &mut TermStore) -> Result<Graph> {
    let mut builder = GraphBuilder::new();
    parse_file(path, scope, store, &mut builder)?;
    Ok(builder.build())
}

/// Reads a streaming source into a standalone graph.
///
/// The [`RdfFormat`] must be given explicitly — there is no file extension to
/// guess it from, which is the whole difference between this and
/// [`load_file`]. Use it for standard input, or any document too large to
/// read into memory as one buffer.
pub fn load_reader(
    reader: impl Read,
    format: RdfFormat,
    base: &str,
    scope: u32,
    store: &mut TermStore,
) -> Result<Graph> {
    let mut builder = GraphBuilder::new();
    parse_reader(reader, format, base, scope, store, &mut builder)?;
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

        let doc = store
            .get_named_node("http://base/doc.ttl")
            .expect("<> resolved");
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
        parse_str(
            text,
            RdfFormat::Turtle,
            "http://b/",
            0,
            &mut seq_store,
            &mut seq,
        )
        .unwrap();
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
    fn builds_file_iris_for_both_platform_path_shapes() {
        // Both shapes are checked everywhere: `canonicalize` only ever yields
        // the host's own, so testing whichever this machine produces would
        // leave the other untested — which is how the mirror of this bug
        // reached CI in the test harness.
        assert_eq!(file_iri("/home/x/data.ttl"), "file:///home/x/data.ttl");
        assert_eq!(
            file_iri(r"\\?\C:\repos\data.ttl"),
            "file:///C:/repos/data.ttl"
        );
        assert_eq!(file_iri("C:/repos/data.ttl"), "file:///C:/repos/data.ttl");
    }

    #[test]
    fn percent_encodes_what_an_iri_cannot_hold() {
        // A space made the IRI invalid outright, so the file simply could not
        // be loaded.
        assert_eq!(
            file_iri("/home/my docs/data.ttl"),
            "file:///home/my%20docs/data.ttl"
        );
        // `#` and `?` are legal in an IRI, which is worse: they would truncate
        // the base into a fragment or query rather than fail loudly.
        assert_eq!(file_iri("/a/b#c/d.ttl"), "file:///a/b%23c/d.ttl");
        assert_eq!(file_iri("/a/b?c/d.ttl"), "file:///a/b%3Fc/d.ttl");
        assert_eq!(file_iri("/a/100%/d.ttl"), "file:///a/100%25/d.ttl");
        // Non-ASCII stays as it is; that is what distinguishes an IRI.
        assert_eq!(file_iri("/tmp/café/d.ttl"), "file:///tmp/café/d.ttl");
    }

    #[test]
    fn a_path_with_a_space_can_actually_be_loaded() {
        let dir = std::env::temp_dir().join("shacl test dir");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("data.ttl");
        std::fs::write(&file, "@prefix ex: <http://ex/> . <> ex:p ex:o .").unwrap();

        let mut store = TermStore::new();
        let graph = load_file(&file, 0, &mut store).expect("a space must not stop a load");
        assert_eq!(graph.len(), 1);

        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn detects_formats_by_extension() {
        assert_eq!(
            format_from_path(Path::new("a/b.ttl")),
            Some(RdfFormat::Turtle)
        );
        assert_eq!(
            format_from_path(Path::new("a.rdf")),
            Some(RdfFormat::RdfXml)
        );
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

    #[test]
    fn parses_from_a_reader_the_same_as_from_a_slice() {
        let mut store = TermStore::new();
        let v = Vocab::new(&mut store);
        let mut b = GraphBuilder::new();
        let text = "@prefix ex: <http://ex/> . <> a ex:Doc . ex:s ex:p 1 .";
        parse_reader(
            text.as_bytes(),
            RdfFormat::Turtle,
            "http://base/doc.ttl",
            0,
            &mut store,
            &mut b,
        )
        .unwrap();
        let g = b.build();

        let doc = store
            .get_named_node("http://base/doc.ttl")
            .expect("<> resolved");
        let ty = store.get_named_node("http://ex/Doc").unwrap();
        assert!(g.contains(doc, v.rdf_type, ty));
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn a_reader_that_yields_one_byte_at_a_time_still_parses() {
        // Proves the buffering is genuinely incremental, not an accident of
        // `&[u8]`'s `Read` impl handing the whole slice back in one call.
        struct OneByteAtATime<'a>(&'a [u8]);
        impl Read for OneByteAtATime<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() || buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }

        let text = "@prefix ex: <http://ex/> . ex:s ex:p \"v\" , \"w\" .";
        let mut store = TermStore::new();
        let mut b = GraphBuilder::new();
        parse_reader(
            OneByteAtATime(text.as_bytes()),
            RdfFormat::Turtle,
            "http://b/",
            0,
            &mut store,
            &mut b,
        )
        .unwrap();
        assert_eq!(b.build().len(), 2);
    }

    #[test]
    fn reader_syntax_errors_are_reported_rather_than_panicking() {
        let mut store = TermStore::new();
        let mut b = GraphBuilder::new();
        let err = parse_reader(
            "this is not turtle @@@".as_bytes(),
            RdfFormat::Turtle,
            "http://base/",
            0,
            &mut store,
            &mut b,
        );
        assert!(matches!(err, Err(Error::Parse(_))));
    }

    #[test]
    fn load_file_streams_non_turtle_formats() {
        let dir = std::env::temp_dir().join("shacl loader stream test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("data.nt");
        std::fs::write(&file, "<http://ex/s> <http://ex/p> <http://ex/o> .\n").unwrap();

        let mut store = TermStore::new();
        let graph = load_file(&file, 0, &mut store).expect("streams a non-Turtle file");
        assert_eq!(graph.len(), 1);

        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn load_reader_returns_a_standalone_graph() {
        let mut store = TermStore::new();
        let graph = load_reader(
            "<http://ex/s> <http://ex/p> <http://ex/o> .".as_bytes(),
            RdfFormat::NTriples,
            "http://base/",
            0,
            &mut store,
        )
        .unwrap();
        assert_eq!(graph.len(), 1);
    }

    /// `.gz` names a wrapper, not a syntax, so the answer comes from what is
    /// underneath it.
    #[test]
    fn gzip_is_seen_through_to_the_real_syntax() {
        let f = |name: &str| format_from_path(std::path::Path::new(name));
        assert!(matches!(f("g.ttl.gz"), Some(RdfFormat::Turtle)));
        assert!(matches!(f("g.nt.gz"), Some(RdfFormat::NTriples)));
        assert!(matches!(f("g.rdf.GZ"), Some(RdfFormat::RdfXml)));

        // A bare `.gz` names no syntax at all, and must not be guessed at.
        assert!(f("g.gz").is_none());
        assert!(f("g.txt.gz").is_none());

        let gz = |name: &str| is_gzipped(std::path::Path::new(name));
        assert!(gz("g.ttl.gz"));
        assert!(gz("g.ttl.GZ"), "case is not significant");
        assert!(!gz("g.ttl"));
        assert!(!gz("gz"));
    }

    /// Every format the writer can emit must also be readable, so a report
    /// this engine produced can be fed back to it.
    #[test]
    fn recognises_every_format_it_can_write() {
        let f = |name: &str| format_from_path(std::path::Path::new(name));
        assert!(matches!(f("g.ttl"), Some(RdfFormat::Turtle)));
        assert!(matches!(f("g.nt"), Some(RdfFormat::NTriples)));
        assert!(matches!(f("g.rdf"), Some(RdfFormat::RdfXml)));
        assert!(matches!(f("g.jsonld"), Some(RdfFormat::JsonLd { .. })));
        assert!(matches!(f("g.json"), Some(RdfFormat::JsonLd { .. })));
        assert!(matches!(f("g.n3"), Some(RdfFormat::N3)));

        // Case is not significant, and an unknown extension stays unknown
        // rather than being guessed at.
        assert!(matches!(f("G.TTL"), Some(RdfFormat::Turtle)));
        assert!(f("g.txt").is_none());
        assert!(f("g").is_none());
    }
}
