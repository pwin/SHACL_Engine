//! The compiled shape IR, and the compiler that builds it from a shapes graph.
//!
//! A shapes graph is read exactly once, into a flat arena of [`Shape`]s holding
//! interned terms and pre-compiled paths and regexes. Validation then never
//! queries the shapes graph again — every operand a constraint needs is already
//! resolved, so the inner loop touches only the data graph's indexes.

use hashbrown::HashMap;
use regex::Regex;

use crate::error::{Error, Result};
use crate::model::{Graph, TermId, TermStore, Vocab};
use crate::path::Path;

/// Index of a [`Shape`] in the compiled arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShapeId(pub u32);

impl ShapeId {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// The node kinds `sh:nodeKind` can require.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Iri,
    BlankNode,
    Literal,
    BlankNodeOrIri,
    BlankNodeOrLiteral,
    IriOrLiteral,
}

impl NodeKind {
    fn from_term(t: TermId, v: &Vocab) -> Option<Self> {
        Some(match t {
            _ if t == v.sh_IRI => Self::Iri,
            _ if t == v.sh_BlankNode => Self::BlankNode,
            _ if t == v.sh_Literal => Self::Literal,
            _ if t == v.sh_BlankNodeOrIRI => Self::BlankNodeOrIri,
            _ if t == v.sh_BlankNodeOrLiteral => Self::BlankNodeOrLiteral,
            _ if t == v.sh_IRIOrLiteral => Self::IriOrLiteral,
            _ => return None,
        })
    }
}

/// How a shape selects its focus nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Node(TermId),
    Class(TermId),
    SubjectsOf(TermId),
    ObjectsOf(TermId),
    /// A shape that is itself an `rdfs:Class` implicitly targets its instances.
    ImplicitClass(TermId),
}

/// A compiled constraint, with every operand already resolved.
#[derive(Debug, Clone)]
pub enum Constraint {
    // Value type. SHACL 1.2 lets each of these take a list, meaning "any of";
    // a single term compiles to a one-element alternative.
    Class(Vec<TermId>),
    Datatype(Vec<TermId>),
    NodeKind(Vec<NodeKind>),

    // Cardinality
    MinCount(u32),
    MaxCount(u32),

    // Value range
    MinExclusive(TermId),
    MinInclusive(TermId),
    MaxExclusive(TermId),
    MaxInclusive(TermId),

    // String based
    MinLength(u32),
    MaxLength(u32),
    Pattern {
        regex: Regex,
        /// The original `sh:pattern` literal, reported as `sh:sourceConstraint`.
        source: TermId,
    },
    LanguageIn(Vec<TermId>),
    UniqueLang,

    // Property pair. The compared sibling is a full property path, not merely a
    // predicate — SHACL 1.2 admits `sh:equals ( ex:a ex:b )` as a sequence.
    Equals(Path),
    Disjoint(Path),
    LessThan(Path),
    LessThanOrEquals(Path),

    // Logical
    Not(ShapeId),
    And(Vec<ShapeId>),
    Or(Vec<ShapeId>),
    Xone(Vec<ShapeId>),

    // Shape based
    Node(ShapeId),
    Property(ShapeId),
    QualifiedValueShape {
        shape: ShapeId,
        min: Option<u32>,
        max: Option<u32>,
        disjoint: bool,
        /// Sibling qualified shapes, needed when `disjoint` is set.
        siblings: Vec<ShapeId>,
    },

    // Other
    Closed {
        ignored: Vec<TermId>,
    },
    HasValue(TermId),
    In(Vec<TermId>),

    // SHACL 1.2. The list constraints treat each value node as the head of an
    // RDF collection; a value that is not a well-formed list always faults.
    MinListLength(u32),
    MaxListLength(u32),
    MemberShape(ShapeId),
    UniqueMembers,
    SingleLine,
    SubsetOf(Path),
    RootClass(TermId),
    SomeValue(ShapeId),
    /// Values must be unique *across* the shape's focus nodes, so this is the
    /// one constraint that reads the whole relation rather than a row. Several
    /// paths form a composite key.
    UniqueValuesFor(Vec<Path>),
}

