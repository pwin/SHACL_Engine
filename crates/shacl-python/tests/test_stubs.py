"""Checks `shacl.pyi` against the module that actually gets built.

A stub is a promise made in a separate file from the code it describes, so
nothing but a test keeps the two together. The failure it prevents is quiet:
an editor completes a method that was renamed, or a type checker passes code
that raises at runtime, and the stub looks authoritative the whole time.

Both directions are checked. A stub that describes something gone is wrong in
the obvious way; a stub missing something the module exports is wrong in the
way nobody notices, because everything it does say is still true.
"""

import ast
import pathlib

import pytest

import shacl

STUB = pathlib.Path(__file__).resolve().parent.parent / "shacl.pyi"


def stub_tree():
    return ast.parse(STUB.read_text(encoding="utf-8"))


def signature(node):
    """The parameter names and defaults of a stub function."""
    args = [a.arg for a in node.args.args if a.arg != "self"]
    defaults = [ast.literal_eval(d) for d in node.args.defaults]
    return args, defaults


def built_signature(obj):
    """The same, read back off the compiled module.

    PyO3 publishes `__text_signature__` with the defaults rendered as source,
    so it is parsed as Python rather than picked apart by hand — the defaults
    here are URLs, and splitting on punctuation would eventually meet one with
    a comma in it.
    """
    text = obj.__text_signature__
    text = text.replace("($self, ", "(").replace("($self)", "()")
    fn = ast.parse(f"def _f{text}: ...").body[0]
    return signature(fn)


def stub_classes():
    return {n.name: n for n in stub_tree().body if isinstance(n, ast.ClassDef)}


def stub_functions():
    return {n.name: n for n in stub_tree().body if isinstance(n, ast.FunctionDef)}


def test_the_stub_file_is_shipped():
    # Packaged via pyproject; without it the wheel type-checks as Any and
    # every other test here would pass against nothing.
    assert STUB.is_file()


@pytest.mark.parametrize("name", sorted(stub_classes()))
def test_stub_classes_exist(name):
    assert hasattr(shacl, name), f"{name} is in the stub but not the module"


@pytest.mark.parametrize(
    "cls,member",
    [
        (cls, m.name)
        for cls, node in stub_classes().items()
        for m in node.body
        if isinstance(m, ast.FunctionDef) and not m.name.startswith("__")
    ],
)
def test_stub_methods_exist(cls, member):
    assert hasattr(getattr(shacl, cls), member), f"{cls}.{member} is only in the stub"


@pytest.mark.parametrize(
    "cls,attr",
    [
        (cls, t.target.id)
        for cls, node in stub_classes().items()
        for t in node.body
        if isinstance(t, ast.AnnAssign)
    ],
)
def test_stub_attributes_exist(cls, attr):
    assert hasattr(getattr(shacl, cls), attr), f"{cls}.{attr} is only in the stub"


@pytest.mark.parametrize(
    "owner,name",
    [(None, f) for f in sorted(stub_functions())]
    + [
        (cls, m.name)
        for cls, node in stub_classes().items()
        for m in node.body
        if isinstance(m, ast.FunctionDef) and not m.name.startswith("__")
    ],
)
def test_defaults_match_the_built_module(owner, name):
    """A default that drifts is the worst kind of stub error: the call still
    works, and quietly does something other than what was read."""
    stub = stub_functions()[name] if owner is None else next(
        m
        for m in stub_classes()[owner].body
        if isinstance(m, ast.FunctionDef) and m.name == name
    )
    obj = getattr(shacl, name) if owner is None else getattr(getattr(shacl, owner), name)
    if not getattr(obj, "__text_signature__", None):
        pytest.skip(f"{name} publishes no signature to compare against")
    assert signature(stub) == built_signature(obj), (
        f"{owner or 'shacl'}.{name}: the stub and the module disagree"
    )


def test_everything_exported_is_in_the_stub():
    """The direction that catches a stub going stale rather than wrong."""
    declared = {
        t.target.id
        for t in stub_tree().body
        if isinstance(t, ast.AnnAssign) and isinstance(t.target, ast.Name)
    }
    described = set(stub_classes()) | set(stub_functions()) | declared
    missing = set(shacl.__all__) - described
    assert not missing, f"exported but undocumented in the stub: {sorted(missing)}"


def test_all_matches_what_the_module_holds():
    for name in shacl.__all__:
        assert hasattr(shacl, name), f"__all__ promises {name}, which does not exist"
