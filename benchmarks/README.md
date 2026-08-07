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
| 100,000 | 689,861 | 10,179 | 1.363s | 78.821s | **58×** | 0.2680s |

Both engines report identical result counts at every size.

At the largest size that is ~506k triples/second end to end, or ~2.6M
triples/second through the validator alone, against ~8.8k/second for pySHACL.

## Reading these numbers

**The end-to-end figure is dominated by parsing, not validation.** At 100,000
instances the engine spends 0.27s validating and roughly 1.1s reading Turtle.
The headline speedup is therefore largely a measure of `oxttl` against `rdflib`,
and the validator is not the bottleneck on this workload.

That bears directly on what to optimise next. Bitmap node sets, class-closure
caching and columnar term attributes all target a phase that is currently about
a fifth of the runtime, so halving it would move the total by a tenth. The two
changes that would actually shift these numbers are a conformance-only path that
skips building a report at all, and parse throughput.

**Validation scales superlinearly, and it should not.** Each 10× increase in
data costs about 16–17× in validation time (1.0ms → 17ms → 268ms), where focus
nodes are independent and the work ought to be close to linear. Some of that is
the `n log n` in sorting and deduplicating focus nodes, but not all of it; the
rest is most likely cache behaviour, since at 690k triples the three sorted
indexes total roughly 25MB and each binary search becomes a chain of misses.
This has not been profiled yet — it is a measurement, not a diagnosis — but it
is the first thing to look at, because it is the term that decides how the
engine behaves on graphs larger than these.

This is also a workload that favours a compiled engine: a small shapes graph,
many focus nodes, and cheap constraints. Shapes dominated by SPARQL constraints
or deep shape recursion would narrow the gap, since both engines then spend
their time inside a query evaluator.
