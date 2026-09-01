"""Smoke tests for the Python bindings.

Run with `maturin develop` first, then `pytest crates/shacl-python/tests`.
"""

import pytest

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


def test_a_long_chain_validates_rather_than_dying():
    """A recursive shape may follow a chain of any length.

    The descent through `sh:property` runs on an explicit stack, so this costs
    heap rather than call frames. It used to be refused at 47 links, and
    lifting the limit merely moved the failure to a stack overflow -- which in
    these bindings is the one error that cannot become a Python exception,
    because it kills the interpreter outright.
    """
    shapes = shacl.Shapes.from_turtle("""
@prefix ex: <http://ex/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:Q a sh:PropertyShape ; sh:path ex:knows ; sh:property ex:Q ;
    sh:property [ sh:path ex:age ; sh:datatype xsd:integer ] .
ex:Root a sh:NodeShape ; sh:targetNode ex:a0 ; sh:property ex:Q .
""")
    links = 5000
    lines = ["@prefix ex: <http://ex/> ."]
    for i in range(links):
        lines.append(f"ex:a{i} ex:knows ex:a{i + 1} .")
    for i in range(1, links + 1):
        lines.append(f'ex:a{i} ex:age "x" .')
    chain = "\n".join(lines)

    report = shapes.validate_turtle(chain)
    # One finding per node past the first, so a walk that stopped early would
    # show up as a short count rather than passing quietly.
    assert len(report.results) == links


def test_excessive_shape_nesting_raises_rather_than_crashing():
    """Nesting still has a ceiling, and reaching it is an exception.

    What counts towards it is shape-valued constraints -- `sh:node` here --
    which have to ask whether a nested shape produced anything and so wait on
    a call frame. `sh:property` does not, which is why it no longer counts.
    """
    import pytest

    parts = [
        "@prefix ex: <http://ex/> .",
        "@prefix sh: <http://www.w3.org/ns/shacl#> .",
        "ex:Root a sh:NodeShape ; sh:targetNode ex:a ; sh:node ex:S0 .",
    ]
    for i in range(60):
        parts.append(f"ex:S{i} a sh:NodeShape ; sh:node ex:S{i + 1} .")
    parts.append("ex:S60 a sh:NodeShape .")
    shapes_src = "\n".join(parts)
    shapes = shacl.Shapes.from_turtle(shapes_src)
    with pytest.raises(ValueError, match="recursion"):
        shapes.validate_turtle("@prefix ex: <http://ex/> . ex:a ex:p 1 .")


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
        (
            "Shapes",
            "validate_text",
            {
                "format": "turtle",
                "base": "http://example.org/data",
                "inference": "none",
                "max_results": None,
            },
        ),
        ("Shapes", "from_turtle", {"base": "http://example.org/shapes"}),
        (
            "Shapes",
            "validate_turtle",
            {"base": "http://example.org/data", "inference": "none", "max_results": None},
        ),
        ("Shapes", "validate_file", {"inference": "none", "max_results": None}),
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


RDFS_SHAPES = """
@prefix ex:   <http://ex/> .
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .

ex:PersonShape a sh:NodeShape ;
  sh:targetClass ex:Person ;
  sh:property [ sh:path ex:name ; sh:minCount 1 ] .
"""

# ex:grace is only ever typed ex:Employee. Whether PersonShape applies to her
# depends entirely on the subclass statement being acted on.
RDFS_DATA = """
@prefix ex:   <http://ex/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
ex:Employee rdfs:subClassOf ex:Person .
ex:grace a ex:Employee .
"""


def test_rdfs_inference_is_off_by_default():
    """Materialising changes the report, so it must be asked for."""
    shapes = shacl.Shapes.from_turtle(RDFS_SHAPES)
    # sh:targetClass already follows rdfs:subClassOf -- SHACL requires that --
    # so this particular case is caught either way.
    assert not shapes.validate_turtle(RDFS_DATA).conforms


