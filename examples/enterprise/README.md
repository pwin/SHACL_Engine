# The enterprise example

A large, complex test: one shapes graph that uses most of SHACL Core in
combination, and a data generator in which every fault is seeded on purpose
and counted — so a run is checked result for result, not "some violations".

| File | What it is |
| --- | --- |
| `shapes.ttl` | The shapes. Nested shapes, every kind of compound path, the logical constraints, qualified value shapes, closedness, property pairs, language handling, three severities, four kinds of target |
| `shapes-advanced.ttl` | Adds a SPARQL constraint with a substituted message, and a SHACL-AF rule. Load it alongside `shapes.ttl` |
| `generate.py` | Writes `data-N.ttl` and `expected-N.txt` for any N |
| `data-2000.ttl` | 2,000 people, 38,002 triples, checked in |
| `expected-2000.txt` | 1,978 results it must produce, per component; 470 more with the advanced shapes |

`crates/shacl/tests/enterprise.rs` runs it on every `cargo test`, and at any
size you give it.

## The quick version

```sh
shacl -d examples/enterprise/data-2000.ttl -s examples/enterprise/shapes.ttl
```

1,978 results, of twenty constraint components, over an organisation of
people in departments reporting to managers, working on projects, writing
documents. Add the advanced shapes and every leaver without an end date is
named:

```sh
shacl -d examples/enterprise/data-2000.ttl \
      -s examples/enterprise/shapes.ttl examples/enterprise/shapes-advanced.ttl -f turtle \
  | grep "has left"
```
```
sh:resultMessage "http://example.org/enterprise#p866 has left but has no end date"@en
```

## What the generator seeds

Nothing is random. Person `i` gets fault `i mod 40` if any, so each fault
kind lands on one person in forty and the counts are a function of the size
alone. Every fault is commented with the results it produces — including the
ones that produce more than one, which is SHACL's answer rather than a quirk:

| Seeded | Results |
| --- | --- |
| no name | `MinCount` |
| name `"Al"` | `MinLength` |
| name `42` | `Datatype`, and `MinLength` — "42" is two characters |
| age `"forty"` | `Datatype`, `MinInclusive`, `MaxInclusive` — a string compares with neither bound |
| `worksIn` a project | `Class`, and `MinCount` on the sequence path `worksIn/name` — a project has a title, not a name |
| `worksIn "D001"` | `NodeKind`, `Class`, and `MinCount` on the sequence path |
| untagged label | `Datatype` (not `rdf:langString`), `LanguageIn` (no language) |
| no status | `MinCount` twice — `PersonShape` and `PaidShape` both require it |
| reports to a non-manager | `Class` on the `reportsTo+` path |
| address with an extra property | `Closed`, on the address, which is targeted as itself |
| second manager for a department | `MaxCount` on the inverse path `^manages` |
| project lead who is not a manager | `Node` — a nested shape, two levels deep |

and a dozen more of one result each. `expected-2000.txt` is the sum.

## At scale

```sh
python examples/enterprise/generate.py --people 200000 --out /tmp/ent
SHACL_ENTERPRISE_DATA=/tmp/ent/data-200000.ttl \
  cargo test --release -p shacl --test enterprise -- --nocapture
```

3.8 million triples, 197,855 results, every count exact. On four cores that
loads in 3.6s and validates in 2.3s. The test then holds the report to the
same bytes on one thread and on all of them, and to the same bytes through
the index cache; loads the advanced shapes and checks the run declines the
parallel path, substitutes `{$this}` and keeps the message's language tag;
and applies the rule and counts exactly one `ex:seniority` per person with an
integer age of fifty or more.

The first run at this size found a bug — in the generator. Department codes
were `D` and three digits, and with 4,000 departments the fourth digit
tripped `^D[0-9]{3}$` on every department past 999: 3,000, less the 429
already seeded with a bad code, 2,571 unexpected `Pattern` results. Which is
the point of holding the count exact. A test that asked for "roughly ten
thousand pattern violations" would have been satisfied.
