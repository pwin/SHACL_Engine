//! The validation engine.
//!
//! Currently a placeholder that conforms unconditionally, so the W3C harness
//! can be built and verified against a known baseline before the shape compiler
//! and constraint components land.

use crate::error::Result;
use crate::model::{Graph, TermStore, Vocab};
use crate::report::ValidationReport;

/// Validates `data` against `shapes`.
pub fn validate(
    _data: &Graph,
    _shapes: &Graph,
    _store: &mut TermStore,
    _vocab: &Vocab,
) -> Result<ValidationReport> {
    Ok(ValidationReport::default())
}
