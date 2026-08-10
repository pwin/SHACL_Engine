//! `shacl` — validate an RDF data graph against a SHACL shapes graph.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

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

    // The document's own address is its base IRI, so a `<>` self-reference
    // and any relative IRI resolve against where it actually came from.
    Ok(loader::load_reader(
        response.body_mut().as_reader(),
        format,
        url,
        scope,
        store,
    )?)
}

/// What the client says it can read, so a content-negotiating server hands
/// back RDF rather than the HTML page describing it.
const ACCEPT: &str = "text/turtle, application/n-triples, application/rdf+xml, \
                      application/ld+json, application/trig, application/n-quads;q=0.9, */*;q=0.1";

/// Guesses a syntax from the extension on a URL's path.
///
/// The query and fragment are cut off first: `data.ttl?v=2` is Turtle, and
/// treating `ttl?v=2` as the extension would find nothing.
fn format_from_url(url: &str) -> Option<loader::RdfFormat> {
    let path = url.split_once(['?', '#']).map_or(url, |(before, _)| before);
    let ext = path.rsplit_once('.')?.1;
    loader::format_from_path(Path::new(&format!("x.{ext}")))
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
        return fetch_source(url, forced, scope, store);
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
) -> Result<Graph> {
    // The single-document case is the overwhelming majority, and keeping it on
    // the plain scope leaves blank node labels in reports as they were.
    if let [only] = paths {
        return load_source(only, forced, base, store);
    }

    let mut merged = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        let scope = scope::FIRST_DYNAMIC + base * 1024 + i as u32;
        let g = load_source(path, forced, scope, store)
            .with_context(|| format!("loading {}", describe(path)))?;
        merged.extend(g.iter());
    }

    let mut b = shacl::model::GraphBuilder::new();
    for [s, p, o] in merged {
        b.push(s, p, o);
    }
    Ok(b.build())
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
    let args = Args::parse();
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
    let mut data = load_all(&args.data, args.data_format, scope::DATA, &mut store)
        .context("loading data graph")?;
    if args.inference == Inference::Rdfs {
        data = shacl::inference::rdfs_closure(&data, &vocab);
    }
    let shapes_graph = if shapes_paths == args.data {
        None
    } else {
        Some(
            load_all(&shapes_paths, args.shapes_format, scope::SHAPES, &mut store)
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

    // `--abort` is the common spelling of a cap of one; if both are given the
    // smaller wins, since each is a ceiling rather than a target.
    let max_results = match (args.abort, args.max_results) {
        (true, Some(n)) => Some(n.min(1)),
        (true, None) => Some(1),
        (false, n) => n,
    };
    let options = shacl::validate::Options { max_results };

    let t2 = Instant::now();
    let mut report = shacl::validate::validate_in_with(
        &data, &compiled, shapes_ref, &mut store, &vocab, options,
    )?;
    let mut best = t2.elapsed();
    for _ in 1..args.repeat {
        let t = Instant::now();
        report = shacl::validate::validate_in_with(
            &data, &compiled, shapes_ref, &mut store, &vocab, options,
        )?;
        best = best.min(t.elapsed());
    }

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
