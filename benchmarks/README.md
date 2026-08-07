# Benchmarks

Compares this engine against [pySHACL](https://github.com/RDFLib/pySHACL) on
synthetic data.

## Method

```sh
python benchmarks/generate.py --out bench --sizes 1000 10000 100000
cargo build --release -p shacl-cli
python benchmarks/run.py \
    --bench-dir bench \
    --shacl target/release/shacl \
    --pyshacl .venv/bin/pyshacl \
    --runs 3
```

Both engines are timed as **whole processes**. Parsing the RDF is real work that
neither can skip, so excluding it would flatter whichever engine has the faster
validator and misrepresent what a user waits for. The Rust engine also reports
its internal load/compile/validate split, shown separately.

`run.py` **compares result counts before reporting any time** and fails on a
mismatch. Two engines that disagree are not comparable, and a validator that
silently under-reports would otherwise look fast.

Neither engine performs inference: pySHACL defaults to none, and this engine
implements none, so the comparison is like for like.

## Workload

`generate.py` emits ordinary shapes — cardinality, datatype, node kind, string
length, a regex, a value range, a class constraint over a subclass hierarchy,
and one sequence path. Ten percent of instances break exactly one constraint, so
both engines build a report of comparable size rather than racing to an empty
one.

The class hierarchy is written into the **data** graph, because that is where
SHACL resolves class membership. Leaving it only in the shapes graph makes
`sh:class` fail for every instance — correct per spec, and both engines agree on
it, but it swamps the measurement with one systematic error instead of
exercising subclass closure.

## Results

Ryzen/Windows 11, release build (`lto = "fat"`, `codegen-units = 1`), best of 3.

| instances | triples | results | ours | pySHACL | speedup | ours: validate only |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1,000 | 6,914 | 88 | 0.021s | 1.127s | **53×** | 0.0010s |
| 10,000 | 69,027 | 997 | 0.118s | 7.262s | **61×** | 0.0170s |

Both engines report identical result counts at every size.

## Reading these numbers

The end-to-end figure is dominated by **parsing**, not validation: at 10,000
instances the engine spends ~17ms validating and the rest reading Turtle. So the
speedup above is largely a measure of `oxttl` against `rdflib`, and the
validator's own contribution is not yet the bottleneck.

That has a direct consequence for optimisation work: the set-at-a-time
machinery, bitmap node sets and class-closure caching all target a phase that is
currently a seventh of the runtime. Profiling should drive what gets optimised
next, and on this workload it points at the parser and at not building a report
when only conformance is wanted.

This is also a favourable shape of workload for a compiled engine — small
shapes graph, many focus nodes, cheap constraints. Shapes with heavy SPARQL
constraints or deep recursion would narrow the gap, since both engines then
spend their time in a query evaluator.
