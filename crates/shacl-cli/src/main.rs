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
use shacl::model::{loader, scope, TermStore, Vocab};

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
    let mut report = shacl::validate::validate_in(&data, &compiled, shapes_ref, &mut store, &vocab)?;
    let mut best = t2.elapsed();
    for _ in 1..args.repeat {
        let t = Instant::now();
        report = shacl::validate::validate_in(&data, &compiled, shapes_ref, &mut store, &vocab)?;
        best = best.min(t.elapsed());
    }

    let conforms = report.conforms(&[vocab.sh_Violation]);

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
