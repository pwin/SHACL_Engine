//! The validation engine.
//!
//! Validation runs shape-at-a-time. Each shape resolves its targets into a
//! focus set, evaluates its path over that whole set at once to obtain the
//! focus→values relation, and then hands that relation to each of its
//! constraints. Constraints that are per-value walk the rows; constraints that
//! are per-focus-node — cardinality, `sh:hasValue`, `sh:uniqueLang` — read row
//! lengths and contents directly.

use std::cmp::Ordering;

use crate::datatypes;
use crate::error::Result;
use crate::model::{Graph, TermId, TermKind, TermStore, Vocab};
use crate::path::Path;
use crate::report::{ValidationReport, ValidationResult};
use crate::shapes::{Constraint, NodeKind, Shape, ShapeId, Shapes, Target};
use crate::valueset::ValueSets;

/// Validates `data` against `shapes_graph`.
pub fn validate(
    data: &Graph,
    shapes_graph: &Graph,
    store: &mut TermStore,
    vocab: &Vocab,
) -> Result<ValidationReport> {
    let shapes = Shapes::compile(shapes_graph, store, vocab)?;
    validate_with(data, &shapes, store, vocab)
}

/// Validates against an already-compiled shapes graph.
///
/// Separate from [`validate`] so a shapes graph can be compiled once and reused
/// across many data graphs.
pub fn validate_with(
    data: &Graph,
    shapes: &Shapes,
    store: &TermStore,
    vocab: &Vocab,
) -> Result<ValidationReport> {
    let engine = Engine {
        data,
        shapes,
        store,
        vocab,
    };
    let mut results = Vec::new();
    let mut stack = Vec::new();
    for &root in shapes.roots() {
        let focus = engine.focus_nodes(shapes.get(root));
        engine.validate_shape(root, &focus, &mut results, &mut stack)?;
    }
    Ok(ValidationReport { results })
}

struct Engine<'a> {
    data: &'a Graph,
    shapes: &'a Shapes,
    store: &'a TermStore,
    vocab: &'a Vocab,
}

/// Shape/node pairs currently being validated, used to break recursion.
type Stack = Vec<(ShapeId, TermId)>;

