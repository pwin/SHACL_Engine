//! Python bindings for the SHACL engine.
//!
//! The API mirrors the engine's own split between compiling a shapes graph and
//! validating data against it, because that is where the leverage is: shapes
//! are fixed while data changes, and compiling once is most of the point.
//!
//! ```python
//! import shacl
//! shapes = shacl.Shapes.from_file("shapes.ttl")
//! report = shapes.validate_file("data.ttl")
//! if not report.conforms:
//!     for r in report.results:
//!         print(r.focus_node, r.component, r.value)
//! ```

use pyo3::exceptions::{PyIOError, PyValueError};
use pyo3::prelude::*;
use std::path::PathBuf;

use engine::model::{Graph, TermStore, Vocab, loader, scope};

/// Translates an engine error into the closest Python exception.
fn to_py_err(e: engine::Error) -> PyErr {
    match e {
        engine::Error::Io(m) => PyIOError::new_err(m),
        other => PyValueError::new_err(other.to_string()),
    }
}

/// One validation result.
#[pyclass(frozen, get_all, skip_from_py_object)]
#[derive(Clone)]
pub struct Result {
    /// The node the violation is about, in N-Triples syntax.
    pub focus_node: String,
    /// The offending value, if the constraint named one.
    pub value: Option<String>,
    /// The `sh:resultPath`, if the shape had one.
    pub path: Option<String>,
    /// The shape that raised it.
    pub source_shape: Option<String>,
    /// Local name of the constraint component, e.g. `MinCountConstraintComponent`.
    pub component: String,
    /// The same as a full IRI, which is the only way to tell two custom
    /// constraint components apart when their local names collide.
    pub component_iri: String,
    /// Local name of the severity: `Violation`, `Warning` or `Info`.
    pub severity: String,
    /// The severity as a full IRI.
    pub severity_iri: String,
    /// The first `sh:message`, or `None`. Kept for convenience; `messages`
    /// holds all of them, which matters when a shape carries one per language.
    pub message: Option<String>,
    /// Every `sh:message` on the result, in the order the shape declared them.
    pub messages: Vec<String>,
}

#[pymethods]
impl Result {
    fn __repr__(&self) -> String {
        format!(
            "<Result {} on {} value={}>",
            self.component,
            self.focus_node,
            self.value.as_deref().unwrap_or("-")
        )
    }
}

/// The outcome of a validation run.
#[pyclass(frozen)]
pub struct Report {
    /// True when nothing of blocking severity was reported.
    #[pyo3(get)]
    pub conforms: bool,
    #[pyo3(get)]
    pub results: Vec<Result>,
    /// The report graph. Kept rather than a fixed serialisation so any format
    /// can be produced on demand; it holds terms rather than handles into the
    /// term store, so it outlives the call that built it.
    pub graph: engine::report::OxGraph,
}

#[pymethods]
impl Report {
    /// The SHACL validation report as an RDF graph, serialised.
    ///
    /// This is the specification's own artefact — a `sh:ValidationReport` —
    /// and is what to hand to another RDF tool. The attributes above are a
    /// convenience for reading it from Python.
    ///
    /// Accepts `turtle`, `ntriples`, `rdfxml`, `jsonld` and `n3`, with the
    /// usual aliases.
    #[pyo3(signature = (format = "turtle"))]
    fn serialize(&self, format: &str) -> PyResult<String> {
        let fmt = match format.to_ascii_lowercase().as_str() {
            "turtle" | "ttl" => engine::report::RdfFormat::Turtle,
            "ntriples" | "n-triples" | "nt" => engine::report::RdfFormat::NTriples,
            "rdfxml" | "rdf/xml" | "xml" | "rdf" => engine::report::RdfFormat::RdfXml,
            "jsonld" | "json-ld" | "json" => engine::report::RdfFormat::JsonLd {
                profile: engine::report::JsonLdProfileSet::empty(),
            },
            "n3" => engine::report::RdfFormat::N3,
            other => {
                return Err(PyValueError::new_err(format!(
                    "unsupported format {other:?}; use one of \
                     turtle, ntriples, rdfxml, jsonld, n3"
                )));
            }
        };
        engine::report::serialize_graph(&self.graph, fmt).map_err(to_py_err)
    }

    /// The report as Turtle, equivalent to `serialize("turtle")`.
    #[getter]
    fn turtle(&self) -> PyResult<String> {
        self.serialize("turtle")
    }

    fn __repr__(&self) -> String {
        format!(
            "<Report conforms={} results={}>",
            self.conforms,
            self.results.len()
        )
    }

    fn __bool__(&self) -> bool {
        self.conforms
    }

