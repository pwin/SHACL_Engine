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

## Conformance

The suites are run by a manifest-driven harness covering both kinds of entry:
`sht:Validate`, which validates a data graph and compares reports, and
`sht:EvalNodeExpr`, which evaluates a node expression and compares the resulting
sequence. Expected and actual reports are compared through one in-memory
representation rather than by diffing serialised RDF; `sh:resultMessage` is
excluded, since the spec leaves message text to the implementation.

**417 of 426** tests pass. Core SHACL 1.0 and 1.2 constraints, property paths,
SPARQL-based constraints with pre-binding, user-declared constraint components,
the node expression algebra, SPARQL-selected targets and RDF 1.2 annotations are
all implemented. The ten that remain are listed in `tests/w3c.rs`.

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
