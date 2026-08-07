//! The focus→values relation, in CSR form.
//!
//! SHACL evaluates one shape against many focus nodes, and every one of them
//! traverses the same path. That makes the natural unit of work not a single
//! focus node but the whole relation between focus nodes and their value nodes
//! — a sparse boolean matrix, held here in compressed sparse row form.
//!
//! Keeping provenance is what distinguishes this from the usual reachability
//! formulation. Evaluating a path as a flat set of reachable nodes would be
//! enough for SPARQL, but SHACL needs to know *which focus node* reached each
//! value: `sh:minCount` is a per-row aggregate, and every violation has to name
//! its own focus node. So rows stay indexed by the originating focus node all
//! the way through a compound path.

use crate::model::TermId;

/// One focus node and the value nodes it reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row<'a> {
    pub focus: TermId,
    pub values: &'a [TermId],
}

impl Row<'_> {
    /// The row's cardinality, which is what `sh:minCount`/`sh:maxCount` test.
    #[inline]
    pub fn count(&self) -> usize {
        self.values.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// A focus→values relation stored as compressed sparse rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValueSets {
    focus: Vec<TermId>,
    /// Row boundaries; length is `focus.len() + 1`.
    offsets: Vec<u32>,
    values: Vec<TermId>,
}

impl ValueSets {
    /// The relation mapping each focus node to itself.
    ///
    /// This is what a node shape validates over: its value nodes *are* its
    /// focus nodes, so node and property shapes share one evaluation path.
    pub fn identity(focus: &[TermId]) -> Self {
        Self {
            focus: focus.to_vec(),
            offsets: (0..=focus.len() as u32).collect(),
            values: focus.to_vec(),
        }
    }

    pub fn builder() -> Builder {
        Builder::default()
    }

    /// The number of focus nodes, i.e. rows.
    #[inline]
    pub fn len(&self) -> usize {
        self.focus.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.focus.is_empty()
    }

    /// Total value nodes across all rows.
    #[inline]
    pub fn total_values(&self) -> usize {
        self.values.len()
    }

    #[inline]
    pub fn row(&self, i: usize) -> Row<'_> {
        let start = self.offsets[i] as usize;
        let end = self.offsets[i + 1] as usize;
        Row {
            focus: self.focus[i],
            values: &self.values[start..end],
        }
    }

    #[inline]
    pub fn rows(&self) -> impl Iterator<Item = Row<'_>> + '_ {
        (0..self.len()).map(|i| self.row(i))
    }

    /// Every value node in the relation, with duplicates across rows retained.
    ///
    /// Used where a constraint cares about values irrespective of which focus
    /// node produced them.
    #[inline]
    pub fn all_values(&self) -> &[TermId] {
        &self.values
    }

    /// The focus nodes, in row order.
    #[inline]
    pub fn focus_nodes(&self) -> &[TermId] {
        &self.focus
    }
}

/// Incremental builder for a [`ValueSets`].
#[derive(Debug, Default)]
pub struct Builder {
    focus: Vec<TermId>,
    offsets: Vec<u32>,
    values: Vec<TermId>,
    /// Where the row currently being built started in `values`.
    row_start: usize,
    open: bool,
}

impl Builder {
    pub fn with_capacity(rows: usize) -> Self {
        let mut offsets = Vec::with_capacity(rows + 1);
        offsets.push(0);
        Self {
            focus: Vec::with_capacity(rows),
            offsets,
            values: Vec::new(),
            row_start: 0,
            open: false,
        }
    }

    fn ensure_started(&mut self) {
        if self.offsets.is_empty() {
            self.offsets.push(0);
        }
    }

    /// Adds a complete row, deduplicating its values.
    pub fn push_row(&mut self, focus: TermId, values: &[TermId]) {
        self.start_row(focus);
        self.extend(values.iter().copied());
        self.end_row();
    }

    /// Begins a row; follow with [`Builder::push_value`] and [`Builder::end_row`].
    pub fn start_row(&mut self, focus: TermId) {
        debug_assert!(!self.open, "previous row was not ended");
        self.ensure_started();
        self.focus.push(focus);
        self.row_start = self.values.len();
        self.open = true;
    }

    /// Appends a value to the open row. Duplicates are dropped at [`Builder::end_row`].
    #[inline]
    pub fn push_value(&mut self, value: TermId) {
        debug_assert!(self.open, "no row is open");
        self.values.push(value);
    }

