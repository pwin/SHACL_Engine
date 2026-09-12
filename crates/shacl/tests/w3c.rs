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

use shacl::model::{Graph, TermId, TermStore, Vocab, loader};
use shacl::path::Path as ShaclPath;
use shacl::report::{ValidationReport, ValidationResult};

/// Where the bundled suites live, relative to this crate.
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

/// Converts a `file:` IRI back into a path.
///
/// Only `file://` is stripped, never `file:///`. A Unix path arrives as
/// `file:///home/x` and must keep its leading slash or it becomes relative,
/// which silently resolved nothing and made the whole suite look empty. A
/// Windows path arrives as `file:///C:/x`, where that same slash is spurious
/// and has to go.
fn iri_to_path(iri: &str) -> Option<PathBuf> {
    let rest = iri.strip_prefix("file://")?;
    let decoded = percent_decode(rest);
    let bytes = decoded.as_bytes();
    let stripped = if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' {
        &decoded[1..]
    } else {
        &decoded[..]
    };
    Some(PathBuf::from(stripped))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v as char);
            i += 3;
            continue;
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
    /// The same test compared the way the suite itself compares: the actual
    /// report graph against the expected one, up to blank-node isomorphism.
    /// `None` when there is no report to compare — a test expecting a failure,
    /// a node-expression test, or one that could not run.
    iso: Option<Result<(), String>>,
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
                iso: None,
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
        let (status, iso) = if is_validate {
            run_one(entry, &graph, &mut store, &vocab, manifest)
        } else {
            (
                run_node_expr(entry, &graph, &mut store, &vocab, &shnex),
                None,
            )
        };
        out.push(Outcome { name, status, iso });
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
    let dir = manifest
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
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
) -> (Status, Option<Result<(), String>>) {
    let Some(action) = manifest.object(entry, vocab.mf_action) else {
        return (Status::Error("entry has no mf:action".into()), None);
    };
    let Some(result_node) = manifest.object(entry, vocab.mf_result) else {
        return (Status::Error("entry has no mf:result".into()), None);
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
        let (actual, compiled) = match validate_compiled(manifest, manifest, store, vocab) {
            Ok((r, _)) if expects_failure => {
                return (
                    Status::Mismatch(format!(
                        "expected a failure, but validation produced {} result(s)",
                        r.results.len()
                    )),
                    None,
                );
            }
            Ok(pair) => pair,
            // A failure was the expected outcome.
            Err(_) if expects_failure => return (Status::Pass, None),
            Err(e) => return (Status::Error(format!("validation failed: {e}")), None),
        };
        let iso = compare_as_graphs(
            result_node,
            manifest,
            &actual,
            manifest,
            &compiled,
            store,
            vocab,
            &expected.disallowed,
        );
        let status = compare(
            &expected.report,
            expected.conforms,
            &actual,
            manifest,
            manifest,
            store,
            vocab,
            &expected.disallowed,
        );
        return (status, Some(iso));
    }

    let (Some(data_path), Some(shapes_path)) = (data_path, shapes_path) else {
        return (
            Status::Error("mf:action is missing a data or shapes graph".into()),
            None,
        );
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
        Err(e) => return (Status::Error(format!("data graph: {e}")), None),
    };
    let shapes = if shapes_path == data_path {
        None
    } else {
        match load(&shapes_path, 101, store) {
            Ok(g) => Some(g),
            Err(e) => return (Status::Error(format!("shapes graph: {e}")), None),
        }
    };
    let shapes_ref = shapes.as_ref().unwrap_or(&data);

    let (actual, compiled) = match validate_compiled(&data, shapes_ref, store, vocab) {
        Ok((r, _)) if expects_failure => {
            return (
                Status::Mismatch(format!(
                    "expected a failure, but validation produced {} result(s)",
                    r.results.len()
                )),
                None,
            );
        }
        Ok(pair) => pair,
        Err(_) if expects_failure => return (Status::Pass, None),
        Err(e) => return (Status::Error(format!("validation failed: {e}")), None),
    };
    let iso = compare_as_graphs(
        result_node,
        manifest,
        &actual,
        shapes_ref,
        &compiled,
        store,
        vocab,
        &expected.disallowed,
    );
    let status = compare(
        &expected.report,
        expected.conforms,
        &actual,
        manifest,
        shapes_ref,
        store,
        vocab,
        &expected.disallowed,
    );
    (status, Some(iso))
}