impl Constraint {
    /// The `sh:sourceConstraintComponent` reported for a violation of this
    /// constraint.
    pub fn component(&self, v: &Vocab) -> TermId {
        match self {
            Self::Class(_) => v.sh_ClassConstraintComponent,
            Self::Datatype(_) => v.sh_DatatypeConstraintComponent,
            Self::NodeKind(_) => v.sh_NodeKindConstraintComponent,
            Self::MinCount(_) => v.sh_MinCountConstraintComponent,
            Self::MaxCount(_) => v.sh_MaxCountConstraintComponent,
            Self::MinExclusive(_) => v.sh_MinExclusiveConstraintComponent,
            Self::MinInclusive(_) => v.sh_MinInclusiveConstraintComponent,
            Self::MaxExclusive(_) => v.sh_MaxExclusiveConstraintComponent,
            Self::MaxInclusive(_) => v.sh_MaxInclusiveConstraintComponent,
            Self::MinLength(_) => v.sh_MinLengthConstraintComponent,
            Self::MaxLength(_) => v.sh_MaxLengthConstraintComponent,
            Self::Pattern { .. } => v.sh_PatternConstraintComponent,
            Self::LanguageIn(_) => v.sh_LanguageInConstraintComponent,
            Self::UniqueLang => v.sh_UniqueLangConstraintComponent,
            Self::Equals(_) => v.sh_EqualsConstraintComponent,
            Self::Disjoint(_) => v.sh_DisjointConstraintComponent,
            Self::LessThan(_) => v.sh_LessThanConstraintComponent,
            Self::LessThanOrEquals(_) => v.sh_LessThanOrEqualsConstraintComponent,
            Self::Not(_) => v.sh_NotConstraintComponent,
            Self::And(_) => v.sh_AndConstraintComponent,
            Self::Or(_) => v.sh_OrConstraintComponent,
            Self::Xone(_) => v.sh_XoneConstraintComponent,
            Self::Node(_) => v.sh_NodeConstraintComponent,
            Self::Property(_) => v.sh_PropertyConstraintComponent,
            // Which of the two qualified components is reported depends on
            // whether the min or the max bound was the one breached, so the
            // evaluator overrides this.
            Self::QualifiedValueShape { .. } => v.sh_QualifiedMinCountConstraintComponent,
            Self::Closed { .. } => v.sh_ClosedConstraintComponent,
            Self::HasValue(_) => v.sh_HasValueConstraintComponent,
            Self::In(_) => v.sh_InConstraintComponent,
            Self::MinListLength(_) => v.sh_MinListLengthConstraintComponent,
            Self::MaxListLength(_) => v.sh_MaxListLengthConstraintComponent,
            Self::MemberShape(_) => v.sh_MemberShapeConstraintComponent,
            Self::UniqueMembers => v.sh_UniqueMembersConstraintComponent,
            Self::SingleLine => v.sh_SingleLineConstraintComponent,
            Self::SubsetOf(_) => v.sh_SubsetOfConstraintComponent,
            Self::RootClass(_) => v.sh_RootClassConstraintComponent,
            Self::SomeValue(_) => v.sh_SomeValueConstraintComponent,
            Self::UniqueValuesFor(_) => v.sh_UniqueValuesForConstraintComponent,
        }
    }

    /// True for constraints that fault the focus node as a whole rather than an
    /// individual value, and so report no `sh:value`.
    pub fn is_focus_level(&self) -> bool {
        matches!(
            self,
            Self::MinCount(_)
                | Self::MaxCount(_)
                | Self::UniqueLang
                | Self::QualifiedValueShape { .. }
        )
    }
}

/// A compiled shape.
#[derive(Debug, Clone)]
pub struct Shape {
    /// The shape's node in the shapes graph, reported as `sh:sourceShape`.
    pub node: TermId,
    /// `Some` for property shapes.
    pub path: Option<Path>,
    /// The raw `sh:path` node, carried so `sh:resultPath` can be serialised
    /// with its original structure.
    pub path_node: Option<TermId>,
    pub targets: Vec<Target>,
    pub constraints: Vec<Constraint>,
    pub severity: TermId,
    pub messages: Vec<TermId>,
    pub deactivated: bool,
}