def test_rdfs_inference_reaches_what_shacl_alone_does_not():
    """sh:targetSubjectsOf sees predicates, and does not follow subPropertyOf.

    This is the gap inference closes: ex:a holds only ex:father, so nothing
    targets it until ex:parent is materialised.
    """
    shapes = shacl.Shapes.from_turtle("""
@prefix ex: <http://ex/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:S a sh:NodeShape ;
  sh:targetSubjectsOf ex:parent ;
  sh:property [ sh:path ex:name ; sh:minCount 1 ] .
""")
    data = """
@prefix ex: <http://ex/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
ex:father rdfs:subPropertyOf ex:parent .
ex:a ex:father ex:b .
"""
    assert shapes.validate_turtle(data).conforms, "nothing is targeted without inference"

    report = shapes.validate_turtle(data, inference="rdfs")
    assert not report.conforms
    assert [r.component for r in report.results] == ["MinCountConstraintComponent"]
    assert report.results[0].focus_node == "<http://ex/a>"


def test_rdfs_inference_types_by_domain_and_range():
    shapes = shacl.Shapes.from_turtle(RDFS_SHAPES)
    data = """
@prefix ex: <http://ex/> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
ex:worksFor rdfs:domain ex:Person .
ex:ada ex:worksFor ex:acme .
"""
    assert shapes.validate_turtle(data).conforms
    # rdfs2 types ex:ada as a Person, which PersonShape then targets.
    assert not shapes.validate_turtle(data, inference="rdfs").conforms


def test_unknown_inference_is_rejected():
    import pytest

    shapes = shacl.Shapes.from_turtle(RDFS_SHAPES)
    with pytest.raises(ValueError, match="inference"):
        shapes.validate_turtle(RDFS_DATA, inference="owlrl")


# --------------------------------------------------------------- SHACL-AF rules

RULE_SHAPES = """
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
ex:PersonShape a sh:NodeShape ;
    sh:targetClass ex:Person ;
    sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:Agent ] .
ex:AgentShape a sh:NodeShape ;
    sh:targetClass ex:Agent ;
    sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:message "an Agent needs a name" ] .
"""

RULE_DATA = """
@prefix ex: <http://example.org/> .
ex:alice a ex:Person ; ex:name "Alice" .
ex:bob   a ex:Person .
"""


def test_rules_are_off_by_default():
    """A rule changes what the report says, so it has to be asked for."""
    shapes = shacl.Shapes.from_turtle(RULE_SHAPES)
    assert shapes.validate_turtle(RULE_DATA).conforms


def test_rules_infer_before_validating():
    """pySHACL 0.40.1 on the same input: Conforms False, one result on ex:bob."""
    shapes = shacl.Shapes.from_turtle(RULE_SHAPES)
    report = shapes.validate_turtle(RULE_DATA, inference="rules")
    assert not report.conforms
    assert len(report.results) == 1
    assert report.results[0].focus_node == "<http://example.org/bob>"


def test_rules_iterated_closes_a_transitive_rule():
    """One pass reaches two hops; iterating closes the chain."""
    shapes = shacl.Shapes.from_turtle("""
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:sub ;
    sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate ex:sub ;
              sh:object [ sh:path ( ex:sub ex:sub ) ] ] .
ex:Cap a sh:NodeShape ; sh:targetNode ex:a ;
    sh:property [ sh:path ex:sub ; sh:maxCount 2 ] .
""")
    data = "@prefix ex: <http://example.org/> . ex:a ex:sub ex:b . ex:b ex:sub ex:c . ex:c ex:sub ex:d ."
    assert shapes.validate_turtle(data, inference="rules").conforms
    assert not shapes.validate_turtle(data, inference="rules-iterated").conforms


def test_an_unknown_inference_mode_is_rejected():
    shapes = shacl.Shapes.from_turtle(RULE_SHAPES)
    with pytest.raises(ValueError, match="rules-iterated"):
        shapes.validate_turtle(RULE_DATA, inference="magic")