    fn __len__(&self) -> usize {
        self.results.len()
    }
}

/// A compiled shapes graph, ready to validate against.
///
/// Every field is immutable after construction, so one instance can be shared
/// across threads without a lock. Validation needs a *mutable* term store —
/// the data graph has terms to intern — so each run clones this one and grows
/// its own copy. The clone is cheap because a store holding only a shapes
/// graph has a few hundred terms in it, and it is what keeps the ids the
/// compiled shapes hold valid: ids are only comparable within one store.
///
/// It also bounds memory. Sharing one store across runs meant every data graph
/// ever validated stayed interned in it.
#[pyclass(frozen)]
pub struct Shapes {
    store: TermStore,
    vocab: Vocab,
    shapes_graph: Graph,
    compiled: engine::shapes::Shapes,
}

impl Shapes {
    fn build(load: impl FnOnce(&mut TermStore) -> engine::Result<Graph>) -> PyResult<Self> {
        let mut store = TermStore::new();
        let vocab = Vocab::new(&mut store);
        let shapes_graph = load(&mut store).map_err(to_py_err)?;
        let compiled =
            engine::shapes::Shapes::compile(&shapes_graph, &store, &vocab).map_err(to_py_err)?;
        Ok(Self {
            store,
            vocab,
            shapes_graph,
            compiled,
        })
    }
}

#[pymethods]
impl Shapes {
    /// Compiles a shapes graph from a file. The syntax is taken from the
    /// extension: `.ttl`, `.nt`, `.nq`, `.trig`, `.rdf`.
    #[staticmethod]
    fn from_file(py: Python<'_>, path: PathBuf) -> PyResult<Self> {
        py.detach(|| Self::build(|store| loader::load_file(&path, scope::SHAPES, store)))
    }

    /// Compiles a shapes graph from Turtle held in memory.
    ///
    /// `base` resolves relative IRIs, including the `<>` that a self-describing
    /// document uses to refer to itself.
    #[staticmethod]
    #[pyo3(signature = (text, base = "http://example.org/shapes"))]
    fn from_turtle(py: Python<'_>, text: &str, base: &str) -> PyResult<Self> {
        py.detach(|| {
            Self::build(|store| {
                let mut b = engine::model::GraphBuilder::new();
                loader::parse_str(
                    text,
                    oxrdf_format_turtle(),
                    base,
                    scope::SHAPES,
                    store,
                    &mut b,
                )?;
                Ok(b.build())
            })
        })
    }

    /// Compiles a shapes graph from text in any supported format.
    ///
    /// The general form of [`Shapes::from_turtle`], for callers holding a
    /// document that is not Turtle — an `rdflib.Graph` serialised to whatever
    /// is cheapest, say, rather than forced through Turtle first.
    #[staticmethod]
    #[pyo3(signature = (text, format = "turtle", base = "http://example.org/shapes"))]
    fn from_text(py: Python<'_>, text: &str, format: &str, base: &str) -> PyResult<Self> {
        let fmt = format_by_name(format)?;
        py.detach(|| {
            Self::build(|store| {
                let mut b = engine::model::GraphBuilder::new();
                loader::parse_str(text, fmt, base, scope::SHAPES, store, &mut b)?;
                Ok(b.build())
            })
        })
    }

    /// Validates a data graph held in memory, in any supported format.
    #[pyo3(signature = (text, format = "turtle", base = "http://example.org/data"))]
    fn validate_text(
        &self,
        py: Python<'_>,
        text: &str,
        format: &str,
        base: &str,
    ) -> PyResult<Report> {
        let fmt = format_by_name(format)?;
        py.detach(|| {
            let mut store = self.store.clone();
            let mut b = engine::model::GraphBuilder::new();
            loader::parse_str(text, fmt, base, scope::DATA, &mut store, &mut b)
                .map_err(to_py_err)?;
            let data = b.build();
            self.run(&mut store, &data)
        })
    }

    /// Validates a data graph read from a file.
    fn validate_file(&self, py: Python<'_>, path: PathBuf) -> PyResult<Report> {
        py.detach(|| {
            let mut store = self.store.clone();
            let data = loader::load_file(&path, scope::DATA, &mut store).map_err(to_py_err)?;
            self.run(&mut store, &data)
        })
    }

    /// Validates a data graph held in memory as Turtle.
    #[pyo3(signature = (text, base = "http://example.org/data"))]
    fn validate_turtle(&self, py: Python<'_>, text: &str, base: &str) -> PyResult<Report> {
        py.detach(|| {
            let mut store = self.store.clone();
            let mut b = engine::model::GraphBuilder::new();
            loader::parse_str(
                text,
                oxrdf_format_turtle(),
                base,
                scope::DATA,
                &mut store,
                &mut b,
            )
            .map_err(to_py_err)?;
            let data = b.build();
            self.run(&mut store, &data)
        })
    }

