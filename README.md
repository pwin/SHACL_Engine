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
- Warnings and infos never break conformance by default, so `-w` is accepted
  but already the default. `--min-severity warning` is the knob in the other
  direction, for making them count.

## Conformance

The suites are run by a manifest-driven harness covering both kinds of entry:
`sht:Validate`, which validates a data graph and compares reports, and
`sht:EvalNodeExpr`, which evaluates a node expression and compares the resulting
sequence. Expected and actual reports are compared through one in-memory
representation rather than by diffing serialised RDF; `sh:resultMessage` is
excluded, since the spec leaves message text to the implementation.

**418 of 426** tests pass. Core SHACL 1.0 and 1.2 constraints, property paths,
SPARQL-based constraints with pre-binding, user-declared constraint components,
the node expression algebra, SPARQL-selected targets and RDF 1.2 annotations are
all implemented. The eight that remain are named, with the reason for each, in
`KNOWN_FAILURES` in `tests/w3c.rs` — the suite asserts that list matches what
actually fails, so it cannot drift.

### Recursion

SHACL allows a shape to refer to itself, directly or through a cycle, and the
specification leaves what to do about it to the implementation. Cycles are
detected — a (shape, focus node) pair already being validated is not entered
again — so an ordinary recursive shape terminates and reports normally.

There is also a hard ceiling of **48 levels of nesting**, and reaching it is an
error rather than a partial report:

```
error: recursion limit exceeded: shapes nested more than 48 deep
```

The ceiling exists because cycle detection alone does not bound the descent:
the number of distinct (shape, node) pairs is the product of the two, so a
recursive shape walked over a long data chain — a linked list of a few hundred
items — can still exhaust the call stack. A stack overflow is not a panic, so
it cannot be caught or turned into an error; it takes the process down, and
with it any host embedding the engine. Refusing early is what makes the failure
recoverable.

48 is far more nesting than a hand-written shapes graph uses, and it is set
well below where the stack actually runs out because the cost of a level is not
fixed — one carrying a SPARQL constraint is much more expensive than one
carrying `sh:datatype`. If you hit it, the shapes are almost certainly walking
a data structure rather than nesting: raising the number would not help, since
the fix is to move the descent off the call stack.

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

Compile once and reuse the `Validator`. Each run gets its own copy of the term
store, so reuse is genuinely cheaper rather than quietly accumulating — and on
wasm32 the first validation of a large graph also pays a one-off cost to grow
the linear memory, which reuse amortises. Measured on 100k instances: 16.7s on
the first run against 4.6s on subsequent ones.

`parallel` is off in this build — there are no threads to split a parse
across — so it takes the sequential path. Conformance is the same 418 of 426
either way, and the WebAssembly build is checked against the native one over
every document in the W3C suite; see `crates/shacl-wasm/differential.js`.

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
