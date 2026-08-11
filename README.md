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

Against pySHACL on synthetic data, best of three, with identical result counts
at every size. See [benchmarks/](benchmarks/) for the method and the caveats.

| instances | triples | ours | pySHACL | speedup |
| ---: | ---: | ---: | ---: | ---: |
| 1,000 | 6,914 | 0.020s | 1.169s | 58× |
| 10,000 | 69,027 | 0.071s | 7.018s | 99× |
| 100,000 | 689,861 | 0.781s | 81.399s | 104× |

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

- **SHACL-AF rules** (`sh:rule`, `sh:TripleRule`, `sh:SPARQLRule`). No
  inference is performed, so a shapes graph relying on rules to derive the
  triples it then validates will find them absent. pySHACL supports these.
- **OWL-RL pre-inference.** RDFS entailment *is* available, opt-in — see below
  — but nothing beyond it. Meta-SHACL is available too, as `--meta-shacl`.

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
pip install maturin
cd crates/shacl-python && maturin develop --release
```

```python
import shacl
shapes = shacl.Shapes.from_file("shapes.ttl")   # compile once
report = shapes.validate_file("data.ttl")       # validate many
if not report:
    for r in report.results:
        print(r.severity, r.component, r.focus_node, r.value)
```

See [crates/shacl-python/README.md](crates/shacl-python/README.md).

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
