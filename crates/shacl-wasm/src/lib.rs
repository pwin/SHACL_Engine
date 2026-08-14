//! WebAssembly bindings for the `shacl` validation engine.
//!
//! The shape of this API is deliberately close to the Python bindings
//! (`crates/shacl-python`): a shapes graph is compiled once into a [`Validator`]
//! and then reused across data graphs, because compiling shapes is the
//! expensive half and a caller validating many documents against one shapes
//! graph should pay it once.
//!
//! Results come back as plain JS objects rather than only as a serialised RDF
//! report. A report *is* a graph and [`Report::to_turtle`] still hands the whole
//! thing over for querying or diffing, but a consumer that just wants to render
//! findings should not have to re-parse RDF to do it.
//!
//! Terms are rendered to the string a JS consumer actually wants: an IRI
//! without angle brackets, a blank node as `_:label`, a literal as its lexical
//! form. That matches the `.value` convention of the RDF/JS term interface, so
//! this drops into code already written against an RDF/JS SHACL engine.

use engine::inference;
use engine::model::loader::{self, RdfFormat};
use engine::model::{Graph, GraphBuilder, TermId, TermStore, Vocab};
use engine::report::ValidationReport;
use engine::rules;
use engine::shapes::Shapes;
use engine::validate;
use serde::Serialize;
use wasm_bindgen::prelude::*;

/// Improves panic reporting from a bare `RuntimeError: unreachable` to the real
/// Rust panic message and stack on `console.error`. Runs automatically on module
/// load; the alternative is a genuinely undebuggable failure mode.
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

fn err(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

/// One SHACL validation result, flattened for JS.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsResult {
    /// The node the finding is about.
    pub focus_node: String,
    /// `sh:resultPath`, absent for node-shape-level findings.
    pub path: Option<String>,
    /// The offending value. Absent for constraints that fault the focus node
    /// itself (`sh:minCount`, `sh:closed`).
    pub value: Option<String>,
    /// Full IRI, e.g. `http://www.w3.org/ns/shacl#Violation`.
    pub severity: String,
    /// The shape that produced the finding, when it is a named node.
    pub source_shape: Option<String>,
    /// `sh:sourceConstraintComponent`.
    pub component: String,
    /// Every `sh:resultMessage` on the result, joined by a space. Empty when
    /// the shape declares no message.
    pub message: String,
}

/// The outcome of one validation run.
#[wasm_bindgen]
pub struct Report {
    conforms: bool,
    results: Vec<JsResult>,
    turtle: String,
}

#[wasm_bindgen]
impl Report {
    /// True when nothing in the report has a conformance-blocking severity
    /// (`sh:Violation`).
    #[wasm_bindgen(getter)]
    pub fn conforms(&self) -> bool {
        self.conforms
    }

    /// Number of results, blocking or not.
    #[wasm_bindgen(getter)]
    pub fn length(&self) -> usize {
        self.results.len()
    }

    /// The results as an array of plain objects.
    #[wasm_bindgen(getter)]
    pub fn results(&self) -> Result<JsValue, JsValue> {
        serde_wasm_bindgen::to_value(&self.results).map_err(err)
    }

    /// The full SHACL validation report as a Turtle graph -- what a SHACL
    /// processor is formally meant to return, for querying or diffing.
    #[wasm_bindgen(js_name = toTurtle)]
    pub fn to_turtle(&self) -> String {
        self.turtle.clone()
    }
}

/// A compiled shapes graph, ready to validate data graphs against.
///
/// `store` is the term store as it stood once the shapes were compiled, and is
/// never mutated afterwards: each validation run clones it and grows its own
/// copy. That is the pattern `TermStore`'s own documentation describes, and
/// following it is what makes reuse actually pay.
///
/// Parsing each data graph into one shared store instead — which this did at
/// first — leaves every earlier run's terms behind, so the store grows without
/// bound and the advertised path (compile once, validate many) is precisely the
/// one that degrades. Measured on 100k instances / 400k triples: 23.7s sharing
/// one store against 6.5s cloning per run, and the gap widens with every run.
#[wasm_bindgen]
pub struct Validator {
    store: TermStore,
    vocab: Vocab,
    shapes: Shapes,
    shapes_graph: Graph,
}

#[wasm_bindgen]
impl Validator {
    /// Compiles a shapes graph given as Turtle.
    #[wasm_bindgen(js_name = fromTurtle)]
    pub fn from_turtle(text: &str, base: Option<String>) -> Result<Validator, JsValue> {
        Validator::from_text(text, "turtle", base)
    }

