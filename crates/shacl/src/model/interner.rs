//! String interning.
//!
//! Every IRI, blank node label and literal lexical form in a graph is stored
//! exactly once in a single contiguous arena. Callers hold a 4-byte [`StrId`]
//! instead of a `String`, which is what lets the rest of the engine compare
//! terms with an integer compare and keep triples in flat arrays.

use std::hash::{BuildHasher, Hash};

use foldhash::fast::RandomState;

use hashbrown::HashTable;

/// A handle to an interned string. Cheap to copy, compare and hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StrId(pub(crate) u32);

/// Append-only string arena with deduplication.
#[derive(Clone, Default)]
pub struct Interner {
    /// All interned bytes, concatenated. Never shrinks, so spans stay valid.
    buf: String,
    /// `(offset, len)` into `buf`, indexed by `StrId`.
    spans: Vec<(u32, u32)>,
    /// Maps a string's hash to its `StrId`. Equality is resolved by looking the
    /// candidate back up in `buf`, which keeps this struct non-self-referential.
    table: HashTable<u32>,
    /// foldhash rather than the default SipHash. Interning is dominated by
    /// hashing short strings — two million of them on a large graph — and
    /// SipHash costs about 17% of load for quality this does not need.
    ///
    /// The seed is still randomised per process. A validator may be handed RDF
    /// from anywhere, and a fixed-seed hash would let a crafted document
    /// collide every IRI into one bucket and turn interning quadratic.
    hasher: RandomState,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Interns `s`, returning the existing id if it has been seen before.
    pub fn intern(&mut self, s: &str) -> StrId {
        // Destructured so the lookup closure can borrow `buf`/`spans` while
        // `table` is borrowed separately.
        let Self {
            buf,
            spans,
            table,
            hasher,
        } = self;
        let hash = hash_str(hasher, s);

        let eq = |&id: &u32| {
            let (off, len) = spans[id as usize];
            &buf[off as usize..(off + len) as usize] == s
        };
        if let Some(&id) = table.find(hash, eq) {
            return StrId(id);
        }

        let id = spans.len() as u32;
        let off = buf.len() as u32;
        buf.push_str(s);
        spans.push((off, s.len() as u32));
        table.insert_unique(hash, id, |&other| {
            let (off, len) = spans[other as usize];
            hash_str(hasher, &buf[off as usize..(off + len) as usize])
        });
        StrId(id)
    }

    /// Returns the id of `s` if it has already been interned.
    ///
    /// Used on hot paths that must not grow the arena, and to fail fast when a
    /// term simply cannot occur in the graph being queried.
    pub fn get(&self, s: &str) -> Option<StrId> {
        let hash = hash_str(&self.hasher, s);
        self.table
            .find(hash, |&id| self.resolve(StrId(id)) == s)
            .map(|&id| StrId(id))
    }

    /// Resolves an id back to its string.
    #[inline]
    pub fn resolve(&self, id: StrId) -> &str {
        let (off, len) = self.spans[id.0 as usize];
        &self.buf[off as usize..(off + len) as usize]
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

#[inline]
fn hash_str(hasher: &RandomState, s: &str) -> u64 {
    hasher.hash_one(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interns_and_deduplicates() {
        let mut i = Interner::new();
        let a = i.intern("http://example.com/a");
        let b = i.intern("http://example.com/b");
        let a2 = i.intern("http://example.com/a");

        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(i.len(), 2, "duplicate must not allocate a new id");
        assert_eq!(i.resolve(a), "http://example.com/a");
        assert_eq!(i.resolve(b), "http://example.com/b");
    }

    #[test]
    fn get_does_not_intern() {
        let mut i = Interner::new();
        i.intern("known");

        assert_eq!(i.get("known"), Some(StrId(0)));
        assert_eq!(i.get("unknown"), None);
        assert_eq!(i.len(), 1);
    }

    #[test]
    fn handles_empty_and_unicode() {
        let mut i = Interner::new();
        let empty = i.intern("");
        let uni = i.intern("héllo→世界");

        assert_eq!(i.resolve(empty), "");
        assert_eq!(i.resolve(uni), "héllo→世界");
        assert_eq!(i.intern(""), empty);
    }
}
