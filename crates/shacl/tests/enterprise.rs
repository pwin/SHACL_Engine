//! The enterprise example, held to its expected counts.
//!
//! `examples/enterprise/` is a shapes graph using most of SHACL Core in
//! combination, and a generator that seeds every fault deliberately and
//! writes down, per constraint component, how many results a conforming
//! processor reports. This test runs the engine over the checked-in
//! 2,000-person graph and requires the report to match that list exactly —
//! not "some violations", the number of each kind.
//!
//! Then it holds the two things the engine promises about *how* it got there:
//! the report is the same bytes across threads and through the index cache.
//! And with the advanced shapes loaded — a SPARQL constraint and a rule — it
//! checks the run declines to split, substitutes `{$this}` in the message,
//! and infers exactly what the rule says.
//!
//! Point `SHACL_ENTERPRISE_DATA` at a larger generated graph (with its
//! `expected-N.txt` beside it) to run the same checks at scale:
//!
//! ```sh
//! python examples/enterprise/generate.py --people 200000 --out /tmp/ent
//! SHACL_ENTERPRISE_DATA=/tmp/ent/data-200000.ttl cargo test --release -p shacl --test enterprise -- --nocapture
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use oxrdfio::RdfFormat;
use shacl::model::index::{self, SourceDigest};
use shacl::model::{Graph, GraphBuilder, TermStore, Vocab, loader};
use shacl::shapes::Shapes;
use shacl::validate::{self, Options};

fn example_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/enterprise")
}

/// The per-component counts under one section of an `expected-N.txt`.
fn expected(path: &Path, section: &str) -> BTreeMap<String, usize> {
    let text = std::fs::read_to_string(path).expect("expected file should be readable");
    let mut in_section = false;
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if line.starts_with('[') {
            in_section = line == format!("[{section}]");
            continue;
        }
        if !in_section || line.starts_with('#') || line.is_empty() {
            continue;
        }
        let (name, count) = line.split_once(' ').expect("`component count` lines");
        if name != "total" {
            out.insert(name.to_owned(), count.parse().expect("a count"));
        }
    }
    assert!(
        !out.is_empty(),
        "no [{section}] section in {}",
        path.display()
    );
    out
}

struct Loaded {
    store: TermStore,
    vocab: Vocab,
    data: Graph,
    shapes_graph: Graph,
    shapes: Shapes,
}

fn load(data_path: &Path, shapes_files: &[&str]) -> Loaded {
    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let data = loader::load_file(data_path, 0, &mut store).expect("data should load");
    let mut text = String::new();
    for f in shapes_files {
        text.push_str(&std::fs::read_to_string(example_dir().join(f)).expect("shapes file"));
        text.push('\n');
    }
    let mut b = GraphBuilder::new();
    loader::parse_str(&text, RdfFormat::Turtle, "http://t/", 1, &mut store, &mut b)
        .expect("shapes should parse");
    let shapes_graph = b.build();
    let shapes = Shapes::compile(&shapes_graph, &store, &vocab).expect("shapes should compile");
    Loaded {
        store,
        vocab,
        data,
        shapes_graph,
        shapes,
    }
}

impl Loaded {
    fn report(&mut self, threads: usize) -> shacl::report::ValidationReport {
        validate::validate_in_with(
            &self.data,
            &self.shapes,
            &self.shapes_graph,
            &mut self.store,
            &self.vocab,
            Options {
                threads,
                ..Options::default()
            },
        )
        .expect("validation should run")
    }

    fn serialise(&self, report: &shacl::report::ValidationReport) -> String {
        report
            .serialize(
                RdfFormat::NTriples,
                &self.store,
                &self.vocab,
                &self.shapes_graph,
                &self.shapes,
                &[],
            )
            .expect("report should serialise")
    }

    /// Results per constraint component, by local name.
    fn counts(&self, report: &shacl::report::ValidationReport) -> BTreeMap<String, usize> {
        let mut out = BTreeMap::new();
        for r in &report.results {
            let iri = self
                .store
                .iri(r.source_constraint_component)
                .expect("component is an IRI");
            let local = iri.rsplit('#').next().unwrap_or(iri).to_owned();
            *out.entry(local).or_insert(0) += 1;
        }
        out
    }
}

fn assert_counts(got: &BTreeMap<String, usize>, want: &BTreeMap<String, usize>, what: &str) {
    if got == want {
        return;
    }
    let mut lines = Vec::new();
    for key in want
        .keys()
        .chain(got.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        let (w, g) = (
            want.get(key).copied().unwrap_or(0),
            got.get(key).copied().unwrap_or(0),
        );
        if w != g {
            lines.push(format!("  {key}: expected {w}, got {g}"));
        }
    }
    panic!(
        "{what}: result counts differ from the generator's\n{}",
        lines.join("\n")
    );
}

