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
use crate::error::{Error, Result};
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
    validate_in(data, &shapes, shapes_graph, store, vocab)
}

/// Validates against an already-compiled shapes graph.
///
/// Separate from [`validate`] so a shapes graph can be compiled once and reused
/// across many data graphs.
pub fn validate_with(
    data: &Graph,
    shapes: &Shapes,
    store: &mut TermStore,
    vocab: &Vocab,
) -> Result<ValidationReport> {
    validate_in(data, shapes, data, store, vocab)
}

/// Validates, naming the graph the shapes were compiled from.
///
/// Node expressions are written in the shapes graph, so evaluating
/// `sh:expression` needs it as well as the compiled form.
pub fn validate_in(
    data: &Graph,
    shapes: &Shapes,
    shapes_graph: &Graph,
    store: &mut TermStore,
    vocab: &Vocab,
) -> Result<ValidationReport> {
    validate_in_with(data, shapes, shapes_graph, store, vocab, Options::default())
}

/// As [`validate_in`], under [`Options`].
pub fn validate_in_with(
    data: &Graph,
    shapes: &Shapes,
    shapes_graph: &Graph,
    store: &mut TermStore,
    vocab: &Vocab,
    options: Options,
) -> Result<ValidationReport> {
    let engine = Engine {
        data,
        shapes,
        shapes_graph,
        vocab,
        shnex: crate::nodeexpr::Shnex::new(store),
        max_results: options.max_results,
        blocking: options.blocking.clone(),
    };
    let mut results = Vec::new();
    let mut stack = Stack::default();
    for &root in shapes.roots() {
        if engine.enough(&results) {
            break;
        }
        let focus = engine.focus_nodes(shapes.get(root), &mut stack, store)?;
        engine.validate_shape(root, &focus, &mut results, &mut stack, store)?;
    }

    // A data node may nominate its own shape with `sh:shape`. This is a target
    // declared from the data side rather than by the shape, so it cannot make
    // a shape a root — a nested property shape must not start validating on
    // its own account just because the vocabulary exists.
    let by_node: Vec<(TermId, TermId)> = data
        .subjects_of(vocab.sh_shape)
        .zip(data.objects_of(vocab.sh_shape))
        .collect();
    for (node, shape_node) in by_node {
        if engine.enough(&results) {
            break;
        }
        if let Some(id) = shapes.id_of(shape_node) {
            engine.validate_shape(id, &[node], &mut results, &mut stack, store)?;
        }
    }

    // A cap stops the work; it does not promise to land exactly on the number,
    // since the constraint in flight when the limit is reached finishes its
    // own row. Trimming here makes the report say what was asked for.
    if let Some(n) = options.max_results {
        match &options.blocking {
            // Cut just after the nth blocking result. A blind `truncate(n)`
            // could drop the only result that breaks conformance and leave
            // the report contradicting itself.
            Some(severities) => {
                let mut seen = 0;
                let mut cut = results.len();
                for (i, r) in results.iter().enumerate() {
                    if severities.contains(&r.severity) {
                        seen += 1;
                        if seen == n {
                            cut = i + 1;
                            break;
                        }
                    }
                }
                results.truncate(cut);
            }
            None => results.truncate(n),
        }
    }
    Ok(ValidationReport { results })
}

/// Whether `node` conforms to the shape declared at `shape_node`.
///
/// Exposed for node expressions, whose shape-valued operators — `shnex:
/// conformsToShape`, `filterShape`, `matchAll`, `findFirst` — all reduce to
/// this question.
pub fn node_conforms(
    node: TermId,
    shape_node: TermId,
    data: &Graph,
    shapes: &Shapes,
    store: &mut TermStore,
    vocab: &Vocab,
) -> Result<bool> {
    let Some(id) = shapes.id_of(shape_node) else {
        // A shape that declares no constraints constrains nothing.
        return Ok(true);
    };
    let engine = Engine {
        data,
        shapes,
        shapes_graph: data,
        vocab,
        shnex: crate::nodeexpr::Shnex::new(store),
        // A yes/no question, so one result is all it ever needs, and every
        // result counts: `sh:node` asks whether the shape produced anything,
        // not how severe it was.
        max_results: Some(1),
        blocking: None,
    };
    engine.conforms(id, node, &mut Stack::default(), store)
}

