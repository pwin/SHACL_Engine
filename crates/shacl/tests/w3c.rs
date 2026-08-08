//! Runs the W3C SHACL test suites.
//!
//! Manifests are walked from `testsuite/*/tests/manifest.ttl`. Each `sht:Validate`
//! entry names a data graph and a shapes graph, validates one against the other,
//! and compares the result to the report embedded in the manifest.
//!
//! Expected and actual reports are both held as [`ValidationReport`], so they are
//! compared through one representation rather than by diffing serialised RDF.
//! `sh:resultMessage` is excluded: the spec leaves message text to the
//! implementation.
//!
//! Set `SHACL_TEST_FILTER` to run a subset, and `SHACL_TEST_VERBOSE=1` to print
//! the first differing result of every failure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use shacl::model::{loader, Graph, TermId, TermStore, Vocab};
use shacl::path::Path as ShaclPath;
use shacl::report::{ValidationReport, ValidationResult};

/// Where the vendored suites live, relative to this crate.
const SUITES: &[(&str, &str)] = &[
    ("shacl12", "../../testsuite/shacl12/tests/manifest.ttl"),
    ("shacl10", "../../testsuite/shacl10/tests/manifest.ttl"),
];

// ---------------------------------------------------------------- discovery

/// Recursively collects manifest files reachable via `mf:include`.
fn collect_manifests(root: &Path, out: &mut Vec<PathBuf>) {
    if !root.exists() || out.iter().any(|p| p == root) {
        return;
    }
    out.push(root.to_path_buf());

    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let Ok(graph) = loader::load_file(root, 0, &mut store) else {
        return;
    };
    // Includes are IRIs resolved against the manifest's own base, so they come
    // back as absolute `file:` IRIs and map straight onto paths.
    let includes: Vec<TermId> = graph.objects_of(vocab.mf_include).collect();
    for inc in includes {
        if let Some(path) = store.iri(inc).and_then(iri_to_path) {
            collect_manifests(&path, out);
        }
    }
}