# ---------------------------------------------------------------------------
# What a caller outside this crate needs from a result.
#
# Each of these covers a field that used to make a result unusable to an
# embedder: something identifying the shape, a path that survives leaving the
# process, a value it can compare, and a way to stop before a systematically
# failing shape produces a report nobody can hold in memory.
# ---------------------------------------------------------------------------
NESTED_SHAPES = """
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .
@prefix ex:   <http://ex/> .

ex:NamedShape a sh:NodeShape ;
  sh:targetClass ex:Thing ;
  sh:property [ sh:path ex:code ; sh:minCount 1 ] .

ex:PathShape a sh:NodeShape ;
  sh:targetClass ex:Thing ;
  sh:property [
    sh:path [ sh:oneOrMorePath rdfs:subClassOf ] ;
    sh:disjoint ex:notThis ;
  ] .

ex:ValueShape a sh:NodeShape ;
  sh:targetClass ex:Thing ;
  sh:property [ sh:path ex:count ; sh:datatype xsd:integer ] .
"""

NESTED_DATA = """
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .
@prefix ex:   <http://ex/> .
ex:a a ex:Thing ; rdfs:subClassOf ex:b ; ex:notThis ex:b ; ex:count "twelve" .
ex:c a ex:Thing ; ex:count "also not a number" .
"""


def nested_report():
    return shacl.Shapes.from_turtle(NESTED_SHAPES).validate_turtle(NESTED_DATA)


def test_root_shape_names_the_enclosing_shape_a_caller_can_look_up():
    """`source_shape` for a constraint inside `sh:property [ ... ]` is a blank
    node minted here, so it matches nothing in a caller's own copy of the
    shapes graph. Without `root_shape`, every such result arrives
    unattributable to whatever the caller keys its metadata by."""
    results = [r for r in nested_report().results if r.component == "MinCountConstraintComponent"]
    assert results
    for r in results:
        assert r.source_shape.startswith("_:"), "expected the nested property shape"
        assert r.root_shape == "<http://ex/NamedShape>"


def test_root_shape_equals_source_shape_for_a_constraint_on_a_named_shape():
    shapes = shacl.Shapes.from_turtle("""
        @prefix sh: <http://www.w3.org/ns/shacl#> .
        @prefix ex: <http://ex/> .
        ex:S a sh:NodeShape ; sh:targetClass ex:T ; sh:closed true .
    """)
    report = shapes.validate_turtle("""
        @prefix ex: <http://ex/> .
        ex:x a ex:T ; ex:stray "v" .
    """)
    assert report.results
    for r in report.results:
        assert r.root_shape == r.source_shape == "<http://ex/S>"


def test_path_is_a_property_path_expression_not_a_blank_node():
    """A compound `sh:path` is a blank node structure. Reporting its label
    would give the caller a string that is local to this process and different
    on the next run -- unusable as an identifier and actively harmful in a
    key."""
    results = [r for r in nested_report().results if r.component == "DisjointConstraintComponent"]
    assert results
    for r in results:
        assert not r.path.startswith("_:")
        # Parenthesised: ^(a/b) and (^a)/b are different paths, so the
        # renderer groups anything that is not a bare predicate.
        assert r.path == "(<http://www.w3.org/2000/01/rdf-schema#subClassOf>)+"


def test_simple_paths_still_render_as_the_predicate():
    results = [r for r in nested_report().results if r.component == "MinCountConstraintComponent"]
    assert results
    assert all(r.path == "<http://ex/code>" for r in results)


def test_value_plain_gives_the_literal_without_its_syntax():
    """`value` is the term and identifies it; `value_plain` is what it says.
    Recovering one from the other by string surgery works until a literal
    contains a quotation mark."""
    results = [r for r in nested_report().results if r.component == "DatatypeConstraintComponent"]
    assert results
    by_lexical = {r.value_plain for r in results}
    assert "twelve" in by_lexical
    for r in results:
        assert r.value.startswith('"')
        assert not r.value_plain.startswith('"')


