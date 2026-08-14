//! A high-performance SHACL validation engine.
//!
//! The engine is built around three ideas:
//!
//! 1. **Interning.** Every term becomes a `u32` up front, so the inner loops
//!    compare integers rather than strings.
//! 2. **Flat indexes.** Graphs are three sorted arrays, not adjacency maps, so
//!    path evaluation walks contiguous memory.
//! 3. **Compile once, validate many.** A shapes graph is compiled into a flat
//!    IR before validation begins, so no shape lookup touches RDF again.

pub mod datatypes;
pub mod error;
pub mod inference;
pub mod model;
pub mod nodeexpr;
pub mod path;
pub mod report;
pub mod rules;
pub mod shapes;
pub mod sparql;
pub mod validate;
pub mod valueset;

pub use error::{Error, Result};
pub use model::{Graph, GraphBuilder, TermId, TermStore, Vocab};
