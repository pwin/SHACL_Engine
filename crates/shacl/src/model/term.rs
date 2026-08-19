//! Interned RDF terms.
//!
//! A [`TermId`] is a 4-byte handle into a [`TermStore`]. Because every graph in
//! a validation run shares one store, term equality — the single most executed
//! operation in the engine — is a `u32` comparison rather than a string compare.

use oxrdf::{BlankNodeRef, LiteralRef, NamedNodeRef, Term, TermRef};

use super::interner::{Interner, StrId};

/// A handle to an interned RDF term.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TermId(pub(crate) u32);

impl TermId {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// The raw handle. Only for callers that must address terms outside the
    /// store, such as the SPARQL adapter's side table for computed terms.
    #[inline]
    pub fn as_raw(self) -> u32 {
        self.0
    }

    /// Rebuilds a handle from [`TermId::as_raw`].
    #[inline]
    pub fn from_raw(raw: u32) -> Self {
        Self(raw)
    }
}

/// What an interned term actually is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermData {
    NamedNode(StrId),
    BlankNode(StrId),
    Literal {
        lex: StrId,
        /// Always a `TermId` denoting a [`TermData::NamedNode`].
        datatype: TermId,
        /// `Some` only for `rdf:langString`.
        lang: Option<StrId>,
        /// RDF 1.2 base direction. Part of the term's identity: `"A"@ar`,
        /// `"A"@ar--ltr` and `"A"@ar--rtl` are three distinct literals, which
        /// `sh:uniqueLang` has to tell apart.
        dir: Option<Direction>,
    },
    /// An RDF 1.2 triple term; indexes into [`TermStore::triple_terms`].
    Triple(u32),
}

/// The base direction of a directional language-tagged string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Ltr,
    Rtl,
}

/// The kind of an RDF term, as `sh:nodeKind` understands it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKind {
    Iri,
    Blank,
    Literal,
    Triple,
}

/// Interner for RDF terms, shared across every graph in a validation run.
///
/// Cloning is cheap once only a shapes graph has been read — a few hundred
/// terms — which is what lets a compiled shapes graph be validated against
/// many data graphs concurrently: each run clones the store and grows its own
/// copy, so the ids the compiled shapes hold stay valid without any sharing.
#[derive(Clone)]
pub struct TermStore {
    strings: Interner,
    terms: Vec<TermData>,
    /// Deduplication for terms whose identity is not a bare string. Named and
    /// blank nodes bypass it entirely — see `named_of`/`blank_of`.
    lookup: hashbrown::HashMap<TermData, TermId>,
    /// `TermId` of the named node for each interned string, or `NONE`.
    ///
    /// Interning a term used to cost two hash lookups: one to intern its
    /// string, one to find the term built from it. For named and blank nodes
    /// the string id already determines the term, so a direct array index
    /// replaces the second lookup — and since these are the bulk of a graph's
    /// terms, that is most of the hashing gone.
    named_of: Vec<u32>,
    /// The same, for blank nodes. Kept separate because a scoped blank node
    /// label and an IRI can never be the same string, but the two term kinds
    /// still need distinct ids.
    blank_of: Vec<u32>,
    /// Subject/predicate/object of each RDF 1.2 triple term.
    triple_terms: Vec<[TermId; 3]>,
    /// Scratch buffer for scoping blank node labels without allocating.
    scratch: String,
    /// Sequential number given to each blank node label, per scope, in the
    /// order the labels were first seen.
    ///
    /// The parser names an anonymous `[ … ]` node with a *random* identifier,
    /// which would otherwise reach the validation report and make two runs
    /// over identical input print byte-different documents — no use to anyone
    /// diffing one report against another. Renumbering on arrival makes the
    /// label a function of the document instead, and drops 32 hex characters
    /// of parser detail out of the report at the same time.
    blank_numbers: hashbrown::HashMap<Box<str>, u32>,
}

impl Default for TermStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TermStore {
    pub fn new() -> Self {
        Self {
            strings: Interner::new(),
            terms: Vec::new(),
            lookup: hashbrown::HashMap::new(),
            named_of: Vec::new(),
            blank_of: Vec::new(),
            triple_terms: Vec::new(),
            scratch: String::new(),
            blank_numbers: hashbrown::HashMap::default(),
        }
    }

