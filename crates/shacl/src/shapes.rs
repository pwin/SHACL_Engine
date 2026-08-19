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
#[derive(Debug, Clone)]
pub enum Target {
    Node(TermId),
    Class(TermId),
    SubjectsOf(TermId),
    ObjectsOf(TermId),
    /// A shape that is itself an `rdfs:Class` implicitly targets its instances.
    ImplicitClass(TermId),
    /// `sh:targetWhere`: every node in the data graph conforming to the shape
    /// declared at this node.
    Where(TermId),
    /// `sh:target` naming a SPARQL selector: the focus nodes are whatever the
    /// query returns.
    Sparql(Box<crate::sparql::SparqlConstraint>),
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
        /// `sh:closed sh:ByTypes`: the permitted properties come from the
        /// shapes attached to the focus node's own types rather than from this
        /// shape alone.
        by_types: bool,
    },
    /// `sh:reifierShape`: the reifiers of each `(focus, path, value)` triple
    /// must conform to the given shape.
    ReifierShape(ShapeId),
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

    /// A `sh:sparql` constraint. Boxed because the parsed query dwarfs every
    /// other variant, and would otherwise set the size of all of them.
    Sparql(Box<crate::sparql::SparqlConstraint>),

    /// An instance of a user-declared constraint component.
    Custom(Box<CustomConstraint>),

    /// `sh:expression`: a node expression that must evaluate to `true` for
    /// each focus node.
    Expression(TermId),
    /// `sh:nodeByExpression`: like `sh:node`, but the shape to conform to is
    /// whatever a node expression yields.
    NodeByExpression(TermId),
}

/// One shape's use of a user-declared constraint component.
#[derive(Debug, Clone)]
pub struct CustomConstraint {
    /// The component IRI, reported as `sh:sourceConstraintComponent`.
    pub component: TermId,
    pub query: crate::sparql::SparqlConstraint,
    /// Parameter values from the shape, pre-bound by the local name of each
    /// parameter's `sh:path` — `ex:test1` binds `$test1`.
    pub bindings: Vec<(String, TermId)>,
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
            Self::ReifierShape(_) => v.sh_ReifierShapeConstraintComponent,
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
            Self::Sparql(_) => v.sh_SPARQLConstraintComponent,
            Self::Custom(c) => c.component,
            Self::Expression(_) => v.sh_ExpressionConstraintComponent,
            Self::NodeByExpression(_) => v.sh_NodeByExpressionConstraintComponent,
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
    /// SHACL-AF rules attached with `sh:rule`. Empty for almost every shape,
    /// and never consulted unless rules are asked for.
    pub rules: Vec<Rule>,
    /// `sh:order`, which SHACL core defines for presentation and SHACL-AF
    /// reuses to sequence rule execution. Absent means 0.
    pub order: f64,
}

/// A SHACL-AF rule: something that infers triples rather than reporting on
/// them.
///
/// Rules are the one part of SHACL that *changes* the data graph, so they are
/// kept firmly out of the validation path: nothing here runs unless a caller
/// asks for it, and what it produces is a new graph rather than a mutation of
/// the one it was given.
#[derive(Debug, Clone)]
pub struct Rule {
    /// The rule's node, for error messages.
    pub node: TermId,
    pub kind: RuleKind,
    /// `sh:condition`: shapes the focus node must conform to before the rule
    /// fires. All of them, not any.
    pub conditions: Vec<TermId>,
    pub order: f64,
    pub deactivated: bool,
}

