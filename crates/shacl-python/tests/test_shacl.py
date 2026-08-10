"""Smoke tests for the Python bindings.

Run with `maturin develop` first, then `pytest crates/shacl-python/tests`.
"""

import shacl

SHAPES = """
@prefix ex:   <http://ex/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .

ex:PersonShape a sh:NodeShape ;
  sh:targetClass ex:Person ;
  sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:datatype xsd:string ] .
"""

VALID = """
@prefix ex: <http://ex/> .
ex:a a ex:Person ; ex:name "Ada" .
"""

INVALID = """
@prefix ex: <http://ex/> .
ex:a a ex:Person .
ex:b a ex:Person ; ex:name 42 .
"""


def test_conforming_data():
    shapes = shacl.Shapes.from_turtle(SHAPES)
    report = shapes.validate_turtle(VALID)
    assert report.conforms
    assert report.results == []
    # A report is truthy exactly when the data conforms.
    assert report


def test_violations_are_reported():
    shapes = shacl.Shapes.from_turtle(SHAPES)
    report = shapes.validate_turtle(INVALID)

    assert not report
    assert len(report) == 2
    components = sorted(r.component for r in report.results)
    assert components == ["DatatypeConstraintComponent", "MinCountConstraintComponent"]

    missing = next(r for r in report.results if r.component == "MinCountConstraintComponent")
    assert missing.focus_node == "<http://ex/a>"
    assert missing.path == "<http://ex/name>"
    assert missing.severity == "Violation"
    # sh:minCount faults the focus node, not a particular value.
    assert missing.value is None


def test_shapes_are_reusable():
    shapes = shacl.Shapes.from_turtle(SHAPES)
    assert len(shapes) > 0
    assert shapes.validate_turtle(VALID).conforms
    assert not shapes.validate_turtle(INVALID).conforms


def test_errors_surface_as_exceptions():
    import pytest

    with pytest.raises(ValueError):
        shacl.Shapes.from_turtle("this is not turtle @@@")
    with pytest.raises(IOError):
        shacl.Shapes.from_file("does-not-exist.ttl")


def test_one_shapes_object_is_usable_from_many_threads():
    """A single Shapes must be safe to share, not just safe to copy.

    Validation clones the term store per run rather than locking a shared one,
    so concurrent callers do not serialise on each other.
    """
    import concurrent.futures

    shapes = shacl.Shapes.from_turtle(SHAPES)

    def run(i):
        return shapes.validate_turtle(VALID if i % 2 == 0 else INVALID).conforms

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        got = list(pool.map(run, range(64)))

    assert got == [i % 2 == 0 for i in range(64)]


def test_repeated_validation_does_not_accumulate_state():
    """Results must not depend on how many runs came before.

    The store used to be shared and mutated, so every data graph ever validated
    stayed interned in it.
    """
    shapes = shacl.Shapes.from_turtle(SHAPES)
    counts = {len(shapes.validate_turtle(INVALID)) for _ in range(20)}
    assert counts == {2}


def test_report_is_available_as_rdf():
    """The specification's artefact is a graph, not a list of strings.

    The attributes are a convenience; `serialize` is what another RDF tool
    should be handed.
    """
    shapes = shacl.Shapes.from_turtle(SHAPES)
    report = shapes.validate_turtle(INVALID)

    ttl = report.serialize()
    assert "sh:ValidationReport" in ttl
    assert "sh:conforms false" in ttl
    assert "sh:DatatypeConstraintComponent" in ttl
    assert ttl == report.turtle
    assert report.serialize("ttl") == ttl

    conforming = shapes.validate_turtle(VALID).serialize()
    assert "sh:conforms true" in conforming
    assert "sh:result " not in conforming


def test_unknown_serialisation_format_is_rejected():
    import pytest

    shapes = shacl.Shapes.from_turtle(SHAPES)
    with pytest.raises(ValueError):
        shapes.validate_turtle(VALID).serialize("yaml")
