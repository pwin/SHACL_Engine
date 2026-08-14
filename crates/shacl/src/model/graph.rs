//! An immutable, indexed RDF graph.
//!
//! Triples are held in three fully-sorted permutations rather than in hash maps
//! of adjacency lists. Every lookup SHACL needs is a prefix range on one of
//! them, resolved with two binary searches and then walked as a contiguous
//! slice — which keeps path evaluation, the engine's hottest loop, free of both
//! pointer chasing and allocation.

use super::term::TermId;

/// A triple of interned terms, stored in whatever component order its index
/// implies.
type Row = [TermId; 3];

/// Accumulates triples before they are sorted into a [`Graph`].
#[derive(Debug, Default)]
pub struct GraphBuilder {
    rows: Vec<Row>,
}

impl GraphBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, s: TermId, p: TermId, o: TermId) {
        self.rows.push([s, p, o]);
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Sorts and deduplicates into the three index permutations.
    pub fn build(mut self) -> Graph {
        self.rows.sort_unstable();
        self.rows.dedup();

        let spo = self.rows;
        let mut pos: Vec<Row> = spo.iter().map(|&[s, p, o]| [p, o, s]).collect();
        pos.sort_unstable();
        let mut osp: Vec<Row> = spo.iter().map(|&[s, p, o]| [o, s, p]).collect();
        osp.sort_unstable();

        Graph { spo, pos, osp }
    }
}

/// An immutable RDF graph indexed for the access patterns SHACL uses.
///
/// `Clone` copies all three permutations — three words per triple — rather
/// than rebuilding and re-sorting them, which is what the rules engine wants
/// when it hands back a graph nothing was added to.
#[derive(Debug, Default, Clone)]
pub struct Graph {
    /// Sorted by `(subject, predicate, object)`.
    spo: Vec<Row>,
    /// Sorted by `(predicate, object, subject)`.
    pos: Vec<Row>,
    /// Sorted by `(object, subject, predicate)`.
    osp: Vec<Row>,
}

impl Graph {
    pub fn len(&self) -> usize {
        self.spo.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spo.is_empty()
    }

    /// Objects of `(s, p, ?)` — the predicate-path hot path.
    #[inline]
    pub fn objects(&self, s: TermId, p: TermId) -> impl Iterator<Item = TermId> + '_ {
        prefix2(&self.spo, s, p).iter().map(|r| r[2])
    }

    /// The first object of `(s, p, ?)`, for functional properties.
    #[inline]
    pub fn object(&self, s: TermId, p: TermId) -> Option<TermId> {
        prefix2(&self.spo, s, p).first().map(|r| r[2])
    }

    /// Subjects of `(?, p, o)` — inverse paths and `sh:targetSubjectsOf`.
    #[inline]
    pub fn subjects(&self, p: TermId, o: TermId) -> impl Iterator<Item = TermId> + '_ {
        prefix2(&self.pos, p, o).iter().map(|r| r[2])
    }

    /// Every subject appearing with predicate `p`, in sorted order with
    /// duplicates retained.
    #[inline]
    pub fn subjects_of(&self, p: TermId) -> impl Iterator<Item = TermId> + '_ {
        prefix1(&self.pos, p).iter().map(|r| r[2])
    }

    /// Every object appearing with predicate `p`.
    #[inline]
    pub fn objects_of(&self, p: TermId) -> impl Iterator<Item = TermId> + '_ {
        prefix1(&self.pos, p).iter().map(|r| r[1])
    }

    /// All `(predicate, object)` pairs of `s` — used by `sh:closed`.
    #[inline]
    pub fn predicate_objects(&self, s: TermId) -> impl Iterator<Item = (TermId, TermId)> + '_ {
        prefix1(&self.spo, s).iter().map(|r| (r[1], r[2]))
    }

    /// All `(subject, predicate)` pairs pointing at `o`.
    #[inline]
    pub fn subject_predicates(&self, o: TermId) -> impl Iterator<Item = (TermId, TermId)> + '_ {
        prefix1(&self.osp, o).iter().map(|r| (r[1], r[2]))
    }

    #[inline]
    pub fn contains(&self, s: TermId, p: TermId, o: TermId) -> bool {
        self.spo.binary_search(&[s, p, o]).is_ok()
    }

    /// True if `s` appears as a subject of any triple.
    #[inline]
    pub fn has_subject(&self, s: TermId) -> bool {
        !prefix1(&self.spo, s).is_empty()
    }

    /// Every triple, in `(s, p, o)` order.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = Row> + '_ {
        self.spo.iter().copied()
    }
}

