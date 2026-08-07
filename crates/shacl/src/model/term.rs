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
#[derive(Debug)]
pub struct TermStore {
    strings: Interner,
    terms: Vec<TermData>,
    lookup: hashbrown::HashMap<TermData, TermId>,
    /// Subject/predicate/object of each RDF 1.2 triple term.
    triple_terms: Vec<[TermId; 3]>,
    /// Scratch buffer for scoping blank node labels without allocating.
    scratch: String,
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
            triple_terms: Vec::new(),
            scratch: String::new(),
        }
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
        self.push(TermData::NamedNode(s))
    }

    /// Interns a blank node, scoping its label to `scope`.
    ///
    /// Labels are only unique within the document they were parsed from, so the
    /// data graph and shapes graph must not share a scope or their `_:b0`s
    /// would collapse into one node.
    pub fn blank_node(&mut self, scope: u32, label: &str) -> TermId {
        use std::fmt::Write;
        self.scratch.clear();
        let _ = write!(self.scratch, "{scope}:{label}");
        // Interning borrows `strings` mutably, so hand it the scratch contents
        // via a reborrow rather than holding `self` across the call.
        let s = {
            let Self {
                strings, scratch, ..
            } = self;
            strings.intern(scratch)
        };
        self.push(TermData::BlankNode(s))
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
        let idx = self.triple_terms.len() as u32;
        self.triple_terms.push([s, p, o]);
        self.push(TermData::Triple(idx))
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
        self.lookup.get(&TermData::NamedNode(s)).copied()
    }

    /// Looks up any already-interned term without growing the store.
    ///
    /// A term absent here cannot appear in any graph built from this store, so
    /// callers matching against the data can treat `None` as "matches nothing"
    /// rather than as an error.
    pub fn get_term(&self, term: TermRef<'_>) -> Option<TermId> {
        let data = match term {
            TermRef::NamedNode(n) => TermData::NamedNode(self.strings.get(n.as_str())?),
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
                        return Term::Literal(oxrdf::Literal::new_directional_language_tagged_literal_unchecked(
                            value,
                            self.strings.resolve(l),
                            match d {
                                Direction::Ltr => oxrdf::BaseDirection::Ltr,
                                Direction::Rtl => oxrdf::BaseDirection::Rtl,
                            },
                        ));
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
        let en = s.literal("1", "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString", Some("en"));

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
        assert_eq!(s.to_oxrdf(bn).to_string(), "_:3_x1");
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
