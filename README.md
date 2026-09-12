# shacl

A high-performance SHACL validation engine in Rust, targeting full conformance
with the W3C SHACL 1.0 and 1.2 test suites.

## Layout

| Path | Purpose |
| --- | --- |
| `crates/shacl` | The engine library |
| `crates/shacl-cli` | `shacl` command line binary |
| `crates/shacl-python` | Python bindings (PyO3 + maturin) |
| `benchmarks/` | Comparison against pySHACL |
| `testsuite/shacl10` | W3C `data-shapes-test-suite`, copied in |
| `testsuite/shacl12` | W3C `shacl12-test-suite`, copied in |

## Performance

Against pySHACL 0.40.1 on synthetic data, best of three, with identical result
counts at every size — the harness compares them and refuses to report a time
if they differ. See [benchmarks/](benchmarks/) for the method and the caveats.

| instances | triples | results | ours | pySHACL | speedup | ours: validate |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1,000 | 6,914 | 88 | 0.053s | 2.099s | 40× | 0.002s |
| 10,000 | 69,027 | 997 | 0.127s | 16.256s | 128× | 0.020s |
| 100,000 | 689,861 | 10,179 | 1.870s | 163.143s | 87× | 0.414s |

Whole-process wall times on one machine, so read the order of magnitude
rather than the digits; the ratio is not even monotonic in the size, which is
a fair warning about how much weight a single number here carries. The last
column is validation alone: at 100k it is under a quarter of the total, and
**loading dominates** — which is where to look first for a speedup, not at the
validator.

These numbers are measured, not aspirational. An earlier version of this table
claimed 0.781s at 100k, and stayed there while a regression made validation
quadratic in the number of focus nodes — 44 seconds at that size, for several
releases. `tests/scaling.rs` now asserts the shape of that curve, because a
benchmark nobody re-runs is a claim rather than a check.

### Since that table

Three things now take the 100k run below what it shows, each measured in one
session on the same machine so they compare with each other rather than with
the table (which was a different day, and a slower one):

