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

The GIL is released around parsing and validation, so threads calling into
separate `Shapes` objects run genuinely in parallel. A single `Shapes` is
internally locked, because the term store it shares between the two graphs is
not safe to mutate concurrently.