    /// Sentinel for "no term built from this string yet".
    const NONE: u32 = u32::MAX;

    /// Looks up, or creates, the term of one kind built from string `s`.
    #[inline]
    fn by_string(&mut self, s: StrId, blank: bool) -> TermId {
        let table = if blank {
            &mut self.blank_of
        } else {
            &mut self.named_of
        };
        let idx = s.0 as usize;
        if idx >= table.len() {
            table.resize(idx + 1, Self::NONE);
        }
        if table[idx] != Self::NONE {
            return TermId(table[idx]);
        }
        let data = if blank {
            TermData::BlankNode(s)
        } else {
            TermData::NamedNode(s)
        };
        let id = TermId(self.terms.len() as u32);
        self.terms.push(data);
        // Deliberately not inserted into `lookup`: the array above is the
        // index for these, and a second copy would cost the hash it saves.
        let table = if blank {
            &mut self.blank_of
        } else {
            &mut self.named_of
        };
        table[idx] = id.0;
        id
    }

    fn push(&mut self, data: TermData) -> TermId {
        if let Some(&id) = self.lookup.get(&data) {
            return id;
        }
        let id = TermId(self.terms.len() as u32);
        self.terms.push(data);
        self.lookup.insert(data, id);
        id
    }

    pub fn named_node(&mut self, iri: &str) -> TermId {
        let s = self.strings.intern(iri);
        self.by_string(s, false)
    }

    /// Interns a blank node, scoping its label to `scope`.
    ///
    /// Labels are only unique within the document they were parsed from, so the
    /// data graph and shapes graph must not share a scope or their `_:b0`s
    /// would collapse into one node.
    pub fn blank_node(&mut self, scope: u32, label: &str) -> TermId {
        use std::fmt::Write;

        // Renumber in first-seen order. Parsing a document is sequential, so
        // the same document always yields the same numbers; the parser's own
        // random naming for anonymous nodes never escapes this point.
        self.scratch.clear();
        let _ = write!(self.scratch, "{scope}:{label}");
        let n = match self.blank_numbers.get(self.scratch.as_str()) {
            Some(&n) => n,
            None => {
                let n = self.blank_numbers.len() as u32;
                self.blank_numbers.insert(self.scratch.as_str().into(), n);
                n
            }
        };

        self.scratch.clear();
        let _ = write!(self.scratch, "{scope}:b{n}");
        // Interning borrows `strings` mutably, so hand it the scratch contents
        // via a reborrow rather than holding `self` across the call.
        let s = {
            let Self {
                strings, scratch, ..
            } = self;
            strings.intern(scratch)
        };
        self.by_string(s, true)
    }

    pub fn literal(&mut self, lex: &str, datatype: &str, lang: Option<&str>) -> TermId {
        self.literal_with_direction(lex, datatype, lang, None)
    }

    pub fn literal_with_direction(
        &mut self,
        lex: &str,
        datatype: &str,
        lang: Option<&str>,
        dir: Option<Direction>,
    ) -> TermId {
        let lex = self.strings.intern(lex);
        let lang = lang.map(|l| self.strings.intern(l));
        let datatype = self.named_node(datatype);
        self.push(TermData::Literal {
            lex,
            datatype,
            lang,
            dir,
        })
    }

    pub fn triple_term(&mut self, s: TermId, p: TermId, o: TermId) -> TermId {
        // Identity is the triple, not the slot it lands in. Allocating a fresh
        // index every time would make two spellings of the same triple term
        // distinct, and `sh:reifierShape` looks its subject up by equality.
        if let Some(i) = self.triple_terms.iter().position(|t| *t == [s, p, o]) {
            return self.push(TermData::Triple(i as u32));
        }
        let idx = self.triple_terms.len() as u32;
        self.triple_terms.push([s, p, o]);
        self.push(TermData::Triple(idx))
    }

