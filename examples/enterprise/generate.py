#!/usr/bin/env python3
"""Generates the enterprise data graph, with every fault seeded and counted.

    python examples/enterprise/generate.py --people 2000 --out examples/enterprise

Writes `data-<N>.ttl` and `expected-<N>.txt`. The second lists, per constraint
component, how many results a conforming SHACL processor reports for the
first — not an estimate, a count of the faults this script put in, each of
which produces a known number of results of known components.

Nothing here is random. A person's faults are a function of their index, so
the same `--people` gives the same bytes and the same expected counts on any
machine, and a count that disagrees with the engine is a disagreement about
SHACL semantics worth reading, not noise.

The predictions are commented against each fault. Where one fault produces
more than one result — an age of "forty" fails the datatype and both range
checks, because a string cannot be compared with a number — that is SHACL's
answer, and the count says so.
"""

import argparse
import collections
import pathlib

PREFIX = """\
@prefix ex:   <http://example.org/enterprise#> .
@prefix rdf:  <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix xsd:  <http://www.w3.org/2001/XMLSchema#> .

ex:Agent      a rdfs:Class .
ex:Person     a rdfs:Class ; rdfs:subClassOf ex:Agent .
ex:Employee   a rdfs:Class ; rdfs:subClassOf ex:Person .
ex:Manager    a rdfs:Class ; rdfs:subClassOf ex:Employee .
ex:Department a rdfs:Class .
ex:Project    a rdfs:Class .
ex:Address    a rdfs:Class .
ex:Document   a rdfs:Class .

"""

# Every 50th person manages the department they belong to; the rest report to
# that manager. Faults cycle over this many slots, so each kind lands on
# roughly people / FAULT_CYCLE persons.
PER_DEPT = 50
FAULT_CYCLE = 40


class Tally:
    """Counts the results each seeded fault will produce, by component."""

    def __init__(self):
        self.counts = collections.Counter()

    def expect(self, *components):
        for c in components:
            self.counts[c] += 1


