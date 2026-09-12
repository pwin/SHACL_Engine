//! An immutable, indexed RDF graph.
//!
//! Triples are held in three fully-sorted permutations rather than in hash maps
//! of adjacency lists. Every lookup SHACL needs is a prefix range on one of
//! them, walked as a contiguous slice — which keeps path evaluation, the
//! engine's hottest loop, free of both pointer chasing and allocation.
//!
//! Each permutation carries a first-level index: for every term that appears
//! in its leading position, the offset of that term's run of rows. Term ids are
//! dense integers, so this is a direct array load rather than a search. It
//! replaced a binary search over the whole permutation, and the difference is
//! not the arithmetic: on 690k triples the rows are 25 MB per permutation,
//! the deep probes of a search miss cache, and a validation run does ten of
//! them per focus node. Measured on that data, single-threaded, the validate
//! phase went from 0.170s to 0.134s — a fifth, not the half predicted, because
//! the term and string arrays are missed the same way and this fixes only the
//! rows. It is the first level of the trie the HOLOS store builds over the
//! same idea.

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

        let spo_starts = Starts::build(&spo);
        let pos_starts = Starts::build(&pos);
        let osp_starts = Starts::build(&osp);
        Graph {
            spo,
            pos,
            osp,
            spo_starts,
            pos_starts,
            osp_starts,
        }
    }
}

/// The first-level index over one sorted permutation.
///
/// `starts[a - base] .. starts[a - base + 1]` is the run of rows whose leading
/// component is `a`. Offsets are `u32` because rows are, and it is indexed by
/// term id, so a lookup is one load — the point of the structure.
///
/// `base` is the smallest leading id, so the array spans the ids the graph
/// actually uses rather than the whole store. That matters for a small graph
/// interned after a large one: the shapes graph, or every graph the rules
/// engine hands back, would otherwise carry an index the size of the data
/// graph's id space for a few hundred triples.
#[derive(Debug, Default, Clone)]
struct Starts {
    base: u32,
    starts: Vec<u32>,
}

impl Starts {
    fn build(rows: &[Row]) -> Self {
        let Some(first) = rows.first() else {
            return Self::default();
        };
        assert!(
            rows.len() < u32::MAX as usize,
            "a graph is indexed by u32 row offsets"
        );
        // Sorted, so the ends of the slice bound the leading component.
        let base = first[0].0;
        let last = rows[rows.len() - 1][0].0;
        let span = (last - base) as usize + 1;
        // One counting pass and one prefix sum: `starts[i + 1]` accumulates
        // the rows led by id `base + i`, so after the sum `starts[i]` is where
        // that id's run begins and `starts[i + 1]` where it ends.
        let mut starts = vec![0u32; span + 1];
        for r in rows {
            starts[(r[0].0 - base) as usize + 1] += 1;
        }
        for i in 1..starts.len() {
            starts[i] += starts[i - 1];
        }
        Self { base, starts }
    }

    /// The rows led by `a`, or none.
    #[inline]
    fn run<'r>(&self, rows: &'r [Row], a: TermId) -> &'r [Row] {
        let Some(i) = a.0.checked_sub(self.base).map(|i| i as usize) else {
            return &[];
        };
        if i + 1 >= self.starts.len() {
            return &[];
        }
        &rows[self.starts[i] as usize..self.starts[i + 1] as usize]
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
    /// Where each subject's rows begin in `spo`.
    spo_starts: Starts,
    /// Where each predicate's rows begin in `pos`.
    pos_starts: Starts,
    /// Where each object's rows begin in `osp`.
    osp_starts: Starts,
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
        prefix2(self.spo_starts.run(&self.spo, s), p)
            .iter()
            .map(|r| r[2])
    }

    /// The first object of `(s, p, ?)`, for functional properties.
    #[inline]
    pub fn object(&self, s: TermId, p: TermId) -> Option<TermId> {
        prefix2(self.spo_starts.run(&self.spo, s), p)
            .first()
            .map(|r| r[2])
    }

    /// Subjects of `(?, p, o)` — inverse paths and `sh:targetSubjectsOf`.
    #[inline]
    pub fn subjects(&self, p: TermId, o: TermId) -> impl Iterator<Item = TermId> + '_ {
        prefix2(self.pos_starts.run(&self.pos, p), o)
            .iter()
            .map(|r| r[2])
    }

    /// Every subject appearing with predicate `p`, in sorted order with
    /// duplicates retained.
    #[inline]
    pub fn subjects_of(&self, p: TermId) -> impl Iterator<Item = TermId> + '_ {
        self.pos_starts.run(&self.pos, p).iter().map(|r| r[2])
    }

    /// Every object appearing with predicate `p`.
    #[inline]
    pub fn objects_of(&self, p: TermId) -> impl Iterator<Item = TermId> + '_ {
        self.pos_starts.run(&self.pos, p).iter().map(|r| r[1])
    }

    /// All `(predicate, object)` pairs of `s` — used by `sh:closed`.
    #[inline]
    pub fn predicate_objects(&self, s: TermId) -> impl Iterator<Item = (TermId, TermId)> + '_ {
        self.spo_starts
            .run(&self.spo, s)
            .iter()
            .map(|r| (r[1], r[2]))
    }

    /// All `(subject, predicate)` pairs pointing at `o`.
    #[inline]
    pub fn subject_predicates(&self, o: TermId) -> impl Iterator<Item = (TermId, TermId)> + '_ {
        self.osp_starts
            .run(&self.osp, o)
            .iter()
            .map(|r| (r[1], r[2]))
    }

    #[inline]
    pub fn contains(&self, s: TermId, p: TermId, o: TermId) -> bool {
        self.spo_starts
            .run(&self.spo, s)
            .binary_search(&[s, p, o])
            .is_ok()
    }

    /// True if `s` appears as a subject of any triple.
    #[inline]
    pub fn has_subject(&self, s: TermId) -> bool {
        !self.spo_starts.run(&self.spo, s).is_empty()
    }

    /// Every triple, in `(s, p, o)` order.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = Row> + '_ {
        self.spo.iter().copied()
    }
}

/// Within `run` — the rows led by one term, from [`Starts::run`] — the
/// contiguous slice whose second component equals `b`.
///
/// Still a binary search, but over one term's rows rather than the graph's: a
/// subject has seven triples on the benchmark data, not 690,000, so this stays
/// in the cache lines the first-level load already brought in.
#[inline]
fn prefix2(run: &[Row], b: TermId) -> &[Row] {
    let lo = run.partition_point(|r| r[1] < b);
    let hi = run[lo..].partition_point(|r| r[1] == b) + lo;
    &run[lo..hi]
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