/// Compiles and validates, keeping the compiled shapes: the report writer
/// renders `sh:resultPath` from them.
fn validate_compiled(
    data: &Graph,
    shapes_graph: &Graph,
    store: &mut TermStore,
    vocab: &Vocab,
) -> shacl::Result<(ValidationReport, shacl::shapes::Shapes)> {
    let compiled = shacl::shapes::Shapes::compile(shapes_graph, store, vocab)?;
    let report = shacl::validate::validate_in(data, &compiled, shapes_graph, store, vocab)?;
    Ok((report, compiled))
}

/// Whether `predicate` carries report *structure* — the edges to follow when
/// extracting an expected report from the manifest it is embedded in.
///
/// A report also references IRIs and data nodes: the focus node, the source
/// shape, the value. Following those would drag the shape's whole definition,
/// or the data graph, into the "expected report", which is not what the test
/// asserts.
fn is_report_structure(predicate: TermId, vocab: &Vocab) -> bool {
    [
        vocab.sh_result,
        vocab.sh_resultPath,
        vocab.sh_detail,
        vocab.sh_inversePath,
        vocab.sh_alternativePath,
        vocab.sh_zeroOrMorePath,
        vocab.sh_oneOrMorePath,
        vocab.sh_zeroOrOnePath,
        vocab.rdf_first,
        vocab.rdf_rest,
    ]
    .contains(&predicate)
}

/// The expected report as a graph: everything reachable from the `mf:result`
/// node through report structure.
fn expected_report_graph(
    result_node: TermId,
    manifest: &Graph,
    store: &TermStore,
    vocab: &Vocab,
) -> oxrdf::Graph {
    let mut out = oxrdf::Graph::new();
    let mut frontier = vec![result_node];
    let mut seen = vec![result_node];
    while let Some(node) = frontier.pop() {
        // Only blank nodes expand; see `is_report_structure`.
        if !store.is_blank(node) {
            continue;
        }
        let subject = match store.to_oxrdf(node) {
            oxrdf::Term::BlankNode(b) => oxrdf::NamedOrBlankNode::BlankNode(b),
            _ => continue,
        };
        for (p, o) in manifest.predicate_objects(node) {
            if is_report_structure(p, vocab) && !seen.contains(&o) {
                seen.push(o);
                frontier.push(o);
            }
            out.insert(&oxrdf::Triple::new(
                subject.clone(),
                oxrdf::NamedNode::new_unchecked(store.iri(p).unwrap_or_default()),
                store.to_oxrdf(o),
            ));
        }
    }
    out
}