    /// Looks up an existing triple term without creating one.
    pub fn get_triple_term(&self, s: TermId, p: TermId, o: TermId) -> Option<TermId> {
        let i = self.triple_terms.iter().position(|t| *t == [s, p, o])?;
        self.lookup.get(&TermData::Triple(i as u32)).copied()
    }

    /// Interns an `oxrdf` term parsed from the document identified by `scope`.
    pub fn intern_oxrdf(&mut self, term: TermRef<'_>, scope: u32) -> TermId {
        match term {
            TermRef::NamedNode(n) => self.named_node(n.as_str()),
            TermRef::BlankNode(b) => self.blank_node(scope, b.as_str()),
            TermRef::Literal(l) => self.literal_with_direction(
                l.value(),
                l.datatype().as_str(),
                l.language(),
                l.direction().map(|d| match d {
                    oxrdf::BaseDirection::Ltr => Direction::Ltr,
                    oxrdf::BaseDirection::Rtl => Direction::Rtl,
                }),
            ),
            TermRef::Triple(t) => {
                let s = self.intern_oxrdf(TermRef::from(t.subject.as_ref()), scope);
                let p = self.named_node(t.predicate.as_str());
                let o = self.intern_oxrdf(t.object.as_ref(), scope);
                self.triple_term(s, p, o)
            }
        }
    }

    #[inline]
    pub fn data(&self, id: TermId) -> TermData {
        self.terms[id.index()]
    }

    #[inline]
    pub fn kind(&self, id: TermId) -> TermKind {
        match self.terms[id.index()] {
            TermData::NamedNode(_) => TermKind::Iri,
            TermData::BlankNode(_) => TermKind::Blank,
            TermData::Literal { .. } => TermKind::Literal,
            TermData::Triple(_) => TermKind::Triple,
        }
    }

    #[inline]
    pub fn is_literal(&self, id: TermId) -> bool {
        matches!(self.terms[id.index()], TermData::Literal { .. })
    }

    #[inline]
    pub fn is_iri(&self, id: TermId) -> bool {
        matches!(self.terms[id.index()], TermData::NamedNode(_))
    }

    #[inline]
    pub fn is_blank(&self, id: TermId) -> bool {
        matches!(self.terms[id.index()], TermData::BlankNode(_))
    }

    /// The IRI of a named node, or `None` for any other term kind.
    #[inline]
    pub fn iri(&self, id: TermId) -> Option<&str> {
        match self.terms[id.index()] {
            TermData::NamedNode(s) => Some(self.strings.resolve(s)),
            _ => None,
        }
    }

    /// The lexical form of a term: an IRI, a blank node label, or a literal's
    /// string value. `None` for triple terms.
    #[inline]
    pub fn lexical_form(&self, id: TermId) -> Option<&str> {
        match self.terms[id.index()] {
            TermData::NamedNode(s) | TermData::BlankNode(s) => Some(self.strings.resolve(s)),
            TermData::Literal { lex, .. } => Some(self.strings.resolve(lex)),
            TermData::Triple(_) => None,
        }
    }

    /// The datatype of a literal, or `None` for non-literals.
    #[inline]
    pub fn datatype(&self, id: TermId) -> Option<TermId> {
        match self.terms[id.index()] {
            TermData::Literal { datatype, .. } => Some(datatype),
            _ => None,
        }
    }

    /// The language tag of a literal. `Some("")` is never returned — an absent
    /// tag is always `None`, matching `sh:languageIn` semantics.
    #[inline]
    pub fn language(&self, id: TermId) -> Option<&str> {
        match self.terms[id.index()] {
            TermData::Literal { lang: Some(l), .. } => Some(self.strings.resolve(l)),
            _ => None,
        }
    }

    /// The base direction of a directional language-tagged string.
    #[inline]
    pub fn direction(&self, id: TermId) -> Option<Direction> {
        match self.terms[id.index()] {
            TermData::Literal { dir, .. } => dir,
            _ => None,
        }
    }

    #[inline]
    pub fn triple_parts(&self, id: TermId) -> Option<[TermId; 3]> {
        match self.terms[id.index()] {
            TermData::Triple(i) => Some(self.triple_terms[i as usize]),
            _ => None,
        }
    }

