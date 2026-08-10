//! `shacl` — validate an RDF data graph against a SHACL shapes graph.

use std::ffi::OsString;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

// Parsing allocates a string per term — millions on a large graph — and the
// system allocator becomes the contention point once that runs across threads.
// Worth about 25% on load, on both the parallel and sequential paths.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::Parser;
use shacl::model::{Graph, TermStore, Vocab, loader, scope};

#[derive(Parser)]
#[command(name = "shacl", version, about = "A high-performance SHACL validator")]
struct Args {
    /// The data graph to validate. A path, an `http(s)` URL, or `-` for
    /// standard input.
    ///
    /// Repeat it to merge several documents into one graph. Blank nodes stay
    /// separate per document, as they must: `_:a` in two files is two nodes.
    #[arg(short, long, required = true, num_args = 1..)]
    data: Vec<PathBuf>,

    /// The shapes graph, on the same terms as `--data` and likewise
    /// repeatable. Defaults to the data graph, matching the convention that a
    /// self-describing document carries its own shapes.
    ///
    /// Only one input in total may be `-`, since standard input can be read
    /// only once.
    #[arg(short, long, num_args = 1..)]
    shapes: Vec<PathBuf>,

    /// The data graph's RDF syntax. Guessed from the file extension, or over
    /// HTTP from the `Content-Type`, when omitted; required when `--data` is
    /// `-`, which has neither.
    #[arg(long, visible_alias = "df", value_enum)]
    data_format: Option<InputFormat>,

    /// The shapes graph's RDF syntax, on the same terms as `--data-format`.
    #[arg(long, visible_alias = "sf", value_enum)]
    shapes_format: Option<InputFormat>,

    /// Write the report to a file rather than standard output.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Stop at the first result rather than validating the whole graph.
    ///
    /// A real early exit, so the report is no longer a complete account of
    /// the graph — it answers "is anything wrong" rather than "what is".
    #[arg(long)]
    abort: bool,

    /// Stop once this many results exist. `--abort` is `--max-results 1`.
    #[arg(long, value_name = "N")]
    max_results: Option<usize>,

    /// Materialise entailed triples into the data graph before validating.
    ///
    /// Off by default: it changes what the report says, since a `sh:closed`
    /// shape starts seeing inferred predicates.
    #[arg(short, long, value_enum, default_value_t = Inference::None)]
    inference: Inference,

    /// Validate the shapes graph itself against SHACL's own shapes first, and
    /// refuse to go on if it is malformed.
    #[arg(short = 'm', long, visible_alias = "metashacl")]
    meta_shacl: bool,

    /// Let `sh:Warning` results stand without breaking conformance. This is
    /// already the default; accepted for pySHACL compatibility.
    #[arg(short = 'w', long, visible_alias = "allow-warning")]
    allow_warnings: bool,

    /// Let `sh:Info` results stand. Also already the default.
    #[arg(long, visible_alias = "allow-info")]
    allow_infos: bool,

    /// How to render the report. `human` is a summary for reading; the rest
    /// emit the RDF validation report the SHACL specification defines.
    #[arg(short, long, value_enum, default_value_t = Format::Human)]
    format: Format,

    /// The least severity that breaks conformance. Results below it are still
    /// reported, but leave the graph conforming and the exit status 0.
    ///
    /// Only `sh:Violation` does so by default, which is already what pySHACL's
    /// `--allow-warnings` gives you; this is the knob in the other direction,
    /// for treating warnings — or everything — as failures.
    #[arg(long, value_enum, default_value_t = Severity::Violation)]
    min_severity: Severity,

    /// Print only whether the data conforms, not the individual results.
    #[arg(short, long)]
    quiet: bool,

    /// Largest document to accept from a URL, in bytes. A server can stream
    /// without end, which no timeout catches.
    #[arg(long, value_name = "BYTES", default_value_t = FETCH_LIMIT)]
    max_download: u64,

    /// Report how long loading, compiling and validating each took.
    #[arg(long)]
    timing: bool,

    /// Validate `n` times, reporting the best wall time. For benchmarking.
    #[arg(long, default_value_t = 1)]
    repeat: u32,
}

/// Which entailed triples to materialise before validating.
#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
enum Inference {
    None,
    Rdfs,
}

/// A severity threshold, ordered as SHACL orders them: Info, Warning,
/// Violation.
#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
enum Severity {
    /// Any result at all breaks conformance.
    Info,
    /// Warnings and violations break conformance.
    Warning,
    /// Only violations break conformance.
    Violation,
}

