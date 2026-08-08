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