def person(i, n_people, tally, sparql):
    """Turtle for person `i`, with their seeded fault if any."""
    dept = i // PER_DEPT
    is_manager = i % PER_DEPT == 0
    fault = None if is_manager else i % FAULT_CYCLE
    iri = f"ex:p{i}"
    t = [f"{iri} a {'ex:Manager' if is_manager else 'ex:Employee'}"]

    # --- name: exactly one string of 3..60 characters
    if fault == 1:
        tally.expect("MinCountConstraintComponent")          # no name
    elif fault == 2:
        t.append('ex:name "Al"')
        tally.expect("MinLengthConstraintComponent")
    elif fault == 25:
        t.append("ex:name 42")
        # A number is not a string, and "42" is two characters long.
        tally.expect("DatatypeConstraintComponent", "MinLengthConstraintComponent")
    elif fault == 27:
        t.append(f'ex:name "{"N" * 61}"')
        tally.expect("MaxLengthConstraintComponent")
    else:
        t.append(f'ex:name "Person {i}"')

    # --- email: at most one, matching the pattern; alternative path needs one
    if fault == 3:
        t.append(f'ex:email "person{i}@example.org", "other{i}@example.org"')
        tally.expect("MaxCountConstraintComponent")
    elif fault == 4:
        t.append('ex:email "not-an-email"')
        tally.expect("PatternConstraintComponent")
    else:
        t.append(f'ex:email "person{i}@example.org"')

    # --- age: integer in 16..100
    if fault == 5:
        t.append("ex:age 200")
        tally.expect("MaxInclusiveConstraintComponent")
    elif fault == 6:
        t.append('ex:age "forty"')
        # Not an integer, and not comparable with either bound.
        tally.expect(
            "DatatypeConstraintComponent",
            "MinInclusiveConstraintComponent",
            "MaxInclusiveConstraintComponent",
        )
    elif fault == 26:
        t.append("ex:age 10")
        tally.expect("MinInclusiveConstraintComponent")
    else:
        t.append(f"ex:age {18 + i % 50}")

    # --- salary: positive decimal (a warning when not)
    if fault == 7:
        t.append("ex:salary -1.00")
        tally.expect("MinExclusiveConstraintComponent")
    else:
        t.append(f"ex:salary {30000 + (i % 100) * 500}.00")

    # --- dates: start before end
    has_end = i % 4 == 0
    if fault == 8:
        t.append('ex:startDate "2020-06-30"^^xsd:date ; ex:endDate "2015-03-01"^^xsd:date')
        tally.expect("LessThanConstraintComponent")
        has_end = True
    elif fault == 22:
        tally.expect("MinCountConstraintComponent")          # no start date
    else:
        t.append('ex:startDate "2015-03-01"^^xsd:date')
        if has_end:
            t.append('ex:endDate "2020-06-30"^^xsd:date')

    # --- status: one of three, required twice over (PersonShape and PaidShape)
    status = ("active", "leave", "left")[i % 3]
    if fault == 10:
        t.append('ex:status "retired"')
        tally.expect("InConstraintComponent")
        status = "retired"
    elif fault == 23:
        # Two shapes each require it: PersonShape, and PaidShape through
        # sh:targetSubjectsOf ex:salary.
        tally.expect("MinCountConstraintComponent", "MinCountConstraintComponent")
        status = None
    else:
        t.append(f'ex:status "{status}"')

    # --- labels: tagged, en/fr/de, one per language (info severity)
    if fault == 11:
        t.append(f'ex:label "Person {i}"@en, "Person {i} again"@en')
        tally.expect("UniqueLangConstraintComponent")
    elif fault == 12:
        t.append(f'ex:label "Persona {i}"@es')
        tally.expect("LanguageInConstraintComponent")
    elif fault == 21:
        t.append(f'ex:label "Person {i}"')
        # Untagged: not a langString, and in no permitted language.
        tally.expect("DatatypeConstraintComponent", "LanguageInConstraintComponent")
    else:
        t.append(f'ex:label "Person {i}"@en, "Personne {i}"@fr')

    # --- department: exactly one IRI of class Department, which has a name
    if fault == 9:
        t.append(f"ex:worksIn ex:project{dept}")
        # A project is not a department, and has a title rather than a name.
        tally.expect("ClassConstraintComponent", "MinCountConstraintComponent")
    elif fault == 20:
        t.append('ex:worksIn "D001"')
        # A literal: wrong node kind, not an instance, and nothing to follow.
        tally.expect(
            "NodeKindConstraintComponent",
            "ClassConstraintComponent",
            "MinCountConstraintComponent",
        )
    elif fault == 24:
        t.append(f"ex:worksIn ex:dept{dept}, ex:dept{(dept + 1) % max(1, n_people // PER_DEPT)}")
        tally.expect("MaxCountConstraintComponent")
    else:
        t.append(f"ex:worksIn ex:dept{dept}")

    # --- identity: exactly one of staffId / contractorId
    if fault == 13:
        t.append(f'ex:staffId "E-{i:06d}" ; ex:contractorId "C-{i:06d}"')
        tally.expect("XoneConstraintComponent")
    elif fault == 14:
        tally.expect("XoneConstraintComponent")
    elif i % 7 == 3:
        t.append(f'ex:contractorId "C-{i:06d}"')
    else:
        t.append(f'ex:staffId "E-{i:06d}"')

    # --- reporting line: managers report to the chief, others to their manager
    if is_manager:
        t.append("ex:reportsTo ex:ceo")
        t.append(f"ex:manages ex:dept{dept}")
    elif fault == 19:
        # A neighbour who is not a manager: everyone reached by reportsTo+
        # must be one.
        peer = i + 1 if (i + 1) % PER_DEPT != 0 and i + 1 < n_people else i - 1
        t.append(f"ex:reportsTo ex:p{peer}")
        tally.expect("ClassConstraintComponent")
    else:
        t.append(f"ex:reportsTo ex:p{dept * PER_DEPT}")

    # --- address: a closed blank node
    addr = ['a ex:Address', f'ex:street "{i} High Street"', 'ex:city "Bristol"']
    if fault == 15:
        addr += ['ex:postcode "BS0001"', 'ex:country "GB"', 'ex:phone "0117"']
        tally.expect("ClosedConstraintComponent")
    elif fault == 16:
        addr += ['ex:country "GB"']
        tally.expect("MinCountConstraintComponent")          # no postcode
    elif fault == 17:
        addr += ['ex:postcode "abc"', 'ex:country "GB"']
        tally.expect("PatternConstraintComponent")
    elif fault == 18:
        addr += ['ex:postcode "BS0001"', 'ex:country "FR"']
        tally.expect("HasValueConstraintComponent")
    else:
        addr += ['ex:postcode "BS0001"', 'ex:country "GB"']
    t.append("ex:address [ " + " ; ".join(addr) + " ]")

    # --- the SPARQL constraint in shapes-advanced.ttl: someone who has left
    # has an end date.
    if status == "left" and not has_end:
        sparql.expect("SPARQLConstraintComponent")

    return " ;\n    ".join(t) + " .\n"