/// The focus nodes `shape` targets in `data`.
///
/// Exposed for the rules engine, which fires a shape's rules on exactly the
/// nodes that shape would validate — so it has to resolve targets the same
/// way, including the ones that need the validator itself (`sh:targetWhere`
/// tests conformance, and a SPARQL selector runs a query).
pub fn focus_nodes_of(
    shape_id: ShapeId,
    data: &Graph,
    shapes: &Shapes,
    shapes_graph: &Graph,
    store: &mut TermStore,
    vocab: &Vocab,
) -> Result<Vec<TermId>> {
    let engine = Engine {
        data,
        shapes,
        shapes_graph,
        vocab,
        shnex: crate::nodeexpr::Shnex::new(store),
        max_results: None,
        blocking: None,
    };
    engine.focus_nodes(shapes.get(shape_id), &mut Stack::default(), store)
}

struct Engine<'a> {
    data: &'a Graph,
    shapes: &'a Shapes,
    /// The shapes graph itself, which node expressions are written in.
    shapes_graph: &'a Graph,
    vocab: &'a Vocab,
    shnex: crate::nodeexpr::Shnex,
    /// Stop once this many blocking results are in hand. `None` reports
    /// everything.
    max_results: Option<usize>,
    /// Severities that count towards `max_results`; `None` counts them all.
    blocking: Option<Vec<TermId>>,
}

/// How much of the graph to validate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Abandon validation once this many *conformance-blocking* results exist.
    ///
    /// This is a real early exit, not a truncation of a finished report: the
    /// work stops, which is the only reason to ask for it. The consequence is
    /// that the report is no longer a complete account of the graph, so the
    /// count is the caller's choice rather than a default.
    pub max_results: Option<usize>,

    /// Which severities block conformance, and so count towards the cap.
    ///
    /// `None` counts every result, which is what an internal yes/no question
    /// wants: `sh:node` and friends care whether a shape produced anything at
    /// all, not how severe it was.
    ///
    /// A caller reporting to a user must pass the same severities it will
    /// later judge conformance by. Counting every result instead would let the
    /// run stop on an `sh:Info`, report `sh:conforms true`, and leave an
    /// `sh:Violation` sitting unexamined further along — the shapes are
    /// evaluated in whatever order they compiled in, so which kind is met
    /// first says nothing about what is in the graph.
    pub blocking: Option<Vec<TermId>>,
}

impl Options {
    /// Stop at the first result that would break conformance.
    pub fn first_blocking(severities: Vec<TermId>) -> Self {
        Self {
            max_results: Some(1),
            blocking: Some(severities),
        }
    }
}

/// The shape/node pairs currently being validated, used to break recursion.
#[derive(Default)]
struct Stack {
    pairs: Vec<(ShapeId, TermId)>,
    /// The same pairs, for membership in constant time.
    ///
    /// `pairs` keeps insertion order so a level can be unwound; it is a poor
    /// thing to *search*, and searching it is the common case. One level
    /// pushes a pair per focus node, so a node shape over N focus nodes leaves
    /// N entries behind, and every property shape beneath it then scanned all
    /// N once per node — quadratic in the size of the data, for the ordinary
    /// shape of a shapes graph rather than for any recursive edge case. At
    /// 100k instances that was 44 seconds of validation against 0.6 of load.
    seen: hashbrown::HashSet<(ShapeId, TermId)>,
    /// Nesting level, which `pairs.len()` does not give: one level pushes a
    /// pair per focus node rather than a single pair.
    depth: usize,
}