#[derive(Debug, Clone)]
pub enum RuleKind {
    /// `sh:TripleRule`. Each component is a node expression, and the inferred
    /// triples are the cross product of what the three evaluate to.
    Triple {
        subject: TermId,
        predicate: TermId,
        object: TermId,
    },
    /// `sh:SPARQLRule`, whose `sh:construct` query is parsed once at compile
    /// time so it is not re-parsed per focus node.
    Sparql(Box<crate::sparql::SparqlConstraint>),
    /// A rule whose query would not compile, carrying why.
    ///
    /// Held rather than raised at compile time. A rule is only consulted when
    /// rules are asked for, so refusing the whole shapes graph would stop
    /// ordinary validation — which never reads rules — over a query it was
    /// never going to run. Raised when the rule would actually have fired, so
    /// it is not quietly skipped either.
    Broken(String),
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
            rules: Vec::new(),
            order: 0.0,
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
    /// Every compiled shape, in compilation order.
    ///
    /// The rules engine walks all of them rather than only the roots: a rule
    /// can hang off a property shape, which is never a root.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = &Shape> {
        self.shapes.iter()
    }

    /// The handle for the shape at `index` in [`Shapes::iter`] order.
    #[inline]
    pub fn id_at(&self, index: usize) -> ShapeId {
        ShapeId(index as u32)
    }

    /// True when no shape declares a rule, which is almost always.
    #[inline]
    pub fn has_rules(&self) -> bool {
        self.shapes.iter().any(|s| !s.rules.is_empty())
    }

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
            pending: Vec::new(),
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
    /// Shapes whose id exists but whose body is not compiled yet.
    pending: Vec<TermId>,
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
        for p in target_predicates(v)
            .into_iter()
            .chain(constraint_predicates(v))
        {
            candidates.extend(self.graph.subjects_of(p));
        }
        candidates.sort_unstable();
        candidates.dedup();

        for node in candidates {
            self.shape_id(node)?;
        }
        // Roots are chosen by looking at compiled targets, so everything the
        // queue holds has to be compiled first.
        self.drain()?;

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
        // Queued rather than compiled here. A constraint only needs the *id*
        // of the shape it points at, and that is known the moment the
        // placeholder is pushed, so nothing has to wait for the body.
        self.pending.push(node);
        Ok(id)
    }

    /// Compiles every queued shape, including any queued while compiling.
    ///
    /// Compiling used to recurse per nested shape reference, so a shapes graph
    /// nesting `sh:node` a few hundred deep overflowed the stack before
    /// validation could apply any limit of its own. The queue is on the heap,
    /// so the only bound now is how many shapes the graph declares.
    fn drain(&mut self) -> Result<()> {
        while let Some(node) = self.pending.pop() {
            let id = self.by_node[&node];
            let shape = self.compile_shape(node)?;
            self.shapes[id.index()] = shape;
        }
        Ok(())
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

        // Constraints are compiled before the shape is assembled because a
        // `sh:sparql` constraint needs the path, to expand `$PATH`.
        let targets = self.compile_targets(node);
        let constraints = self.compile_constraints(node, path.as_ref())?;

        let rules = self.compile_rules(node)?;

        Ok(Shape {
            node,
            path,
            path_node,
            targets,
            constraints,
            severity,
            messages: g.objects(node, v.sh_message).collect(),
            deactivated,
            rules,
            order: self.number(node, v.sh_order).unwrap_or(0.0),
        })
    }

    /// Reads a numeric literal, for `sh:order`.
    fn number(&self, node: TermId, predicate: TermId) -> Option<f64> {
        self.graph
            .object(node, predicate)
            .and_then(|t| self.store.lexical_form(t))
            .and_then(|s| s.parse().ok())
    }

    /// Compiles the SHACL-AF rules attached to `node` with `sh:rule`.
    ///
    /// A rule whose type cannot be told is an error rather than something to
    /// skip: silently inferring nothing is the failure mode that makes rules
    /// look supported when they are not.
    fn compile_rules(&mut self, node: TermId) -> Result<Vec<Rule>> {
        let v = self.vocab;
        let g = self.graph;
        let mut out = Vec::new();

        for rule in g.objects(node, v.sh_rule).collect::<Vec<_>>() {
            let kind = if let Some(construct) = g.object(rule, v.sh_construct) {
                let text = self.store.lexical_form(construct).unwrap_or("").to_string();
                match crate::sparql::SparqlConstraint::compile_construct(
                    &text, rule, g, self.store, v,
                ) {
                    Ok(q) => RuleKind::Sparql(Box::new(q)),
                    Err(e) => RuleKind::Broken(e.to_string()),
                }
            } else if let (Some(subject), Some(predicate), Some(object)) = (
                g.object(rule, v.sh_subject),
                g.object(rule, v.sh_predicate),
                g.object(rule, v.sh_object),
            ) {
                RuleKind::Triple {
                    subject,
                    predicate,
                    object,
                }
            } else {
                // Held rather than raised, for the reason `RuleKind::Broken`
                // gives: this shapes graph may never be asked to run rules.
                RuleKind::Broken(format!(
                    "rule on {} is neither a triple rule (sh:subject, sh:predicate, sh:object) \
                     nor a SPARQL rule (sh:construct)",
                    self.store.to_oxrdf(node)
                ))
            };

            out.push(Rule {
                node: rule,
                kind,
                conditions: g.objects(rule, v.sh_condition).collect(),
                order: self.number(rule, v.sh_order).unwrap_or(0.0),
                deactivated: g
                    .object(rule, v.sh_deactivated)
                    .and_then(|t| self.store.lexical_form(t))
                    .map(|s| s == "true")
                    .unwrap_or(false),
            });
        }
        Ok(out)
    }

    fn compile_targets(&mut self, node: TermId) -> Vec<Target> {
        let v = self.vocab;
        let g = self.graph;
        let mut targets = Vec::new();

        // A selector node is handled below, not as a literal target.
        targets.extend(
            g.objects(node, v.sh_targetNode)
                .filter(|&t| g.object(t, v.sh_select).is_none())
                .map(Target::Node),
        );
        targets.extend(g.objects(node, v.sh_targetClass).map(Target::Class));
        targets.extend(
            g.objects(node, v.sh_targetSubjectsOf)
                .map(Target::SubjectsOf),
        );
        targets.extend(g.objects(node, v.sh_targetObjectsOf).map(Target::ObjectsOf));
        targets.extend(g.objects(node, v.sh_targetWhere).map(Target::Where));

        // Either `sh:target` or `sh:targetNode` may name a SPARQL selector
        // instead of a term: `sh:targetNode [ sh:select ... ]` selects nodes by
        // query rather than listing them.
        let selectors: Vec<TermId> = g
            .objects(node, v.sh_target)
            .chain(g.objects(node, v.sh_targetNode))
            .filter(|&t| g.object(t, v.sh_select).is_some())
            .collect();
        for t in selectors {
            if let Ok(q) = self.compile_sparql(t, None) {
                targets.push(Target::Sparql(Box::new(q)));
            }
        }

        // An IRI shape that is also a class targets its own instances.
        if self.store.is_iri(node)
            && (g.contains(node, v.rdf_type, v.rdfs_Class)
                || g.contains(node, v.rdf_type, v.sh_ShapeClass))
        {
            targets.push(Target::ImplicitClass(node));
        }
        targets
    }

    fn compile_constraints(
        &mut self,
        node: TermId,
        path: Option<&Path>,
    ) -> Result<Vec<Constraint>> {
        let v = self.vocab;
        let g = self.graph;
        let mut out = Vec::new();

        // --- value type
        for t in self.objects_active(node, v.sh_class) {
            out.push(Constraint::Class(self.alternatives(t)));
        }
        for t in self.objects_active(node, v.sh_datatype) {
            out.push(Constraint::Datatype(self.alternatives(t)));
        }
        for t in self.objects_active(node, v.sh_nodeKind) {
            let kinds = self
                .alternatives(t)
                .into_iter()
                .map(|k| {
                    NodeKind::from_term(k, v)
                        .ok_or_else(|| Error::Shape("sh:nodeKind is not a known node kind".into()))
                })
                .collect::<Result<Vec<_>>>()?;
            out.push(Constraint::NodeKind(kinds));
        }

        // --- cardinality
        for t in self.objects_active(node, v.sh_minCount) {
            out.push(Constraint::MinCount(self.uint(t, "sh:minCount")?));
        }
        for t in self.objects_active(node, v.sh_maxCount) {
            out.push(Constraint::MaxCount(self.uint(t, "sh:maxCount")?));
        }

        // --- value range
        for t in self.objects_active(node, v.sh_minExclusive) {
            out.push(Constraint::MinExclusive(t));
        }
        for t in self.objects_active(node, v.sh_minInclusive) {
            out.push(Constraint::MinInclusive(t));
        }
        for t in self.objects_active(node, v.sh_maxExclusive) {
            out.push(Constraint::MaxExclusive(t));
        }
        for t in self.objects_active(node, v.sh_maxInclusive) {
            out.push(Constraint::MaxInclusive(t));
        }

        // --- string based
        for t in self.objects_active(node, v.sh_minLength) {
            out.push(Constraint::MinLength(self.uint(t, "sh:minLength")?));
        }
        for t in self.objects_active(node, v.sh_maxLength) {
            out.push(Constraint::MaxLength(self.uint(t, "sh:maxLength")?));
        }
        for t in self.objects_active(node, v.sh_pattern) {
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
        for t in self.objects_active(node, v.sh_languageIn) {
            let langs = g
                .list(t, v)
                .ok_or_else(|| Error::Shape("sh:languageIn is not a well-formed list".into()))?;
            out.push(Constraint::LanguageIn(langs));
        }
        if g.object(node, v.sh_uniqueLang)
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
        for t in self.objects_active(node, v.sh_not) {
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
        for t in self.objects_active(node, v.sh_node) {
            let id = self.shape_id(t)?;
            out.push(Constraint::Node(id));
        }
        for t in self.objects_active(node, v.sh_property) {
            let id = self.shape_id(t)?;
            out.push(Constraint::Property(id));
        }
        for t in self.objects_active(node, v.sh_qualifiedValueShape) {
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
        // `sh:closed` is a boolean in SHACL 1.0 and may also be
        // `sh:ByTypes` in 1.2, which widens the permitted set to whatever the
        // focus node's own types declare.
        if let Some(mode) = g.object(node, v.sh_closed) {
            let lex = self.store.lexical_form(mode).unwrap_or_default();
            let by_types = mode == v.sh_ByTypes;
            if by_types || lex == "true" {
                let ignored = g
                    .object(node, v.sh_ignoredProperties)
                    .and_then(|l| g.list(l, v))
                    .unwrap_or_default();
                out.push(Constraint::Closed { ignored, by_types });
            }
        }
        for t in self.objects_active(node, v.sh_hasValue) {
            out.push(Constraint::HasValue(t));
        }
        for t in self.objects_active(node, v.sh_in) {
            let items = g
                .list(t, v)
                .ok_or_else(|| Error::Shape("sh:in is not a well-formed list".into()))?;
            out.push(Constraint::In(items));
        }

        // --- SHACL 1.2
        for t in self.objects_active(node, v.sh_minListLength) {
            out.push(Constraint::MinListLength(self.uint(t, "sh:minListLength")?));
        }
        for t in self.objects_active(node, v.sh_maxListLength) {
            out.push(Constraint::MaxListLength(self.uint(t, "sh:maxListLength")?));
        }
        for t in self.objects_active(node, v.sh_reifierShape) {
            let id = self.shape_id(t)?;
            out.push(Constraint::ReifierShape(id));
        }
        for t in self.objects_active(node, v.sh_memberShape) {
            let id = self.shape_id(t)?;
            out.push(Constraint::MemberShape(id));
        }
        if self.flag(node, v.sh_uniqueMembers) {
            out.push(Constraint::UniqueMembers);
        }
        if self.flag(node, v.sh_singleLine) {
            out.push(Constraint::SingleLine);
        }
        for t in self.objects_active(node, v.sh_subsetOf) {
            out.push(Constraint::SubsetOf(Path::compile(t, g, self.store, v)?));
        }
        for t in self.objects_active(node, v.sh_rootClass) {
            out.push(Constraint::RootClass(t));
        }
        for t in self.objects_active(node, v.sh_someValue) {
            let id = self.shape_id(t)?;
            out.push(Constraint::SomeValue(id));
        }
        for t in self.objects_active(node, v.sh_uniqueValuesFor) {
            // A list here is a composite key — several paths that must be
            // unique in combination — not a single sequence path.
            let paths = self
                .alternatives(t)
                .into_iter()
                .map(|p| Path::compile(p, g, self.store, v))
                .collect::<Result<Vec<_>>>()?;
            out.push(Constraint::UniqueValuesFor(paths));
        }

        // --- SPARQL based
        for node_c in g.objects(node, v.sh_sparql) {
            out.push(Constraint::Sparql(Box::new(
                self.compile_sparql(node_c, path)?,
            )));
        }

        // --- node expressions
        for t in self.objects_active(node, v.sh_expression) {
            out.push(Constraint::Expression(t));
        }
        for t in self.objects_active(node, v.sh_nodeByExpression) {
            out.push(Constraint::NodeByExpression(t));
        }

        // --- user-declared constraint components
        self.compile_custom(node, path, &mut out)?;

        Ok(out)
    }

    /// Instantiates every declared constraint component whose parameters this
    /// shape supplies.
    ///
    /// A component applies only when the shape carries a value for each of its
    /// non-optional parameters; a shape mentioning just some of them does not
    /// trigger it.
    fn compile_custom(
        &mut self,
        node: TermId,
        path: Option<&Path>,
        out: &mut Vec<Constraint>,
    ) -> Result<()> {
        let v = self.vocab;
        let g = self.graph;

        // A constraint component is exactly a node declaring parameters.
        let components: Vec<TermId> = {
            let mut c: Vec<TermId> = g.subjects_of(v.sh_parameter).collect();
            c.sort_unstable();
            c.dedup();
            c
        };

        for component in components {
            let mut bindings = Vec::new();
            let mut applies = true;
            for param in g.objects(component, v.sh_parameter) {
                let Some(param_path) = g.object(param, v.sh_path) else {
                    continue;
                };
                let optional = self.flag(param, v.sh_optional);
                match g.object(node, param_path) {
                    Some(value) => {
                        if let Some(name) = self.store.iri(param_path).map(local_name) {
                            bindings.push((name, value));
                        }
                    }
                    None if optional => {}
                    None => {
                        applies = false;
                        break;
                    }
                }
            }
            if !applies || bindings.is_empty() {
                continue;
            }

            // A component may offer separate validators per shape kind; the
            // generic `sh:validator` is the fallback.
            let specific = if path.is_some() {
                v.sh_propertyValidator
            } else {
                v.sh_nodeValidator
            };
            let Some(validator) = g
                .object(component, specific)
                .or_else(|| g.object(component, v.sh_validator))
            else {
                continue;
            };

            out.push(Constraint::Custom(Box::new(CustomConstraint {
                component,
                query: self.compile_sparql(validator, path)?,
                bindings,
            })));
        }
        Ok(())
    }

    /// Compiles a `sh:SPARQLConstraint`, parsing its query once so validation
    /// never re-parses.
    fn compile_sparql(
        &self,
        node: TermId,
        path: Option<&Path>,
    ) -> Result<crate::sparql::SparqlConstraint> {
        use crate::sparql;
        let v = self.vocab;
        let g = self.graph;

        let (text, is_ask) = match g.object(node, v.sh_select) {
            Some(t) => (t, false),
            None => match g.object(node, v.sh_ask) {
                Some(t) => (t, true),
                None => {
                    return Err(Error::Shape(
                        "sh:sparql needs either sh:select or sh:ask".into(),
                    ));
                }
            },
        };
        let text = self
            .store
            .lexical_form(text)
            .ok_or_else(|| Error::Shape("SPARQL query is not a string".into()))?;

        // `$PATH` is a textual substitution, not a pre-bound variable: for a
        // property shape it stands for the path's SPARQL syntax, which for
        // anything but a bare predicate cannot be a term.
        let text = match path {
            Some(p) if text.contains("$PATH") => text.replace("$PATH", &p.to_sparql(self.store)),
            _ => text.to_string(),
        };

        let header = sparql::prefix_header(node, g, self.store, v);
        let query = sparql::parse_query(&header, &text)?;

        Ok(sparql::SparqlConstraint {
            query,
            is_ask,
            source: node,
            message: g.objects(node, v.sh_message).collect(),
            severity: g.object(node, v.sh_severity),
        })
    }

    /// The objects of `node pred ?o`, minus any statement annotated
    /// `sh:deactivated true`.
    ///
    /// RDF 1.2 annotation syntax lets a single constraint be switched off
    /// without removing it: `sh:datatype xsd:boolean {| sh:deactivated true |}`
    /// reifies that one triple and marks the reifier. So deactivation is a
    /// property of the statement, not of the shape.
    fn objects_active(&self, node: TermId, pred: TermId) -> Vec<TermId> {
        self.graph
            .objects(node, pred)
            .filter(|&o| !self.statement_deactivated(node, pred, o))
            .collect()
    }

    fn statement_deactivated(&self, s: TermId, p: TermId, o: TermId) -> bool {
        let Some(tt) = self.store.get_triple_term(s, p, o) else {
            return false;
        };
        self.graph
            .subjects(self.vocab.rdf_reifies, tt)
            .any(|r| self.flag(r, self.vocab.sh_deactivated))
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
        if self.store.is_blank(t)
            && self.graph.object(t, self.vocab.rdf_first).is_some()
            && let Some(items) = self.graph.list(t, self.vocab)
        {
            return items;
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

/// The local name of an IRI: whatever follows the last `#` or `/`.
///
/// Constraint component parameters bind by local name, so `ex:test1` supplies
/// the SPARQL variable `$test1`.
fn local_name(iri: &str) -> String {
    iri.rsplit_once(['#', '/'])
        .map(|(_, local)| local.to_string())
        .unwrap_or_else(|| iri.to_string())
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
                return Err(Error::Shape(format!(
                    "unsupported sh:flags value '{other}'"
                )));
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
    use crate::model::{GraphBuilder, loader};
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
        assert!(
            shape
                .targets
                .iter()
                .any(|t| matches!(t, Target::SubjectsOf(_)))
        );
        assert!(
            shape
                .targets
                .iter()
                .any(|t| matches!(t, Target::ObjectsOf(_)))
        );
        assert_eq!(s.roots().len(), 1);
    }

    #[test]
    fn a_shape_that_is_a_class_targets_its_instances() {
        let (mut store, _, s) =
            compile("ex:S a sh:NodeShape, rdfs:Class ; sh:datatype xsd:string .");
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
        assert!(
            shape
                .constraints
                .iter()
                .any(|c| matches!(c, Constraint::Not(_)))
        );
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
        assert!(has(
            |c| matches!(c, Constraint::LanguageIn(v) if v.len() == 2)
        ));
        assert!(has(
            |c| matches!(c, Constraint::Closed { ignored, .. } if ignored.len() == 1)
        ));
    }

    #[test]
    fn closed_false_produces_no_constraint() {
        let (mut store, _, s) =
            compile("ex:S a sh:NodeShape ; sh:closed false ; sh:datatype xsd:string .");
        let shape = shape_of(&s, &mut store, "http://ex/S");
        assert!(
            !shape
                .constraints
                .iter()
                .any(|c| matches!(c, Constraint::Closed { .. }))
        );
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