/// Reduces an actual report to what the suite compares.
///
/// The SHACL test suite's own description (`testsuite/shacl10/index.html`)
/// lists the predicates an expected report uses and says every other triple
/// "needs to be removed from the actual graph prior to comparison". The one
/// exception it states is `sh:resultMessage`: those are removed too, unless
/// the expected graph contains a message with the same object, so that the
/// tests written to check message handling still can. SHACL 1.2's suite
/// carries no such text, but its expected reports use `sh:detail` and
/// `sh:conformanceDisallows`, so those join the list.
///
/// Done as a walk from the report node rather than a filter over triples, so
/// that dropping an edge — a message, a detail — drops what hung off it
/// rather than leaving it in the graph as an unconnected fragment.
fn normalise_for_suite(got: &oxrdf::Graph, expected: &oxrdf::Graph) -> oxrdf::Graph {
    use oxrdf::{NamedNodeRef, TermRef};
    const SH: &str = "http://www.w3.org/ns/shacl#";
    const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
    let sh = |l: &str| oxrdf::NamedNode::new_unchecked(format!("{SH}{l}"));
    let rdf = |l: &str| oxrdf::NamedNode::new_unchecked(format!("{RDF}{l}"));

    let compared: Vec<oxrdf::NamedNode> = vec![
        rdf("type"),
        sh("result"),
        sh("conforms"),
        sh("conformanceDisallows"),
        sh("focusNode"),
        sh("resultPath"),
        sh("resultSeverity"),
        sh("sourceConstraint"),
        sh("sourceConstraintComponent"),
        sh("sourceShape"),
        sh("value"),
        sh("inversePath"),
        sh("alternativePath"),
        sh("zeroOrMorePath"),
        sh("oneOrMorePath"),
        sh("zeroOrOnePath"),
        rdf("first"),
        rdf("rest"),
    ];
    let message = sh("resultMessage");
    let detail = sh("detail");
    let expected_messages: Vec<oxrdf::Term> = expected
        .triples_for_predicate(message.as_ref())
        .map(|t| t.object.into_owned())
        .collect();
    let expected_has_detail = expected
        .triples_for_predicate(detail.as_ref())
        .next()
        .is_some();

    let keep = |p: NamedNodeRef<'_>, o: TermRef<'_>| -> bool {
        if compared.iter().any(|c| c.as_ref() == p) {
            return true;
        }
        if p == message.as_ref() {
            return expected_messages.iter().any(|m| m.as_ref() == o);
        }
        if p == detail.as_ref() {
            return expected_has_detail;
        }
        false
    };

    let mut out = oxrdf::Graph::new();
    let report_type = sh("ValidationReport");
    let mut frontier: Vec<oxrdf::NamedOrBlankNode> = got
        .triples_for_object(TermRef::NamedNode(report_type.as_ref()))
        .map(|t| t.subject.into_owned())
        .collect();
    let mut seen: Vec<oxrdf::NamedOrBlankNode> = frontier.clone();
    while let Some(node) = frontier.pop() {
        for t in got.triples_for_subject(node.as_ref()) {
            if !keep(t.predicate, t.object) {
                continue;
            }
            out.insert(&t.into_owned());
            if let TermRef::BlankNode(b) = t.object {
                let next = oxrdf::NamedOrBlankNode::BlankNode(b.into_owned());
                if !seen.contains(&next) {
                    seen.push(next.clone());
                    frontier.push(next);
                }
            }
        }
    }
    out
}

/// The comparison the suite itself defines: the report graphs, up to
/// blank-node isomorphism, after [`normalise_for_suite`].
///
/// Stricter than [`compare`], which matches result by result. It catches
/// what that cannot: a report whose *shape* is wrong — a compound path
/// shared between results, say — while every result in it is individually
/// right.
#[allow(clippy::too_many_arguments)]
fn compare_as_graphs(
    result_node: TermId,
    manifest: &Graph,
    actual: &ValidationReport,
    shapes_graph: &Graph,
    compiled: &shacl::shapes::Shapes,
    store: &TermStore,
    vocab: &Vocab,
    disallowed: &[TermId],
) -> Result<(), String> {
    use oxrdf::dataset::CanonicalizationAlgorithm;
    let mut expected = expected_report_graph(result_node, manifest, store, vocab);
    let raw = actual.to_oxrdf(store, vocab, shapes_graph, compiled, disallowed);
    let mut got = normalise_for_suite(&raw, &expected);
    expected.canonicalize(CanonicalizationAlgorithm::Unstable);
    got.canonicalize(CanonicalizationAlgorithm::Unstable);
    if expected == got {
        return Ok(());
    }
    let mut missing: Vec<String> = expected
        .iter()
        .filter(|t| !got.contains(*t))
        .map(|t| t.to_string())
        .collect();
    let mut extra: Vec<String> = got
        .iter()
        .filter(|t| !expected.contains(*t))
        .map(|t| t.to_string())
        .collect();
    missing.sort();
    extra.sort();
    // The first of each by default; every line under SHACL_TEST_ISO_FULL,
    // since the first line of a canonical diff is often a relabelled blank
    // node and the real difference is further down.
    let shown = if std::env::var_os("SHACL_TEST_ISO_FULL").is_some() {
        usize::MAX
    } else {
        1
    };
    let list = |v: &[String]| -> String {
        if v.is_empty() {
            return "-".into();
        }
        v.iter()
            .take(shown)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n               ")
    };
    Err(format!(
        "not isomorphic: {} triple(s) expected but absent, {} present but unexpected\n      missing: {}\n      extra:   {}",
        missing.len(),
        extra.len(),
        list(&missing),
        list(&extra),
    ))
}

// --------------------------------------------------------------- comparison