/// How deeply shapes may nest before validation is abandoned.
///
/// The visited set breaks *cycles*, but on its own it does not bound the
/// descent: the number of distinct (shape, node) pairs is the product of the
/// two, so a recursive shape over a long enough data chain still runs the
/// process out of stack. A stack overflow is not a panic and cannot be caught,
/// so it would take the host process with it — the Python bindings included,
/// whose whole error story is that a Rust failure becomes a Python exception.
///
/// The limit is deliberately well below where the stack actually runs out,
/// because how much stack a level costs is not fixed: a debug build overflows
/// somewhere between 96 and 128 levels of the *cheapest* possible shape, and a
/// level carrying a SPARQL constraint or a node expression costs considerably
/// more than one carrying `sh:datatype`. Since the guarantee being made here is
/// that the process never dies, the margin belongs on the safe side of that.
///
/// Measured rather than guessed, on the cheapest possible recursive shape —
/// one `sh:property` pointing at itself, walked down a chain of `ex:next`:
///
/// | build | chain length that kills the process |
/// | --- | --- |
/// | debug | under 100 |
/// | release | about 410 |
///
/// The constant has to be safe in the tightest of those, because that is where
/// the tests run and where a `cargo run` lands. 48 leaves roughly a factor of
/// two under the debug figure, and a level carrying a SPARQL constraint costs
/// more stack than the one measured, so the margin is not generous.
///
/// This is a real limit on real data, not a theoretical one. A recursive shape
/// spends one level per link, plus one for the shape it starts from, so a
/// chain of 47 links is refused — and an RDF collection of 47 items is a
/// 47-link `rdf:rest` chain. Raising the number is not available: the debug
/// build dies at 100, which is still short of an ordinary list.
///
/// Lifting it properly means moving the descent off the call stack. The depth
/// has two unrelated sources — shape nesting, which is written by hand and
/// genuinely shallow, and property descent, which is as deep as the data — and
/// only the second needs to become iterative. `sh:property` is the tractable
/// case: its results go straight to the output buffer and nothing reads its
/// return value, unlike `sh:node` or `sh:not`, which have to ask whether the
/// nested shape produced anything.
const MAX_DEPTH: usize = 48;

