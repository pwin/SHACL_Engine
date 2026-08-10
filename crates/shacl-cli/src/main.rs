//! `shacl` — validate an RDF data graph against a SHACL shapes graph.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result};

// Parsing allocates a string per term — millions on a large graph — and the
// system allocator becomes the contention point once that runs across threads.
// Worth about 25% on load, on both the parallel and sequential paths.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

use clap::Parser;
use shacl::model::{TermStore, Vocab, loader, scope};

#[derive(Parser)]
#[command(name = "shacl", version, about = "A high-performance SHACL validator")]
struct Args {
    /// The data graph to validate.
    #[arg(short, long)]
    data: PathBuf,

    /// The shapes graph. Defaults to the data graph, matching the convention
    /// that a self-describing document carries its own shapes.
    #[arg(short, long)]
    shapes: Option<PathBuf>,

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
    let shapes_path = args.shapes.clone().unwrap_or_else(|| args.data.clone());

    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);

    let t0 = Instant::now();
    let data = loader::load_file(&args.data, scope::DATA, &mut store)
        .with_context(|| format!("loading data graph {}", args.data.display()))?;
    let shapes_graph = if shapes_path == args.data {
        None
    } else {
        Some(
            loader::load_file(&shapes_path, scope::SHAPES, &mut store)
                .with_context(|| format!("loading shapes graph {}", shapes_path.display()))?,
        )
    };
    let shapes_ref = shapes_graph.as_ref().unwrap_or(&data);
    let load_time = t0.elapsed();

    // Compiling once and validating many is the point of the split: a shapes
    // graph is fixed while data changes.
    let t1 = Instant::now();
    let compiled = shacl::shapes::Shapes::compile(shapes_ref, &store, &vocab)
        .context("compiling shapes graph")?;
    let compile_time = t1.elapsed();

    let t2 = Instant::now();
    let mut report =
        shacl::validate::validate_in(&data, &compiled, shapes_ref, &mut store, &vocab)?;
    let mut best = t2.elapsed();
    for _ in 1..args.repeat {
        let t = Instant::now();
        report = shacl::validate::validate_in(&data, &compiled, shapes_ref, &mut store, &vocab)?;
        best = best.min(t.elapsed());
    }

    let disallowed = args.min_severity.disallowed(&vocab);
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
    if let Some(rdf) = args.format.rdf() {
        let text = report.serialize(rdf, &store, &vocab, shapes_ref, &disallowed)?;
        print!("{text}");
        return Ok(conforms);
    }

    println!("conforms: {conforms}");
    if !args.quiet {
        for r in &report.results {
            let show = |t: Option<shacl::TermId>| {
                t.map(|t| store.to_oxrdf(t).to_string())
                    .unwrap_or_else(|| "-".into())
            };
            println!(
                "  {} | focus {} | value {} | path {}",
                store
                    .iri(r.source_constraint_component)
                    .and_then(|i| i.rsplit_once('#').map(|(_, l)| l.to_string()))
                    .unwrap_or_default(),
                show(Some(r.focus_node)),
                show(r.value),
                show(r.path),
            );
        }
    }
    Ok(conforms)
}