    /// Looks up an already-interned IRI without growing the store.
    pub fn get_named_node(&self, iri: &str) -> Option<TermId> {
        let s = self.strings.get(iri)?;
        match self.named_of.get(s.0 as usize) {
            Some(&id) if id != Self::NONE => Some(TermId(id)),
            _ => None,
        }
    }

    /// Looks up any already-interned term without growing the store.
    ///
    /// A term absent here cannot appear in any graph built from this store, so
    /// callers matching against the data can treat `None` as "matches nothing"
    /// rather than as an error.
    pub fn get_term(&self, term: TermRef<'_>) -> Option<TermId> {
        let data = match term {
            // Named nodes are indexed by string id, not by `lookup`.
            TermRef::NamedNode(n) => return self.get_named_node(n.as_str()),
            TermRef::BlankNode(_) => {
                // Blank node labels are scope-prefixed on the way in, so an
                // externally-supplied label has no meaningful identity here.
                return None;
            }
            TermRef::Literal(l) => TermData::Literal {
                lex: self.strings.get(l.value())?,
                datatype: self.get_named_node(l.datatype().as_str())?,
                lang: match l.language() {
                    Some(t) => Some(self.strings.get(t)?),
                    None => None,
                },
                dir: l.direction().map(|d| match d {
                    oxrdf::BaseDirection::Ltr => Direction::Ltr,
                    oxrdf::BaseDirection::Rtl => Direction::Rtl,
                }),
            },
            TermRef::Triple(_) => return None,
        };
        self.lookup.get(&data).copied()
    }

    /// Resolves a term this store rendered, whatever its kind.
    ///
    /// The single inverse of [`TermStore::to_oxrdf`], and the one a boundary
    /// should call. [`TermStore::get_term`] is not that inverse: it refuses
    /// blank nodes, because an externally-supplied label names nothing here,
    /// and it refuses triple terms, whose components need resolving first.
    /// Both refusals are correct for a term that arrived from outside and
    /// wrong for one that left through `to_oxrdf` and came back.
    ///
    /// Keeping the two apart is what let a blank node handed back from a
    /// SPARQL solution resolve to nothing, so a result lost its `sh:value`
    /// without saying so. A triple term does the same thing today, which
    /// `tests/roundtrip.rs` is what noticed.
    pub fn resolve_rendered(&self, term: TermRef<'_>) -> Option<TermId> {
        match term {
            TermRef::BlankNode(b) => self.blank_node_from_output_label(b.as_str()),
            TermRef::Triple(t) => {
                // Components recurse, since any of them may itself be a blank
                // node or a nested triple term.
                let s = self.resolve_rendered(TermRef::from(t.subject.as_ref()))?;
                let p = self.get_named_node(t.predicate.as_str())?;
                let o = self.resolve_rendered(t.object.as_ref())?;
                self.get_triple_term(s, p, o)
            }
            other => self.get_term(other),
        }
    }

    /// Resolves a blank node rendered by [`TermStore::to_oxrdf`] back to its
    /// handle, or `None` if this store never produced it.
    ///
    /// The inverse of the label transform below, and deliberately next to it
    /// so the two cannot drift apart. [`TermStore::get_term`] refuses blank
    /// nodes because an externally-supplied label names nothing here — that
    /// stays true, and this is the narrow exception: a label this store wrote,
    /// coming back from a round trip through the SPARQL evaluator, which
    /// externalises terms to `oxrdf` and hands them back as solutions.
    pub fn blank_node_from_output_label(&self, label: &str) -> Option<TermId> {
        // The stored form is `<scope>:<label>` and the rendered one swaps that
        // single `:` for `_`. A scope is decimal, so the first `_` is always
        // the separator and the swap is reversible.
        let cut = label.find('_')?;
        let mut stored = String::with_capacity(label.len());
        stored.push_str(&label[..cut]);
        stored.push(':');
        stored.push_str(&label[cut + 1..]);

        let s = self.strings.get(&stored)?;
        let id = *self.blank_of.get(s.0 as usize)?;
        (id != Self::NONE).then_some(TermId(id))
    }