impl Engine<'_> {
    // ------------------------------------------------------------- targeting

    fn focus_nodes(&self, shape: &Shape) -> Vec<TermId> {
        let mut out = Vec::new();
        for target in &shape.targets {
            match target {
                Target::Node(n) => out.push(*n),
                Target::Class(c) | Target::ImplicitClass(c) => self.instances_of(*c, &mut out),
                Target::SubjectsOf(p) => out.extend(self.data.subjects_of(*p)),
                Target::ObjectsOf(p) => out.extend(self.data.objects_of(*p)),
            }
        }
        sort_dedup(&mut out);
        out
    }

    /// Every node that is a SHACL instance of `class`, i.e. typed with `class`
    /// or with any of its subclasses.
    fn instances_of(&self, class: TermId, out: &mut Vec<TermId>) {
        for sub in self.subclasses(class) {
            out.extend(self.data.subjects(self.vocab.rdf_type, sub));
        }
    }

    /// `class` together with everything below it under `rdfs:subClassOf`.
    ///
    /// The hierarchy is read from the data graph, which is where SHACL says
    /// class membership lives.
    fn subclasses(&self, class: TermId) -> Vec<TermId> {
        let mut seen = vec![class];
        let mut queue = vec![class];
        while let Some(c) = queue.pop() {
            for sub in self.data.subjects(self.vocab.rdfs_subClassOf, c) {
                if !seen.contains(&sub) {
                    seen.push(sub);
                    queue.push(sub);
                }
            }
        }
        seen
    }

    fn is_instance(&self, node: TermId, classes: &[TermId]) -> bool {
        self.data
            .objects(node, self.vocab.rdf_type)
            .any(|t| classes.contains(&t))
    }

    // ------------------------------------------------------------ validation

    fn validate_shape(
        &self,
        id: ShapeId,
        focus: &[TermId],
        out: &mut Vec<ValidationResult>,
        stack: &mut Stack,
    ) -> Result<()> {
        let shape = self.shapes.get(id);
        if shape.deactivated || focus.is_empty() {
            return Ok(());
        }
        let sets = match &shape.path {
            Some(p) => p.eval_sets(focus, self.data),
            None => ValueSets::identity(focus),
        };
        for constraint in &shape.constraints {
            self.eval(shape, constraint, &sets, out, stack)?;
        }
        Ok(())
    }

    /// Whether `node` conforms to the shape, producing no results.
    fn conforms(&self, id: ShapeId, node: TermId, stack: &mut Stack) -> Result<bool> {
        // A shapes graph may be recursive. The spec leaves recursion
        // undefined, so treat a repeat visit as conforming rather than
        // diverging.
        if stack.contains(&(id, node)) {
            return Ok(true);
        }
        stack.push((id, node));
        let mut scratch = Vec::new();
        let outcome = self.validate_shape(id, &[node], &mut scratch, stack);
        stack.pop();
        outcome?;
        Ok(scratch.is_empty())
    }

    /// A result carrying the fields every violation of `shape` shares.
    fn result(&self, shape: &Shape, component: TermId, focus: TermId) -> ValidationResult {
        let mut r = ValidationResult::new(focus, component, shape.severity)
            .with_path(shape.path_node)
            .with_source_shape(shape.node);
        r.messages.clone_from(&shape.messages);
        r
    }

    // ------------------------------------------------------------ constraints

    fn eval(
        &self,
        shape: &Shape,
        c: &Constraint,
        sets: &ValueSets,
        out: &mut Vec<ValidationResult>,
        stack: &mut Stack,
    ) -> Result<()> {
        let v = self.vocab;
        let component = c.component(v);

        // Emits a violation naming the offending value.
        macro_rules! per_value {
            ($ok:expr) => {{
                let ok = $ok;
                for row in sets.rows() {
                    for &value in row.values {
                        if !ok(value) {
                            out.push(self.result(shape, component, row.focus).with_value(value));
                        }
                    }
                }
            }};
        }

        match c {
            // --- value type
            // Each of these admits a list of alternatives; a value conforms if
            // it matches any one of them.
            Constraint::Class(classes) => {
                // The subclass closures are resolved once for the whole
                // relation rather than per value node.
                let subs: Vec<Vec<TermId>> =
                    classes.iter().map(|&c| self.subclasses(c)).collect();
                per_value!(|value| subs.iter().any(|s| self.is_instance(value, s)));
            }
            Constraint::Datatype(dts) => per_value!(|value| {
                dts.iter().any(|&dt| {
                    self.store.datatype(value) == Some(dt)
                        && datatypes::is_well_formed(
                            self.store.lexical_form(value).unwrap_or_default(),
                            dt,
                            v,
                        )
                })
            }),
            Constraint::NodeKind(kinds) => per_value!(|value| {
                let kind = self.store.kind(value);
                kinds.iter().any(|&k| node_kind_matches(kind, k))
            }),

            // --- cardinality, read straight off the row lengths
            Constraint::MinCount(n) => {
                for row in sets.rows() {
                    if (row.count() as u32) < *n {
                        out.push(self.result(shape, component, row.focus));
                    }
                }
            }
            Constraint::MaxCount(n) => {
                for row in sets.rows() {
                    if (row.count() as u32) > *n {
                        out.push(self.result(shape, component, row.focus));
                    }
                }
            }

            // --- value range
            Constraint::MinExclusive(b) => {
                per_value!(|value| self.cmp_is(value, *b, &[Ordering::Greater]))
            }
            Constraint::MinInclusive(b) => {
                per_value!(|value| self.cmp_is(value, *b, &[Ordering::Greater, Ordering::Equal]))
            }
            Constraint::MaxExclusive(b) => {
                per_value!(|value| self.cmp_is(value, *b, &[Ordering::Less]))
            }
            Constraint::MaxInclusive(b) => {
                per_value!(|value| self.cmp_is(value, *b, &[Ordering::Less, Ordering::Equal]))
            }

            // --- string based
            Constraint::MinLength(n) => {
                per_value!(|value| self.str_len(value).is_some_and(|l| l >= *n as usize))
            }
            Constraint::MaxLength(n) => {
                per_value!(|value| self.str_len(value).is_some_and(|l| l <= *n as usize))
            }
            Constraint::Pattern { regex, source } => {
                for row in sets.rows() {
                    for &value in row.values {
                        // Blank nodes have no lexical form to match against.
                        let ok = self.store.kind(value) != TermKind::Blank
                            && self
                                .store
                                .lexical_form(value)
                                .is_some_and(|s| regex.is_match(s));
                        if !ok {
                            let mut r =
                                self.result(shape, component, row.focus).with_value(value);
                            r.source_constraint = Some(*source);
                            out.push(r);
                        }
                    }
                }
            }
            Constraint::LanguageIn(ranges) => per_value!(|value| {
                self.store.language(value).is_some_and(|tag| {
                    ranges.iter().any(|&r| {
                        self.store
                            .lexical_form(r)
                            .is_some_and(|range| datatypes::language_matches(tag, range))
                    })
                })
            }),
            Constraint::UniqueLang => {
                for row in sets.rows() {
                    // The key includes the RDF 1.2 base direction: "A"@ar,
                    // "A"@ar--ltr and "A"@ar--rtl are three distinct tags, not
                    // three uses of "ar".
                    type LangKey<'k> = (&'k str, Option<crate::model::term::Direction>);
                    let mut seen: Vec<LangKey<'_>> = Vec::new();
                    let mut reported: Vec<LangKey<'_>> = Vec::new();
                    for &value in row.values {
                        let Some(tag) = self.store.language(value) else {
                            continue;
                        };
                        if tag.is_empty() {
                            continue;
                        }
                        let key = (tag, self.store.direction(value));
                        if seen.contains(&key) {
                            if !reported.contains(&key) {
                                reported.push(key);
                                out.push(self.result(shape, component, row.focus));
                            }
                        } else {
                            seen.push(key);
                        }
                    }
                }
            }

            // --- property pairs, all comparing against a sibling path
            Constraint::Equals(p) => {
                for row in sets.rows() {
                    let other = self.path_values(row.focus, p);
                    // Both directions: a value missing from either side faults.
                    for &value in row.values {
                        if !other.contains(&value) {
                            out.push(self.result(shape, component, row.focus).with_value(value));
                        }
                    }
                    for &value in &other {
                        if !row.values.contains(&value) {
                            out.push(self.result(shape, component, row.focus).with_value(value));
                        }
                    }
                }
            }
            Constraint::Disjoint(p) => {
                for row in sets.rows() {
                    let other = self.path_values(row.focus, p);
                    for &value in row.values {
                        if other.contains(&value) {
                            out.push(self.result(shape, component, row.focus).with_value(value));
                        }
                    }
                }
            }
            Constraint::LessThan(p) => {
                self.pair_order(shape, component, sets, p, &[Ordering::Less], out)
            }
            Constraint::LessThanOrEquals(p) => self.pair_order(
                shape,
                component,
                sets,
                p,
                &[Ordering::Less, Ordering::Equal],
                out,
            ),

            // --- logical
            Constraint::Not(inner) => {
                for row in sets.rows() {
                    for &value in row.values {
                        if self.conforms(*inner, value, stack)? {
                            out.push(self.result(shape, component, row.focus).with_value(value));
                        }
                    }
                }
            }
            Constraint::And(members) => {
                for row in sets.rows() {
                    for &value in row.values {
                        let mut all = true;
                        for &m in members {
                            if !self.conforms(m, value, stack)? {
                                all = false;
                                break;
                            }
                        }
                        if !all {
                            out.push(self.result(shape, component, row.focus).with_value(value));
                        }
                    }
                }
            }
            Constraint::Or(members) => {
                for row in sets.rows() {
                    for &value in row.values {
                        let mut any = false;
                        for &m in members {
                            if self.conforms(m, value, stack)? {
                                any = true;
                                break;
                            }
                        }
                        if !any {
                            out.push(self.result(shape, component, row.focus).with_value(value));
                        }
                    }
                }
            }
            Constraint::Xone(members) => {
                for row in sets.rows() {
                    for &value in row.values {
                        let mut n = 0;
                        for &m in members {
                            if self.conforms(m, value, stack)? {
                                n += 1;
                            }
                        }
                        if n != 1 {
                            out.push(self.result(shape, component, row.focus).with_value(value));
                        }
                    }
                }
            }

            // --- shape based
            Constraint::Node(inner) => {
                for row in sets.rows() {
                    for &value in row.values {
                        if !self.conforms(*inner, value, stack)? {
                            out.push(self.result(shape, component, row.focus).with_value(value));
                        }
                    }
                }
            }
            Constraint::Property(inner) => {
                // The nested shape's own results are reported directly, not
                // wrapped in a result for the property constraint itself.
                //
                // Values are deliberately not deduplicated across rows: the
                // nested shape is evaluated once per (focus node, value) pair,
                // so a value reached from two focus nodes is validated twice and
                // yields two results.
                self.validate_shape(*inner, sets.all_values(), out, stack)?;
            }
            Constraint::QualifiedValueShape {
                shape: qshape,
                min,
                max,
                disjoint,
                siblings,
            } => {
                for row in sets.rows() {
                    let mut n = 0u32;
                    for &value in row.values {
                        if !self.conforms(*qshape, value, stack)? {
                            continue;
                        }
                        if *disjoint {
                            let mut clashes = false;
                            for &s in siblings {
                                if self.conforms(s, value, stack)? {
                                    clashes = true;
                                    break;
                                }
                            }
                            if clashes {
                                continue;
                            }
                        }
                        n += 1;
                    }
                    if min.is_some_and(|m| n < m) {
                        out.push(self.result(
                            shape,
                            v.sh_QualifiedMinCountConstraintComponent,
                            row.focus,
                        ));
                    }
                    if max.is_some_and(|m| n > m) {
                        out.push(self.result(
                            shape,
                            v.sh_QualifiedMaxCountConstraintComponent,
                            row.focus,
                        ));
                    }
                }
            }

            // --- other
            Constraint::Closed { ignored } => {
                let allowed = self.closed_allowed(shape, ignored);
                for row in sets.rows() {
                    for (p, o) in self.data.predicate_objects(row.focus) {
                        if !allowed.contains(&p) {
                            // The offending predicate is the path here, not the
                            // enclosing shape's.
                            let mut r = self
                                .result(shape, component, row.focus)
                                .with_value(o);
                            r.path = Some(p);
                            out.push(r);
                        }
                    }
                }
            }
            Constraint::HasValue(wanted) => {
                for row in sets.rows() {
                    if !row.values.contains(wanted) {
                        out.push(self.result(shape, component, row.focus));
                    }
                }
            }
            Constraint::In(items) => per_value!(|value| items.contains(&value)),

            // --- SHACL 1.2
            Constraint::MinListLength(n) => {
                per_value!(|value| self.list_len(value).is_some_and(|l| l >= *n as usize))
            }
            Constraint::MaxListLength(n) => {
                per_value!(|value| self.list_len(value).is_some_and(|l| l <= *n as usize))
            }
            Constraint::MemberShape(inner) => {
                for row in sets.rows() {
                    for &value in row.values {
                        let Some(members) = self.data.list(value, v) else {
                            // Not a list at all: fault the value itself, with
                            // nothing to nest underneath.
                            out.push(self.result(shape, component, row.focus).with_value(value));
                            continue;
                        };
                        let mut nested = Vec::new();
                        self.validate_shape(*inner, &members, &mut nested, stack)?;
                        if !nested.is_empty() {
                            let mut r =
                                self.result(shape, component, row.focus).with_value(value);
                            r.details = nested;
                            out.push(r);
                        }
                    }
                }
            }
            Constraint::UniqueMembers => {
                for row in sets.rows() {
                    for &value in row.values {
                        let Some(members) = self.data.list(value, v) else {
                            out.push(self.result(shape, component, row.focus).with_value(value));
                            continue;
                        };
                        // One detail per member that occurs more than once.
                        let mut seen: Vec<TermId> = Vec::new();
                        let mut dupes: Vec<TermId> = Vec::new();
                        for m in members {
                            if seen.contains(&m) {
                                if !dupes.contains(&m) {
                                    dupes.push(m);
                                }
                            } else {
                                seen.push(m);
                            }
                        }
                        if !dupes.is_empty() {
                            let mut r =
                                self.result(shape, component, row.focus).with_value(value);
                            r.details = dupes
                                .into_iter()
                                .map(|m| {
                                    self.result(shape, component, row.focus).with_value(m)
                                })
                                .collect();
                            out.push(r);
                        }
                    }
                }
            }
            Constraint::SingleLine => per_value!(|value| {
                self.store
                    .lexical_form(value)
                    .is_some_and(|s| !s.contains(is_line_break))
            }),
            Constraint::SubsetOf(p) => {
                for row in sets.rows() {
                    let superset = self.path_values(row.focus, p);
                    for &value in row.values {
                        if !superset.contains(&value) {
                            out.push(self.result(shape, component, row.focus).with_value(value));
                        }
                    }
                }
            }
            Constraint::RootClass(root) => {
                let below = self.subclasses(*root);
                per_value!(|value| below.contains(&value));
            }
            Constraint::SomeValue(inner) => {
                for row in sets.rows() {
                    let mut any = false;
                    for &value in row.values {
                        if self.conforms(*inner, value, stack)? {
                            any = true;
                            break;
                        }
                    }
                    if !any {
                        out.push(self.result(shape, component, row.focus));
                    }
                }
            }
            Constraint::UniqueValuesFor(paths) => {
                // Uniqueness is a property of the focus set as a whole, so this
                // is evaluated across rows rather than within one.
                let keys: Vec<(TermId, Vec<Vec<TermId>>)> = sets
                    .rows()
                    .map(|row| {
                        let key = paths
                            .iter()
                            .map(|p| self.path_values(row.focus, p))
                            .collect();
                        (row.focus, key)
                    })
                    .collect();
                for (i, (focus, key)) in keys.iter().enumerate() {
                    // An absent key cannot clash with anything.
                    if key.iter().any(|k| k.is_empty()) {
                        continue;
                    }
                    let clashes = keys
                        .iter()
                        .enumerate()
                        .any(|(j, (_, other))| j != i && other == key);
                    if clashes {
                        out.push(self.result(shape, component, *focus));
                    }
                }
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- helpers

    fn cmp_is(&self, a: TermId, b: TermId, wanted: &[Ordering]) -> bool {
        datatypes::compare(a, b, self.store, self.vocab).is_some_and(|o| wanted.contains(&o))
    }

    /// Length in characters, or `None` for terms that have no lexical form to
    /// measure — blank nodes, which `sh:minLength`/`sh:maxLength` always fault.
    fn str_len(&self, t: TermId) -> Option<usize> {
        if self.store.kind(t) == TermKind::Blank {
            return None;
        }
        self.store.lexical_form(t).map(|s| s.chars().count())
    }

    /// The length of the RDF collection headed by `t`, or `None` if it is not a
    /// well-formed list — which the list constraints treat as a violation.
    fn list_len(&self, t: TermId) -> Option<usize> {
        self.data.list(t, self.vocab).map(|items| items.len())
    }

    /// The value nodes of `focus` under the compared path, as a set.
    fn path_values(&self, focus: TermId, p: &Path) -> Vec<TermId> {
        let mut v = Vec::new();
        p.eval(focus, self.data, &mut v);
        sort_dedup(&mut v);
        v
    }

    /// Faults every pair that does not stand in `wanted` order.
    ///
    /// One result per failing *pair*, not per failing value: a value node
    /// compared against two incomparable siblings yields two results, which is
    /// what the suite's expected reports contain.
    fn pair_order(
        &self,
        shape: &Shape,
        component: TermId,
        sets: &ValueSets,
        p: &Path,
        wanted: &[Ordering],
        out: &mut Vec<ValidationResult>,
    ) {
        for row in sets.rows() {
            let other = self.path_values(row.focus, p);
            for &value in row.values {
                for &o in &other {
                    if !self.cmp_is(value, o, wanted) {
                        out.push(self.result(shape, component, row.focus).with_value(value));
                    }
                }
            }
        }
    }

    /// Predicates a closed shape permits: those declared by its own property
    /// shapes, plus `sh:ignoredProperties`.
    fn closed_allowed(&self, shape: &Shape, ignored: &[TermId]) -> Vec<TermId> {
        let mut allowed = ignored.to_vec();
        for c in &shape.constraints {
            if let Constraint::Property(id) = c {
                if let Some(p) = self.shapes.get(*id).path.as_ref().and_then(|p| p.as_predicate())
                {
                    allowed.push(p);
                }
            }
        }
        sort_dedup(&mut allowed);
        allowed
    }
}

fn node_kind_matches(kind: TermKind, wanted: NodeKind) -> bool {
    match wanted {
        NodeKind::Iri => kind == TermKind::Iri,
        NodeKind::BlankNode => kind == TermKind::Blank,
        NodeKind::Literal => kind == TermKind::Literal,
        NodeKind::BlankNodeOrIri => matches!(kind, TermKind::Blank | TermKind::Iri),
        NodeKind::BlankNodeOrLiteral => matches!(kind, TermKind::Blank | TermKind::Literal),
        NodeKind::IriOrLiteral => matches!(kind, TermKind::Iri | TermKind::Literal),
    }
}

/// The characters `sh:singleLine` forbids: line feed, carriage return, form
/// feed and vertical tab.
fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{000C}' | '\u{000B}')
}

fn sort_dedup(v: &mut Vec<TermId>) {
    v.sort_unstable();
    v.dedup();
}