| 100k instances, whole process | wall |
| --- | ---: |
| parse, one thread | 0.61s |
| parse, all cores | 0.57s |
| [cached index](#caching-the-parsed-graph), one thread | 0.26s |
| cached index, all cores — the defaults | **0.23s** |

**The index cache** is most of it. **Parallel validation** takes the validate
phase itself from 0.14s to 0.05s on four cores, which shows up as less than
that end to end because what remains is reading the index and writing a
1.3 MB report — a run with fewer violations would see more of it. And a
**first-level index on each permutation** — one array load per subject
instead of a binary search over the whole graph — took the sequential validate
phase down by a fifth. The last is the first level of the trie the
[HOLOS](https://github.com/pwin/new_triplestore_sparql_engine) store builds
over this engine's design, and reading that design is where two of the three
came from.

What was *not* done is as informative. HOLOS inlines integers, floats and
dates into the term id so that comparing them never touches the dictionary,
and it looked like the obvious lesson to take. Measured per constraint on the
benchmark, the `sh:minInclusive`/`sh:maxInclusive` shape costs 22ms of 115 and
the regex 16ms; the cost is spread evenly across every constraint at about
two cache misses each, and no single one is worth an encoding change. The
profile is in the commit that added the index, and it is the reason the
parallel curve flattens on four cores rather than climbing: more threads
share the same memory system.

`--threads N` sets the worker count; `1` is sequential. Only a SHACL Core
shapes graph splits — one using `sh:sparql`, a SPARQL-based constraint
component, node expressions, `sh:uniqueValuesFor`, or a shape that can reach
itself runs sequentially whatever is asked, because each of those makes a
focus set more than the sum of its nodes. The report is the same either way,
byte for byte: results are returned in an order that depends on what they say
rather than on how they were produced, so the same graph gives the same bytes
on a machine with a different number of cores. That order is by focus node
first — a change from earlier releases, where it followed the traversal and
grouped by constraint — and `tests/parallel.rs` holds a split run to it.

## Design

Three decisions carry most of the performance:

1. **Interned terms.** Every IRI, blank node and literal becomes a `u32` on
   load, so the inner loops compare integers, not strings.
2. **Flat indexes.** A graph is three fully-sorted arrays — `SPO`, `POS`, `OSP`
   — rather than hash maps of adjacency lists. Every lookup SHACL performs is a
   prefix range found by binary search and then walked as contiguous memory.
3. **Compile once.** A shapes graph is compiled into a flat IR before validation
   starts; evaluating a constraint never queries the shapes graph again.

The cost of the second is memory. A graph is held three times over, once per
permutation, plus the term store — so budget roughly **3 × 12 bytes per triple**
for the indexes, on top of the interned strings, and expect the whole document
to be resident: there is no streaming or memory-bounded path. That is the trade
being made for the lookup behaviour above, and it is the wrong trade for a graph
too large to hold three sorted copies of.

## Output

SHACL defines the validation report as an RDF graph — a `sh:ValidationReport` —
so that is what `--format` emits. It can be queried, diffed, or fed to another
tool, none of which a rendered summary supports.

```sh
shacl -d data.ttl -s shapes.ttl -f turtle     # also: nt, rdfxml, jsonld
```

```turtle
_:r0 a sh:ValidationReport ;
    sh:conforms false ;
    sh:result _:r1 .
_:r1 a sh:ValidationResult ;
    sh:focusNode <http://ex/a> ;
    sh:resultPath <http://ex/age> ;
    sh:resultSeverity sh:Violation ;
    sh:sourceConstraintComponent sh:DatatypeConstraintComponent ;
    sh:value "old" .
```

The default, `human`, is a one-line-per-result summary for reading rather than
parsing. Reach for it at a terminal and for anything else use the RDF.

The RDF output is **reproducible**: the same inputs give the same bytes, so two
reports can be diffed and only real differences show up. That needs the triples
written in a fixed order and blank nodes numbered in first-seen order rather
than carrying the parser's random names for anonymous `[ … ]` nodes — which
means a `_:label` written in the source does not survive into the report. RDF
treats blank node labels as local syntax rather than identity, so a processor
is free to relabel; this one does.

This holds across machines, not just across runs: `tests/determinism.rs` pins
the exact bytes of a report, and CI runs it on Linux, Windows and 64-bit ARM
macOS, so the claim is checked rather than assumed.

What it does **not** promise is stability between releases. Adding a
constraint, or changing the order shapes compile in, changes which result is
`_:r1` — the pinned test turns that into a visible failure to be accepted
deliberately, but it does mean a report checked into version control can move
under a version bump. Compare reports from one version, or compare them as
graphs rather than as text.

That acceptance happened once, in 0.3.0. Results used to come out in the
order validation met them, which grouped them by constraint; they now come out
by focus node, with everything about one node together. The reason was not
tidiness: validation had started running across threads, and the order it
meets results in then depends on how many threads there are. Sorting by
content is what keeps "the same bytes on any machine" true.

## Inputs

`--data` and `--shapes` each take a path, an `http(s)` URL, or `-` for standard
input, and each can be repeated to merge several documents into one graph.

```sh
shacl -d data.ttl -s https://example.org/shapes.ttl
shacl -d instances.ttl schema.ttl -s shapes.ttl        # merged
cat data.ttl | shacl -d - --data-format ttl -s shapes.ttl
```

Syntax is taken from the file extension, or over HTTP from the `Content-Type`,
and `--data-format`/`--shapes-format` (`--df`/`--sf`) override both — required
for `-`, which has neither.

Only the URLs given on the command line are ever fetched. Nothing in a fetched
document triggers a further request: `owl:imports` is not followed, so a
shapes graph cannot reach anywhere you did not name.

### Caching the parsed graph

Parsing is most of a run — 1.3s of the 1.8s at 100k instances above — and a
data graph that has not changed parses to exactly the same thing every time.
`--build-index` writes what parsing produced beside the source as
`<file>.shix`, and later runs read that instead:

```sh
shacl -d data.ttl -s shapes.ttl --build-index   # parses, and writes data.ttl.shix
shacl -d data.ttl -s shapes.ttl                 # reads the index; no parsing
shacl -d data.ttl -s shapes.ttl --no-index      # ignores it and parses anyway
```

On the 100k benchmark that takes loading from 1.31s to 0.15s — best of three
either way, as elsewhere in this file; the middle runs were 1.36s and 0.21s —
and leaves the report byte-identical, which is the property the tests assert
rather than that the same triples come back in some order.

**The cache cannot go stale.** The index records a digest of the source, and
every run re-reads the source to check it. A file that has changed by so much
as a byte falls back to parsing, with a note saying so. That check costs a read
of the file but not a parse, which is why it is affordable enough to do every
time rather than trusting a timestamp — copying a file or checking it out fresh
updates its mtime without changing a byte, and a cache keyed on that would
answer for data that is no longer there.

Two limits worth knowing. It applies to a single local `--data` file: a URL has
nothing on disk to check against, and several merged documents have no one
source whose digest would mean anything. And the index is a little *larger*
than the source it replaces — 19.5 MB against 18.6 MB here — because it stores
terms expanded rather than in Turtle's abbreviated syntax. It buys time, not
space.

### Compressed input

`.gz` is read directly, locally and over HTTP, and the syntax comes from what
is underneath the wrapper — `dump.ttl.gz` is Turtle. Published RDF is usually
compressed, so this is the ordinary case rather than a special one.

```sh
shacl -d dump.nt.gz -s shapes.ttl
shacl -d https://example.org/dumps/latest.ttl.gz -s shapes.ttl
```

`--max-download` is applied to the **decompressed** stream. A limit on the
compressed bytes would be no limit at all: a megabyte of gzip can expand to a
gigabyte, and it is the expanded size that has to fit in memory. A document
that declares an oversized `Content-Length` is refused before any of it
transfers; one that declares nothing, or lies, is caught as it is read.

Exceeding it is reported as what it is, naming the limit and the flag that
sets it — not as a malformed document, which is what a stream cut short
otherwise looks like from inside a parser:

```
error: loading data graph: http://example.org/dump.ttl.gz is larger than the
4.0 MB limit; raise --max-download to accept it
```

**Zip is not supported, deliberately.** A `.gz` is one document that happens to
be compressed, so it slots in where a file would go and nothing else changes. A
`.zip` is an archive: it holds *members*, and the moment there is more than one
the tool has to decide which is the data graph — or whether they should be
merged, and what to do with the README and the licence file sitting alongside
them. That is a policy with no obviously right answer, and guessing it silently
is how a validator ends up confidently checking the wrong document. Unpack it
and pass the files you mean, which `-d` already accepts several of:

```sh
unzip -q dump.zip -d dump/ && shacl -d dump/*.ttl -s shapes.ttl
```

### Coming from pySHACL

Most of its flags work here, spelled the same way: `-df`/`-sf`, `-o`, `-m`/
`--metashacl`, `-i`, `-w`/`--allow-warnings`, `--allow-info`, `--abort`. The
two-letter short options are translated for compatibility — clap has no
multi-character shorts of its own — so `--df` works too. Two differences worth
knowing:

- `-f` has no `table`; `human` is the readable format.
- `-w`/`--allow-warnings` means what it means in pySHACL: warnings and infos
  stop breaking conformance. Without it, as in pySHACL and as the SHACL
  specification says, a `sh:Warning` or `sh:Info` result makes `sh:conforms`
  false. See [What conforms means](#what-conforms-means).

## Conformance

The suites are run by a manifest-driven harness covering both kinds of entry:
`sht:Validate`, which validates a data graph and compares reports, and
`sht:EvalNodeExpr`, which evaluates a node expression and compares the resulting
sequence. Expected and actual reports are compared two ways — result by
result through one in-memory representation, and as RDF graphs the way the
suite itself specifies (below) — and both have to agree.

**418 of 426** tests pass. Core SHACL 1.0 and 1.2 constraints, property paths,
SPARQL-based constraints with pre-binding, user-declared constraint components,
the node expression algebra, SPARQL-selected targets and RDF 1.2 annotations are
all implemented. The eight that remain are named, with the reason for each, in
`KNOWN_FAILURES` in `tests/w3c.rs` — the suite asserts that list matches what
actually fails, so it cannot drift.

### Compared the way the suite compares

The suite defines its own comparison, and it is not result by result. Its
description (`testsuite/shacl10/index.html`) says the actual report must be
**isomorphic** to the expected one as an RDF graph, after removing every
triple whose predicate the expected reports do not use, and removing
`sh:resultMessage` except where the expected report carries a message with
the same object. It also says, in so many words, that path structures under
`sh:resultPath` must not be shared between results and must not reuse a
blank node in two places.

The harness now runs that comparison alongside its own, reports both, and
asserts they agree on every passing test:

```
  shacl10    118/120 passing  (0 could not run)   as graphs: 113/113
  shacl12    300/306 passing  (0 could not run)   as graphs: 153/158
  TOTAL      418/426 passing                        as graphs: 266/271
```

The second column counts the tests that produce a report at all — the rest
expect a failure, or evaluate a node expression — and the five it does not
reach are five of the eight known failures.

Adding it found six deviations the per-result comparison had let through for
every release so far, each a report that was right result by result and wrong
as a document. Compound paths were shared between results and a repeated
sub-expression was written once. `sh:conforms` followed the engine's rule
rather than the specification's — and the harness recomputed the expected
report's conformance under the same rule instead of reading the value the
test wrote, so it agreed with itself. `sh:sourceConstraint` carried a pattern
literal, and appeared on results from constraint components, where the
specification gives it to `sh:sparql` constraints alone; and it was missing
from `sh:nodeByExpression` results. `{?var}` in a SPARQL constraint's message
was not substituted. And a message annotated onto a constraint triple —
`sh:datatype xsd:integer {| sh:message "…" |}` in Turtle 1.2 — was not read.
All six are fixed, and a report that is right but misshapen is now a test
failure.

### What conforms means

`sh:conforms` is false when the report holds a result whose severity breaks
conformance. Which severities do is `sh:conformanceDisallows`, and when a
report declares none — the ordinary case — the specification's default
applies: **`sh:Violation`, `sh:Warning` and `sh:Info`**. In SHACL 1.0 those
were the only severities, so the rule read as "conforms means no results";
SHACL 1.2 added `sh:Debug` and `sh:Trace` beneath them, which report without
blocking. The suite pins each edge: one `sh:Warning` result is `conforms
false` (`severity-001`), one `sh:Debug` result is `conforms true`
(`severity-004`).

Until 0.3.0 this engine's default was `sh:Violation` alone — pySHACL's
`--allow-warnings` reading, taken as if it were pySHACL's default, which it is
not. A report with one warning said `conforms true`, which is the wrong side
of the only answer a validator gives.

`--min-severity` sets the threshold: `info` is the default and the
specification's; `warning` and `violation` narrow it; `debug` and `trace`
widen it to the 1.2 severities. Anything but the default is written into the
report as `sh:conformanceDisallows`, so a reader can tell `conforms true`
over a warning from `conforms true` over nothing. `-w`/`--allow-warnings` is
`--min-severity violation` spelled as pySHACL spells it. In Python the same
knob is `allow_warnings=` on `validate`, `validate_file`, `validate_text` and
`validate_turtle`; the WebAssembly `Report.conforms` uses the default, and
`results` carries every severity for a caller who wants another reading.

### Recursion

SHACL allows a shape to refer to itself, directly or through a cycle, and the
specification leaves what to do about it to the implementation. Cycles are
detected — a (shape, focus node) pair already being validated is not entered
again — so an ordinary recursive shape terminates and reports normally.

**A recursive shape can follow a chain of any length.** The descent through
`sh:property` runs on an explicit stack rather than the call stack, so
following a linked list, a `rdf:rest` chain, a `skos:broader` ladder or a
part-of hierarchy costs heap rather than stack. A 20,000-link chain is in the
test suite; a debug build used to die at about 100.

There is still a ceiling of **48 levels of nesting**, and reaching it is an
error rather than a partial report:

```
error: recursion limit exceeded: shapes nested more than 48 deep; a recursive shape nests one level per shape-valued constraint, not per link of data
```

What it counts is worth being exact about, because the two things are easy to
confuse:

| | counted? |
| --- | --- |
| `sh:property` descending through data | no — heap, unbounded |
| `sh:node`, `sh:not`, `sh:or`, `sh:qualifiedValueShape` nested by hand | yes |

The difference is that `sh:property` needs no answer — its results go straight
to the report — so the work can be deferred to a stack. `sh:node` and the
logical constraints have to *ask* whether a nested shape produced anything, so
they wait, and waiting costs a call frame. Those nest by shape structure, which
is written by hand: 48 is far more than anyone writes.

The ceiling exists because a stack overflow is not a panic. It cannot be caught
or turned into an error; it takes the process down, and with it any host
embedding the engine — the Python and WebAssembly bindings included, whose
whole error story is that a Rust failure becomes an exception. Measured on the
cheapest possible recursive shape, the process dies somewhere under 100 levels
in a debug build and at about 410 in a release one, so 48 leaves roughly a
factor of two in the tighter of the two.

Compiling a shapes graph is iterative too, so nesting `sh:node` thousands deep
compiles and then meets the limit above as an error rather than overflowing
before validation begins.

### Not implemented

Deliberately, rather than pending:

- **SHACL functions** (`sh:SPARQLFunction`, `sh:returnType`) and the node
  expression form that calls them. A rule using one gets an error naming the
  expression, not silence — see [SHACL-AF rules](#shacl-af-rules).
- **Result annotations** (`sh:resultAnnotation`), which copy extra properties
  from a SPARQL constraint's solution onto the result.
- **OWL-RL pre-inference.** RDFS entailment *is* available, opt-in — see below
  — but nothing beyond it. Meta-SHACL is available too, as `--meta-shacl`.
- **SHACL 1.2 Rules** (the `RULE { } WHERE { }` language) is a different
  design from SHACL-AF rules and is not implemented. The 1.2 rules test suite
  is vendored under `testsuite/shacl12/tests/rules/` but is not wired into the
  harness, so it is not counted in the conformance figure above.

### Named graphs are flattened

TriG and N-Quads are accepted, and their named graphs are **merged into one
data graph**. A TriG file with two named graphs and a default graph gives
exactly the results the same triples give flattened into Turtle.

That is what SHACL 1.0 defines — validation is over *a* data graph — but the
consequences are worth stating plainly, because holding each contributor's
data in its own named graph is a common and sensible arrangement:

- there is no way to validate one graph rather than the union;
- a result does not say which graph its focus node came from;
- triples a rule infers are not attributed to a graph either.

You can still hold data that way and validate the union. What you cannot do
is validate per contributor, or answer "whose data broke this" from the report.
Splitting the file, or passing only the graphs you mean, is the way to get
per-source answers today.

`GRAPH` patterns inside a `sh:sparql` constraint are a separate matter: the
shapes graph is exposed as `$shapesGraph`, and the data is the default graph.

## SHACL-AF rules

Rules infer triples before validation, so a report can depend on data that was
derived rather than asserted. They are off by default and enabled with `-a`
(pySHACL's spelling):

```sh
shacl -d data.ttl -s shapes.ttl --advanced
```

Everything below is exercised by `crates/shacl/tests/rules.rs`, including the
cases that do *not* work.

### Triple rules

`sh:subject`, `sh:predicate` and `sh:object` are node expressions, evaluated
per focus node. The inferred triples are their **cross product**, so a path
expression yielding three values yields three triples.

```turtle
ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    # Every Person is also an Agent.
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ;
        sh:predicate rdf:type ;
        sh:object ex:Agent ] ;
    # ex:contact for each ex:knows — one triple per value.
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ;
        sh:predicate ex:contact ;
        sh:object [ sh:path ex:knows ] ] .
```

The node expressions supported here are SHACL-AF's: `sh:this`, a constant,
`[ sh:path P ]` with an optional `sh:nodes` operand, `[ sh:filterShape S ;
sh:nodes N ]`, `sh:union` and `sh:intersection`. Anything else — a function
call in particular — is an error rather than an empty result.

### SPARQL rules

`sh:construct` takes a `CONSTRUCT` query with `$this` pre-bound to the focus
node:

```turtle
ex:BoxShape a sh:NodeShape ;
    sh:targetClass ex:Box ;
    sh:rule [ a sh:SPARQLRule ; sh:construct """
        CONSTRUCT { $this ex:area ?a }
        WHERE { $this ex:width ?w ; ex:height ?h . BIND(?w * ?h AS ?a) }""" ] .
```

### Conditions and ordering

`sh:condition` names shapes the focus node must conform to — all of them —
before the rule fires. `sh:order` sequences execution, default 0.

```turtle
ex:S a sh:NodeShape ; sh:targetClass ex:Person ; sh:order 1 ;
    sh:rule [ a sh:TripleRule ;
        sh:condition ex:HasAge ;
        sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:Adult ] .
```

## Where rule authors have to be careful

These are the five things that produce a wrong answer rather than an error.

**A rule fires on its shape's targets, so a shape without one does nothing.**
This is the most common mistake, because attaching a rule to a nested property
shape reads naturally and never runs:

```turtle
ex:S a sh:NodeShape ; sh:targetClass ex:Person ;
    sh:property [ sh:path ex:name ;
        sh:rule [ … ] ] .        # never fires: this shape has no target
```

**One pass, so a transitive rule does not close.** The specification defines a
single iteration and declines to say what repeating it means. A rule deriving
`ex:sub` from two hops of `ex:sub` reaches two hops and stops:

```sh
shacl -d data.ttl -s shapes.ttl -a                    # one pass
shacl -d data.ttl -s shapes.ttl -a --iterate-rules 10 # to a fixpoint, max 10 rounds
```

`--iterate-rules` is outside the spec. A rule set that never settles — one
minting a new term each round — stops with an error rather than running until
memory does.

**Rules at the same `sh:order` cannot see each other's inferences.** Two rules
both at the default order 0 each see the graph as it was before either ran, so
one cannot consume what the other produces. Give the consumer a higher
`sh:order`.

**Negation is not monotonic, and iteration exposes it.** Negation as failure is
available two ways — `sh:condition [ sh:not S ]` and SPARQL's `FILTER NOT
EXISTS` — and both are fine in a single pass. Under `--iterate-rules` a
conclusion drawn from absence *outlives* the absence, because rules only ever
add triples:

```turtle
# Round 1 marks ex:a as Unnamed. Round 2's rule then gives it a name.
# The mark stays. It is simply no longer true.
ex:Mark a sh:NodeShape ; sh:targetClass ex:Person ; sh:order 1 ;
    sh:rule [ a sh:TripleRule ;
        sh:condition [ sh:not ex:HasName ] ;
        sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:Unnamed ] .
ex:Fill a sh:NodeShape ; sh:targetClass ex:Person ; sh:order 2 ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ; sh:predicate ex:name ; sh:object "given" ] .
```

SHACL 1.2 Rules answers this by requiring a stratified rule set. SHACL-AF has
no such requirement, so the responsibility is the author's: if a rule tests for
absence, either keep to a single pass or make sure nothing later supplies what
it tested for.

**Inference changes what `sh:closed` means.** A closed shape starts seeing
derived predicates and starts failing on them, exactly as it does under
`--inference rdfs`. This is why rules are opt-in rather than automatic.

One thing that is *not* a hazard: rules never modify the graph you passed in.
The expanded graph is a new one, so a report is always relative to an input you
still have.

### RDFS inference

SHACL follows `rdfs:subClassOf` when deciding class membership and nothing
else, so `sh:targetSubjectsOf ex:parent` will not see a subject holding only
`ex:father`, whatever `rdfs:subPropertyOf` says. Materialising the RDFS closure
first closes that gap:

```python
report = shapes.validate_file("data.ttl", inference="rdfs")
```

It covers `rdfs2`, `rdfs3`, `rdfs5`, `rdfs7`, `rdfs9` and `rdfs11` — domain,
range, both hierarchies and their transitivity. The axiomatic and reflexive
rules are left out: they entail `rdf:type rdfs:Resource` for every term, which
no shape is improved by.

Off by default, because it changes what the report says — a `sh:closed` shape
starts seeing inferred predicates, and counts move — so it should be asked for
rather than assumed.

```sh
cargo test -p shacl --test w3c -- --nocapture      # summary
SHACL_TEST_VERBOSE=1 cargo test -p shacl --test w3c -- --nocapture   # per-failure detail
SHACL_TEST_FILTER=core/node cargo test -p shacl --test w3c -- --nocapture
```

`tests/w3c.rs` also holds a `progress` test asserting the pass count never drops
below a recorded baseline.

## Python

```sh
pip install shacl
```

```python
import shacl
shapes = shacl.Shapes.from_file("shapes.ttl")   # compile once
report = shapes.validate_file("data.ttl")       # validate many
if not report:
    for r in report.results:
        print(r.severity, r.component, r.focus_node, r.value)
```

### What a result carries

Reading the fields is meant to be enough; a caller should not have to parse
the report graph back to find out what happened, nor do string surgery on
what it gets.

| | |
| --- | --- |
| `focus_node`, `value`, `path` | The result, in N-Triples syntax — except `path`, which is SPARQL property-path syntax (`<http://ex/p>`, or `(<http://ex/p>)+`) because a compound `sh:path` is a blank node structure and its label would mean nothing outside this process. |
| `value_plain` | The value with the syntax removed: a literal's lexical form, a named node's IRI, a blank node's label. What a report prints, and what a de-duplication key wants. |
| `source_shape` | The shape that raised the result. For a constraint written inside `sh:property [ ... ]` this is the nested property shape, which is a blank node. |
| `root_shape` | The nearest enclosing shape that has an IRI. This is the one a caller can look up: a registry keyed by shape IRI — the usual way to attach a check id or remediation text to a finding — has nothing to match a blank node against. Equal to `source_shape` for a constraint on a named shape. |
| `component`, `component_iri`, `severity`, `severity_iri` | Local name and full IRI of each. |
| `message`, `messages` | `messages` holds all of them, which matters when a shape carries one per language. |

### Stopping early on a large graph

`max_results` abandons validation once that many conformance-blocking results
exist:

```python
report = shapes.validate_file("data.ttl", max_results=100)
```

It is a real early exit rather than a truncation of a finished report, so it
bounds the memory a run costs as well as its time. That is the reason to want
it: one systematically failing shape over a large graph can produce results in
the hundreds of thousands, and every one is built, held and handed over before
the caller can say it only wanted a sample.

The severities counted are the ones that break conformance, matching
`--max-results` in the CLI. Counting every result instead would let a run stop
on an `sh:Info` and report `conforms = True` with an `sh:Violation` left
unexamined further along — shapes are evaluated in whatever order they
compiled in, so which kind is met first says nothing about what is in the
graph.

See [crates/shacl-python/README.md](crates/shacl-python/README.md). To work on
the bindings themselves, build them in place with
`pip install maturin && cd crates/shacl-python && maturin develop --release`.

## JavaScript

The same engine compiled to WebAssembly. Two packages, because wasm-pack's two
targets are not interchangeable and one file cannot be both:

```sh
npm install shacl-wasm        # ESM, for bundlers (vite, webpack, rollup)
npm install shacl-wasm-node   # CommonJS, for Node and require()
```

```js
import { Validator } from 'shacl-wasm';

const v = Validator.fromTurtle(shapesTurtle);   // compile once
const report = v.validateTurtle(dataTurtle);    // validate many
for (const r of report.results) {
  console.log(r.severity, r.component, r.focusNode, r.value);
}
```

Results come back as plain objects rather than only as RDF, so rendering
findings does not mean re-parsing a graph — though `report.toTurtle()` still
hands over the whole `sh:ValidationReport` for querying or diffing. Terms are
rendered the way RDF/JS renders `.value`: an IRI bare, a literal as its lexical
form.

There is also a one-shot for a caller with a single data graph, which takes
`data` first like everything else here:

```js
import { validateTurtle } from 'shacl-wasm';

validateTurtle(dataTurtle, shapesTurtle);   // shapes given
validateTurtle(selfDescribingTurtle);       // shapes carried by the data
```

**It took `(shapes, data)` before 0.2.0** — the only surface in this repository
with data second. See [Validating against nothing](#validating-against-nothing)
for why transposing it was worth making impossible.

Compile once and reuse the `Validator`. Each run gets its own copy of the term
store, so reuse is genuinely cheaper rather than quietly accumulating — and on
wasm32 the first validation of a large graph also pays a one-off cost to grow
the linear memory, which reuse amortises. Measured on 100k instances: 16.7s on
the first run against 4.6s on subsequent ones.

Inference, including SHACL-AF rules, is the second argument to
`validateTurtle` and the fourth to `validateText`:

```js
v.validateTurtle(data, base, 'rules');           // sh:rule, one pass
v.validateTurtle(data, base, 'rules-iterated');  // to a fixpoint, max 10 rounds
v.validateTurtle(data, base, 'rdfs');            // the RDFS closure
```

The same four modes the CLI and the Python bindings take — `none`, `rdfs`,
`rules`, `rules-iterated` — and the same caveats apply, in particular the ones
under [where rule authors have to be careful](#where-rule-authors-have-to-be-careful).
An unrecognised mode is an error rather than a silent `none`.

`parallel` is off in this build — there are no threads to split a parse
across — so it takes the sequential path. Conformance is the same 418 of 426
either way, and the WebAssembly build is checked against the native one over
every document in the W3C suite; see `crates/shacl-wasm/differential.js`.

## Validating against nothing

Validating a graph against no shapes conforms. That is correct — there is
nothing to violate — and it is also the most dangerous answer this engine can
give, because "conforms" is exactly what a caller hopes to see. A shapes graph
that parsed but declared nothing recognisable reports success indistinguishable
from success.

The easiest way to reach it was to transpose the arguments of a one-shot call.
The data graph compiles as a shapes graph, yields no shapes, and the report says
the data is valid without a single constraint having been evaluated.

So the one-shot entry points refuse it:

```python
shacl.validate("shapes.ttl", "data.ttl")   # transposed
# ValueError: the shapes graph declares no shapes, so this would report that
# the data conforms without having checked anything. Note the argument order
# is validate(data, shapes). ...
```

The explicit two-step form stays permissive, because a caller who compiled a
shapes graph on purpose can ask how it went — `len(shapes)` in Python,
`validator.shapeCount` in JavaScript — and may legitimately want the empty case:

```python
shapes = shacl.Shapes.from_file("shapes.ttl")
if len(shapes) == 0:
    raise SystemExit("shapes.ttl declares no shapes")
```

The convenience function is opinionated; the explicit one is not. That line is
deliberate: the API that hides the compile step is the one that has to speak up
about it.

## Licence

Dual licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE), at your
option. Copyright (c) 2026 pwin (Peter Winstanley).

This is the Rust ecosystem's convention, and it exists to solve one problem:
Apache 2.0 carries an express patent grant that MIT lacks, but is incompatible
with GPLv2, which MIT is not. Offering both lets a GPLv2 project take the MIT
terms and a patent-cautious one take Apache, excluding neither.

The W3C test suites bundled under `testsuite/` are not covered by that; they
are published by the W3C under their own licence — see
[testsuite/README.md](testsuite/README.md).