fn iri_to_path(iri: &str) -> Option<PathBuf> {
    let rest = iri.strip_prefix("file:///").or_else(|| iri.strip_prefix("file://"))?;
    let decoded = percent_decode(rest);
    Some(PathBuf::from(decoded))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

// ------------------------------------------------------------------ running

struct Outcome {
    name: String,
    status: Status,
}

enum Status {
    Pass,
    /// Ran to completion but disagreed with the expected report.
    Mismatch(String),
    /// Could not be run at all.
    Error(String),
}

/// Runs every `sht:Validate` entry in one manifest file.
fn run_manifest(manifest: &Path, out: &mut Vec<Outcome>) {
    let mut store = TermStore::new();
    let vocab = Vocab::new(&mut store);
    let graph = match loader::load_file(manifest, 0, &mut store) {
        Ok(g) => g,
        Err(e) => {
            out.push(Outcome {
                name: manifest.display().to_string(),
                status: Status::Error(format!("could not load manifest: {e}")),
            });
            return;
        }
    };

    // `mf:entries` is an RDF list hanging off the manifest node.
    let entry_lists: Vec<TermId> = graph.objects_of(vocab.mf_entries).collect();
    let mut entries = Vec::new();
    for head in entry_lists {
        if let Some(items) = graph.list(head, &vocab) {
            entries.extend(items);
        }
    }

    // Two kinds of entry: `sht:Validate` runs a validation and compares
    // reports, `sht:EvalNodeExpr` evaluates a node expression and compares the
    // resulting sequence.
    let eval_node_expr = store.named_node(&format!("{}EvalNodeExpr", shacl::model::vocab::SHT));
    let shnex = shacl::nodeexpr::Shnex::new(&mut store);

    for entry in entries {
        let is_validate = graph.contains(entry, vocab.rdf_type, vocab.sht_Validate);
        let is_eval = graph.contains(entry, vocab.rdf_type, eval_node_expr);
        if !is_validate && !is_eval {
            continue;
        }
        let name = store
            .iri(entry)
            .map(|i| short_name(i, manifest))
            .unwrap_or_else(|| format!("{}#?", manifest.display()));
        let status = if is_validate {
            run_one(entry, &graph, &mut store, &vocab, manifest)
        } else {
            run_node_expr(entry, &graph, &mut store, &vocab, &shnex)
        };
        out.push(Outcome { name, status });
    }
}

/// Runs one `sht:EvalNodeExpr` entry.
///
/// `mf:result` is an RDF list, and node expressions produce a *sequence*, so
/// the comparison is order-sensitive.
fn run_node_expr(
    entry: TermId,
    manifest: &Graph,
    store: &mut TermStore,
    vocab: &Vocab,
    shnex: &shacl::nodeexpr::Shnex,
) -> Status {
    let Some(action) = manifest.object(entry, vocab.mf_action) else {
        return Status::Error("entry has no mf:action".into());
    };
    let node_expr_p = store.named_node(&format!("{}nodeExpr", shacl::model::vocab::SHT));
    let focus_p = store.named_node(&format!("{}focusNode", shacl::model::vocab::SHT));

    let Some(expr) = manifest.object(action, node_expr_p) else {
        return Status::Error("mf:action has no sht:nodeExpr".into());
    };
    let focus = manifest.object(action, focus_p);

    // Variables reach the expression as `sht:scope-<name>` on the action.
    let scope_prefix = format!("{}scope-", shacl::model::vocab::SHT);
    let vars: Vec<(String, TermId)> = manifest
        .predicate_objects(action)
        .filter_map(|(p, o)| {
            let name = store.iri(p)?.strip_prefix(&scope_prefix)?.to_string();
            Some((name, o))
        })
        .collect();

    // Some entries only fix the *set* of results, not their order.
    let ignore_order_p = store.named_node(&format!("{}ignoreOrder", shacl::model::vocab::SHT));
    let ignore_order = manifest
        .object(action, ignore_order_p)
        .and_then(|t| store.lexical_form(t))
        .is_some_and(|v| v == "true");

    let expected: Vec<TermId> = match manifest.object(entry, vocab.mf_result) {
        Some(head) => match manifest.list(head, vocab) {
            Some(items) => items,
            None => return Status::Error("mf:result is not a well-formed list".into()),
        },
        None => return Status::Error("entry has no mf:result".into()),
    };

    // Shape-valued operators validate against shapes declared in the same
    // document as the expression.
    let shapes = shacl::shapes::Shapes::compile(manifest, store, vocab).ok();

    let actual = {
        let ctx = shacl::nodeexpr::Ctx {
            data: manifest,
            exprs: manifest,
            vocab,
            shnex,
            shapes: shapes.as_ref(),
            vars: &vars,
        };
        match shacl::nodeexpr::eval(expr, focus, &ctx, store) {
            Ok(v) => v,
            Err(e) => return Status::Error(format!("{e}")),
        }
    };

    let matches = if ignore_order {
        let (mut a, mut b) = (actual.clone(), expected.clone());
        a.sort_unstable();
        b.sort_unstable();
        a == b
    } else {
        actual == expected
    };
    if matches {
        return Status::Pass;
    }
    let show = |ts: &[TermId], store: &TermStore| {
        ts.iter()
            .map(|&t| store.to_oxrdf(t).to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    Status::Mismatch(format!(
        "expected ({})\n      got      ({})",
        show(&expected, store),
        show(&actual, store)
    ))
}

/// A stable, readable id like `core/node/datatype-001`.
fn short_name(iri: &str, manifest: &Path) -> String {
    let dir = manifest.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let dir = dir.replace('\\', "/");
    iri.rsplit_once('/')
        .map(|(_, last)| {
            let group = dir
                .rsplit_once("/tests/")
                .map(|(_, g)| g.to_string())
                .unwrap_or_default();
            if group.is_empty() {
                last.to_string()
            } else {
                format!("{group}/{last}")
            }
        })
        .unwrap_or_else(|| iri.to_string())
}

fn run_one(
    entry: TermId,
    manifest: &Graph,
    store: &mut TermStore,
    vocab: &Vocab,
    manifest_path: &Path,
) -> Status {
    let Some(action) = manifest.object(entry, vocab.mf_action) else {
        return Status::Error("entry has no mf:action".into());
    };
    let Some(result_node) = manifest.object(entry, vocab.mf_result) else {
        return Status::Error("entry has no mf:result".into());
    };

    // A test may point at the manifest itself (`<>`) or at a sibling document.
    // `files` caches by path so the self-referring case resolves to the very
    // same graph and blank node scope, which is what lets an expected report
    // name a blank node from the data.
    let resolve = |t: Option<TermId>, store: &TermStore| -> Option<PathBuf> {
        let iri = store.iri(t?)?;
        iri_to_path(iri)
    };
    let data_path = resolve(manifest.object(action, vocab.sht_dataGraph), store);
    let shapes_path = resolve(manifest.object(action, vocab.sht_shapesGraph), store);

    // `mf:result sht:Failure` says the shape cannot be evaluated at all, so the
    // expected outcome is an error rather than a report. Parsing it as a report
    // would silently read as "conforms, with no results".
    let failure = store.named_node(&format!("{}Failure", shacl::model::vocab::SHT));
    let expects_failure = result_node == failure;

    let expected = ValidationReport::parse(result_node, manifest, store, vocab);

    let self_path = manifest_path.canonicalize().ok();
    let same_as_manifest = |p: &Option<PathBuf>| match (p, &self_path) {
        (Some(p), Some(s)) => p.canonicalize().ok().as_ref() == Some(s),
        _ => false,
    };

    // Fast path: both graphs are the manifest document, which covers the great
    // majority of the suite and needs no extra loading.
    if same_as_manifest(&data_path) && same_as_manifest(&shapes_path) {
        let actual = match shacl::validate::validate(manifest, manifest, store, vocab) {
            Ok(r) if expects_failure => {
                return Status::Mismatch(format!(
                    "expected a failure, but validation produced {} result(s)",
                    r.results.len()
                ))
            }
            Ok(r) => r,
            // A failure was the expected outcome.
            Err(_) if expects_failure => return Status::Pass,
            Err(e) => return Status::Error(format!("validation failed: {e}")),
        };
        return compare(
            &expected.report,
            &actual,
            manifest,
            manifest,
            store,
            vocab,
            &expected.disallowed,
        );
    }

    let (Some(data_path), Some(shapes_path)) = (data_path, shapes_path) else {
        return Status::Error("mf:action is missing a data or shapes graph".into());
    };

    let load = |path: &PathBuf, scope: u32, store: &mut TermStore| {
        if same_as_manifest(&Some(path.clone())) {
            // Re-parsing would mint a second set of blank nodes for the same
            // document; hand back the manifest's own scope instead.
            return loader::load_file(path, 0, store);
        }
        loader::load_file(path, scope, store)
    };
    let data = match load(&data_path, 100, store) {
        Ok(g) => g,
        Err(e) => return Status::Error(format!("data graph: {e}")),
    };
    let shapes = if shapes_path == data_path {
        None
    } else {
        match load(&shapes_path, 101, store) {
            Ok(g) => Some(g),
            Err(e) => return Status::Error(format!("shapes graph: {e}")),
        }
    };
    let shapes_ref = shapes.as_ref().unwrap_or(&data);

    let actual = match shacl::validate::validate(&data, shapes_ref, store, vocab) {
        Ok(r) if expects_failure => {
            return Status::Mismatch(format!(
                "expected a failure, but validation produced {} result(s)",
                r.results.len()
            ))
        }
        Ok(r) => r,
        Err(_) if expects_failure => return Status::Pass,
        Err(e) => return Status::Error(format!("validation failed: {e}")),
    };
    compare(
        &expected.report,
        &actual,
        manifest,
        shapes_ref,
        store,
        vocab,
        &expected.disallowed,
    )
}

// --------------------------------------------------------------- comparison

/// Compares the two reports.
///
/// `expected_graph` and `actual_graph` are usually the same document, but when
/// a test names an external shapes graph they differ: the expected report's
/// `sh:resultPath` blank nodes live in the manifest, while the actual report's
/// live in the shapes graph. Each side must compile its paths against the graph
/// that actually contains them.
fn compare(
    expected: &ValidationReport,
    actual: &ValidationReport,
    expected_graph: &Graph,
    actual_graph: &Graph,
    store: &TermStore,
    vocab: &Vocab,
    disallowed: &[TermId],
) -> Status {
    if expected.conforms(disallowed) != actual.conforms(disallowed) {
        return Status::Mismatch(format!(
            "conforms: expected {}, got {}",
            expected.conforms(disallowed),
            actual.conforms(disallowed)
        ));
    }

    let mut want: Vec<String> = expected
        .results
        .iter()
        .map(|r| fingerprint(r, expected_graph, store, vocab))
        .collect();
    let mut got: Vec<String> = actual
        .results
        .iter()
        .map(|r| fingerprint(r, actual_graph, store, vocab))
        .collect();
    want.sort();
    got.sort();

    if want == got {
        return Status::Pass;
    }

    let missing: Vec<_> = want.iter().filter(|w| !got.contains(w)).cloned().collect();
    let extra: Vec<_> = got.iter().filter(|g| !want.contains(g)).cloned().collect();
    Status::Mismatch(format!(
        "{} expected vs {} actual result(s)\n      missing: {}\n      extra:   {}",
        want.len(),
        got.len(),
        missing.first().map(String::as_str).unwrap_or("-"),
        extra.first().map(String::as_str).unwrap_or("-"),
    ))
}

/// A canonical string for one result, ignoring `sh:resultMessage`.
///
/// Paths are compared as compiled [`ShaclPath`]s rather than as terms: a
/// complex path in an expected report is a different blank node from the one in
/// the shapes graph, but must still count as equal.
fn fingerprint(r: &ValidationResult, shapes: &Graph, store: &TermStore, vocab: &Vocab) -> String {
    let term = |t: Option<TermId>| {
        t.map(|t| store.to_oxrdf(t).to_string())
            .unwrap_or_else(|| "-".into())
    };
    let path = match r.path {
        Some(p) => match ShaclPath::compile(p, shapes, store, vocab) {
            Ok(compiled) => format!("{compiled:?}"),
            Err(_) => term(Some(p)),
        },
        None => "-".into(),
    };
    let mut details: Vec<String> = r
        .details
        .iter()
        .map(|d| fingerprint(d, shapes, store, vocab))
        .collect();
    details.sort();

    format!(
        "focus={} value={} path={} shape={} component={} severity={} details=[{}]",
        term(Some(r.focus_node)),
        term(r.value),
        path,
        term(r.source_shape),
        term(Some(r.source_constraint_component)),
        term(Some(r.severity)),
        details.join("; ")
    )
}

// ------------------------------------------------------------------ reporting

#[test]
fn w3c_test_suites() {
    let filter = std::env::var("SHACL_TEST_FILTER").unwrap_or_default();
    let verbose = std::env::var("SHACL_TEST_VERBOSE").is_ok();

    let mut per_suite: BTreeMap<&str, Vec<Outcome>> = BTreeMap::new();
    for (suite, root) in SUITES {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(root);
        let mut manifests = Vec::new();
        collect_manifests(&root, &mut manifests);
        assert!(
            !manifests.is_empty(),
            "no manifests found under {} — is the test suite vendored?",
            root.display()
        );

        let mut outcomes = Vec::new();
        for m in &manifests {
            run_manifest(m, &mut outcomes);
        }
        outcomes.retain(|o| filter.is_empty() || o.name.contains(&filter));
        per_suite.insert(suite, outcomes);
    }

    let mut total = 0usize;
    let mut total_pass = 0usize;
    println!();
    for (suite, outcomes) in &per_suite {
        let pass = outcomes.iter().filter(|o| matches!(o.status, Status::Pass)).count();
        let errors = outcomes.iter().filter(|o| matches!(o.status, Status::Error(_))).count();
        total += outcomes.len();
        total_pass += pass;
        println!(
            "  {suite:<10} {pass:>3}/{:<3} passing  ({errors} could not run)",
            outcomes.len()
        );
    }
    println!("  {:<10} {total_pass:>3}/{total:<3} passing", "TOTAL");

    if verbose {
        println!("\n  failures:");
        for outcomes in per_suite.values() {
            for o in outcomes {
                match &o.status {
                    Status::Pass => {}
                    Status::Mismatch(m) => println!("    [diff] {}\n      {m}", o.name),
                    Status::Error(e) => println!("    [err ] {}: {e}", o.name),
                }
            }
        }
    }

    // The suites must at least be discovered; the pass count is tracked by
    // `progress` below rather than asserted here.
    assert!(total > 0, "no tests were discovered");
}

/// Guards against regressions: the pass count must never drop below this.
/// Raise it as the engine gains coverage.
const BASELINE_PASSING: usize = 414;

#[test]
fn progress() {
    let mut outcomes = Vec::new();
    for (_, root) in SUITES {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(root);
        let mut manifests = Vec::new();
        collect_manifests(&root, &mut manifests);
        for m in &manifests {
            run_manifest(m, &mut outcomes);
        }
    }
    let pass = outcomes.iter().filter(|o| matches!(o.status, Status::Pass)).count();
    assert!(
        pass >= BASELINE_PASSING,
        "regression: {pass} passing, baseline is {BASELINE_PASSING}"
    );
}