/// Every check, over one data file.
fn check(data_path: &Path, expected_path: &Path) {
    // --- the core shapes: exact counts
    let t = Instant::now();
    let mut l = load(data_path, &["shapes.ttl"]);
    let loaded = t.elapsed();
    assert!(
        l.shapes.is_focus_separable(),
        "the core shapes should qualify for the parallel path"
    );
    let t = Instant::now();
    let report = l.report(0);
    let validated = t.elapsed();
    let want = expected(expected_path, "shapes.ttl");
    assert_counts(&l.counts(&report), &want, "shapes.ttl");
    assert_eq!(report.results.len(), want.values().sum::<usize>());
    assert!(!report.conforms(&[], &l.vocab));
    println!(
        "  {}: {} triples, {} results — load {:.3}s, validate {:.3}s",
        data_path.file_name().unwrap().to_string_lossy(),
        l.data.len(),
        report.results.len(),
        loaded.as_secs_f64(),
        validated.as_secs_f64()
    );

    // --- the same bytes across threads
    let parallel = l.serialise(&report);
    let one_thread = l.report(1);
    let sequential = l.serialise(&one_thread);
    assert!(
        parallel == sequential,
        "the report differs between one thread and many"
    );

    // --- the same bytes through the index cache
    let via_index = {
        let mut bytes = Vec::new();
        let mut store = TermStore::new();
        let _ = Vocab::new(&mut store);
        let data = loader::load_file(data_path, 0, &mut store).expect("data should load");
        let digest = SourceDigest::of(&std::fs::read(data_path).expect("data bytes"));
        index::write(&mut bytes, &store, &data, digest).expect("index should write");
        let (mut store, data) =
            index::read(&mut &bytes[..], Some(digest)).expect("index should read");
        let vocab = Vocab::new(&mut store);
        let mut b = GraphBuilder::new();
        let text = std::fs::read_to_string(example_dir().join("shapes.ttl")).unwrap();
        loader::parse_str(&text, RdfFormat::Turtle, "http://t/", 1, &mut store, &mut b).unwrap();
        let shapes_graph = b.build();
        let shapes = Shapes::compile(&shapes_graph, &store, &vocab).unwrap();
        let report =
            validate::validate_in(&data, &shapes, &shapes_graph, &mut store, &vocab).unwrap();
        report
            .serialize(
                RdfFormat::NTriples,
                &store,
                &vocab,
                &shapes_graph,
                &shapes,
                &[],
            )
            .unwrap()
    };
    assert!(
        via_index == parallel,
        "the report differs through the index cache"
    );

    // --- the advanced shapes: a SPARQL constraint, a substituted message,
    // and no parallel path
    let mut l = load(data_path, &["shapes.ttl", "shapes-advanced.ttl"]);
    assert!(
        !l.shapes.is_focus_separable(),
        "a SPARQL constraint must take the run off the parallel path"
    );
    let report = l.report(0);
    assert_counts(
        &l.counts(&report),
        &expected(expected_path, "shapes-advanced.ttl"),
        "shapes.ttl + shapes-advanced.ttl",
    );
    let text = l.serialise(&report);
    assert!(
        text.contains("http://example.org/enterprise#p")
            && text.contains(" has left but has no end date\"@en"),
        "the SPARQL message should carry the substituted focus node and keep its language tag"
    );
    assert!(
        !text.contains("{$this}"),
        "a placeholder was left unsubstituted"
    );

    // --- the rule infers exactly the seniorities the data earns
    let inferred = shacl::rules::apply(&l.data, &l.shapes, &l.shapes_graph, &mut l.store, &l.vocab)
        .expect("rules should run");
    let seniority = l
        .store
        .named_node("http://example.org/enterprise#seniority");
    let age = l.store.named_node("http://example.org/enterprise#age");
    let got: usize = inferred.subjects_of(seniority).count();
    let want = l
        .data
        .objects_of(age)
        .zip(l.data.subjects_of(age))
        .filter(|&(o, _)| {
            l.store
                .lexical_form(o)
                .and_then(|s| s.parse::<i64>().ok())
                .is_some_and(|n| n >= 50)
        })
        .count();
    assert_eq!(
        got, want,
        "one ex:seniority per person with an integer age of 50 or more"
    );
    assert!(got > 0, "the fixture should have someone over fifty");
}

#[test]
fn the_checked_in_graph_reports_exactly_the_seeded_faults() {
    let dir = example_dir();
    check(&dir.join("data-2000.ttl"), &dir.join("expected-2000.txt"));
}

#[test]
fn a_generated_graph_at_scale_if_one_is_given() {
    let Some(data) = std::env::var_os("SHACL_ENTERPRISE_DATA") else {
        println!("  SHACL_ENTERPRISE_DATA not set; the scale check is skipped");
        return;
    };
    let data = PathBuf::from(data);
    let name = data.file_name().unwrap().to_string_lossy().to_string();
    let expected = data.with_file_name(name.replace("data-", "expected-").replace(".ttl", ".txt"));
    check(&data, &expected);
}
