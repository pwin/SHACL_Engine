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


def test_report_serialises_to_every_format_the_engine_supports():
    """The report is a graph, so the choice of syntax is the caller's."""
    shapes = shacl.Shapes.from_turtle(SHAPES)
    report = shapes.validate_turtle(INVALID)

    assert "<rdf:RDF" in report.serialize("rdfxml")
    assert report.serialize("jsonld").lstrip().startswith(("[", "{"))
    # N-Triples writes one absolute triple per line and no prefixes.
    nt = report.serialize("nt")
    assert "@prefix" not in nt
    assert nt.count("http://www.w3.org/ns/shacl#ValidationResult") == 2

    # Aliases reach the same writer, and case is not significant.
    assert report.serialize("n-triples") == nt
    assert report.serialize("NT") == nt


def test_in_memory_documents_need_not_be_turtle():
    """An rdflib.Graph should not have to go through Turtle to get here."""
    shapes_nt = (
        '<http://ex/PersonShape> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> '
        '<http://www.w3.org/ns/shacl#NodeShape> .\n'
        '<http://ex/PersonShape> <http://www.w3.org/ns/shacl#targetClass> '
        '<http://ex/Person> .\n'
        '<http://ex/PersonShape> <http://www.w3.org/ns/shacl#property> _:p .\n'
        '_:p <http://www.w3.org/ns/shacl#path> <http://ex/name> .\n'
        '_:p <http://www.w3.org/ns/shacl#minCount> '
        '"1"^^<http://www.w3.org/2001/XMLSchema#integer> .\n'
    )
    shapes = shacl.Shapes.from_text(shapes_nt, "ntriples")
    assert len(shapes) > 0

    data_nt = (
        '<http://ex/a> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> '
        '<http://ex/Person> .\n'
    )
    assert not shapes.validate_text(data_nt, "ntriples").conforms
    assert shapes.validate_text(VALID, "turtle").conforms

    # The same shapes read either way must behave identically.
    assert shacl.Shapes.from_text(SHAPES).validate_turtle(INVALID).results != []


def test_results_expose_full_iris_and_every_message():
    """Local names collide; a custom component is only identified by its IRI."""
    shapes = shacl.Shapes.from_turtle("""
@prefix ex:  <http://ex/> .
@prefix sh:  <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:S a sh:NodeShape ;
  sh:targetNode ex:a ;
  sh:property [
    sh:path ex:name ; sh:minCount 1 ; sh:severity sh:Warning ;
    sh:message "a name is required"@en, "il faut un nom"@fr ;
  ] .
""")
    report = shapes.validate_turtle("@prefix ex: <http://ex/> . ex:a ex:other 1 .")
    (r,) = report.results

    assert r.component == "MinCountConstraintComponent"
    assert r.component_iri == "http://www.w3.org/ns/shacl#MinCountConstraintComponent"
    assert r.severity == "Warning"
    assert r.severity_iri == "http://www.w3.org/ns/shacl#Warning"

    # Both messages survive; `message` is the first of them, not the only one.
    assert sorted(r.messages) == ["a name is required", "il faut un nom"]
    assert r.message in r.messages

    # sh:Warning does not block conformance by default.
    assert report.conforms


def test_unknown_parse_format_is_rejected():
    import pytest

    with pytest.raises(ValueError):
        shacl.Shapes.from_text(SHAPES, "yaml")
    with pytest.raises(ValueError):
        shacl.Shapes.from_turtle(SHAPES).validate_text(VALID, "yaml")


def test_recursive_shapes_do_not_kill_the_interpreter():
    """A stack overflow is not a panic and cannot be turned into an exception.

    Two property shapes naming each other over a data cycle used to take the
    whole process down, which is the one failure the bindings' error handling
    could not contain.
    """
    shapes = shacl.Shapes.from_turtle("""
@prefix ex: <http://ex/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:Q1 a sh:PropertyShape ; sh:path ex:knows ; sh:property ex:Q2 .
ex:Q2 a sh:PropertyShape ; sh:path ex:knows ; sh:property ex:Q1 .
ex:Root a sh:NodeShape ; sh:targetNode ex:a ; sh:property ex:Q1 .
""")
    report = shapes.validate_turtle(
        "@prefix ex: <http://ex/> . ex:a ex:knows ex:b . ex:b ex:knows ex:a ."
    )
    assert report.conforms


def test_excessive_nesting_raises_rather_than_crashing():
    """Beyond the depth limit the answer is an exception, not a dead process."""
    import pytest

    shapes = shacl.Shapes.from_turtle("""
@prefix ex: <http://ex/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:Q a sh:PropertyShape ; sh:path ex:knows ; sh:property ex:Q .
ex:Root a sh:NodeShape ; sh:targetNode ex:a0 ; sh:property ex:Q .
""")
    chain = "@prefix ex: <http://ex/> .\n" + "".join(
        f"ex:a{i} ex:knows ex:a{i + 1} .\n" for i in range(500)
    )
    with pytest.raises(ValueError, match="recursion"):
        shapes.validate_turtle(chain)


def test_stubs_match_the_module():
    """The .pyi is hand-written, so something has to hold it to the module.

    Checks the public names line up in both directions -- a stub that has
    drifted is worse than none, because a type checker believes it.
    """
    import ast
    import inspect
    import pathlib

    stub = pathlib.Path(shacl.__file__).with_name("__init__.pyi")
    tree = ast.parse(stub.read_text(encoding="utf-8"))

    stub_classes = {
        n.name: {b.name for b in n.body if isinstance(b, ast.FunctionDef)}
        for n in tree.body
        if isinstance(n, ast.ClassDef)
    }
    assert set(stub_classes) == {"Result", "Report", "Shapes"}

    for name, stub_methods in stub_classes.items():
        actual = {
            m
            for m in vars(getattr(shacl, name))
            if not m.startswith("_") or m in {"__repr__", "__bool__", "__len__"}
        }
        # Attributes are declared in the stub as annotations, not methods, so
        # compare only what the stub calls a method.
        missing = {m for m in actual if m not in stub_methods and callable(
            getattr(getattr(shacl, name), m, None)
        )}
        assert not missing, f"{name}: undeclared in the stub: {sorted(missing)}"
        extra = {m for m in stub_methods if not hasattr(getattr(shacl, name), m)}
        assert not extra, f"{name}: in the stub but not the module: {sorted(extra)}"

    # Defaults must agree too, since a wrong one type-checks and then misbehaves.
    for cls, method, expected in [
        ("Report", "serialize", {"format": "turtle"}),
        ("Shapes", "from_text", {"format": "turtle", "base": "http://example.org/shapes"}),
        ("Shapes", "validate_text", {"format": "turtle", "base": "http://example.org/data"}),
        ("Shapes", "from_turtle", {"base": "http://example.org/shapes"}),
        ("Shapes", "validate_turtle", {"base": "http://example.org/data"}),
    ]:
        node = next(
            n
            for n in tree.body
            if isinstance(n, ast.ClassDef) and n.name == cls
            for n in n.body
            if isinstance(n, ast.FunctionDef) and n.name == method
        )
        args = node.args.args[-len(node.args.defaults):]
        got = {
            a.arg: ast.literal_eval(d)
            for a, d in zip(args, node.args.defaults)
        }
        assert got == expected, f"{cls}.{method}: stub defaults {got} != {expected}"