    /// Materialises an interned term back into an `oxrdf` term, for report
    /// serialisation. Blank node labels keep their scope prefix stripped.
    pub fn to_oxrdf(&self, id: TermId) -> Term {
        match self.terms[id.index()] {
            TermData::NamedNode(s) => {
                Term::NamedNode(NamedNodeRef::new_unchecked(self.strings.resolve(s)).into_owned())
            }
            TermData::BlankNode(s) => {
                // Stored as `<scope>:<label>`. The scope must survive into the
                // output — a report can mention blank nodes from both the data
                // and shapes graphs, and dropping the scope would alias two
                // distinct `_:b0`s into one node, silently changing the graph.
                // `:` is illegal in a label, so swapping it for `_` stays
                // injective while producing a valid label.
                let label = self.strings.resolve(s).replace(':', "_");
                Term::BlankNode(BlankNodeRef::new_unchecked(&label).into_owned())
            }
            TermData::Literal {
                lex,
                datatype,
                lang,
                dir,
            } => {
                let value = self.strings.resolve(lex);
                let lit = match (lang, dir) {
                    (Some(l), Some(d)) => {
                        return Term::Literal(
                            oxrdf::Literal::new_directional_language_tagged_literal_unchecked(
                                value,
                                self.strings.resolve(l),
                                match d {
                                    Direction::Ltr => oxrdf::BaseDirection::Ltr,
                                    Direction::Rtl => oxrdf::BaseDirection::Rtl,
                                },
                            ),
                        );
                    }
                    (Some(l), None) => LiteralRef::new_language_tagged_literal_unchecked(
                        value,
                        self.strings.resolve(l),
                    ),
                    (None, _) => LiteralRef::new_typed_literal(
                        value,
                        NamedNodeRef::new_unchecked(self.iri(datatype).unwrap_or("")),
                    ),
                };
                Term::Literal(lit.into_owned())
            }
            TermData::Triple(i) => {
                let [s, p, o] = self.triple_terms[i as usize];
                let subject = match self.to_oxrdf(s) {
                    Term::NamedNode(n) => oxrdf::NamedOrBlankNode::NamedNode(n),
                    Term::BlankNode(b) => oxrdf::NamedOrBlankNode::BlankNode(b),
                    other => oxrdf::NamedOrBlankNode::NamedNode(
                        NamedNodeRef::new_unchecked(&other.to_string()).into_owned(),
                    ),
                };
                let predicate = NamedNodeRef::new_unchecked(self.iri(p).unwrap_or("")).into_owned();
                Term::Triple(Box::new(oxrdf::Triple {
                    subject,
                    predicate,
                    object: self.to_oxrdf(o),
                }))
            }
        }
    }

    pub fn len(&self) -> usize {
        self.terms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `to_oxrdf` and `blank_node_from_output_label` are inverses.
    ///
    /// They encode the same convention in two places — one swaps `:` for `_`,
    /// the other swaps it back — so this walks every blank node in a store
    /// with several scopes and round-trips it, rather than trusting a pair of
    /// string edits to stay agreed.
    #[test]
    fn a_rendered_blank_node_resolves_to_the_node_it_came_from() {
        let mut s = TermStore::new();
        let mut ids = Vec::new();
        for scope in [0u32, 1, 7, 1024] {
            for label in ["b0", "b1", "x", "has_underscore", "b12345"] {
                ids.push(s.blank_node(scope, label));
            }
        }
        // Distinct nodes, or the round trip below could pass by collision.
        let unique: std::collections::HashSet<_> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len());

        for id in ids {
            let Term::BlankNode(b) = s.to_oxrdf(id) else {
                panic!("should render as a blank node");
            };
            assert_eq!(
                s.blank_node_from_output_label(b.as_str()),
                Some(id),
                "{} did not round-trip",
                b.as_str()
            );
        }
    }

