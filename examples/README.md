# Examples

Worked examples for checking the CLI does what it claims — against a build you
made, or a binary you downloaded and would rather not take on trust.

| File | What it is |
| --- | --- |
| `person-shapes.ttl` | The shapes. One constraint of each common family |
| `person-valid.ttl` | Data that conforms |
| `person-invalid.ttl` | Data where each subject breaks exactly one constraint |
| `self-contained.ttl` | Shapes and data in one document |
| `check.sh` | Runs all of the below and checks the answers |

## The quick version

```sh
examples/check.sh                        # against target/release/shacl
examples/check.sh ~/Downloads/shacl      # against a downloaded release binary
```

Eighteen checks, exit status 0 only if every one holds.

## By hand

**Conforming data.** Exits 0.

```sh
shacl -d examples/person-valid.ttl -s examples/person-shapes.ttl
```
```
conforms: true
```

**Non-conforming data.** Exits 1 — a graph that does not conform is a normal
outcome, not an error, so it is distinguishable from a genuine failure, which
exits 2.

```sh
shacl -d examples/person-invalid.ttl -s examples/person-shapes.ttl
```

Ten results, one per subject. Every subject in `person-invalid.ttl` carries an
`rdfs:comment` naming the component it is supposed to trip, so the report can be
checked line by line:

| Subject | Component |
| --- | --- |
| `ex:noName` | `MinCountConstraintComponent` |
| `ex:shortName` | `MinLengthConstraintComponent` |
| `ex:twoNames` | `MaxCountConstraintComponent` |
| `ex:numericName` | `DatatypeConstraintComponent` |
| `ex:impossibleAge` | `MaxInclusiveConstraintComponent` |
| `ex:badEmail` | `PatternConstraintComponent` |
| `ex:noEmail` | `MinCountConstraintComponent` |
| `ex:badStatus` | `InConstraintComponent` |
| `ex:knowsANonAgent` | `ClassConstraintComponent` |
| `ex:employeeWithoutDepartment` | `MinCountConstraintComponent` |

**Shapes in the data graph.** With no `--shapes`, the data graph supplies them.

```sh
shacl -d examples/self-contained.ttl
```

**The RDF report.** `human` is a summary for reading; the report SHACL actually
defines is a graph.

```sh
shacl -d examples/person-invalid.ttl -s examples/person-shapes.ttl -f turtle
```

It is a real graph, so it round-trips — which `check.sh` verifies by feeding the
report back in as a data graph:

```sh
shacl -d examples/person-invalid.ttl -s examples/person-shapes.ttl -f turtle > report.ttl
shacl -d report.ttl          # conforms: true — it parsed
```

Also available: `nt`, `rdfxml`, `jsonld`.

## Things worth noticing

**The class hierarchy is in the data files, not only the shapes.** SHACL
resolves class membership in the *data* graph. Delete the `rdfs:subClassOf`
lines from `person-valid.ttl` and watch `sh:class ex:Agent` start failing for
everyone, and `ex:Employee` stop being recognised as an `ex:Person` at all. It
is the single most common surprise in SHACL.

**One fault at a time.** Each subject in `person-invalid.ttl` is otherwise
valid, which is what makes the count exactly ten. Give `ex:noName` an
`ex:age "old"` and you get three more results, not one: the datatype fails, and
then both range constraints fail because a string cannot be compared with a
number.

**Try breaking something.** Change a `sh:minCount` in the shapes, or an email
address in the data, and re-run. The report should move in exactly the way you
expect — and if it does not, that is worth reporting.
