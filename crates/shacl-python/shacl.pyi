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
    # The offending value, if the constraint named one.
    value: str | None
    # The `sh:resultPath`, if the shape had one.
    path: str | None
    # The shape that raised it.
    source_shape: str | None
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

    def validate_file(self, path: str, inference: str = "none") -> Report:
        """Validates a data graph read from a file."""
        ...

    def validate_turtle(
        self,
        text: str,
        base: str = "http://example.org/data",
        inference: str = "none",
    ) -> Report:
        """Validates a data graph held in memory as Turtle."""
        ...

    def validate_text(
        self,
        text: str,
        format: str = "turtle",
        base: str = "http://example.org/data",
        inference: str = "none",
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
) -> Report:
    """Validates `data_path` against `shapes_path` in one call.

    Use `Shapes` directly when the same shapes are reused — compiling once is
    most of the benefit.
    """
    ...

__all__: Sequence[str]