/// Compares the two reports.
///
/// `expected_graph` and `actual_graph` are usually the same document, but when
/// a test names an external shapes graph they differ: the expected report's
/// `sh:resultPath` blank nodes live in the manifest, while the actual report's
/// live in the shapes graph. Each side must compile its paths against the graph
/// that actually contains them.
#[allow(clippy::too_many_arguments)]
fn compare(
    expected: &ValidationReport,
    expected_conforms: bool,
    actual: &ValidationReport,
    expected_graph: &Graph,
    actual_graph: &Graph,
    store: &TermStore,
    vocab: &Vocab,
    disallowed: &[TermId],
) -> Status {
    // The `sh:conforms` the test *wrote*, not one recomputed from its results
    // under this engine's own rule. Recomputing it is how the engine's default
    // — only `sh:Violation` blocking — went unnoticed against a suite whose
    // reports say a lone `sh:Warning` does not conform.
    if expected_conforms != actual.conforms(disallowed, vocab) {
        return Status::Mismatch(format!(
            "conforms: expected {expected_conforms}, got {}",
            actual.conforms(disallowed, vocab)
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
            "no manifests found under {} — is the test suite present?",
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
    let mut total_iso = 0usize;
    let mut total_iso_pass = 0usize;
    println!();
    for (suite, outcomes) in &per_suite {
        let pass = outcomes
            .iter()
            .filter(|o| matches!(o.status, Status::Pass))
            .count();
        let errors = outcomes
            .iter()
            .filter(|o| matches!(o.status, Status::Error(_)))
            .count();
        // The suite's own comparison, over the tests that produce a report.
        let iso_total = outcomes.iter().filter(|o| o.iso.is_some()).count();
        let iso_pass = outcomes
            .iter()
            .filter(|o| matches!(o.iso, Some(Ok(()))))
            .count();
        total += outcomes.len();
        total_pass += pass;
        total_iso += iso_total;
        total_iso_pass += iso_pass;
        println!(
            "  {suite:<10} {pass:>3}/{:<3} passing  ({errors} could not run)   as graphs: {iso_pass:>3}/{iso_total:<3}",
            outcomes.len()
        );
    }
    println!(
        "  {:<10} {total_pass:>3}/{total:<3} passing                        as graphs: {total_iso_pass:>3}/{total_iso:<3}",
        "TOTAL"
    );

    if verbose {
        println!("\n  failures:");
        for outcomes in per_suite.values() {
            for o in outcomes {
                match &o.status {
                    Status::Pass => {}
                    Status::Mismatch(m) => println!("    [diff] {}\n      {m}", o.name),
                    Status::Error(e) => println!("    [err ] {}: {e}", o.name),
                }
                if let (Status::Pass, Some(Err(m))) = (&o.status, &o.iso) {
                    println!("    [iso ] {}\n      {m}", o.name);
                }
            }
        }
    }

    // The suites must at least be discovered; the pass count is tracked by
    // `progress` below rather than asserted here.
    assert!(total > 0, "no tests were discovered");
}

#[test]
fn file_iris_round_trip_on_both_platform_shapes() {
    // Both forms are checked on every platform. Each host only ever produces
    // one of them, so testing whichever the runner happens to make would have
    // missed the bug this guards: a Unix path losing its leading slash turned
    // every manifest include into an unresolvable relative path, and the suite
    // reported nothing rather than failing.
    assert_eq!(
        iri_to_path("file:///home/runner/work/tests/manifest.ttl"),
        Some(PathBuf::from("/home/runner/work/tests/manifest.ttl"))
    );
    assert_eq!(
        iri_to_path("file:///C:/repos/tests/manifest.ttl"),
        Some(PathBuf::from("C:/repos/tests/manifest.ttl"))
    );
    assert_eq!(
        iri_to_path("file:///tmp/a%20b/manifest.ttl"),
        Some(PathBuf::from("/tmp/a b/manifest.ttl")),
        "percent escapes are decoded"
    );
    assert_eq!(iri_to_path("http://example.org/x.ttl"), None);
}

/// Guards against regressions: the pass count must never drop below this.
/// Raise it as the engine gains coverage.
const BASELINE_PASSING: usize = 418;

/// The eight of 426 that do not pass, and why. Named here so the gap is
/// legible without setting `SHACL_TEST_VERBOSE=1` and reading the output.
///
/// Every test still runs; this filters nothing. `progress` asserts the set of
/// failures matches this list exactly, so a name cannot go stale: fixing one
/// means deleting its line and raising `BASELINE_PASSING`, and a newly broken
/// test is named in the failure rather than just shrinking a number.
///
/// - `sparql/node/prefixes-002` — a global `sh:ShapesGraph` prefix declaration
///   is not brought into scope, so the query fails to parse.
/// - `sparql/pre-binding/pre-binding-006` — a pre-binding that should be
///   rejected as unsupported is accepted instead.
/// - `sparql/pre-binding/unsupported-sparql-004` — likewise.
/// - `sparql/property/property-sparqlExpr-001` — `sh:sparqlExpr` is not
///   implemented.
/// - `sparql/property/property-select-001` — result mismatch on a SELECT-based
///   constraint.
/// - `core/misc/severity-003` — a per-statement `{| sh:severity … |}`
///   annotation does not override the shape's severity.
/// - `core/property/reifierShape-002` — a reifier shape case that still
///   reports conforming.
/// - `node-expr/shnex-sparql/seconds-example` — oxigraph returns `"0"` where
///   the spec's expected output is `"00"`; a lexical form, not a value,
///   difference.
const KNOWN_FAILURES: &[&str] = &[
    "sparql/node/prefixes-002",
    "sparql/pre-binding/pre-binding-006",
    "sparql/pre-binding/unsupported-sparql-004",
    "sparql/property/property-sparqlExpr-001",
    "sparql/property/property-select-001",
    "core/misc/severity-003",
    "core/property/reifierShape-002",
    "node-expr/shnex-sparql/seconds-example",
];

/// The README's conformance figures must match the suite.
///
/// They have drifted twice: prose is not checked by anything, so it goes stale
/// the moment a test starts passing. `KNOWN_FAILURES` fixed the list; this
/// fixes the numbers, which are what a reader actually takes away.
#[test]
fn the_readme_states_the_real_numbers() {
    let readme =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../README.md"))
            .expect("README.md should be readable");

    let total = BASELINE_PASSING + KNOWN_FAILURES.len();
    let passing = format!("**{BASELINE_PASSING} of {total}** tests pass");
    assert!(
        readme.contains(&passing),
        "README should say {passing:?}; update it when the count moves"
    );

    // The count of remaining failures is spelled out in words, which is the
    // part that went stale last time.
    let remaining = match KNOWN_FAILURES.len() {
        8 => "eight",
        9 => "nine",
        10 => "ten",
        n => panic!("no spelling for {n} remaining failures; add one here"),
    };
    assert!(
        readme.contains(&format!("The {remaining} that remain")),
        "README should say {remaining:?} tests remain"
    );
}

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
    let pass = outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Pass))
        .count();
    assert!(
        pass >= BASELINE_PASSING,
        "regression: {pass} passing, baseline is {BASELINE_PASSING}"
    );

    // Name what is failing, not just how much. A count alone cannot tell a
    // fix from a swap: one test starting to pass while another breaks leaves
    // it unchanged.
    let mut failing: Vec<&str> = outcomes
        .iter()
        .filter(|o| !matches!(o.status, Status::Pass))
        .map(|o| o.name.as_str())
        .collect();
    failing.sort_unstable();
    let mut known: Vec<&str> = KNOWN_FAILURES.to_vec();
    known.sort_unstable();

    // The suite's own comparison must agree with the per-result one on every
    // test that passes. A report right result by result but wrong as a graph
    // — a path shared between results, a message the shape did not declare,
    // a `sh:conforms` computed by the engine's rule rather than the
    // specification's — is exactly the class of defect this harness used to
    // let through, and it is not allowed back.
    let right_but_misshapen: Vec<&str> = outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Pass) && matches!(o.iso, Some(Err(_))))
        .map(|o| o.name.as_str())
        .collect();
    assert!(
        right_but_misshapen.is_empty(),
        "these pass result by result but their report is not isomorphic to the          expected one — run with SHACL_TEST_VERBOSE=1 SHACL_TEST_ISO_FULL=1 to see          the difference: {right_but_misshapen:#?}"
    );

    let unexpected: Vec<_> = failing.iter().filter(|n| !known.contains(n)).collect();
    let fixed: Vec<_> = known.iter().filter(|n| !failing.contains(n)).collect();
    assert!(
        unexpected.is_empty(),
        "these tests broke and are not in KNOWN_FAILURES: {unexpected:#?}"
    );
    assert!(
        fixed.is_empty(),
        "these now pass — delete them from KNOWN_FAILURES and raise \
         BASELINE_PASSING: {fixed:#?}"
    );
}