    /// Compiles a shapes graph in any format the engine can read: `turtle`,
    /// `ntriples`, `nquads`, `trig`, `rdfxml`, `jsonld`.
    #[wasm_bindgen(js_name = fromText)]
    pub fn from_text(text: &str, format: &str, base: Option<String>) -> Result<Validator, JsValue> {
        let base = base.unwrap_or_else(default_base);
        let mut store = TermStore::new();
        let vocab = Vocab::new(&mut store);
        let shapes_graph = parse_graph(text, format, &base, 0, &mut store)?;
        let shapes = Shapes::compile(&shapes_graph, &store, &vocab).map_err(err)?;
        Ok(Validator {
            store,
            vocab,
            shapes,
            shapes_graph,
        })
    }

    /// How many shapes were compiled. Zero usually means the shapes graph
    /// parsed but declared nothing the engine recognises as a shape.
    #[wasm_bindgen(getter, js_name = shapeCount)]
    pub fn shape_count(&self) -> usize {
        self.shapes.len()
    }

    /// Validates a data graph given as Turtle.
    #[wasm_bindgen(js_name = validateTurtle)]
    pub fn validate_turtle(
        &self,
        text: &str,
        base: Option<String>,
        inference: Option<String>,
    ) -> Result<Report, JsValue> {
        self.validate_text(text, "turtle", base, inference)
    }

    /// Validates a data graph in any supported format.
    ///
    /// `inference` selects what is materialised before validating:
    ///
    /// - `"none"` (default)
    /// - `"rdfs"` — the RDFS closure, so a finding can depend on an entailed
    ///   `rdf:type` rather than only an asserted one
    /// - `"rules"` — SHACL-AF rules (`sh:rule`), one pass, as the
    ///   specification defines
    /// - `"rules-iterated"` — the same, repeated to a fixpoint, which a
    ///   transitive rule needs and the specification does not define
    #[wasm_bindgen(js_name = validateText)]
    pub fn validate_text(
        &self,
        text: &str,
        format: &str,
        base: Option<String>,
        inference: Option<String>,
    ) -> Result<Report, JsValue> {
        let base = base.unwrap_or_else(default_base);
        // This run's own copy of the store, so the data graph's terms are
        // discarded with it rather than accumulating in `self` -- see the note
        // on `Validator`. The clone is of the *shapes* store, which is small and
        // the same size on every run; the ids the compiled shapes hold stay
        // valid because a clone preserves them.
        let mut store = self.store.clone();

        // Scope 1: blank node labels in the data must not be merged with
        // identically-labelled ones in the shapes graph, which took scope 0.
        let data = parse_graph(text, format, &base, 1, &mut store)?;
        let data = match inference.as_deref().unwrap_or("none") {
            "none" => data,
            "rdfs" => inference::rdfs_closure(&data, &self.vocab).map_err(err)?,
            // SHACL-AF rules, spelled as an inference mode because that is
            // what they are from a caller's point of view: triples that exist
            // in the report's world and not in the input. `rules-iterated`
            // repeats to a fixpoint, which the specification does not define
            // but every transitive rule needs.
            "rules" => rules::apply(
                &data,
                &self.shapes,
                &self.shapes_graph,
                &mut store,
                &self.vocab,
            )
            .map_err(err)?,
            "rules-iterated" => rules::apply_iterated(
                &data,
                &self.shapes,
                &self.shapes_graph,
                &mut store,
                &self.vocab,
                MAX_RULE_ROUNDS,
            )
            .map_err(err)?,
            other => {
                return Err(JsValue::from_str(&format!(
                    "unknown inference {other:?}: expected \"none\", \"rdfs\", \
                     \"rules\" or \"rules-iterated\""
                )));
            }
        };

        let report = validate::validate_in(
            &data,
            &self.shapes,
            &self.shapes_graph,
            &mut store,
            &self.vocab,
        )
        .map_err(err)?;

        self.build_report(report, &store)
    }

