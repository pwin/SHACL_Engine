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
| 1,000 | 6,914 | 88 | 0.021s | 1.369s | **64×** | 0.0010s |
| 10,000 | 69,027 | 997 | 0.146s | 8.412s | **58×** | 0.0120s |
| 100,000 | 689,861 | 10,179 | 1.422s | 81.654s | **57×** | 0.1520s |

Both engines report identical result counts at every size.

At the largest size that is ~485k triples/second end to end, or ~4.5M
triples/second through the validator alone, against ~8.4k/second for pySHACL.

Run the benchmark with nothing else on the machine. An earlier run taken while
a compile was in progress inflated the 100k end-to-end figure by 70% while
leaving the validate-only figure alone — the parse phase is what picks up the
contention.

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

**Validation used to scale superlinearly**, at about 16–17× per 10× of data.
It is now 12.0× and 12.7×, and validation at 100k dropped from 0.268s to
0.152s. Two changes got it there, both found by profiling rather than guessed
at, and both attacking the same underlying cause.

`cargo run --release --example profile -- <data> <shapes>` breaks the run down
by phase and by constraint. What it showed was **probe locality**, not the
amount of work:

| | sorted probes | random probes | penalty |
| --- | ---: | ---: | ---: |
| 10,000 instances (69k triples) | 73 ns | 119 ns | 1.6× |
| 100,000 instances (690k triples) | 85 ns | 277 ns | 3.3× |

Probing the index in sorted order is nearly flat across a 10× size increase —
73ns to 85ns — because consecutive binary searches share cache lines. Probing in
scrambled order degrades 2.3×, and the *penalty itself* grows with size as the
three sorted indexes outgrow cache at roughly 25MB.

That is why the sequence path `( ex:knows ex:name )` is the worst offender,
taking 73% of path evaluation at 100k and scaling ~25× per 10× of data. Its
first step probes focus nodes in sorted order and behaves well; its second step
probes whatever the first step reached, which bears no relation to index order.

### What was done about it

**Compound paths evaluate as a batched sort-merge join.** The traversal state is
a list of `(origin, reached)` pairs covering every focus node at once, so the
whole frontier can be sorted between steps without losing track of which focus
node a value belongs to. Each step probes in sorted node order, and origins that
reached the same node share one probe. The sequence path went from 0.113s to
0.043s at 100k, and from 25.6× to 14.7× per 10× of data.

**`sh:class` tests against a materialised instance set.** It had become the
largest single cost — 0.080s of 0.254s, 86% of all constraint checking —
probing `rdf:type` once per value node, 300k times, each landing at a random
offset in the index. Building the instance set once and testing membership by
binary search moves that random access from an index too large for cache to an
array of a few hundred kilobytes that stays resident.

Pooling the working buffers that compound paths allocated per focus node was
also done, and was worth about 10% — allocation was real but minor beside
locality.

### Where the time goes now

Parsing, overwhelmingly: 1.27s of the 1.42s at 100k, or 89%. Validation is
0.152s of it, and roughly 60% of *that* is still path evaluation.

So the next lever is the parser, not the validator. Within validation, the
remaining candidates are a conformance-only path that skips building a report
when the caller only wants a boolean, and parallelising across focus nodes,
which are independent. Neither is likely to show up end to end until parsing
improves.

This is also a workload that favours a compiled engine: a small shapes graph,
many focus nodes, and cheap constraints. Shapes dominated by SPARQL constraints
or deep shape recursion would narrow the gap, since both engines then spend
their time inside a query evaluator.