impl Engine<'_> {
    /// Whether enough results are in hand to stop.
    ///
    /// Checked between constraints and between shapes rather than after every
    /// individual result: those are the points where stopping is cheap and
    /// leaves the report coherent, and a per-result check would cost more on
    /// the overwhelmingly common uncapped path than it could ever save.
    ///
    /// Only blocking results count, so stopping can never turn a graph that
    /// does not conform into one that appears to. When nothing blocks, the
    /// cap is never reached and the whole graph is validated — which is the
    /// right answer, since that is the only way to be sure.
    fn enough(&self, results: &[ValidationResult]) -> bool {
        let Some(n) = self.max_results else {
            return false;
        };
        match &self.blocking {
            None => results.len() >= n,
            // Linear, but only ever walked while a cap is set, and a cap keeps
            // the list short by construction.
            Some(severities) => {
                results
                    .iter()
                    .filter(|r| severities.contains(&r.severity))
                    .count()
                    >= n
            }
        }
    }

    // ------------------------------------------------------------- targeting

    fn focus_nodes(
        &self,
        shape: &Shape,
        stack: &mut Stack,
        store: &mut TermStore,
    ) -> Result<Vec<TermId>> {
        let mut out = Vec::new();
        for target in &shape.targets {
            match target {
                Target::Node(n) => out.push(*n),
                Target::Class(c) | Target::ImplicitClass(c) => self.instances_of(*c, &mut out),
                Target::SubjectsOf(p) => out.extend(self.data.subjects_of(*p)),
                Target::ObjectsOf(p) => out.extend(self.data.objects_of(*p)),
                Target::Sparql(q) => {
                    // The selector binds one variable per focus node. SHACL
                    // names it `$this`, but a query is free to project
                    // something else, so the sole binding is taken when there
                    // is no `?this`.
                    for row in crate::sparql::run(&q.query, &[], self.data, store)? {
                        let picked = row.get("this").or_else(|| row.values().next()).cloned();
                        if let Some(t) = picked
                            && let Some(id) = crate::sparql::from_term(t.as_ref(), store)
                        {
                            out.push(id);
                        }
                    }
                }
                Target::Where(shape_node) => {
                    // Every node in the data that conforms to the given shape.
                    let Some(id) = self.shapes.id_of(*shape_node) else {
                        continue;
                    };
                    for candidate in all_nodes(self.data) {
                        if self.conforms(id, candidate, stack, store)? {
                            out.push(candidate);
                        }
                    }
                }
            }
        }
        sort_dedup(&mut out);
        Ok(out)
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

    // ------------------------------------------------------------ validation

    fn validate_shape(
        &self,
        id: ShapeId,
        focus: &[TermId],
        out: &mut Vec<ValidationResult>,
        stack: &mut Stack,
        store: &mut TermStore,
    ) -> Result<()> {
        let shape = self.shapes.get(id);
        if shape.deactivated || focus.is_empty() {
            return Ok(());
        }

        // A shapes graph may be recursive: `sh:property`, `sh:memberShape` and
        // `sh:reifierShape` can all reach a shape that reaches them back. Drop
        // the focus nodes already being validated against this shape, which
        // breaks the cycle and counts the repeat visit as conforming — the
        // reading the spec leaves open, and the one `conforms` depends on.
        //
        // Only the same (shape, node) pair is dropped, so a shape cycle walked
        // over a chain of distinct data nodes still runs to the end of the
        // chain; it is not truncated at the first repeated shape.
        //
        // Nothing can repeat while the stack is empty, so the top-level focus
        // set — much the largest — skips the filtering and its allocation.
        let filtered: Vec<TermId>;
        let focus = if stack.pairs.is_empty() {
            focus
        } else {
            filtered = focus
                .iter()
                .copied()
                .filter(|&n| !stack.seen.contains(&(id, n)))
                .collect();
            if filtered.is_empty() {
                return Ok(());
            }
            &filtered
        };
        if stack.depth >= MAX_DEPTH {
            return Err(Error::Recursion(format!(
                "shapes nested more than {MAX_DEPTH} deep; a recursive shape spends one level per link of the data it walks, so a chain longer than {} is refused",
                MAX_DEPTH - 2
            )));
        }

        let sets = match &shape.path {
            Some(p) => p.eval_sets(focus, self.data),
            None => ValueSets::identity(focus),
        };

        let mark = stack.pairs.len();
        stack.pairs.extend(focus.iter().map(|&n| (id, n)));
        stack.seen.extend(focus.iter().map(|&n| (id, n)));
        stack.depth += 1;
        let mut outcome = Ok(());
        for constraint in &shape.constraints {
            // `out` is whichever buffer this shape is filling, which for a
            // nested shape is a scratch one rather than the report. Stopping
            // early is still sound there, and is in fact the point: `conforms`
            // and the nested-detail constraints only ever ask whether their
            // buffer stayed empty, so once anything is in it the answer cannot
            // change however many more results would have followed.
            if self.enough(out) {
                break;
            }
            outcome = self.eval(shape, constraint, &sets, out, stack, store);
            if outcome.is_err() {
                break;
            }
        }
        stack.depth -= 1;
        // Unwind both together. Removing by value is idempotent, which is what
        // makes a repeated focus node — possible at the top level, where the
        // filter above is skipped because nothing can have been visited yet —
        // safe to push twice and drop once.
        for pair in &stack.pairs[mark..] {
            stack.seen.remove(pair);
        }
        stack.pairs.truncate(mark);
        outcome
    }

    /// Whether `node` conforms to the shape, producing no results.
    fn conforms(
        &self,
        id: ShapeId,
        node: TermId,
        stack: &mut Stack,
        store: &mut TermStore,
    ) -> Result<bool> {
        // The recursion guard lives in `validate_shape`, which this routes
        // through, so a repeat visit yields no results — which is to say it
        // conforms, the reading this has always taken.
        let mut scratch = Vec::new();
        self.validate_shape(id, &[node], &mut scratch, stack, store)?;
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
        store: &mut TermStore,
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
                // Materialise the instance set once and test membership by
                // binary search, rather than probing `rdf:type` per value node.
                //
                // The probe is the expensive part: it lands at a random offset
                // in an index that runs to tens of megabytes on a large graph,
                // so it misses cache nearly every time. The instance set is a
                // few hundred kilobytes and stays resident.
                let mut instances = Vec::new();
                for &c in classes {
                    self.instances_of(c, &mut instances);
                }
                sort_dedup(&mut instances);
                per_value!(|value| instances.binary_search(&value).is_ok());
            }
            Constraint::Datatype(dts) => per_value!(|value| {
                dts.iter().any(|&dt| {
                    store.datatype(value) == Some(dt)
                        && datatypes::is_well_formed(
                            store.lexical_form(value).unwrap_or_default(),
                            dt,
                            v,
                        )
                })
            }),
            Constraint::NodeKind(kinds) => per_value!(|value| {
                let kind = store.kind(value);
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
                per_value!(|value| self.cmp_is(value, *b, &[Ordering::Greater], store))
            }
            Constraint::MinInclusive(b) => {
                per_value!(|value| self.cmp_is(
                    value,
                    *b,
                    &[Ordering::Greater, Ordering::Equal],
                    store
                ))
            }
            Constraint::MaxExclusive(b) => {
                per_value!(|value| self.cmp_is(value, *b, &[Ordering::Less], store))
            }
            Constraint::MaxInclusive(b) => {
                per_value!(|value| self.cmp_is(
                    value,
                    *b,
                    &[Ordering::Less, Ordering::Equal],
                    store
                ))
            }

            // --- string based
            Constraint::MinLength(n) => {
                per_value!(|value| self.str_len(value, store).is_some_and(|l| l >= *n as usize))
            }
            Constraint::MaxLength(n) => {
                per_value!(|value| self.str_len(value, store).is_some_and(|l| l <= *n as usize))
            }
            Constraint::Pattern { regex, source } => {
                for row in sets.rows() {
                    for &value in row.values {
                        // Blank nodes have no lexical form to match against.
                        let ok = store.kind(value) != TermKind::Blank
                            && store.lexical_form(value).is_some_and(|s| regex.is_match(s));
                        if !ok {
                            let mut r = self.result(shape, component, row.focus).with_value(value);
                            r.source_constraint = Some(*source);
                            out.push(r);
                        }
                    }
                }
            }
            Constraint::LanguageIn(ranges) => per_value!(|value| {
                store.language(value).is_some_and(|tag| {
                    ranges.iter().any(|&r| {
                        store
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
                        let Some(tag) = store.language(value) else {
                            continue;
                        };
                        if tag.is_empty() {
                            continue;
                        }
                        let key = (tag, store.direction(value));
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
                self.pair_order(shape, component, sets, p, &[Ordering::Less], out, store)
            }
            Constraint::LessThanOrEquals(p) => self.pair_order(
                shape,
                component,
                sets,
                p,
                &[Ordering::Less, Ordering::Equal],
                out,
                store,
            ),

            // --- logical
            Constraint::Not(inner) => {
                for row in sets.rows() {
                    for &value in row.values {
                        if self.conforms(*inner, value, stack, store)? {
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
                            if !self.conforms(m, value, stack, store)? {
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
                            if self.conforms(m, value, stack, store)? {
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
                            if self.conforms(m, value, stack, store)? {
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
                        if !self.conforms(*inner, value, stack, store)? {
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
                self.validate_shape(*inner, sets.all_values(), out, stack, store)?;
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
                        if !self.conforms(*qshape, value, stack, store)? {
                            continue;
                        }
                        if *disjoint {
                            let mut clashes = false;
                            for &s in siblings {
                                if self.conforms(s, value, stack, store)? {
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
            Constraint::ReifierShape(inner) => {
                // The annotation syntax `{| ... |}` produces a node that
                // `rdf:reifies` the triple term, so the reifiers of a statement
                // are found by looking that term up.
                let Some(p) = shape.path.as_ref().and_then(|p| p.as_predicate()) else {
                    return Ok(());
                };
                for row in sets.rows() {
                    for &value in row.values {
                        let Some(tt) = store.get_triple_term(row.focus, p, value) else {
                            continue;
                        };
                        let reifiers: Vec<TermId> = self.data.subjects(v.rdf_reifies, tt).collect();
                        // Like `sh:node`, the nested results are not reported
                        // directly: a non-conforming reifier faults the value
                        // whose statement it annotates.
                        let mut nested = Vec::new();
                        self.validate_shape(*inner, &reifiers, &mut nested, stack, store)?;
                        if !nested.is_empty() {
                            out.push(self.result(shape, component, row.focus).with_value(value));
                        }
                    }
                }
            }
            Constraint::Closed { ignored, by_types } => {
                let allowed = if *by_types {
                    Vec::new()
                } else {
                    self.closed_allowed(shape, ignored)
                };
                for row in sets.rows() {
                    // Under `sh:ByTypes` the permitted set depends on the
                    // focus node, so it is recomputed per row.
                    let allowed = if *by_types {
                        self.closed_by_types(row.focus, ignored)
                    } else {
                        allowed.clone()
                    };
                    for (p, o) in self.data.predicate_objects(row.focus) {
                        if !allowed.contains(&p) {
                            // The offending predicate is the path here, not the
                            // enclosing shape's.
                            let mut r = self.result(shape, component, row.focus).with_value(o);
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
                        self.validate_shape(*inner, &members, &mut nested, stack, store)?;
                        if !nested.is_empty() {
                            let mut r = self.result(shape, component, row.focus).with_value(value);
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
                            let mut r = self.result(shape, component, row.focus).with_value(value);
                            r.details = dupes
                                .into_iter()
                                .map(|m| self.result(shape, component, row.focus).with_value(m))
                                .collect();
                            out.push(r);
                        }
                    }
                }
            }
            Constraint::SingleLine => per_value!(|value| {
                store
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
                        if self.conforms(*inner, value, stack, store)? {
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

            Constraint::Expression(expr) => {
                for row in sets.rows() {
                    for &value in row.values {
                        let got = self.eval_expr(*expr, value, store)?;
                        let truthy = got
                            .first()
                            .is_some_and(|&t| store.lexical_form(t) == Some("true"));
                        if !truthy {
                            let mut r = self.result(shape, component, row.focus).with_value(value);
                            // The expression itself is what was violated.
                            r.source_constraint = Some(*expr);
                            out.push(r);
                        }
                    }
                }
            }
            Constraint::NodeByExpression(expr) => {
                for row in sets.rows() {
                    for &value in row.values {
                        // The expression names the shapes to conform to.
                        let shapes = self.eval_expr(*expr, value, store)?;
                        for target in shapes {
                            let ok = match self.shapes.id_of(target) {
                                Some(id) => self.conforms(id, value, stack, store)?,
                                None => true,
                            };
                            if !ok {
                                out.push(
                                    self.result(shape, component, row.focus).with_value(value),
                                );
                            }
                        }
                    }
                }
            }

            Constraint::Sparql(sc) => self.eval_sparql(shape, sc, sets, out, store)?,

            Constraint::Custom(cc) => {
                // Unlike sh:sparql, a component's validator runs once per value
                // node, with ?value pre-bound alongside the parameters.
                for row in sets.rows() {
                    for &value in row.values {
                        let mut bindings = vec![
                            ("this", crate::sparql::to_term(row.focus, store)),
                            ("value", crate::sparql::to_term(value, store)),
                        ];
                        for (name, term) in &cc.bindings {
                            bindings.push((name.as_str(), crate::sparql::to_term(*term, store)));
                        }

                        let solutions =
                            crate::sparql::run(&cc.query.query, &bindings, self.data, store)?;
                        // An ASK validator passes when it answers true; a
                        // SELECT validator faults once per solution.
                        let failed = if cc.query.is_ask {
                            solutions.is_empty()
                        } else {
                            !solutions.is_empty()
                        };
                        if failed {
                            let mut r = self
                                .result(shape, cc.component, row.focus)
                                .with_value(value);
                            r.source_constraint = Some(cc.query.source);
                            if !cc.query.message.is_empty() {
                                r.messages.clone_from(&cc.query.message);
                            }
                            out.push(r);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Runs a `sh:sparql` constraint once per focus node.
    ///
    /// A `sh:select` faults for every solution it returns; a `sh:ask` faults
    /// when it answers false. Solutions may name the offending value and path
    /// through `?value` and `?path`, which override the shape's own.
    fn eval_sparql(
        &self,
        shape: &Shape,
        sc: &crate::sparql::SparqlConstraint,
        sets: &ValueSets,
        out: &mut Vec<ValidationResult>,
        store: &mut TermStore,
    ) -> Result<()> {
        let v = self.vocab;
        let severity = sc.severity.unwrap_or(shape.severity);

        // SHACL binds two more variables alongside `$this`: the shape the
        // constraint hangs off, and an IRI naming the shapes graph, which
        // `GRAPH $shapesGraph { … }` can then read.
        let shapes_graph_iri = oxrdf::Term::from(oxrdf::NamedNode::new_unchecked(
            crate::sparql::SHAPES_GRAPH_IRI,
        ));

        for row in sets.rows() {
            let this = crate::sparql::to_term(row.focus, store);
            let current_shape = crate::sparql::to_term(shape.node, store);
            let solutions = crate::sparql::run_in(
                &sc.query,
                &[
                    ("this", this),
                    ("currentShape", current_shape),
                    ("shapesGraph", shapes_graph_iri.clone()),
                ],
                self.data,
                store,
                Some(self.shapes_graph),
            )?;

            // ASK inverts: answering true means the constraint is satisfied.
            let failures: Vec<_> = if sc.is_ask {
                if solutions.is_empty() {
                    vec![crate::sparql::empty_solution()]
                } else {
                    Vec::new()
                }
            } else {
                solutions
            };

            for solution in failures {
                let mut r =
                    ValidationResult::new(row.focus, v.sh_SPARQLConstraintComponent, severity)
                        .with_path(shape.path_node)
                        .with_source_shape(shape.node);
                r.source_constraint = Some(sc.source);
                r.messages.clone_from(&sc.message);
                if r.messages.is_empty() {
                    r.messages.clone_from(&shape.messages);
                }

                // A solution may override the focus node, value and path. Terms
                // the query invented rather than read are not in the store and
                // simply do not appear in the report.
                if let Some(t) = solution.get("this")
                    && let Some(id) = crate::sparql::from_term(t.as_ref(), store)
                {
                    r.focus_node = id;
                }
                // A solution that says nothing about the offending value faults
                // the focus node itself, which is what the suite's expected
                // reports contain for queries projecting only `$this`.
                r.value = match solution.get("value") {
                    Some(t) => crate::sparql::from_term(t.as_ref(), store),
                    None => Some(r.focus_node),
                };
                if let Some(t) = solution.get("path")
                    && let Some(id) = crate::sparql::from_term(t.as_ref(), store)
                {
                    r.path = Some(id);
                }
                out.push(r);
            }
        }
        Ok(())
    }

    /// Evaluates a node expression written in the shapes graph.
    fn eval_expr(&self, expr: TermId, focus: TermId, store: &mut TermStore) -> Result<Vec<TermId>> {
        let ctx = crate::nodeexpr::Ctx {
            data: self.data,
            exprs: self.shapes_graph,
            vocab: self.vocab,
            shnex: &self.shnex,
            shapes: Some(self.shapes),
            vars: &[],
        };
        crate::nodeexpr::eval(expr, Some(focus), &ctx, store)
    }

    // ---------------------------------------------------------------- helpers

    fn cmp_is(&self, a: TermId, b: TermId, wanted: &[Ordering], store: &TermStore) -> bool {
        datatypes::compare(a, b, store, self.vocab).is_some_and(|o| wanted.contains(&o))
    }

    /// Length in characters, or `None` for terms that have no lexical form to
    /// measure — blank nodes, which `sh:minLength`/`sh:maxLength` always fault.
    fn str_len(&self, t: TermId, store: &TermStore) -> Option<usize> {
        if store.kind(t) == TermKind::Blank {
            return None;
        }
        store.lexical_form(t).map(|s| s.chars().count())
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
    #[allow(clippy::too_many_arguments)]
    fn pair_order(
        &self,
        shape: &Shape,
        component: TermId,
        sets: &ValueSets,
        p: &Path,
        wanted: &[Ordering],
        out: &mut Vec<ValidationResult>,
        store: &TermStore,
    ) {
        for row in sets.rows() {
            let other = self.path_values(row.focus, p);
            for &value in row.values {
                for &o in &other {
                    if !self.cmp_is(value, o, wanted, store) {
                        out.push(self.result(shape, component, row.focus).with_value(value));
                    }
                }
            }
        }
    }

    /// Predicates permitted under `sh:closed sh:ByTypes`.
    ///
    /// The focus node's own types decide: each type that is itself a shape
    /// contributes its property paths, and so does every superclass, so an
    /// instance of a subclass may still use what its parent declares — but an
    /// instance of the parent may not use what only the subclass declares.
    fn closed_by_types(&self, focus: TermId, ignored: &[TermId]) -> Vec<TermId> {
        let mut allowed = ignored.to_vec();
        // The types themselves are what select the permitted set, so stating
        // them cannot be what makes a node violate it.
        allowed.push(self.vocab.rdf_type);
        let mut queue: Vec<TermId> = self.data.objects(focus, self.vocab.rdf_type).collect();
        let mut seen = queue.clone();
        while let Some(class) = queue.pop() {
            // A class contributes what it declares as a shape in its own right…
            if let Some(id) = self.shapes.id_of(class) {
                allowed.extend(self.property_paths(self.shapes.get(id)));
            }
            // …and what any shape aimed at it declares. A separate shape with
            // `sh:targetClass` is describing the same instances, so closing
            // over only the class-as-shape would reject properties the model
            // plainly permits.
            for shape_node in self.shapes_graph.subjects(self.vocab.sh_targetClass, class) {
                if let Some(id) = self.shapes.id_of(shape_node) {
                    allowed.extend(self.property_paths(self.shapes.get(id)));
                }
            }
            for up in self.data.objects(class, self.vocab.rdfs_subClassOf) {
                if !seen.contains(&up) {
                    seen.push(up);
                    queue.push(up);
                }
            }
        }
        sort_dedup(&mut allowed);
        allowed
    }

    /// The predicate paths a shape declares, following the shapes it composes
    /// with.
    ///
    /// `sh:node` and the logical combinators pull in another shape's
    /// properties as surely as `sh:property` does, so closing over only the
    /// direct ones would reject what a composed shape plainly permits.
    fn property_paths(&self, shape: &Shape) -> Vec<TermId> {
        let mut out = Vec::new();
        let mut queue = vec![shape];
        let mut depth = 0;
        while let Some(s) = queue.pop() {
            depth += 1;
            if depth > 64 {
                break;
            }
            for c in &s.constraints {
                match c {
                    Constraint::Property(id) => {
                        if let Some(p) = self
                            .shapes
                            .get(*id)
                            .path
                            .as_ref()
                            .and_then(|p| p.as_predicate())
                        {
                            out.push(p);
                        }
                    }
                    Constraint::Node(id) => queue.push(self.shapes.get(*id)),
                    Constraint::And(ids) | Constraint::Or(ids) | Constraint::Xone(ids) => {
                        queue.extend(ids.iter().map(|id| self.shapes.get(*id)));
                    }
                    _ => {}
                }
            }
        }
        out
    }

    /// Predicates a closed shape permits: those declared by its own property
    /// shapes, plus `sh:ignoredProperties`.
    fn closed_allowed(&self, shape: &Shape, ignored: &[TermId]) -> Vec<TermId> {
        let mut allowed = ignored.to_vec();
        for c in &shape.constraints {
            if let Constraint::Property(id) = c
                && let Some(p) = self
                    .shapes
                    .get(*id)
                    .path
                    .as_ref()
                    .and_then(|p| p.as_predicate())
            {
                allowed.push(p);
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

/// Every distinct term appearing anywhere in `g`.
fn all_nodes(g: &Graph) -> Vec<TermId> {
    let mut out = Vec::with_capacity(g.len() * 2);
    for [s, _, o] in g.iter() {
        out.push(s);
        out.push(o);
    }
    sort_dedup(&mut out);
    out
}
