"""Type stubs for the `shacl` extension module.

Kept alongside the Rust source rather than generated, because PyO3's runtime
docstrings carry no types: without this there is no static checking and no
editor completion.

Every signature here has to match `src/lib.rs`. `tests/test_stubs.py` checks
the names and defaults against the built module, so a drift is a test failure
rather than something a user discovers.
"""

from typing import Final, Sequence

__version__: Final[str]

class Result:
    """One SHACL validation result."""

    # The node the violation is about, in N-Triples syntax.
    focus_node: str
    # The offending value, if the constraint named one, in N-Triples syntax.
    value: str | None
    # The offending value with its RDF syntax removed: the lexical form of a
    # literal, the IRI of a named node, the label of a blank node. "12x" where
    # `value` is '"12x"^^<http://www.w3.org/2001/XMLSchema#integer>'. Both are
    # here because `value` identifies the term and `value_plain` says what it
    # holds, which is what a report prints and what a de-duplication key wants.
    value_plain: str | None
    # The `sh:resultPath` in SPARQL property-path syntax, e.g. "<http://ex/p>"
    # or "(<http://ex/p>)+" — rendered, because the path *node* is a blank
    # node for anything but a bare predicate.
    path: str | None
    # The shape that raised it. A constraint inside `sh:property [ ... ]`
    # reports the nested property shape, which is a blank node and so cannot
    # be looked up in a separately-parsed copy of the shapes graph.
    source_shape: str | None
    # The nearest enclosing shape that has an IRI, found by walking
    # `sh:property` upwards. This is the one a registry keyed by shape IRI can
    # match. Equal to `source_shape` when the constraint sits on a named shape.
    root_shape: str | None
    # Local name of the constraint component, e.g. "MinCountConstraintComponent".
    component: str
    # The same as a full IRI, which is what distinguishes two custom
    # constraint components whose local names collide.
    component_iri: str
    # Local name of the severity: "Violation", "Warning" or "Info".
    severity: str
    severity_iri: str
    # The first `sh:message`; `messages` holds all of them, which matters when
    # a shape carries one per language.
    message: str | None
    messages: list[str]

    def __repr__(self) -> str: ...

class Report:
    """The outcome of a validation run."""

    # True when nothing of blocking severity was reported.
    conforms: bool
    results: list[Result]
    # The report as Turtle; equivalent to serialize("turtle").
    turtle: str

    def serialize(self, format: str = "turtle") -> str:
        """The validation report as RDF.

        This is the artefact SHACL actually defines, and what to hand to
        another RDF tool. Accepts "turtle", "ntriples", "rdfxml", "jsonld" and
        "n3", with the usual aliases.
        """
        ...

    def __repr__(self) -> str: ...
    # Truthy exactly when the data conforms.
    def __bool__(self) -> bool: ...
    # The number of results.
    def __len__(self) -> int: ...

class Shapes:
    """A compiled shapes graph, ready to validate against.

    Immutable after construction and safe to share across threads without a
    lock: each validation clones the term store rather than locking a shared
    one. Compiling once and validating many times is the case worth having.
    """

    @staticmethod
    def from_file(path: str) -> Shapes:
        """Compiles a shapes graph from a file, format taken from the extension."""
        ...

    @staticmethod
    def from_turtle(text: str, base: str = "http://example.org/shapes") -> Shapes:
        """Compiles a shapes graph from Turtle held in memory."""
        ...

    @staticmethod
    def from_text(
        text: str,
        format: str = "turtle",
        base: str = "http://example.org/shapes",
    ) -> Shapes:
        """Compiles a shapes graph from text in any supported format."""
        ...

    def validate_file(
        self,
        path: str,
        inference: str = "none",
        max_results: int | None = None,
    ) -> Report:
        """Validates a data graph read from a file.

        `max_results` stops the run once that many conformance-blocking
        results exist. It is a real early exit rather than a truncation, so it
        bounds the memory a run costs as well as its time — which is the
        reason to want it on a graph large enough that one systematically
        failing shape can produce results in the hundreds of thousands.
        """
        ...

    def validate_turtle(
        self,
        text: str,
        base: str = "http://example.org/data",
        inference: str = "none",
        max_results: int | None = None,
    ) -> Report:
        """Validates a data graph held in memory as Turtle.

        `max_results` stops the run once that many conformance-blocking
        results exist. It is a real early exit rather than a truncation, so it
        bounds the memory a run costs as well as its time — which is the
        reason to want it on a graph large enough that one systematically
        failing shape can produce results in the hundreds of thousands.
        """
        ...

    def validate_text(
        self,
        text: str,
        format: str = "turtle",
        base: str = "http://example.org/data",
        inference: str = "none",
        max_results: int | None = None,
    ) -> Report:
        """Validates a data graph held in memory, in any supported format.

        `inference` materialises triples into the data graph first:

        - "none" (the default)
        - "rdfs" — the RDFS closure
        - "rules" — SHACL-AF rules (`sh:rule`), one pass, as the spec defines
        - "rules-iterated" — the same, repeated to a fixpoint, which a
          transitive rule needs and the spec does not define

        Off by default because it changes what the report says, so it should
        be asked for rather than assumed.
        """
        ...

    def __repr__(self) -> str: ...
    # The number of compiled shapes.
    def __len__(self) -> int: ...

def validate(
    data_path: str,
    shapes_path: str | None = None,
    inference: str = "none",
    max_results: int | None = None,
) -> Report:
    """Validates `data_path` against `shapes_path` in one call.

    Use `Shapes` directly when the same shapes are reused — compiling once is
    most of the benefit.

    `shapes_path` may be omitted for a self-describing document that carries
    its own shapes.

    Raises `ValueError` if the shapes graph declares no shapes. Validating
    against no shapes conforms, so without this the call would report the data
    valid without having checked anything — which is what passing these two
    arguments the wrong way round looks like. `Shapes.from_file` does not
    raise; check `len()` there instead.
    """
    ...

__all__: Sequence[str]