impl Severity {
    /// The severities at or above this threshold, which is what
    /// `ValidationReport::conforms` wants.
    fn disallowed(self, v: &Vocab) -> Vec<shacl::TermId> {
        match self {
            Self::Violation => vec![v.sh_Violation],
            Self::Warning => vec![v.sh_Violation, v.sh_Warning],
            Self::Info => vec![v.sh_Violation, v.sh_Warning, v.sh_Info],
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
enum Format {
    /// One line per result, for reading rather than parsing.
    Human,
    // Aliases because clap derives `n-triples` from the variant name, and
    // nobody types that.
    #[value(alias = "ttl")]
    Turtle,
    #[value(alias = "ntriples", alias = "nt")]
    NTriples,
    #[value(alias = "rdfxml", alias = "xml")]
    RdfXml,
    #[value(alias = "jsonld")]
    JsonLd,
}

impl Format {
    /// The RDF syntax to serialise as, or `None` for the human summary.
    fn rdf(self) -> Option<shacl::model::loader::RdfFormat> {
        use shacl::model::loader::RdfFormat as F;
        Some(match self {
            Self::Human => return None,
            Self::Turtle => F::Turtle,
            Self::NTriples => F::NTriples,
            Self::RdfXml => F::RdfXml,
            Self::JsonLd => F::JsonLd {
                profile: Default::default(),
            },
        })
    }
}

/// Forces an RDF syntax rather than guessing it from a file extension.
///
/// Needed for `-`, which has none, and useful for a file whose extension
/// doesn't match its content.
#[derive(Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
enum InputFormat {
    #[value(alias = "ttl")]
    Turtle,
    #[value(alias = "ntriples", alias = "nt")]
    NTriples,
    #[value(alias = "nquads", alias = "nq")]
    NQuads,
    TriG,
    #[value(alias = "rdfxml", alias = "xml")]
    RdfXml,
    #[value(alias = "jsonld")]
    JsonLd,
    N3,
}

impl InputFormat {
    fn rdf(self) -> shacl::model::loader::RdfFormat {
        use shacl::model::loader::RdfFormat as F;
        match self {
            Self::Turtle => F::Turtle,
            Self::NTriples => F::NTriples,
            Self::NQuads => F::NQuads,
            Self::TriG => F::TriG,
            Self::RdfXml => F::RdfXml,
            Self::JsonLd => F::JsonLd {
                profile: Default::default(),
            },
            Self::N3 => F::N3,
        }
    }
}

/// The W3C "SHACL for SHACL" shapes, embedded so `--meta-shacl` works from an
/// installed binary and not only from inside this repository.
const SHACL_SHACL: &str = include_str!("../shacl-shacl.ttl");

/// Validates the shapes graph against SHACL's own shapes.
///
/// Returns the results rather than deciding what to do with them: a malformed
/// shapes graph is worth stopping for, but that judgement belongs to the
/// caller, not here.
///
/// SHACL-SHACL covers a *subset* of the syntax rules — the specification says
/// so itself — so a clean pass means nothing detectably malformed rather than
/// certainly correct.
fn check_shapes_graph(
    shapes_graph: &Graph,
    store: &mut TermStore,
    vocab: &Vocab,
) -> Result<shacl::report::ValidationReport> {
    let meta = loader::load_reader(
        SHACL_SHACL.as_bytes(),
        loader::RdfFormat::Turtle,
        "http://www.w3.org/ns/shacl-shacl#",
        scope::FIRST_DYNAMIC,
        store,
    )
    .context("parsing the embedded SHACL-SHACL shapes")?;

    let compiled = shacl::shapes::Shapes::compile(&meta, store, vocab)
        .context("compiling the embedded SHACL-SHACL shapes")?;

    // The shapes graph under test is the *data* here, which is the whole idea.
    Ok(shacl::validate::validate_in(
        shapes_graph,
        &compiled,
        &meta,
        store,
        vocab,
    )?)
}

/// `path` displayed the way an error message should read it: `-` is standard
/// input, not a file named `-`.
fn describe(path: &Path) -> String {
    if path.as_os_str() == "-" {
        "standard input".to_string()
    } else {
        path.display().to_string()
    }
}

/// The URL `path` names, if it names one rather than a file.
///
/// Only `http` and `https`. A `file:` URL would be a second, subtly different
/// way of saying what a path already says, and the remaining schemes are ways
/// of reaching things a validator has no business reaching.
fn url_of(path: &Path) -> Option<&str> {
    let s = path.to_str()?;
    (s.starts_with("http://") || s.starts_with("https://")).then_some(s)
}

/// How long to wait for a document, end to end.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of a fetched document to read before giving up.
///
/// `ureq`'s reader is unbounded by default — its own documentation says a
/// malicious server could exhaust the client's memory — and a timeout does not
/// help, since a server can stream fast and forever. This engine holds a graph
/// roughly three times over in its indexes, so a gigabyte of RDF is already
/// past what the machine will take; refusing it is kinder than being killed by
/// the OOM killer halfway through.
const FETCH_LIMIT: u64 = 1 << 30;

/// Fetches an RDF document over HTTP.
///
/// The syntax is settled in the order the caller's intent runs: an explicit
/// `--data-format` wins, then the server's `Content-Type`, then the extension
/// on the URL path. A server that says `text/turtle` is more likely to be
/// right than a URL ending in `.php`, but a caller who names the format
/// outright has overruled both.
///
/// Only the URL given on the command line is ever fetched. Nothing in a
/// fetched document causes a further request — `owl:imports` is not followed
/// and there is no other mechanism — so a hostile shapes graph cannot use this
/// to reach anywhere the caller did not name.
fn fetch_source(
    url: &str,
    forced: Option<InputFormat>,
    scope: u32,
    store: &mut TermStore,
    limit: u64,
) -> Result<Graph> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .user_agent(concat!("shacl/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();

    let mut response = agent
        .get(url)
        .header("accept", ACCEPT)
        .call()
        .with_context(|| format!("fetching {url}"))?;

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let format = match forced {
        Some(f) => f.rdf(),
        None => content_type
            .as_deref()
            .and_then(loader::RdfFormat::from_media_type)
            .or_else(|| format_from_url(url))
            .with_context(|| {
                let seen = content_type.as_deref().unwrap_or("none");
                format!(
                    "cannot tell the RDF syntax of {url} \
                     (Content-Type: {seen}); name it with --data-format \
                     or --shapes-format"
                )
            })?,
    };

    // A server that declares an oversized body is refused before any of it is
    // transferred. Nothing depends on this — the cap below catches a body that
    // lies or declares nothing at all — but there is no reason to stream a
    // gigabyte only to reject it at the end.
    if let Some(declared) = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        && declared > limit
    {
        bail!("{}", too_large(url, limit, Some(declared)));
    }

    // The document's own address is its base IRI, so a `<>` self-reference
    // and any relative IRI resolve against where it actually came from.
    // `ureq` undoes `Content-Encoding: gzip` itself. A URL ending `.gz` is a
    // different thing — the *document* is compressed, not just the transfer —
    // and has to be decoded here. The two compose: a `.ttl.gz` served with
    // transport compression is decoded once by ureq and once again below.
    //
    // ureq's own `limit` is left off deliberately. It enforces the same bound,
    // but reports it in its own words — naming neither the flag that sets it
    // nor the size that broke it — and it applies to the compressed stream,
    // so the two paths below would answer the same question differently
    // depending on whether the URL happened to end in `.gz`.
    let tripped = Tripped::default();
    let body = response.body_mut().with_config().limit(u64::MAX).reader();
    let body = Limited {
        inner: body,
        left: limit,
        tripped: tripped.clone(),
    };

    let loaded = if url_file_name(url).to_ascii_lowercase().ends_with(".gz") {
        // The cap is applied again to the decompressed stream, where it means
        // what it says: `limit` bytes of compressed data is no bound at all on
        // what comes out of it. Keeping it on the compressed side too bounds
        // the pathological stream that expands to almost nothing, which would
        // otherwise be read for as long as the server cared to send it.
        loader::load_reader(
            Limited {
                inner: flate2::read::GzDecoder::new(body),
                left: limit,
                tripped: tripped.clone(),
            },
            format,
            url,
            scope,
            store,
        )
    } else {
        loader::load_reader(body, format, url, scope, store)
    };

    // The reader's error travels back through the parser, which knows nothing
    // about download limits and can only describe what it saw. The flag says
    // what actually happened, so the size cap explains itself rather than
    // arriving as a truncated or unreadable document.
    match loaded {
        // Replacing the error rather than wrapping it: what it wraps is the
        // parser's account of a stream that stopped, which adds nothing to a
        // limit that can describe itself exactly.
        Err(_) if tripped.hit() => bail!("{}", too_large(url, limit, None)),
        other => Ok(other?),
    }
}

/// The message for a document that will not fit under the cap.
///
/// One wording for both paths, naming the flag that sets the limit — the point
/// of an error a person can act on is that it says what to do next.
fn too_large(url: &str, limit: u64, declared: Option<u64>) -> String {
    let limit = human_bytes(limit);
    match declared {
        Some(n) => format!(
            "{url} is {}, over the {limit} limit; raise --max-download to accept it",
            human_bytes(n)
        ),
        // Unknown by construction: the read stops at the limit, so how much
        // more was coming is exactly what was never transferred.
        None => {
            format!("{url} is larger than the {limit} limit; raise --max-download to accept it")
        }
    }
}

/// Bytes at human scale. `2788903` tells you nothing at a glance; `2.7 MB`
/// does, and this number exists to be compared against a flag a person types.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// What the client says it can read, so a content-negotiating server hands
/// back RDF rather than the HTML page describing it.
const ACCEPT: &str = "text/turtle, application/n-triples, application/rdf+xml, \
                      application/ld+json, application/trig, application/n-quads;q=0.9, */*;q=0.1";

/// The last path segment of a URL, with any query and fragment removed.
///
/// `data.ttl?v=2` has to lose the query before its extension can be read, or
/// the extension reads as `ttl?v=2` and matches nothing.
fn url_file_name(url: &str) -> &str {
    let path = url.split_once(['?', '#']).map_or(url, |(before, _)| before);
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// Guesses a syntax from the extension on a URL's path.
///
/// The whole file name goes to [`loader::format_from_path`] rather than just
/// the final extension, so `data.ttl.gz` resolves through the `.gz` to Turtle
/// rather than stopping at a wrapper that names no syntax.
fn format_from_url(url: &str) -> Option<loader::RdfFormat> {
    let name = url_file_name(url);
    if !name.contains('.') {
        return None;
    }
    loader::format_from_path(Path::new(name))
}

/// Reads at most `left` bytes, then fails.
///
/// `Read::take` stops silently at the limit, which is the wrong answer here: a
/// truncated document parses as a valid but incomplete graph and is then
/// validated as though it were the whole thing. This also sits *after*
/// decompression, since a few megabytes of gzip can expand without bound and a
/// limit on the compressed bytes would never notice.
struct Limited<R> {
    inner: R,
    left: u64,
    tripped: Tripped,
}

/// Records that a [`Limited`] refused to read further.
///
/// Shared rather than returned, because by the time the failure surfaces it
/// has been through the RDF parser, which reports what it can see — an
/// unreadable stream — and cannot know a limit caused it. One `Tripped` is
/// shared by the compressed and decompressed readers of a single fetch: either
/// firing means the same thing to the caller.
#[derive(Clone, Default)]
struct Tripped(std::rc::Rc<std::cell::Cell<bool>>);

impl Tripped {
    fn hit(&self) -> bool {
        self.0.get()
    }
}

impl<R: std::io::Read> std::io::Read for Limited<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            self.tripped.0.set(true);
            // Terse: the caller sees `too_large` instead, which knows the URL
            // and the limit. This text only surfaces if a future caller wires
            // up a `Limited` and forgets to check the flag.
            return Err(std::io::Error::other("download limit reached"));
        }
        let cap = buf.len().min(self.left as usize);
        let n = self.inner.read(&mut buf[..cap])?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// Loads one input graph, from a path or, when it is exactly `-`, standard
/// input.
///
/// Standard input has no extension to guess a syntax from, so it requires
/// `forced`. A real path with `forced` given opts out of both the extension
/// guess and, for Turtle, the parallel chunked reader, in favour of the plain
/// streaming one — the reasonable trade for a caller overriding the format at
/// all, and never the default path a plain `--data data.ttl` takes.
fn load_source(
    path: &Path,
    forced: Option<InputFormat>,
    scope: u32,
    store: &mut TermStore,
    limit: u64,
) -> Result<Graph> {
    if path.as_os_str() == "-" {
        let format = forced
            .context("reading from `-` needs an explicit --data-format or --shapes-format")?;
        return Ok(loader::load_reader(
            std::io::stdin().lock(),
            format.rdf(),
            "http://example.org/stdin",
            scope,
            store,
        )?);
    }

    if let Some(url) = url_of(path) {
        return fetch_source(url, forced, scope, store, limit);
    }

    match forced {
        Some(fmt) => {
            let base = loader::path_to_base_iri(path)?;
            let file =
                std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            Ok(loader::load_reader(
                BufReader::new(file),
                fmt.rdf(),
                &base,
                scope,
                store,
            )?)
        }
        None => Ok(loader::load_file(path, scope, store)?),
    }
}

/// Loads several documents into one graph.
///
/// Each gets its own blank node scope, because a blank node label is local to
/// the document that introduced it: `_:a` in two files names two nodes, and
/// merging them under one scope would silently weld them together. `base`
/// separates the data and shapes runs so their scopes cannot collide either.
fn load_all(
    paths: &[PathBuf],
    forced: Option<InputFormat>,
    base: u32,
    store: &mut TermStore,
    limit: u64,
) -> Result<Graph> {
    // The single-document case is the overwhelming majority, and keeping it on
    // the plain scope leaves blank node labels in reports as they were.
    if let [only] = paths {
        return load_source(only, forced, base, store, limit);
    }

    let mut merged = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        let scope = scope::FIRST_DYNAMIC + base * 1024 + i as u32;
        let g = load_source(path, forced, scope, store, limit)
            .with_context(|| format!("loading {}", describe(path)))?;
        merged.extend(g.iter());
    }

    let mut b = shacl::model::GraphBuilder::new();
    for [s, p, o] in merged {
        b.push(s, p, o);
    }
    Ok(b.build())
}

/// Rewrites pySHACL's multi-character short options into long ones.
///
/// pySHACL is built on `argparse`, which allows `-df`; clap does not, and no
/// combination of aliases makes it. Since these are advertised as compatible,
/// they are translated here rather than left as a footnote saying the flag you
/// already know does not work.
///
/// Only exact matches are rewritten, so a value that happens to read `-df` —
/// after `--data`, say — is untouched, and `--` still ends option parsing.
///
/// Works in `OsString` rather than `String`, because a path need not be valid
/// UTF-8: `std::env::args()` panics on one that is not, which would have made
/// this refuse a file clap itself would have accepted. `to_str` simply returns
/// `None` for those, and they pass through as they arrived.
fn translate_pyshacl_shorts(args: impl Iterator<Item = OsString>) -> Vec<OsString> {
    let mut out = Vec::new();
    let mut literal = false;
    for arg in args {
        if literal {
            out.push(arg);
            continue;
        }
        if arg == "--" {
            literal = true;
            out.push(arg);
            continue;
        }
        let rewritten = match arg.to_str() {
            Some("-df") => Some("--data-format"),
            Some("-sf") => Some("--shapes-format"),
            _ => None,
        };
        out.push(rewritten.map_or(arg, OsString::from));
    }
    out
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        // A non-conforming graph is a normal outcome, not an error, but it
        // still deserves a distinct exit code so scripts can branch on it.
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool> {
    let args = Args::parse_from(translate_pyshacl_shorts(std::env::args_os()));
    let is_stdin = |p: &PathBuf| p.as_os_str() == "-";
    let stdin_count = args
        .data
        .iter()
        .chain(&args.shapes)
        .filter(|p| is_stdin(p))
        .count();
    if stdin_count > 1 {
        anyhow::bail!("only one input may be `-`: standard input can be read only once");
    }
    // No --shapes means the data graph carries its own, which is the
    // convention for a self-describing document.
    let shapes_paths = if args.shapes.is_empty() {
        args.data.clone()
    } else {
        args.shapes.clone()
    };

    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);

    let t0 = Instant::now();
    let mut data = load_all(
        &args.data,
        args.data_format,
        scope::DATA,
        &mut store,
        args.max_download,
    )
    .context("loading data graph")?;
    if args.inference == Inference::Rdfs {
        data = shacl::inference::rdfs_closure(&data, &vocab)
            .context("materialising RDFS entailments")?;
    }
    let shapes_graph = if shapes_paths == args.data {
        None
    } else {
        Some(
            load_all(
                &shapes_paths,
                args.shapes_format,
                scope::SHAPES,
                &mut store,
                args.max_download,
            )
            .context("loading shapes graph")?,
        )
    };
    let shapes_ref = shapes_graph.as_ref().unwrap_or(&data);
    let load_time = t0.elapsed();

    // Before trusting the shapes graph to say anything about the data, check
    // it is well formed. A violation here means the results that follow would
    // be answering the wrong question, so it stops rather than warns.
    if args.meta_shacl {
        let meta = check_shapes_graph(shapes_ref, &mut store, &vocab)
            .context("validating the shapes graph against SHACL-SHACL")?;
        if !meta.conforms(&[vocab.sh_Violation]) {
            for r in &meta.results {
                eprintln!(
                    "  shapes graph: {} on {}",
                    store
                        .iri(r.source_constraint_component)
                        .and_then(|i| i.rsplit_once('#').map(|(_, l)| l.to_string()))
                        .unwrap_or_default(),
                    store.to_oxrdf(r.focus_node),
                );
            }
            anyhow::bail!(
                "the shapes graph is not well formed ({} result(s) above)",
                meta.results.len()
            );
        }
    }

    // Compiling once and validating many is the point of the split: a shapes
    // graph is fixed while data changes.
    let t1 = Instant::now();
    let compiled = shacl::shapes::Shapes::compile(shapes_ref, &store, &vocab)
        .context("compiling shapes graph")?;
    let compile_time = t1.elapsed();

    // pySHACL spells the default as a pair of flags. They are honoured rather
    // than ignored: passing one alongside a `--min-severity` that contradicts
    // it means the graph should still be allowed to hold results at that
    // level, so the permissive flag wins over the threshold.
    let mut severity = args.min_severity;
    if args.allow_infos && severity == Severity::Info {
        severity = Severity::Warning;
    }
    if args.allow_warnings {
        severity = Severity::Violation;
    }
    let disallowed = severity.disallowed(&vocab);

    // `--abort` is the common spelling of a cap of one; if both are given the
    // smaller wins, since each is a ceiling rather than a target.
    let max_results = match (args.abort, args.max_results) {
        (true, Some(n)) => Some(n.min(1)),
        (true, None) => Some(1),
        (false, n) => n,
    };
    // The cap must count exactly the severities conformance is judged by,
    // resolved above. Stopping on a result that does not block would let the
    // run print `conforms: true` with a violation still unexamined.
    let options = shacl::validate::Options {
        max_results,
        blocking: Some(disallowed.clone()),
    };

    let t2 = Instant::now();
    let mut report = shacl::validate::validate_in_with(
        &data,
        &compiled,
        shapes_ref,
        &mut store,
        &vocab,
        options.clone(),
    )?;
    let mut best = t2.elapsed();
    for _ in 1..args.repeat {
        let t = Instant::now();
        report = shacl::validate::validate_in_with(
            &data,
            &compiled,
            shapes_ref,
            &mut store,
            &vocab,
            options.clone(),
        )?;
        best = best.min(t.elapsed());
    }

    let conforms = report.conforms(&disallowed);

    if args.timing {
        eprintln!(
            "load {:.3}s  compile {:.3}s  validate {:.3}s  ({} triples, {} shapes, {} results)",
            load_time.as_secs_f64(),
            compile_time.as_secs_f64(),
            best.as_secs_f64(),
            data.len(),
            compiled.len(),
            report.results.len(),
        );
    }

    // The RDF report is the specification's own artefact: a graph, so it can be
    // queried, diffed, or handed to another tool. It carries `sh:conforms`
    // itself, so nothing is printed alongside it.
    let text = if let Some(rdf) = args.format.rdf() {
        report.serialize(rdf, &store, &vocab, shapes_ref, &disallowed)?
    } else {
        let mut out = format!("conforms: {conforms}\n");
        if !args.quiet {
            for r in &report.results {
                let show = |t: Option<shacl::TermId>| {
                    t.map(|t| store.to_oxrdf(t).to_string())
                        .unwrap_or_else(|| "-".into())
                };
                out.push_str(&format!(
                    "  {} | focus {} | value {} | path {}\n",
                    store
                        .iri(r.source_constraint_component)
                        .and_then(|i| i.rsplit_once('#').map(|(_, l)| l.to_string()))
                        .unwrap_or_default(),
                    show(Some(r.focus_node)),
                    show(r.value),
                    show(r.path),
                ));
            }
        }
        out
    };

    match &args.output {
        Some(path) => std::fs::write(path, text)
            .with_context(|| format!("writing report to {}", path.display()))?,
        None => print!("{text}"),
    }
    Ok(conforms)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves `body` once, from a real socket, and returns its URL.
    fn serve(body: Vec<u8>, content_type: &str, name: &str) -> String {
        serve_framed(body, content_type, name, true)
    }

    fn serve_framed(body: Vec<u8>, content_type: &str, name: &str, length: bool) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let ct = content_type.to_string();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let _ = sock.read(&mut [0u8; 2048]);
                let framing = if length {
                    format!("Content-Length: {}\r\n", body.len())
                } else {
                    "Transfer-Encoding: chunked\r\n".to_string()
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\n{framing}Connection: close\r\n\r\n"
                );
                let _ = sock.write_all(head.as_bytes());
                if length {
                    let _ = sock.write_all(&body);
                } else {
                    for part in body.chunks(64 * 1024) {
                        let _ = sock.write_all(format!("{:x}\r\n", part.len()).as_bytes());
                        let _ = sock.write_all(part);
                        let _ = sock.write_all(b"\r\n");
                    }
                    let _ = sock.write_all(b"0\r\n\r\n");
                }
            }
        });
        format!("http://{addr}/{name}")
    }

    fn turtle(triples: usize) -> String {
        let mut doc = String::from("@prefix ex: <http://ex/> .\n");
        for i in 0..triples {
            doc.push_str(&format!("ex:s{i} ex:p ex:o{i} .\n"));
        }
        doc
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        std::io::Write::write_all(&mut enc, bytes).unwrap();
        enc.finish().unwrap()
    }

    fn fetch_err(url: &str, limit: u64) -> String {
        let mut store = TermStore::new();
        format!(
            "{:#}",
            fetch_source(url, None, 0, &mut store, limit).unwrap_err()
        )
    }

    /// The size cap has to say it is a size cap.
    ///
    /// It reached the user as `parse error: … unexpected end of file` once: the
    /// protection worked and the diagnosis sent you to look for corruption in a
    /// document that was fine. Every framing is covered because they take
    /// different routes out — a declared length is refused before the transfer,
    /// while a chunked body is only caught as it is read.
    #[test]
    fn an_oversized_download_names_the_limit_and_the_flag() {
        let doc = turtle(200_000);
        let limit = (doc.len() / 2) as u64;
        let gz = gzip(doc.as_bytes());

        let cases = [
            (
                "declared",
                serve(doc.clone().into_bytes(), "text/turtle", "d.ttl"),
            ),
            ("gzipped", serve(gz.clone(), "application/gzip", "d.ttl.gz")),
            (
                "chunked",
                serve_framed(doc.into_bytes(), "text/turtle", "d.ttl", false),
            ),
            (
                "chunked gzip",
                serve_framed(gz, "application/gzip", "d.ttl.gz", false),
            ),
        ];

        for (case, url) in cases {
            let err = fetch_err(&url, limit);
            assert!(
                err.contains("--max-download"),
                "{case}: no flag to act on: {err}"
            );
            assert!(err.contains("2.7 MB"), "{case}: no limit named: {err}");
            assert!(
                !err.contains("parse error"),
                "{case}: blamed the document: {err}"
            );
        }
    }

    /// The cap counts decompressed bytes, so a small download cannot expand
    /// past it. 200 KB on the wire against a 4 MB cap: every check on the
    /// transfer passes, and only the expansion is over.
    #[test]
    fn a_compressed_bomb_is_refused_on_its_expanded_size() {
        // Repeating one statement, which Turtle allows, is what gives a bomb
        // its ratio — `turtle(n)` writes a distinct subject per line and
        // manages only about 10:1, not enough to stay under the cap on the
        // wire while breaking it on expansion.
        let doc =
            "@prefix ex: <http://ex/> .\n".to_string() + &"ex:s ex:p ex:o .\n".repeat(2_000_000);
        let gz = gzip(doc.as_bytes());
        let limit = 4 * 1024 * 1024;
        assert!(
            (gz.len() as u64) < limit / 4,
            "the wire bytes must sit well under the cap for this to test anything, got {}",
            gz.len()
        );
        assert!(doc.len() as u64 > limit * 4, "and the expansion well over");

        let url = serve(gz, "application/gzip", "bomb.ttl.gz");
        let err = fetch_err(&url, limit);
        assert!(err.contains("larger than the 4.0 MB limit"), "{err}");
    }

    /// The cap is a cap, not a truncation: a document that fits still loads
    /// whole. Without this the test above passes just as well on a build that
    /// refuses everything.
    #[test]
    fn a_document_inside_the_limit_loads_whole() {
        let doc = turtle(20_000);
        let url = serve(gzip(doc.as_bytes()), "application/gzip", "ok.ttl.gz");
        let mut store = TermStore::new();
        let graph = fetch_source(&url, None, 0, &mut store, 1 << 30).unwrap();
        assert_eq!(graph.len(), 20_000);
    }

    #[test]
    fn byte_sizes_are_readable() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2_788_903), "2.7 MB");
        assert_eq!(human_bytes(1 << 30), "1.0 GB");
    }

    #[test]
    fn only_http_urls_are_fetched() {
        let u = |s: &str| url_of(Path::new(s)).is_some();
        assert!(u("https://example.org/data.ttl"));
        assert!(u("http://example.org/data.ttl"));

        // Everything else is a path. `file:` is a second, subtly different way
        // of saying what a path already says; the rest reach places a
        // validator has no business reaching.
        assert!(!u("file:///tmp/data.ttl"));
        assert!(!u("ftp://example.org/data.ttl"));
        assert!(!u("/tmp/data.ttl"));
        assert!(!u(r"C:\data\graph.ttl"));
        assert!(!u("-"));
        // Not a scheme, just a filename that starts the same way.
        assert!(!u("https-notes.ttl"));
    }

    #[test]
    fn a_url_extension_survives_a_query_string() {
        let f = |s: &str| format_from_url(s);
        assert!(matches!(
            f("https://ex.org/d.ttl"),
            Some(loader::RdfFormat::Turtle)
        ));
        // The query and fragment must come off first, or the extension reads
        // as "ttl?v=2" and matches nothing.
        assert!(matches!(
            f("https://ex.org/d.ttl?v=2"),
            Some(loader::RdfFormat::Turtle)
        ));
        assert!(matches!(
            f("https://ex.org/d.ttl#frag"),
            Some(loader::RdfFormat::Turtle)
        ));
        assert!(matches!(
            f("https://ex.org/a.b/c.nt"),
            Some(loader::RdfFormat::NTriples)
        ));

        // No extension to go on: the caller must be told, not guessed at.
        assert!(f("https://ex.org/sparql").is_none());
        assert!(f("https://ex.org/").is_none());
    }

    /// A URL naming a compressed document resolves to the syntax inside it.
    #[test]
    fn a_gzipped_url_names_the_syntax_underneath() {
        let f = |s: &str| format_from_url(s);
        assert!(matches!(
            f("https://ex.org/d.ttl.gz"),
            Some(loader::RdfFormat::Turtle)
        ));
        assert!(matches!(
            f("https://ex.org/dumps/latest.nt.gz?v=2"),
            Some(loader::RdfFormat::NTriples)
        ));
        // A directory in the path must not be mistaken for the file name.
        assert!(matches!(
            f("https://ex.org/a.b.c/d.ttl"),
            Some(loader::RdfFormat::Turtle)
        ));
        assert!(f("https://ex.org/d.gz").is_none());
        assert!(f("https://ex.org/sparql").is_none());
    }

    /// An argument need not be valid UTF-8. Reading `std::env::args()` made
    /// this panic before parsing began, on a path clap itself would take.
    #[test]
    fn arguments_that_are_not_utf8_pass_through() {
        let odd = OsString::from("plain.ttl");
        let got = translate_pyshacl_shorts(vec![OsString::from("-df"), odd.clone()].into_iter());
        assert_eq!(got, vec![OsString::from("--data-format"), odd]);

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            // A lone 0x80 byte is a legal file name and not valid UTF-8.
            let bad = OsString::from_vec(vec![b'/', 0x80, b'.', b't', b't', b'l']);
            let got = translate_pyshacl_shorts(vec![bad.clone()].into_iter());
            assert_eq!(got, vec![bad]);
        }
    }

    /// pySHACL spells these with one dash and two letters, which clap cannot
    /// parse, so they are rewritten before it sees them.
    #[test]
    fn pyshacl_two_letter_shorts_become_long_options() {
        let go = |args: &[&str]| translate_pyshacl_shorts(args.iter().map(OsString::from));
        assert_eq!(go(&["-df", "ttl"]), vec!["--data-format", "ttl"]);
        assert_eq!(go(&["-sf", "nt"]), vec!["--shapes-format", "nt"]);

        // A value that merely looks like one is left alone.
        assert_eq!(
            go(&["--data", "--", "-df"]),
            vec!["--data", "--", "-df"],
            "`--` ends option parsing"
        );
        // And anything unrecognised passes straight through.
        assert_eq!(
            go(&["-d", "x.ttl", "--abort"]),
            vec!["-d", "x.ttl", "--abort"]
        );
    }

    /// `--abort` and `--max-results` are both ceilings, so the lower wins
    /// rather than the later.
    #[test]
    fn abort_and_max_results_take_the_smaller() {
        let pick = |abort: bool, max: Option<usize>| match (abort, max) {
            (true, Some(n)) => Some(n.min(1)),
            (true, None) => Some(1),
            (false, n) => n,
        };
        assert_eq!(pick(true, None), Some(1));
        assert_eq!(pick(false, Some(5)), Some(5));
        assert_eq!(pick(true, Some(5)), Some(1));
        assert_eq!(pick(false, None), None);
    }
}
