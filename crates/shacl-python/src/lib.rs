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
    /// Local name of the severity: `Violation`, `Warning` or `Info`.
    pub severity: String,
    pub message: Option<String>,
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
#[pyclass(frozen, get_all)]
pub struct Report {
    /// True when nothing of blocking severity was reported.
    pub conforms: bool,
    pub results: Vec<Result>,
}

#[pymethods]
impl Report {
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

        let results = report
            .results
            .iter()
            .map(|r| Result {
                focus_node: term(r.focus_node),
                value: r.value.map(term),
                path: r.path.map(term),
                source_shape: r.source_shape.map(term),
                component: local(r.source_constraint_component),
                severity: local(r.severity),
                message: r
                    .messages
                    .first()
                    .and_then(|&m| store.lexical_form(m))
                    .map(str::to_string),
            })
            .collect();

        Ok(Report {
            conforms: report.conforms(&[vocab.sh_Violation]),
            results,
        })
    }
}

fn oxrdf_format_turtle() -> engine::model::loader::RdfFormat {
    engine::model::loader::RdfFormat::Turtle
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