impl Shape {
    fn placeholder(node: TermId, severity: TermId) -> Self {
        Self {
            node,
            path: None,
            path_node: None,
            targets: Vec::new(),
            constraints: Vec::new(),
            severity,
            messages: Vec::new(),
            deactivated: false,
        }
    }

    #[inline]
    pub fn is_property_shape(&self) -> bool {
        self.path.is_some()
    }
}

/// A whole shapes graph, compiled.
#[derive(Debug, Clone)]
pub struct Shapes {
    shapes: Vec<Shape>,
    by_node: HashMap<TermId, ShapeId>,
    /// Shapes carrying at least one target, i.e. the roots of validation.
    roots: Vec<ShapeId>,
}

impl Shapes {
    #[inline]
    pub fn get(&self, id: ShapeId) -> &Shape {
        &self.shapes[id.index()]
    }

    #[inline]
    pub fn id_of(&self, node: TermId) -> Option<ShapeId> {
        self.by_node.get(&node).copied()
    }

    #[inline]
    pub fn roots(&self) -> &[ShapeId] {
        &self.roots
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.shapes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }

    /// Compiles every shape reachable in `graph`.
    pub fn compile(graph: &Graph, store: &TermStore, vocab: &Vocab) -> Result<Self> {
        Compiler {
            graph,
            store,
            vocab,
            shapes: Vec::new(),
            by_node: HashMap::new(),
        }
        .run()
    }
}

// ------------------------------------------------------------------ compiler

struct Compiler<'a> {
    graph: &'a Graph,
    store: &'a TermStore,
    vocab: &'a Vocab,
    shapes: Vec<Shape>,
    by_node: HashMap<TermId, ShapeId>,
}

/// Every predicate whose subject is necessarily a shape.
fn constraint_predicates(v: &Vocab) -> [TermId; 30] {
    [
        v.sh_class,
        v.sh_datatype,
        v.sh_nodeKind,
        v.sh_minCount,
        v.sh_maxCount,
        v.sh_minExclusive,
        v.sh_minInclusive,
        v.sh_maxExclusive,
        v.sh_maxInclusive,
        v.sh_minLength,
        v.sh_maxLength,
        v.sh_pattern,
        v.sh_languageIn,
        v.sh_uniqueLang,
        v.sh_equals,
        v.sh_disjoint,
        v.sh_lessThan,
        v.sh_lessThanOrEquals,
        v.sh_not,
        v.sh_and,
        v.sh_or,
        v.sh_xone,
        v.sh_node,
        v.sh_property,
        v.sh_qualifiedValueShape,
        v.sh_closed,
        v.sh_hasValue,
        v.sh_in,
        v.sh_path,
        v.sh_severity,
    ]
}

fn target_predicates(v: &Vocab) -> [TermId; 4] {
    [
        v.sh_targetClass,
        v.sh_targetNode,
        v.sh_targetSubjectsOf,
        v.sh_targetObjectsOf,
    ]
}

impl<'a> Compiler<'a> {
    fn run(mut self) -> Result<Shapes> {
        let v = self.vocab;

        // A node is a shape if it is declared one, carries a target, or carries
        // any constraint parameter. Nested shapes are reached by recursion from
        // these, so they need no separate discovery.
        let mut candidates: Vec<TermId> = Vec::new();
        for &ty in &[v.sh_NodeShape, v.sh_PropertyShape] {
            candidates.extend(self.graph.subjects(v.rdf_type, ty));
        }
        for p in target_predicates(v).into_iter().chain(constraint_predicates(v)) {
            candidates.extend(self.graph.subjects_of(p));
        }
        candidates.sort_unstable();
        candidates.dedup();

        for node in candidates {
            self.shape_id(node)?;
        }

        let roots = (0..self.shapes.len() as u32)
            .map(ShapeId)
            .filter(|id| !self.shapes[id.index()].targets.is_empty())
            .collect();

        Ok(Shapes {
            shapes: self.shapes,
            by_node: self.by_node,
            roots,
        })
    }

    /// Returns the id for `node`, compiling it if this is the first sighting.
    ///
    /// The id is registered before the body is compiled, so a shapes graph in
    /// which two shapes refer to each other terminates instead of recursing
    /// forever.
    fn shape_id(&mut self, node: TermId) -> Result<ShapeId> {
        if let Some(&id) = self.by_node.get(&node) {
            return Ok(id);
        }
        let id = ShapeId(self.shapes.len() as u32);
        self.shapes
            .push(Shape::placeholder(node, self.vocab.sh_Violation));
        self.by_node.insert(node, id);

        let shape = self.compile_shape(node)?;
        self.shapes[id.index()] = shape;
        Ok(id)
    }