    #[test]
    fn a_label_this_store_never_wrote_resolves_to_nothing() {
        let mut s = TermStore::new();
        s.blank_node(0, "b0");
        // No separator, and a scope that exists but a label that does not.
        assert_eq!(s.blank_node_from_output_label("b0"), None);
        assert_eq!(s.blank_node_from_output_label("0_nope"), None);
        assert_eq!(s.blank_node_from_output_label("9_b0"), None);
    }

    #[test]
    fn same_term_interns_once() {
        let mut s = TermStore::new();
        let a = s.named_node("http://ex/a");
        let b = s.named_node("http://ex/a");
        assert_eq!(a, b);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn blank_nodes_are_scoped_per_document() {
        let mut s = TermStore::new();
        let data = s.blank_node(0, "b0");
        let shapes = s.blank_node(1, "b0");
        assert_ne!(
            data, shapes,
            "identical labels from different documents must stay distinct"
        );
        assert_eq!(s.blank_node(0, "b0"), data);
    }

    #[test]
    fn literals_distinguish_datatype_and_language() {
        let mut s = TermStore::new();
        let int = s.literal("1", "http://www.w3.org/2001/XMLSchema#integer", None);
        let string = s.literal("1", "http://www.w3.org/2001/XMLSchema#string", None);
        let en = s.literal(
            "1",
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString",
            Some("en"),
        );

        assert_ne!(int, string);
        assert_ne!(en, string);
        assert_eq!(s.lexical_form(int), Some("1"));
        assert_eq!(s.language(en), Some("en"));
        assert_eq!(s.language(int), None);
        assert_eq!(s.kind(int), TermKind::Literal);
    }

    #[test]
    fn base_direction_is_part_of_a_literal_s_identity() {
        // "A"@ar, "A"@ar--ltr and "A"@ar--rtl are three distinct literals.
        // Collapsing them would make sh:uniqueLang see false duplicates.
        let mut s = TermStore::new();
        const LS: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString";
        let plain = s.literal_with_direction("A", LS, Some("ar"), None);
        let ltr = s.literal_with_direction("A", LS, Some("ar"), Some(Direction::Ltr));
        let rtl = s.literal_with_direction("A", LS, Some("ar"), Some(Direction::Rtl));

        assert_ne!(plain, ltr);
        assert_ne!(ltr, rtl);
        assert_eq!(s.direction(ltr), Some(Direction::Ltr));
        assert_eq!(s.direction(plain), None);
        // The language tag itself is unchanged by the direction.
        assert_eq!(s.language(ltr), Some("ar"));
    }

    #[test]
    fn roundtrips_through_oxrdf() {
        let mut s = TermStore::new();
        let iri = s.named_node("http://ex/a");
        let lit = s.literal("hi", "http://www.w3.org/2001/XMLSchema#string", None);
        let bn = s.blank_node(3, "x1");

        assert_eq!(s.to_oxrdf(iri).to_string(), "<http://ex/a>");
        assert_eq!(s.to_oxrdf(lit).to_string(), "\"hi\"");

        // The document's own label is *not* carried through: blank nodes are
        // renumbered in first-seen order. RDF treats a label as local syntax
        // rather than identity — a processor may relabel — and renumbering is
        // what makes a report reproducible, since the parser names anonymous
        // `[ … ]` nodes randomly and those labels would otherwise reach the
        // output. The cost is that `_:x1` in the source cannot be matched by
        // eye to a node in the report.
        assert_eq!(s.to_oxrdf(bn).to_string(), "_:3_b0");
        // Same label, same node; a different one gets the next number.
        assert_eq!(s.blank_node(3, "x1"), bn);
        let other = s.blank_node(3, "x2");
        assert_eq!(s.to_oxrdf(other).to_string(), "_:3_b1");
        // Numbering is per store, but the scope still separates documents.
        assert_ne!(s.blank_node(4, "x1"), bn);
    }

    #[test]
    fn oxrdf_blank_labels_stay_distinct_across_scopes() {
        let mut s = TermStore::new();
        let a = s.blank_node(0, "b0");
        let b = s.blank_node(1, "b0");
        assert_ne!(
            s.to_oxrdf(a).to_string(),
            s.to_oxrdf(b).to_string(),
            "same label from two documents must not alias in report output"
        );
    }
}