    fn build_report(&self, report: ValidationReport, store: &TermStore) -> Result<Report, JsValue> {
        let disallowed = [self.vocab.sh_Violation];
        let conforms = report.conforms(&disallowed);
        let turtle = report
            .serialize(
                oxrdf_turtle(),
                store,
                &self.vocab,
                &self.shapes_graph,
                &disallowed,
            )
            .map_err(err)?;

        let results = report
            .results
            .iter()
            .map(|r| JsResult {
                focus_node: term(store, r.focus_node),
                path: r.path.map(|t| term(store, t)),
                value: r.value.map(|t| term(store, t)),
                severity: term(store, r.severity),
                source_shape: r.source_shape.map(|t| term(store, t)),
                component: term(store, r.source_constraint_component),
                message: r
                    .messages
                    .iter()
                    .map(|&m| term(store, m))
                    .collect::<Vec<_>>()
                    .join(" "),
            })
            .collect();

        Ok(Report {
            conforms,
            results,
            turtle,
        })
    }
}

/// Renders a term for JS: an IRI bare, a blank node as `_:label`, a literal as
/// its lexical form.
///
/// The `_:` on a blank node is deliberate, and the one place this departs from
/// RDF/JS's `.value` (which gives the bare label). Without it a blank node is
/// indistinguishable from a relative IRI or a literal in a field typed as a
/// plain string, and a consumer's only way to tell — needed at minimum to skip
/// blank-node focus nodes, which have no stable identity to report against —
/// would be to guess.
///
/// Blank nodes go through `to_oxrdf`, not `lexical_form`. `lexical_form`
/// answers for them with the *stored* label, which is scope-prefixed with a
/// colon (`1:b1`); emitting that would be doubly wrong — a colon is illegal in
/// a blank node label, and it would not match the `_:1_b1` the same node gets
/// in `to_turtle()`, leaving a consumer unable to correlate the two views of
/// one report. `to_oxrdf` applies the engine's own output convention
/// (`:` → `_`), which `TermStore::blank_node_from_output_label` is the pinned
/// inverse of, so a label from here resolves back to the node it came from.
fn term(store: &TermStore, id: TermId) -> String {
    if let Some(iri) = store.iri(id) {
        return iri.to_string();
    }
    // Must precede the `lexical_form` case, which would otherwise intercept
    // blank nodes and hand back the internal storage label.
    if store.is_blank(id) {
        return store.to_oxrdf(id).to_string();
    }
    if let Some(lex) = store.lexical_form(id) {
        return lex.to_string();
    }
    // Triple terms, which have no lexical form of their own.
    store.to_oxrdf(id).to_string()
}

/// One-shot validation, for a caller with a single data graph that gains
/// nothing from holding a compiled [`Validator`].
#[wasm_bindgen(js_name = validateTurtle)]
pub fn validate_turtle_once(
    shapes: &str,
    data: &str,
    base: Option<String>,
) -> Result<Report, JsValue> {
    let validator = Validator::from_turtle(shapes, base.clone())?;
    validator.validate_turtle(data, base, None)
}

/// Rounds `"rules-iterated"` allows before giving up. A rule set that mints a
/// fresh term each round has no fixpoint, and a browser tab is a worse place
/// than most to discover that.
const MAX_RULE_ROUNDS: usize = 10;

fn default_base() -> String {
    // Turtle needs *some* base to resolve relative IRIs against. This one is
    // deliberately obviously-synthetic, so a relative IRI that was never meant
    // to be relative shows up as such rather than silently resolving against
    // something plausible.
    "http://shacl-wasm.invalid/".to_string()
}

fn parse_graph(
    text: &str,
    format: &str,
    base: &str,
    scope: u32,
    store: &mut TermStore,
) -> Result<Graph, JsValue> {
    let format = match format.to_ascii_lowercase().as_str() {
        "turtle" | "ttl" => RdfFormat::Turtle,
        "ntriples" | "nt" => RdfFormat::NTriples,
        "nquads" | "nq" => RdfFormat::NQuads,
        "trig" => RdfFormat::TriG,
        "rdfxml" | "xml" | "rdf" => RdfFormat::RdfXml,
        "jsonld" | "json-ld" => RdfFormat::JsonLd {
            profile: Default::default(),
        },
        other => {
            return Err(JsValue::from_str(&format!(
                "unknown RDF format {other:?}: expected turtle, ntriples, nquads, trig, rdfxml or jsonld"
            )));
        }
    };
    let mut builder = GraphBuilder::new();
    loader::parse_str(text, format, base, scope, store, &mut builder).map_err(err)?;
    Ok(builder.build())
}

fn oxrdf_turtle() -> engine::report::RdfFormat {
    engine::report::RdfFormat::Turtle
}