    fn compile_shape(&mut self, node: TermId) -> Result<Shape> {
        let v = self.vocab;
        let g = self.graph;

        let path_node = g.object(node, v.sh_path);
        let path = match path_node {
            Some(p) => Some(Path::compile(p, g, self.store, v)?),
            None => None,
        };

        let severity = g.object(node, v.sh_severity).unwrap_or(v.sh_Violation);
        let deactivated = g
            .object(node, v.sh_deactivated)
            .and_then(|t| self.store.lexical_form(t))
            .map(|s| s == "true")
            .unwrap_or(false);

        let mut shape = Shape {
            node,
            path,
            path_node,
            targets: self.compile_targets(node),
            constraints: Vec::new(),
            severity,
            messages: g.objects(node, v.sh_message).collect(),
            deactivated,
        };
        shape.constraints = self.compile_constraints(node)?;
        Ok(shape)
    }

    fn compile_targets(&self, node: TermId) -> Vec<Target> {
        let v = self.vocab;
        let g = self.graph;
        let mut targets = Vec::new();

        targets.extend(g.objects(node, v.sh_targetNode).map(Target::Node));
        targets.extend(g.objects(node, v.sh_targetClass).map(Target::Class));
        targets.extend(g.objects(node, v.sh_targetSubjectsOf).map(Target::SubjectsOf));
        targets.extend(g.objects(node, v.sh_targetObjectsOf).map(Target::ObjectsOf));

        // An IRI shape that is also a class targets its own instances.
        if self.store.is_iri(node)
            && (g.contains(node, v.rdf_type, v.rdfs_Class)
                || g.contains(node, v.rdf_type, v.sh_ShapeClass))
        {
            targets.push(Target::ImplicitClass(node));
        }
        targets
    }

