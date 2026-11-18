# Provenance

This is **spareval 0.2.9** from the [Oxigraph](https://github.com/oxigraph/oxigraph) project,
as published on [crates.io](https://crates.io/crates/spareval/0.2.9), with the one change below.
The workspace `Cargo.toml` puts it in place of the registry crate with `[patch.crates-io]`, so
every crate here resolves this copy. It is the same `src/` as HOLOS's `vendor/spareval`
(pwin/triplestore 0.26.4), so the two engines evaluate SPARQL alike.

| | |
|---|---|
| Upstream | https://github.com/oxigraph/oxigraph/tree/main/lib/spareval |
| Taken from | the crates.io package `spareval-0.2.9`, unpacked |
| Licence | MIT OR Apache-2.0, the same terms as this workspace |
| Copyright | the Oxigraph developers |

The package on crates.io carries no licence files, so `LICENSE-MIT` and `LICENSE-APACHE` here
were taken from the root of the Oxigraph repository. `Cargo.toml` is the normalised manifest
cargo publishes, unchanged; `Cargo.toml.orig` is the one it was normalised from and is not read
by cargo.

The workspace lists this directory under `exclude`, so it is built as a dependency only and is
not a workspace member. That keeps clippy and rustfmt, which CI runs over every member with
`-D warnings`, off upstream code: under clippy's defaults Oxigraph's code draws
`type_complexity` and `too_many_arguments`, which are its design choices, not defects. The
package's own 32 tests (14 unit tests and 18 doctests in `src/`) therefore do not run here; they
run in HOLOS's workspace, where the same `src/` is a member, and pass with the change.

## The change

`src/expression.rs`: the evaluator for `Expression::Add`, and one function,
`temporal_operand_first`, placed before `enum NumericBinaryOperands`.

With the `sep-0002` feature, upstream adds a duration to an `xsd:date`, `xsd:time` or
`xsd:dateTime` only when the temporal value comes first: `NumericBinaryOperands::new` has a case
for (dateTime, duration) and none for (duration, dateTime), so the second order is an error and
leaves its variable unbound. XPath's operator mapping defines `A + B` for both orders and computes
both as the temporal value plus the duration (`op:add-yearMonthDuration-to-dateTime(B, A)` when
`A` is the duration, and so on).

That gap is wider than a query written with the duration first. `sparopt`'s optimizer, which
`spareval` runs before evaluating, rebuilds every `+` through `order_pair`
(`sparopt/src/algebra.rs`), and `order_pair` orders the two operands by their `DefaultHasher`
hash, as if `+` commuted for every type. So `?d + "P2Y"^^xsd:yearMonthDuration`, written the
supported way round, reached the evaluator reversed whenever the duration hashed lower, and its
result was unbound. A hash of the same value differs between 64-bit and 32-bit builds: of thirty
pairs of six dateTimes and five durations, HOLOS 0.26.3, which runs this evaluator, bound 7
natively and 24 in WebAssembly. A SPARQL constraint, target or rule here computes with the same
evaluator, so it lost the same sums.

The change puts the temporal operand first before `NumericBinaryOperands::new` is consulted, for
`+` only. Both orders, and so both outcomes of the hash, now give the sum. Subtraction does not
commute and is untouched: `duration - dateTime` is still an error. Without `sep-0002` the
function returns its arguments as they came.

## Tests

`crates/shacl/tests/date_arithmetic.rs` computes the thirty sums inside a SPARQL constraint, in
both orders, and compares each with `oxsdatatypes` computing it directly; and checks that a
constraint built on `dateTime + duration` fires for every value. HOLOS's
`crates/holos-engine/tests/date_arithmetic.rs` covers the same through its query engine. The
package's own tests run in HOLOS, as above.

## Updating

Dropping this copy is right once an upstream release accepts `+` with the duration on either side,
or once `sparopt` stops reordering `+` across types and HOLOS can leave the written order to
users. Before that, moving to a later 0.2.x means taking its package and reapplying the change,
which is self-contained in `src/expression.rs`: the `Expression::Add` evaluator's first line and
`temporal_operand_first`.

`spareval` must stay at the version the other Oxigraph crates in the lock expect, and pins
`oxsdatatypes` to an exact version, which the copy in `vendor/oxsdatatypes` has to match.