def department(d, n_depts, tally):
    iri = f"ex:dept{d}"
    fault = d % 7 if d > 0 else None
    t = [f"{iri} a ex:Department"]
    if fault == 4:
        t.append(f'ex:name "Department {d}", "Dept {d}"')
        tally.expect("MaxCountConstraintComponent")
    else:
        t.append(f'ex:name "Department {d}"')
    if fault == 1:
        t.append('ex:code "X1"')
        tally.expect("PatternConstraintComponent")
    else:
        # Three digits, whatever the size: codes need not be unique, and a
        # fourth digit would trip the pattern for every department past 999
        # — which the 200,000-person run found, as it was meant to.
        t.append(f'ex:code "D{d % 1000:03d}"')
    if fault == 3:
        t.append('ex:budget "lots"')
        tally.expect("DatatypeConstraintComponent", "MinInclusiveConstraintComponent")
    else:
        t.append(f"ex:budget {1000000 + d * 1000}.00")
    out = " ;\n    ".join(t) + " .\n"
    if fault == 2:
        # A second manager for this department: the inverse path allows one.
        m = f"ex:extra{d}"
        out += (
            f'{m} a ex:Manager ;\n    ex:name "Extra Manager {d}" ;\n'
            f'    ex:email "extra{d}@example.org" ;\n    ex:age 45 ;\n'
            f'    ex:salary 90000.00 ;\n    ex:startDate "2010-01-01"^^xsd:date ;\n'
            f'    ex:status "active" ;\n    ex:label "Extra {d}"@en ;\n'
            f'    ex:worksIn {iri} ;\n    ex:staffId "E-{900000 + d:06d}" ;\n'
            f'    ex:reportsTo ex:ceo ;\n    ex:manages {iri} ;\n'
            f'    ex:address [ a ex:Address ; ex:street "1 Side St" ; ex:city "Bristol" ; '
            f'ex:postcode "BS0002" ; ex:country "GB" ] .\n'
        )
        tally.expect("MaxCountConstraintComponent")
    return out


def project(p, n_depts, tally):
    iri = f"ex:project{p}"
    fault = p % 5
    lead = f"ex:p{p * PER_DEPT}"
    t = [f"{iri} a ex:Project"]
    if fault == 1:
        t.append(f'ex:title "Project {p}"@en, "Projekt {p}"@de')
        tally.expect("LanguageInConstraintComponent")
    else:
        t.append(f'ex:title "Project {p}"@en, "Projet {p}"@fr')
    if fault == 2:
        t.append(f"ex:lead ex:p{p * PER_DEPT + 1}")
        tally.expect("NodeConstraintComponent")
    else:
        t.append(f"ex:lead {lead}")
    if fault == 3:
        t.append(f"ex:member {lead}")
        tally.expect("MinCountConstraintComponent")
    else:
        t.append(f"ex:member {lead}, ex:p{p * PER_DEPT + 2}, ex:p{p * PER_DEPT + 3}")
    if fault == 4:
        t.append("ex:externalContact ex:ext1, ex:ext2, ex:ext3")
        tally.expect("MaxCountConstraintComponent")
    else:
        t.append("ex:externalContact ex:ext1")
    if fault == 0 and p > 0:
        tally.expect("OrConstraintComponent")                # neither budget nor sponsor
    else:
        t.append("ex:budget 50000.00")
    return " ;\n    ".join(t) + " .\n"


