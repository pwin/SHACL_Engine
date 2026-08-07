#!/usr/bin/env python3
"""Generates synthetic data and shapes graphs for benchmarking.

The data is deliberately ordinary: a class hierarchy, a handful of literal
properties, and links between instances. A fixed fraction of instances is made
invalid so that both engines build a report of comparable size rather than
racing to an empty one.
"""

import argparse
import pathlib
import random

SHAPES = """\
@prefix ex:   <http://example.org/ns#> .
@prefix rdf:  <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix sh:   <http://www.w3.org/ns/shacl#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .

ex:Agent    a rdfs:Class .
ex:Person   a rdfs:Class ; rdfs:subClassOf ex:Agent .
ex:Employee a rdfs:Class ; rdfs:subClassOf ex:Person .

ex:PersonShape
  a sh:NodeShape ;
  sh:targetClass ex:Person ;
  sh:property [
    sh:path ex:name ;
    sh:minCount 1 ; sh:maxCount 1 ;
    sh:datatype xsd:string ;
    sh:minLength 2 ;
  ] ;
  sh:property [
    sh:path ex:age ;
    sh:maxCount 1 ;
    sh:datatype xsd:integer ;
    sh:minInclusive 0 ;
    sh:maxInclusive 150 ;
  ] ;
  sh:property [
    sh:path ex:email ;
    sh:minCount 1 ;
    sh:nodeKind sh:Literal ;
    sh:pattern "^[^@]+@[^@]+\\\\.[a-z]+$" ;
  ] ;
  sh:property [
    sh:path ex:knows ;
    sh:nodeKind sh:IRI ;
    sh:class ex:Agent ;
  ] ;
  sh:property [
    sh:path ( ex:knows ex:name ) ;
    sh:datatype xsd:string ;
  ] ;
.
"""

# The class hierarchy belongs in the *data* graph: SHACL resolves class
# membership there, not in the shapes graph. Leaving it only in the shapes would
# make sh:class ex:Agent fail for every instance — correct per spec, but it
# would swamp the benchmark with one systematic error instead of exercising
# subclass closure.
PREAMBLE = """\
@prefix ex:   <http://example.org/ns#> .
@prefix rdf:  <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .

ex:Agent    a rdfs:Class .
ex:Person   a rdfs:Class ; rdfs:subClassOf ex:Agent .
ex:Employee a rdfs:Class ; rdfs:subClassOf ex:Person .

"""


def generate(n: int, invalid_ratio: float, seed: int) -> str:
    rng = random.Random(seed)
    out = [PREAMBLE]
    for i in range(n):
        subject = f"ex:p{i}"
        # A third of instances are Employees, exercising subclass closure.
        cls = "ex:Employee" if i % 3 == 0 else "ex:Person"
        lines = [f"{subject} a {cls}"]

        if rng.random() < invalid_ratio:
            # Each invalid instance breaks exactly one constraint, so the
            # result count scales predictably with the ratio.
            match rng.randrange(4):
                case 0:
                    lines.append('ex:name "x"')          # too short
                    lines.append(f'ex:email "p{i}@example.com"')
                case 1:
                    lines.append(f'ex:name "Person {i}"')
                    lines.append('ex:email "not-an-email"')   # pattern
                case 2:
                    lines.append(f'ex:name "Person {i}"')
                    lines.append(f'ex:email "p{i}@example.com"')
                    lines.append('ex:age "200"^^xsd:integer')  # out of range
                case _:
                    lines.append(f'ex:name "Person {i}"')      # no email
        else:
            lines.append(f'ex:name "Person {i}"')
            lines.append(f'ex:email "p{i}@example.com"')
            lines.append(f'ex:age "{rng.randrange(18, 80)}"^^xsd:integer')

        # Link to a few other instances, giving the path constraints work.
        for _ in range(3):
            if n > 1:
                lines.append(f"ex:knows ex:p{rng.randrange(n)}")

        out.append(" ;\n  ".join(lines) + " .\n")
    return "".join(out)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=pathlib.Path, required=True)
    ap.add_argument("--sizes", type=int, nargs="+", default=[1000, 10000, 100000])
    ap.add_argument("--invalid-ratio", type=float, default=0.1)
    ap.add_argument("--seed", type=int, default=42)
    args = ap.parse_args()

    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "shapes.ttl").write_text(SHAPES, encoding="utf-8")
    print(f"wrote {args.out / 'shapes.ttl'}")

    for n in args.sizes:
        path = args.out / f"data-{n}.ttl"
        path.write_text(generate(n, args.invalid_ratio, args.seed), encoding="utf-8")
        print(f"wrote {path} ({path.stat().st_size / 1e6:.1f} MB)")


if __name__ == "__main__":
    main()