    fn compile_constraints(&mut self, node: TermId) -> Result<Vec<Constraint>> {
        let v = self.vocab;
        let g = self.graph;
        let mut out = Vec::new();

        // --- value type
        for t in g.objects(node, v.sh_class) {
            out.push(Constraint::Class(self.alternatives(t)));
        }
        for t in g.objects(node, v.sh_datatype) {
            out.push(Constraint::Datatype(self.alternatives(t)));
        }
        for t in g.objects(node, v.sh_nodeKind) {
            let kinds = self
                .alternatives(t)
                .into_iter()
                .map(|k| {
                    NodeKind::from_term(k, v).ok_or_else(|| {
                        Error::Shape("sh:nodeKind is not a known node kind".into())
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            out.push(Constraint::NodeKind(kinds));
        }

        // --- cardinality
        for t in g.objects(node, v.sh_minCount) {
            out.push(Constraint::MinCount(self.uint(t, "sh:minCount")?));
        }
        for t in g.objects(node, v.sh_maxCount) {
            out.push(Constraint::MaxCount(self.uint(t, "sh:maxCount")?));
        }

        // --- value range
        for t in g.objects(node, v.sh_minExclusive) {
            out.push(Constraint::MinExclusive(t));
        }
        for t in g.objects(node, v.sh_minInclusive) {
            out.push(Constraint::MinInclusive(t));
        }
        for t in g.objects(node, v.sh_maxExclusive) {
            out.push(Constraint::MaxExclusive(t));
        }
        for t in g.objects(node, v.sh_maxInclusive) {
            out.push(Constraint::MaxInclusive(t));
        }

        // --- string based
        for t in g.objects(node, v.sh_minLength) {
            out.push(Constraint::MinLength(self.uint(t, "sh:minLength")?));
        }
        for t in g.objects(node, v.sh_maxLength) {
            out.push(Constraint::MaxLength(self.uint(t, "sh:maxLength")?));
        }
        for t in g.objects(node, v.sh_pattern) {
            let flags = g
                .object(node, v.sh_flags)
                .and_then(|f| self.store.lexical_form(f))
                .unwrap_or("");
            let pattern = self
                .store
                .lexical_form(t)
                .ok_or_else(|| Error::Shape("sh:pattern is not a string".into()))?;
            out.push(Constraint::Pattern {
                regex: build_regex(pattern, flags)?,
                source: t,
            });
        }
        for t in g.objects(node, v.sh_languageIn) {
            let langs = g
                .list(t, v)
                .ok_or_else(|| Error::Shape("sh:languageIn is not a well-formed list".into()))?;
            out.push(Constraint::LanguageIn(langs));
        }
        if g
            .object(node, v.sh_uniqueLang)
            .and_then(|t| self.store.lexical_form(t))
            .map(|s| s == "true")
            .unwrap_or(false)
        {
            out.push(Constraint::UniqueLang);
        }

        // --- property pair
        for (pred, wrap) in [
            (v.sh_equals, Constraint::Equals as fn(Path) -> Constraint),
            (v.sh_disjoint, Constraint::Disjoint),
            (v.sh_lessThan, Constraint::LessThan),
            (v.sh_lessThanOrEquals, Constraint::LessThanOrEquals),
        ] {
            for t in g.objects(node, pred) {
                out.push(wrap(Path::compile(t, g, self.store, v)?));
            }
        }

        // --- logical
        for t in g.objects(node, v.sh_not) {
            let id = self.shape_id(t)?;
            out.push(Constraint::Not(id));
        }
        for (pred, wrap) in [
            (v.sh_and, Constraint::And as fn(Vec<ShapeId>) -> Constraint),
            (v.sh_or, Constraint::Or),
            (v.sh_xone, Constraint::Xone),
        ] {
            for t in g.objects(node, pred) {
                let members = g
                    .list(t, v)
                    .ok_or_else(|| Error::Shape("logical constraint is not a list".into()))?;
                let ids = members
                    .into_iter()
                    .map(|m| self.shape_id(m))
                    .collect::<Result<Vec<_>>>()?;
                out.push(wrap(ids));
            }
        }

        // --- shape based
        for t in g.objects(node, v.sh_node) {
            let id = self.shape_id(t)?;
            out.push(Constraint::Node(id));
        }
        for t in g.objects(node, v.sh_property) {
            let id = self.shape_id(t)?;
            out.push(Constraint::Property(id));
        }
        for t in g.objects(node, v.sh_qualifiedValueShape) {
            let shape = self.shape_id(t)?;
            let min = g
                .object(node, v.sh_qualifiedMinCount)
                .map(|c| self.uint(c, "sh:qualifiedMinCount"))
                .transpose()?;
            let max = g
                .object(node, v.sh_qualifiedMaxCount)
                .map(|c| self.uint(c, "sh:qualifiedMaxCount"))
                .transpose()?;
            let disjoint = g
                .object(node, v.sh_qualifiedValueShapesDisjoint)
                .and_then(|d| self.store.lexical_form(d))
                .map(|s| s == "true")
                .unwrap_or(false);
            let siblings = if disjoint {
                self.sibling_qualified_shapes(node)?
            } else {
                Vec::new()
            };
            out.push(Constraint::QualifiedValueShape {
                shape,
                min,
                max,
                disjoint,
                siblings,
            });
        }

        // --- other
        if g
            .object(node, v.sh_closed)
            .and_then(|t| self.store.lexical_form(t))
            .map(|s| s == "true")
            .unwrap_or(false)
        {
            let ignored = g
                .object(node, v.sh_ignoredProperties)
                .and_then(|l| g.list(l, v))
                .unwrap_or_default();
            out.push(Constraint::Closed { ignored });
        }
        for t in g.objects(node, v.sh_hasValue) {
            out.push(Constraint::HasValue(t));
        }
        for t in g.objects(node, v.sh_in) {
            let items = g
                .list(t, v)
                .ok_or_else(|| Error::Shape("sh:in is not a well-formed list".into()))?;
            out.push(Constraint::In(items));
        }

        // --- SHACL 1.2
        for t in g.objects(node, v.sh_minListLength) {
            out.push(Constraint::MinListLength(self.uint(t, "sh:minListLength")?));
        }
        for t in g.objects(node, v.sh_maxListLength) {
            out.push(Constraint::MaxListLength(self.uint(t, "sh:maxListLength")?));
        }
        for t in g.objects(node, v.sh_memberShape) {
            let id = self.shape_id(t)?;
            out.push(Constraint::MemberShape(id));
        }
        if self.flag(node, v.sh_uniqueMembers) {
            out.push(Constraint::UniqueMembers);
        }
        if self.flag(node, v.sh_singleLine) {
            out.push(Constraint::SingleLine);
        }
        for t in g.objects(node, v.sh_subsetOf) {
            out.push(Constraint::SubsetOf(Path::compile(t, g, self.store, v)?));
        }
        for t in g.objects(node, v.sh_rootClass) {
            out.push(Constraint::RootClass(t));
        }
        for t in g.objects(node, v.sh_someValue) {
            let id = self.shape_id(t)?;
            out.push(Constraint::SomeValue(id));
        }
        for t in g.objects(node, v.sh_uniqueValuesFor) {
            // A list here is a composite key — several paths that must be
            // unique in combination — not a single sequence path.
            let paths = self
                .alternatives(t)
                .into_iter()
                .map(|p| Path::compile(p, g, self.store, v))
                .collect::<Result<Vec<_>>>()?;
            out.push(Constraint::UniqueValuesFor(paths));
        }

        Ok(out)
    }

    /// Reads a boolean-valued shape parameter, absent meaning false.
    fn flag(&self, node: TermId, pred: TermId) -> bool {
        self.graph
            .object(node, pred)
            .and_then(|t| self.store.lexical_form(t))
            .map(|s| s == "true")
            .unwrap_or(false)
    }

    /// The qualified value shapes of this shape's siblings.
    ///
    /// `sh:qualifiedValueShapesDisjoint` requires a value to not conform to any
    /// qualified shape of a sibling property shape — the sibling set being every
    /// other `sh:property` of the shapes that declare this one.
    fn sibling_qualified_shapes(&mut self, node: TermId) -> Result<Vec<ShapeId>> {
        let v = self.vocab;
        let g = self.graph;
        let mut out = Vec::new();
        for parent in g.subjects(v.sh_property, node) {
            for sibling in g.objects(parent, v.sh_property) {
                if sibling == node {
                    continue;
                }
                for qvs in g.objects(sibling, v.sh_qualifiedValueShape) {
                    out.push(self.shape_id(qvs)?);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Reads a parameter that may be either a single term or a list of
    /// alternatives.
    ///
    /// SHACL 1.2 widened `sh:class`, `sh:datatype` and `sh:nodeKind` to accept
    /// `( a b )` meaning "any of these". A bare IRI stays a one-element list, so
    /// 1.0 shapes compile unchanged.
    fn alternatives(&self, t: TermId) -> Vec<TermId> {
        // Only a blank node heading an `rdf:first` can be a list; an IRI is
        // always the value itself, even if it happens to have list properties.
        if self.store.is_blank(t) && self.graph.object(t, self.vocab.rdf_first).is_some() {
            if let Some(items) = self.graph.list(t, self.vocab) {
                return items;
            }
        }
        vec![t]
    }

    fn uint(&self, t: TermId, what: &str) -> Result<u32> {
        self.store
            .lexical_form(t)
            .and_then(|s| s.trim().parse::<u32>().ok())
            .ok_or_else(|| Error::Shape(format!("{what} is not a non-negative integer")))
    }
}

/// Translates an XPath regex and `sh:flags` into a Rust regex.
///
/// `sh:pattern` follows XPath `fn:matches`, which searches rather than anchors,
/// so the unanchored default is correct.
fn build_regex(pattern: &str, flags: &str) -> Result<Regex> {
    let mut inline = String::new();
    for f in flags.chars() {
        match f {
            'i' => inline.push('i'),
            's' => inline.push('s'),
            'm' => inline.push('m'),
            'x' => inline.push('x'),
            // `q` (literal) has no inline equivalent; handled below.
            'q' => {}
            other => {
                return Err(Error::Shape(format!("unsupported sh:flags value '{other}'")))
            }
        }
    }
    let body = if flags.contains('q') {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let full = if inline.is_empty() {
        body
    } else {
        format!("(?{inline}){body}")
    };
    Regex::new(&full).map_err(|e| Error::Shape(format!("invalid sh:pattern: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{loader, GraphBuilder};
    use oxrdfio::RdfFormat;

    const PREFIX: &str = "@prefix sh: <http://www.w3.org/ns/shacl#> .
        @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
        @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
        @prefix ex: <http://ex/> . ";

    fn compile(turtle: &str) -> (TermStore, Vocab, Shapes) {
        let mut store = TermStore::new();
        let vocab = Vocab::new(&mut store);
        let mut b = GraphBuilder::new();
        loader::parse_str(
            &format!("{PREFIX}{turtle}"),
            RdfFormat::Turtle,
            "http://t/",
            1,
            &mut store,
            &mut b,
        )
        .unwrap();
        let g = b.build();
        let shapes = Shapes::compile(&g, &store, &vocab).unwrap();
        (store, vocab, shapes)
    }

    fn shape_of<'a>(s: &'a Shapes, store: &mut TermStore, iri: &str) -> &'a Shape {
        let node = store.named_node(iri);
        s.get(s.id_of(node).expect("shape was compiled"))
    }

    #[test]
    fn compiles_targets() {
        let (mut store, _, s) = compile(
            "ex:S a sh:NodeShape ;
               sh:targetClass ex:C ; sh:targetNode ex:n ;
               sh:targetSubjectsOf ex:p ; sh:targetObjectsOf ex:q .",
        );
        let shape = shape_of(&s, &mut store, "http://ex/S");
        assert_eq!(shape.targets.len(), 4);
        assert!(shape.targets.iter().any(|t| matches!(t, Target::Class(_))));
        assert!(shape.targets.iter().any(|t| matches!(t, Target::Node(_))));
        assert!(shape.targets.iter().any(|t| matches!(t, Target::SubjectsOf(_))));
        assert!(shape.targets.iter().any(|t| matches!(t, Target::ObjectsOf(_))));
        assert_eq!(s.roots().len(), 1);
    }

    #[test]
    fn a_shape_that_is_a_class_targets_its_instances() {
        let (mut store, _, s) = compile("ex:S a sh:NodeShape, rdfs:Class ; sh:datatype xsd:string .");
        let shape = shape_of(&s, &mut store, "http://ex/S");
        assert!(matches!(shape.targets[..], [Target::ImplicitClass(_)]));
    }

    #[test]
    fn shapes_without_targets_are_not_roots() {
        let (_, _, s) = compile("ex:S a sh:NodeShape ; sh:datatype xsd:string .");
        assert_eq!(s.len(), 1);
        assert!(s.roots().is_empty());
    }

    #[test]
    fn compiles_a_property_shape_with_its_path() {
        let (mut store, _, s) = compile(
            "ex:S a sh:NodeShape ; sh:targetNode ex:n ;
               sh:property [ sh:path ex:p ; sh:minCount 1 ; sh:maxCount 2 ] .",
        );
        let outer = shape_of(&s, &mut store, "http://ex/S");
        let Constraint::Property(pid) = &outer.constraints[0] else {
            panic!("expected sh:property, got {:?}", outer.constraints);
        };
        let inner = s.get(*pid);
        assert!(inner.is_property_shape());
        assert!(inner.path_node.is_some(), "raw path kept for sh:resultPath");
        assert!(matches!(
            inner.constraints[..],
            [Constraint::MinCount(1), Constraint::MaxCount(2)]
        ));
    }

    #[test]
    fn severity_and_deactivation_are_read() {
        let (mut store, v, s) = compile(
            "ex:S a sh:NodeShape ; sh:severity sh:Warning ; sh:deactivated true ;
                  sh:message \"nope\" ; sh:datatype xsd:string .
             ex:T a sh:NodeShape ; sh:datatype xsd:string .",
        );
        let a = shape_of(&s, &mut store, "http://ex/S");
        assert_eq!(a.severity, v.sh_Warning);
        assert!(a.deactivated);
        assert_eq!(a.messages.len(), 1);

        let b = shape_of(&s, &mut store, "http://ex/T");
        assert_eq!(b.severity, v.sh_Violation, "defaults to Violation");
        assert!(!b.deactivated);
    }

    #[test]
    fn compiles_logical_constraints_as_shape_references() {
        let (mut store, _, s) = compile(
            "ex:S a sh:NodeShape ; sh:targetNode ex:n ;
               sh:or ( [ sh:datatype xsd:string ] [ sh:datatype xsd:integer ] ) ;
               sh:not [ sh:nodeKind sh:IRI ] .",
        );
        let shape = shape_of(&s, &mut store, "http://ex/S");
        let or = shape
            .constraints
            .iter()
            .find_map(|c| match c {
                Constraint::Or(ids) => Some(ids),
                _ => None,
            })
            .expect("sh:or");
        assert_eq!(or.len(), 2);
        assert!(shape.constraints.iter().any(|c| matches!(c, Constraint::Not(_))));
    }

    #[test]
    fn mutually_recursive_shapes_terminate() {
        let (mut store, _, s) = compile(
            "ex:A a sh:NodeShape ; sh:targetNode ex:n ; sh:node ex:B .
             ex:B a sh:NodeShape ; sh:node ex:A .",
        );
        let a = shape_of(&s, &mut store, "http://ex/A");
        let Constraint::Node(b_id) = a.constraints[0] else {
            panic!("expected sh:node");
        };
        let b = s.get(b_id);
        assert!(matches!(b.constraints[0], Constraint::Node(_)));
    }

    #[test]
    fn compiles_list_valued_constraints() {
        let (mut store, _, s) = compile(
            "ex:S a sh:NodeShape ; sh:targetNode ex:n ;
               sh:in ( ex:a ex:b ) ; sh:languageIn ( \"en\" \"de\" ) ;
               sh:closed true ; sh:ignoredProperties ( rdf:type ) .",
        );
        let shape = shape_of(&s, &mut store, "http://ex/S");
        let has = |f: fn(&Constraint) -> bool| shape.constraints.iter().any(f);
        assert!(has(|c| matches!(c, Constraint::In(v) if v.len() == 2)));
        assert!(has(|c| matches!(c, Constraint::LanguageIn(v) if v.len() == 2)));
        assert!(has(|c| matches!(c, Constraint::Closed { ignored } if ignored.len() == 1)));
    }

    #[test]
    fn closed_false_produces_no_constraint() {
        let (mut store, _, s) = compile("ex:S a sh:NodeShape ; sh:closed false ; sh:datatype xsd:string .");
        let shape = shape_of(&s, &mut store, "http://ex/S");
        assert!(!shape
            .constraints
            .iter()
            .any(|c| matches!(c, Constraint::Closed { .. })));
    }

    #[test]
    fn compiles_pattern_with_flags() {
        let (mut store, _, s) =
            compile("ex:S a sh:NodeShape ; sh:pattern \"^a\" ; sh:flags \"i\" .");
        let shape = shape_of(&s, &mut store, "http://ex/S");
        let Constraint::Pattern { regex, .. } = &shape.constraints[0] else {
            panic!("expected sh:pattern");
        };
        assert!(regex.is_match("Abc"));
        assert!(!regex.is_match("bca"));
    }

    #[test]
    fn patterns_search_rather_than_anchor() {
        // XPath fn:matches, which sh:pattern follows, is a search.
        let (mut store, _, s) = compile("ex:S a sh:NodeShape ; sh:pattern \"b\" .");
        let shape = shape_of(&s, &mut store, "http://ex/S");
        let Constraint::Pattern { regex, .. } = &shape.constraints[0] else {
            panic!()
        };
        assert!(regex.is_match("abc"));
    }

    #[test]
    fn rejects_malformed_shapes() {
        let bad = |t: &str| {
            let mut store = TermStore::new();
            let vocab = Vocab::new(&mut store);
            let mut b = GraphBuilder::new();
            loader::parse_str(
                &format!("{PREFIX}{t}"),
                RdfFormat::Turtle,
                "http://t/",
                1,
                &mut store,
                &mut b,
            )
            .unwrap();
            Shapes::compile(&b.build(), &store, &vocab)
        };

        assert!(bad("ex:S sh:minCount \"lots\" .").is_err());
        assert!(bad("ex:S sh:nodeKind ex:Nonsense .").is_err());
        assert!(bad("ex:S sh:pattern \"[unclosed\" .").is_err());
        assert!(bad("ex:S sh:path 42 .").is_err());
    }
}
