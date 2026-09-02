//! A pre-parsed graph on disk, so the same document is not read twice.
//!
//! Parsing is where the time goes. Measured on 690k triples: 1.06s to parse,
//! 0.12s to intern what was parsed, 0.10s to build the three permutations — so
//! the RDF parser is around nine tenths of loading, and loading is around three
//! quarters of a validation run. Every run pays it again for a document that
//! has not changed since the last one.
//!
//! An index file holds what parsing produced: the string arena, the interned
//! terms, and the triples in `SPO` order. Reading it back still costs the
//! interning and the index build, but not the parse, which is the part worth
//! avoiding.
//!
//! Three things are deliberately *not* stored:
//!
//! * The interner's hash table and the term lookup, whose seeds are randomised
//!   per process. A persisted table would be read back under a different seed
//!   and find nothing in itself.
//! * The `POS` and `OSP` permutations, because sorting them back costs less
//!   than the bytes would cost to write and read.
//!
//! The file records a digest of the source it was built from, so an index left
//! behind by an edited document is a reported error rather than a confident
//! answer about data that no longer exists.

use std::io::{Read, Write};

use hashbrown::HashMap;

use super::graph::{Graph, GraphBuilder};
use super::interner::{Interner, StrId};
use super::term::{Direction, TermData, TermId, TermStore};
use crate::error::{Error, Result};

/// The first four bytes of an index file.
const MAGIC: [u8; 4] = *b"SHIX";

/// The format's own version, bumped whenever the layout below changes.
///
/// Deliberately separate from the crate version: an index stays readable
/// across the releases that do not touch the format, and is refused by exactly
/// those that do.
const FORMAT_VERSION: u32 = 1;

/// `u32::MAX` stands for an absent id. No real id can collide with it: the
/// store would have exhausted memory long before interning four billion
/// strings.
const ABSENT: u32 = u32::MAX;

/// Identifies the source document an index was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceDigest(pub u64);

impl SourceDigest {
    /// Digests the source bytes.
    ///
    /// Not a cryptographic hash, and not meant as one: this answers "has this
    /// file changed since the index was built", not "did someone tamper with
    /// it". An index is a cache of a document the caller already trusts enough
    /// to validate against.
    pub fn of(bytes: &[u8]) -> Self {
        use std::hash::BuildHasher;
        // A fixed seed, unlike the interner's: this value is written to disk
        // and compared in a later process, so it has to be reproducible.
        Self(foldhash::fast::FixedState::default().hash_one(bytes))
    }
}

// ------------------------------------------------------------------- writing