    /// The number of compiled shapes.
    fn __len__(&self) -> usize {
        self.compiled.len()
    }

    fn __repr__(&self) -> String {
        format!("<Shapes {} compiled>", self.__len__())
    }
}

impl Shapes {
    fn run(&self, store: &mut TermStore, data: &Graph) -> PyResult<Report> {
        let vocab = &self.vocab;
        let report =
            engine::validate::validate_in(data, &self.compiled, &self.shapes_graph, store, vocab)
                .map_err(to_py_err)?;

        let local = |t: engine::TermId| -> String {
            store
                .iri(t)
                .and_then(|i| i.rsplit_once(['#', '/']).map(|(_, l)| l.to_string()))
                .unwrap_or_else(|| store.to_oxrdf(t).to_string())
        };
        let term = |t: engine::TermId| store.to_oxrdf(t).to_string();
        // The bare IRI, without the angle brackets `term` would add.
        let full = |t: engine::TermId| {
            store
                .iri(t)
                .map(str::to_string)
                .unwrap_or_else(|| store.to_oxrdf(t).to_string())
        };

        let results = report
            .results
            .iter()
            .map(|r| {
                let messages: Vec<String> = r
                    .messages
                    .iter()
                    .filter_map(|&m| store.lexical_form(m))
                    .map(str::to_string)
                    .collect();
                Result {
                    focus_node: term(r.focus_node),
                    value: r.value.map(term),
                    path: r.path.map(term),
                    source_shape: r.source_shape.map(term),
                    component: local(r.source_constraint_component),
                    component_iri: full(r.source_constraint_component),
                    severity: local(r.severity),
                    severity_iri: full(r.severity),
                    message: messages.first().cloned(),
                    messages,
                }
            })
            .collect();

        Ok(Report {
            conforms: report.conforms(&[vocab.sh_Violation]),
            results,
            graph: report.to_oxrdf(store, vocab, &self.shapes_graph, &[vocab.sh_Violation]),
        })
    }
}

fn oxrdf_format_turtle() -> engine::model::loader::RdfFormat {
    engine::model::loader::RdfFormat::Turtle
}

/// Resolves a format name to the parser for it.
///
/// The same names [`Report::serialize`] writes, so a document can be round
/// tripped through this module without a lookup table on the Python side.
fn format_by_name(name: &str) -> PyResult<engine::report::RdfFormat> {
    use engine::report::RdfFormat;
    Ok(match name.to_ascii_lowercase().as_str() {
        "turtle" | "ttl" => RdfFormat::Turtle,
        "ntriples" | "n-triples" | "nt" => RdfFormat::NTriples,
        "nquads" | "n-quads" | "nq" => RdfFormat::NQuads,
        "trig" => RdfFormat::TriG,
        "rdfxml" | "rdf/xml" | "xml" | "rdf" => RdfFormat::RdfXml,
        "jsonld" | "json-ld" | "json" => RdfFormat::JsonLd {
            profile: engine::report::JsonLdProfileSet::empty(),
        },
        "n3" => RdfFormat::N3,
        other => {
            return Err(PyValueError::new_err(format!(
                "unsupported format {other:?}; use one of \
                 turtle, ntriples, nquads, trig, rdfxml, jsonld, n3"
            )));
        }
    })
}

/// Validates `data_path` against `shapes_path` in one call.
///
/// Equivalent to compiling the shapes and validating once; use [`Shapes`]
/// directly when the same shapes are reused, which is the case worth
/// optimising for.
#[pyfunction]
#[pyo3(signature = (data_path, shapes_path = None))]
fn validate(py: Python<'_>, data_path: PathBuf, shapes_path: Option<PathBuf>) -> PyResult<Report> {
    // A self-describing document carries its own shapes, which is the
    // convention the CLI follows too.
    let shapes_path = shapes_path.unwrap_or_else(|| data_path.clone());
    let shapes = Shapes::from_file(py, shapes_path)?;
    shapes.validate_file(py, data_path)
}

/// `gil_used = false` declares the module safe for free-threaded CPython.
/// Without it, importing into a `python3.14t` build makes the interpreter
/// switch the GIL back on. It is sound here because `Shapes` is immutable and
/// each validation works on its own cloned store; it is a no-op on the usual
/// GIL-enabled builds.
#[pymodule(gil_used = false)]
fn shacl(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Shapes>()?;
    m.add_class::<Report>()?;
    m.add_class::<Result>()?;
    m.add_function(wrap_pyfunction!(validate, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