def document(j, n_people, tally):
    iri = f"ex:doc{j}"
    fault = j % 4
    author = f"ex:p{(j * 7) % n_people}"
    approver = f"ex:p{((j * 7) % n_people) // PER_DEPT * PER_DEPT}"
    t = [f"{iri} a ex:Document"]
    if fault == 1:
        tally.expect("MinCountConstraintComponent")          # no title
    else:
        t.append(f'ex:title "Document {j}"@en')
    if fault == 3:
        t.append("ex:author ex:dept0")
        # Not a person — though a department does have a name, so the
        # sequence path is satisfied.
        tally.expect("ClassConstraintComponent")
    else:
        t.append(f"ex:author {author}")
    if fault == 2:
        t.append("ex:version 0")
        tally.expect("MinInclusiveConstraintComponent")
    else:
        t.append("ex:version 3")
    if fault == 0 and j > 0:
        t.append(f"ex:approvedBy ex:p{((j * 7) % n_people) // PER_DEPT * PER_DEPT + 1}")
        tally.expect("NodeConstraintComponent")
    else:
        t.append(f"ex:approvedBy {approver}")
    return " ;\n    ".join(t) + " .\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--people", type=int, default=2000)
    ap.add_argument("--out", type=pathlib.Path, default=pathlib.Path(__file__).parent)
    args = ap.parse_args()
    n = args.people
    n_depts = max(1, n // PER_DEPT)
    core, sparql = Tally(), Tally()

    parts = [PREFIX]
    # The chief executive: a manager of the head office, reporting to nobody.
    parts.append(
        'ex:hq a ex:Department ; ex:name "Head Office" ; ex:code "D999" ; ex:budget 9000000.00 .\n'
        'ex:ceo a ex:Manager ; ex:name "Chief Executive" ; ex:email "ceo@example.org" ;\n'
        '    ex:age 55 ; ex:salary 250000.00 ; ex:startDate "2000-01-01"^^xsd:date ;\n'
        '    ex:status "active" ; ex:label "CEO"@en ; ex:worksIn ex:hq ; ex:staffId "E-000000" ;\n'
        '    ex:manages ex:hq ;\n'
        '    ex:address [ a ex:Address ; ex:street "1 Board Room" ; ex:city "Bristol" ; '
        'ex:postcode "BS0000" ; ex:country "GB" ] .\n'
        "ex:ext1 a ex:Agent . ex:ext2 a ex:Agent . ex:ext3 a ex:Agent .\n"
    )
    for d in range(n_depts):
        parts.append(department(d, n_depts, core))
    for i in range(n):
        parts.append(person(i, n, core, sparql))
    for p in range(n_depts):
        parts.append(project(p, n_depts, core))
    for j in range(n // 10):
        parts.append(document(j, n, core))

    data = args.out / f"data-{n}.ttl"
    data.write_text("".join(parts), encoding="utf-8", newline="\n")

    lines = [f"# Expected results for data-{n}.ttl. Generated; do not edit.\n"]
    lines.append("[shapes.ttl]\n")
    for c, k in sorted(core.counts.items()):
        lines.append(f"{c} {k}\n")
    lines.append(f"total {sum(core.counts.values())}\n")
    lines.append("[shapes-advanced.ttl]\n")
    for c, k in sorted((core.counts + sparql.counts).items()):
        lines.append(f"{c} {k}\n")
    lines.append(f"total {sum(core.counts.values()) + sum(sparql.counts.values())}\n")
    expected = args.out / f"expected-{n}.txt"
    expected.write_text("".join(lines), encoding="utf-8", newline="\n")
    print(f"wrote {data} ({data.stat().st_size / 1e6:.1f} MB) and {expected}")
    print(f"  {sum(core.counts.values())} expected results under shapes.ttl, "
          f"{sum(sparql.counts.values())} more under shapes-advanced.ttl")


if __name__ == "__main__":
    main()