/// Writes `graph`, and the store its term ids point into.
///
/// The whole store is written, not just the part this graph reaches. Term ids
/// are positions in it, so writing a subset would mean renumbering, and
/// renumbering is how ids stop meaning what the triples say.
pub fn write(
    w: &mut impl Write,
    store: &TermStore,
    graph: &Graph,
    source: SourceDigest,
) -> Result<()> {
    let parts = store.parts();

    w.write_all(&MAGIC).map_err(io)?;
    u32_out(w, FORMAT_VERSION)?;
    u64_out(w, source.0)?;

    // --- the string arena
    let (buf, spans) = parts.strings.parts();
    u64_out(w, buf.len() as u64)?;
    w.write_all(buf.as_bytes()).map_err(io)?;
    u64_out(w, spans.len() as u64)?;
    for &(off, len) in spans {
        u32_out(w, off)?;
        u32_out(w, len)?;
    }

    // --- the terms
    u64_out(w, parts.terms.len() as u64)?;
    for term in parts.terms {
        match *term {
            TermData::NamedNode(s) => {
                w.write_all(&[0]).map_err(io)?;
                u32_out(w, s.0)?;
            }
            TermData::BlankNode(s) => {
                w.write_all(&[1]).map_err(io)?;
                u32_out(w, s.0)?;
            }
            TermData::Literal {
                lex,
                datatype,
                lang,
                dir,
            } => {
                w.write_all(&[2]).map_err(io)?;
                u32_out(w, lex.0)?;
                u32_out(w, datatype.as_raw())?;
                u32_out(w, lang.map_or(ABSENT, |l| l.0))?;
                w.write_all(&[match dir {
                    None => 0,
                    Some(Direction::Ltr) => 1,
                    Some(Direction::Rtl) => 2,
                }])
                .map_err(io)?;
            }
            TermData::Triple(i) => {
                w.write_all(&[3]).map_err(io)?;
                u32_out(w, i)?;
            }
        }
    }

    // --- RDF 1.2 triple terms
    u64_out(w, parts.triple_terms.len() as u64)?;
    for row in parts.triple_terms {
        for t in row {
            u32_out(w, t.as_raw())?;
        }
    }

    // --- blank node numbering
    //
    // This has to survive, or a report built from an index would name blank
    // nodes differently from one built from the source. Reports over identical
    // input are supposed to be byte-identical, and that is the property the
    // round-trip test checks.
    u64_out(w, parts.blank_numbers.len() as u64)?;
    // Written in sorted order so the file itself is reproducible: the map's
    // iteration order is not stable across processes.
    let mut numbering: Vec<_> = parts.blank_numbers.iter().collect();
    numbering.sort_unstable();
    for (label, &n) in numbering {
        u64_out(w, label.len() as u64)?;
        w.write_all(label.as_bytes()).map_err(io)?;
        u32_out(w, n)?;
    }

    // --- the triples, in SPO order
    u64_out(w, graph.len() as u64)?;
    for row in graph.iter() {
        for t in row {
            u32_out(w, t.as_raw())?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- reading

/// Reads back what [`write`] wrote.
///
/// `expect` is the digest of the document this index should describe. `None`
/// skips the check, which is for a caller that has no source to compare
/// against — not for one that would rather not know.
pub fn read(r: &mut impl Read, expect: Option<SourceDigest>) -> Result<(TermStore, Graph)> {
    let mut bytes = Vec::new();
    r.read_to_end(&mut bytes).map_err(io)?;
    let mut c = Cursor {
        bytes: &bytes,
        at: 0,
    };

    if c.take(4)? != MAGIC {
        return Err(Error::Parse("not a SHACL index file".into()));
    }
    let version = c.u32()?;
    if version != FORMAT_VERSION {
        return Err(Error::Parse(format!(
            "index file is format version {version}, this build reads \
             {FORMAT_VERSION}; rebuild it from the source document"
        )));
    }
    let digest = SourceDigest(c.u64()?);
    if let Some(want) = expect
        && want != digest
    {
        return Err(Error::Parse(
            "index file was built from a different document; rebuild it".into(),
        ));
    }

    // --- the string arena
    let n = c.u64()? as usize;
    let buf = c.utf8(n, "string arena")?.to_string();
    let n = c.u64()? as usize;
    let mut spans = Vec::with_capacity(n.min(bytes.len()));
    for _ in 0..n {
        spans.push((c.u32()?, c.u32()?));
    }
    // Every span is checked before the interner is handed strings it would
    // slice with: a corrupt file must be an error here, not a panic deep in
    // a later lookup.
    for &(off, len) in &spans {
        let end = off.checked_add(len).ok_or_else(corrupt)?;
        if !buf.is_char_boundary(off as usize) || !buf.is_char_boundary(end as usize) {
            return Err(corrupt());
        }
    }
    let strings = Interner::from_parts(buf, spans);
    let string_count = strings.len() as u32;

    // --- the terms
    let n = c.u64()? as usize;
    let mut terms = Vec::with_capacity(n.min(bytes.len()));
    for _ in 0..n {
        let term = match c.byte()? {
            0 => TermData::NamedNode(c.str_id(string_count)?),
            1 => TermData::BlankNode(c.str_id(string_count)?),
            2 => {
                let lex = c.str_id(string_count)?;
                let datatype = TermId::from_raw(c.u32()?);
                let lang = match c.u32()? {
                    ABSENT => None,
                    raw if raw < string_count => Some(StrId(raw)),
                    _ => return Err(corrupt()),
                };
                let dir = match c.byte()? {
                    0 => None,
                    1 => Some(Direction::Ltr),
                    2 => Some(Direction::Rtl),
                    _ => return Err(corrupt()),
                };
                TermData::Literal {
                    lex,
                    datatype,
                    lang,
                    dir,
                }
            }
            3 => TermData::Triple(c.u32()?),
            _ => return Err(corrupt()),
        };
        terms.push(term);
    }

    // A literal's datatype is always interned before the literal itself, so
    // this could only fail on a corrupt file — but it is checked here rather
    // than trusted, because an out-of-range id would otherwise surface as a
    // panic somewhere far from the file that caused it.
    if terms.iter().any(|t| match t {
        TermData::Literal { datatype, .. } => datatype.as_raw() as usize >= terms.len(),
        TermData::Triple(_) => false,
        _ => false,
    }) {
        return Err(corrupt());
    }

    // --- RDF 1.2 triple terms
    let n = c.u64()? as usize;
    let mut triple_terms = Vec::with_capacity(n.min(bytes.len()));
    for _ in 0..n {
        triple_terms.push([c.term(&terms)?, c.term(&terms)?, c.term(&terms)?]);
    }

    // --- blank node numbering
    let n = c.u64()? as usize;
    let mut blank_numbers = HashMap::with_capacity(n.min(bytes.len()));
    for _ in 0..n {
        let len = c.u64()? as usize;
        let label = c.utf8(len, "blank node label")?.into();
        blank_numbers.insert(label, c.u32()?);
    }

    let store = TermStore::from_parts(strings, terms, triple_terms, blank_numbers);

    // --- the triples
    let n = c.u64()? as usize;
    let mut b = GraphBuilder::new();
    let terms = store.parts().terms;
    for _ in 0..n {
        b.push(c.term(terms)?, c.term(terms)?, c.term(terms)?);
    }
    Ok((store, b.build()))
}

fn io(e: std::io::Error) -> Error {
    Error::Io(e.to_string())
}

fn corrupt() -> Error {
    Error::Parse("index file is corrupt; rebuild it from the source document".into())
}

fn u32_out(w: &mut impl Write, v: u32) -> Result<()> {
    w.write_all(&v.to_le_bytes()).map_err(io)
}

fn u64_out(w: &mut impl Write, v: u64) -> Result<()> {
    w.write_all(&v.to_le_bytes()).map_err(io)
}

/// A bounds-checked reader over the file's bytes.
///
/// Every field is checked, including the ids: an index file is input like any
/// other, and a truncated or hand-edited one has to surface as an error a host
/// process can catch, not as a panic or an out-of-range term id that would
/// index into the wrong term much later.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or_else(truncated)?;
        let slice = self.bytes.get(self.at..end).ok_or_else(truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn utf8(&mut self, n: usize, what: &str) -> Result<&'a str> {
        std::str::from_utf8(self.take(n)?)
            .map_err(|e| Error::Parse(format!("index {what} is not valid UTF-8: {e}")))
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    /// Reads a string id, rejecting one the arena cannot answer for.
    fn str_id(&mut self, count: u32) -> Result<StrId> {
        match self.u32()? {
            raw if raw < count => Ok(StrId(raw)),
            _ => Err(corrupt()),
        }
    }

    /// Reads a term id, rejecting one past the end of `terms`.
    fn term(&mut self, terms: &[TermData]) -> Result<TermId> {
        match self.u32()? {
            raw if (raw as usize) < terms.len() => Ok(TermId::from_raw(raw)),
            _ => Err(corrupt()),
        }
    }
}

fn truncated() -> Error {
    Error::Parse("index file ends in the middle of a record".into())
}