/// The contiguous slice of `rows` whose first component equals `a`.
#[inline]
fn prefix1(rows: &[Row], a: TermId) -> &[Row] {
    let lo = rows.partition_point(|r| r[0] < a);
    let hi = rows[lo..].partition_point(|r| r[0] == a) + lo;
    &rows[lo..hi]
}

/// The contiguous slice of `rows` whose first two components equal `(a, b)`.
#[inline]
fn prefix2(rows: &[Row], a: TermId, b: TermId) -> &[Row] {
    let lo = rows.partition_point(|r| (r[0], r[1]) < (a, b));
    let hi = rows[lo..].partition_point(|r| (r[0], r[1]) == (a, b)) + lo;
    &rows[lo..hi]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(n: u32) -> TermId {
        TermId(n)
    }

    /// `:a :p :b`, `:a :p :c`, `:a :q :b`, `:d :p :b`
    fn sample() -> Graph {
        let mut b = GraphBuilder::new();
        b.push(t(0), t(1), t(2));
        b.push(t(0), t(1), t(3));
        b.push(t(0), t(4), t(2));
        b.push(t(5), t(1), t(2));
        b.push(t(0), t(1), t(2)); // duplicate
        b.build()
    }

    #[test]
    fn deduplicates_on_build() {
        assert_eq!(sample().len(), 4);
    }

    #[test]
    fn objects_selects_by_subject_and_predicate() {
        let g = sample();
        let got: Vec<_> = g.objects(t(0), t(1)).collect();
        assert_eq!(got, vec![t(2), t(3)]);
        assert_eq!(g.objects(t(0), t(4)).collect::<Vec<_>>(), vec![t(2)]);
        assert_eq!(g.objects(t(9), t(1)).count(), 0, "absent subject");
    }

    #[test]
    fn subjects_selects_by_predicate_and_object() {
        let g = sample();
        let got: Vec<_> = g.subjects(t(1), t(2)).collect();
        assert_eq!(got, vec![t(0), t(5)]);
        assert_eq!(g.subjects(t(1), t(9)).count(), 0);
    }

    #[test]
    fn predicate_and_object_projections() {
        let g = sample();
        // `pos` is ordered by (predicate, object, subject), so subjects come
        // back grouped by object rather than sorted.
        assert_eq!(
            g.subjects_of(t(1)).collect::<Vec<_>>(),
            vec![t(0), t(5), t(0)]
        );
        assert_eq!(
            g.objects_of(t(1)).collect::<Vec<_>>(),
            vec![t(2), t(2), t(3)]
        );
        assert_eq!(g.objects_of(t(99)).count(), 0);
    }

    #[test]
    fn adjacency_views() {
        let g = sample();
        let mut po: Vec<_> = g.predicate_objects(t(0)).collect();
        po.sort();
        assert_eq!(po, vec![(t(1), t(2)), (t(1), t(3)), (t(4), t(2))]);

        let mut sp: Vec<_> = g.subject_predicates(t(3)).collect();
        sp.sort();
        assert_eq!(sp, vec![(t(0), t(1))]);
    }

    #[test]
    fn contains_and_has_subject() {
        let g = sample();
        assert!(g.contains(t(0), t(1), t(2)));
        assert!(!g.contains(t(0), t(1), t(9)));
        assert!(g.has_subject(t(5)));
        assert!(!g.has_subject(t(2)), "only ever an object");
    }

    #[test]
    fn empty_graph_answers_everything_emptily() {
        let g = GraphBuilder::new().build();
        assert!(g.is_empty());
        assert_eq!(g.objects(t(0), t(1)).count(), 0);
        assert!(!g.contains(t(0), t(1), t(2)));
    }
}
