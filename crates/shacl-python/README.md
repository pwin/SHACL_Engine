# shacl (Python)

Python bindings for the SHACL validation engine, built with
[PyO3](https://pyo3.rs) and [maturin](https://maturin.rs).

## Building

```sh
pip install maturin
cd crates/shacl-python
maturin develop --release      # build and install into the current venv
maturin build --release        # produce a wheel in target/wheels
```

The extension targets the stable ABI (`abi3-py39`), so one wheel serves every
CPython from 3.9 upwards.

## Usage

```python
import shacl

# Compile once, validate many: shapes are fixed while data changes, and
# compiling is a meaningful share of a small run.
shapes = shacl.Shapes.from_file("shapes.ttl")
report = shapes.validate_file("data.ttl")

if not report.conforms:
    for r in report.results:
        print(r.severity, r.component, r.focus_node, r.value)
```

`Report` is truthy when the data conforms and `len()` gives the result count, so
the common case reads plainly:

```python
if shapes.validate_file("data.ttl"):
    print("ok")
```

For a one-off, or for a self-describing document that carries its own shapes:

```python
shacl.validate("data.ttl")                  # shapes come from the data graph
shacl.validate("data.ttl", "shapes.ttl")
```

Turtle can also be passed directly, which is convenient for tests:

```python
shapes = shacl.Shapes.from_turtle(shapes_text)
report = shapes.validate_turtle(data_text)
```

## Notes

Terms are returned as strings in N-Triples syntax — `<http://ex/a>`,
`"42"^^<http://www.w3.org/2001/XMLSchema#integer>` — rather than as objects.
That keeps the binding free of a dependency on any particular Python RDF
library; parse them with whichever one you already use.

## Threading

A `Shapes` object is immutable and safe to share. Validation releases the GIL
and clones the term store per run rather than locking a shared one, so
concurrent callers do not serialise on each other.

Measured on stock CPython 3.14 with the GIL enabled — 16 validations of a
69k-triple graph through one shared `Shapes`:

| threads | throughput |
| ---: | ---: |
| 1 | 7.8 runs/s |
| 4 | 26.9 runs/s |
| 8 | 27.8 runs/s |

It plateaus at four because each validation already parses across threads
internally, so the cores are busy either way.

### Free-threaded builds

The module declares `gil_used = false`, so it will not force the GIL back on
when imported into a free-threaded interpreter (`python3.14t`). Note that
free-threading is a separate *build*, not something a version number brings: a
stock 3.14 still reports `sys._is_gil_enabled() == True`.

There is little to gain from it here. The GIL is already released around the
work, which is why the table above scales. Free-threading would only help code
doing significant Python-level work in parallel as well — and it costs the
single `abi3` wheel, since `abi3` is a no-op on free-threaded builds and PyO3
falls back to a version-specific one.

## The report as RDF

SHACL defines the validation report as a graph — a `sh:ValidationReport` — not
as a list of strings. The attributes above are a convenience for reading it from
Python; `serialize` is what to hand to another RDF tool.

```python
print(shapes.validate_file("data.ttl").serialize())
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