    #[inline]
    pub fn extend(&mut self, values: impl Iterator<Item = TermId>) {
        debug_assert!(self.open, "no row is open");
        self.values.extend(values);
    }

    /// Closes the open row, making its values a set.
    ///
    /// Value nodes are a set in SHACL, and reports are compared as multisets of
    /// results, so ordering within a row carries no meaning — sorting is both
    /// cheaper than order-preserving dedup and deterministic.
    pub fn end_row(&mut self) {
        debug_assert!(self.open, "no row is open");
        let row = &mut self.values[self.row_start..];
        row.sort_unstable();
        let kept = {
            let mut w = 0;
            for r in 0..row.len() {
                if r == 0 || row[r] != row[r - 1] {
                    row[w] = row[r];
                    w += 1;
                }
            }
            w
        };
        self.values.truncate(self.row_start + kept);
        self.offsets.push(self.values.len() as u32);
        self.open = false;
    }

    /// The values staged in the currently open row.
    #[inline]
    pub fn current_row(&self) -> &[TermId] {
        &self.values[self.row_start..]
    }

    pub fn finish(mut self) -> ValueSets {
        debug_assert!(!self.open, "a row is still open");
        self.ensure_started();
        ValueSets {
            focus: self.focus,
            offsets: self.offsets,
            values: self.values,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: u32) -> TermId {
        TermId(n)
    }

    #[test]
    fn identity_maps_each_focus_to_itself() {
        let vs = ValueSets::identity(&[t(1), t(2), t(3)]);
        assert_eq!(vs.len(), 3);
        for (i, row) in vs.rows().enumerate() {
            assert_eq!(row.values, &[row.focus]);
            assert_eq!(row.focus, t(i as u32 + 1));
        }
    }

    #[test]
    fn empty_relation_has_no_rows() {
        let vs = ValueSets::default();
        assert!(vs.is_empty());
        assert_eq!(vs.rows().count(), 0);
        assert_eq!(ValueSets::identity(&[]).len(), 0);
    }

    #[test]
    fn builds_rows_and_deduplicates_within_them() {
        let mut b = ValueSets::builder();
        b.push_row(t(1), &[t(10), t(11), t(10)]);
        b.push_row(t(2), &[]);
        b.push_row(t(3), &[t(11)]);
        let vs = b.finish();

        assert_eq!(vs.len(), 3);
        assert_eq!(vs.row(0).values, &[t(10), t(11)], "duplicate dropped");
        assert_eq!(vs.row(1).count(), 0);
        assert_eq!(vs.row(2).values, &[t(11)]);
        assert_eq!(vs.total_values(), 3);
    }

    #[test]
    fn duplicates_across_rows_are_kept() {
        // Two focus nodes may legitimately reach the same value; only
        // within-row duplication is meaningless.
        let mut b = ValueSets::builder();
        b.push_row(t(1), &[t(9)]);
        b.push_row(t(2), &[t(9)]);
        let vs = b.finish();
        assert_eq!(vs.all_values(), &[t(9), t(9)]);
    }

    #[test]
    fn incremental_row_building_matches_push_row() {
        let mut b = ValueSets::builder();
        b.start_row(t(1));
        b.push_value(t(5));
        b.extend([t(4), t(5)].into_iter());
        assert_eq!(b.current_row(), &[t(5), t(4), t(5)], "raw before dedup");
        b.end_row();
        let vs = b.finish();

        assert_eq!(vs.row(0).values, &[t(4), t(5)]);
    }

    #[test]
    fn row_counts_drive_cardinality() {
        let mut b = ValueSets::builder();
        b.push_row(t(1), &[t(10), t(11)]);
        b.push_row(t(2), &[]);
        let vs = b.finish();

        let counts: Vec<_> = vs.rows().map(|r| r.count()).collect();
        assert_eq!(counts, vec![2, 0]);
        assert!(vs.row(1).is_empty());
    }

    #[test]
    fn focus_nodes_are_exposed_in_row_order() {
        let mut b = ValueSets::builder();
        b.push_row(t(7), &[]);
        b.push_row(t(4), &[]);
        assert_eq!(b.finish().focus_nodes(), &[t(7), t(4)]);
    }
}
