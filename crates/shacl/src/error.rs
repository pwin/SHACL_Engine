//! Engine-wide error type.

use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A file could not be read or written.
    Io(String),
    /// An RDF document could not be parsed.
    Parse(String),
    /// The shapes graph is not well-formed, e.g. a malformed property path.
    Shape(String),
    /// A SPARQL query could not be parsed or evaluated.
    Sparql(String),
    /// Materialising entailed triples would cost more than was allowed.
    Inference(String),
    /// Validation nested deeper than the engine allows. Reported rather than
    /// truncated: a report quietly missing results would be worse than none.
    Recursion(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(m) => write!(f, "I/O error: {m}"),
            Self::Parse(m) => write!(f, "parse error: {m}"),
            Self::Shape(m) => write!(f, "invalid shapes graph: {m}"),
            Self::Sparql(m) => write!(f, "SPARQL error: {m}"),
            Self::Recursion(m) => write!(f, "recursion limit exceeded: {m}"),
            Self::Inference(m) => write!(f, "inference limit exceeded: {m}"),
        }
    }
}

impl std::error::Error for Error {}