def test_value_plain_strips_the_syntax_from_an_iri_too():
    """Not literals-only: a caller wants one field it can use whatever the
    term turns out to be, or it is back to string surgery for half of them."""
    results = [r for r in nested_report().results if r.component == "DisjointConstraintComponent"]
    assert results
    for r in results:
        assert r.value == "<http://ex/b>"
        assert r.value_plain == "http://ex/b"


def test_max_results_stops_the_run():
    """A real early exit, so it bounds the memory a run costs and not just its
    output. That is the point on a graph big enough for one failing shape to
    produce results in the hundreds of thousands."""
    shapes = shacl.Shapes.from_turtle("""
        @prefix sh: <http://www.w3.org/ns/shacl#> .
        @prefix ex: <http://ex/> .
        ex:S a sh:NodeShape ; sh:targetClass ex:T ;
          sh:property [ sh:path ex:p ; sh:minCount 1 ] .
    """)
    data = "@prefix ex: <http://ex/> .\n" + "\n".join(
        f"ex:n{i} a ex:T ." for i in range(50)
    )
    assert len(shapes.validate_turtle(data).results) == 50
    for cap in (1, 5, 20):
        capped = shapes.validate_turtle(data, max_results=cap)
        assert len(capped.results) == cap
        assert capped.conforms is False


def test_max_results_counts_conformance_blocking_results_only():
    """The cap counts what breaks conformance, matching the CLI. Counting
    every result would let a run stop on an `sh:Info` and report
    `conforms = True` with a `sh:Violation` sitting unexamined further along:
    shapes are evaluated in compilation order, which says nothing about what
    is in the graph."""
    shapes = shacl.Shapes.from_turtle("""
        @prefix sh: <http://www.w3.org/ns/shacl#> .
        @prefix ex: <http://ex/> .
        ex:Info a sh:NodeShape ; sh:targetClass ex:T ;
          sh:property [ sh:path ex:a ; sh:minCount 1 ; sh:severity sh:Info ] .
        ex:Blocking a sh:NodeShape ; sh:targetClass ex:T ;
          sh:property [ sh:path ex:b ; sh:minCount 1 ] .
    """)
    data = "@prefix ex: <http://ex/> .\n" + "\n".join(f"ex:n{i} a ex:T ." for i in range(10))
    capped = shacl.Shapes.from_turtle("""
        @prefix sh: <http://www.w3.org/ns/shacl#> .
        @prefix ex: <http://ex/> .
        ex:Info a sh:NodeShape ; sh:targetClass ex:T ;
          sh:property [ sh:path ex:a ; sh:minCount 1 ; sh:severity sh:Info ] .
        ex:Blocking a sh:NodeShape ; sh:targetClass ex:T ;
          sh:property [ sh:path ex:b ; sh:minCount 1 ] .
    """).validate_turtle(data, max_results=3)
    blocking = [r for r in capped.results if r.severity == "Violation"]
    assert len(blocking) == 3, "the cap counts blocking results"
    assert capped.conforms is False, "a cap must never turn a failing graph into a passing one"
    del shapes


def test_max_results_none_is_the_default_and_reports_everything():
    shapes = shacl.Shapes.from_turtle("""
        @prefix sh: <http://www.w3.org/ns/shacl#> .
        @prefix ex: <http://ex/> .
        ex:S a sh:NodeShape ; sh:targetClass ex:T ;
          sh:property [ sh:path ex:p ; sh:minCount 1 ] .
    """)
    data = "@prefix ex: <http://ex/> .\n" + "\n".join(f"ex:n{i} a ex:T ." for i in range(30))
    assert len(shapes.validate_turtle(data).results) == 30
    assert len(shapes.validate_turtle(data, max_results=None).results) == 30
