# Provenance

This is **oxsdatatypes 0.2.2** from the [Oxigraph](https://github.com/oxigraph/oxigraph)
project, as published on [crates.io](https://crates.io/crates/oxsdatatypes/0.2.2), with
the one change below. The workspace `Cargo.toml` puts it in place of the registry crate with
`[patch.crates-io]`, so `spareval` and every crate here resolve this copy.

| | |
|---|---|
| Upstream | https://github.com/oxigraph/oxigraph/tree/main/lib/oxsdatatypes |
| Taken from | the crates.io package `oxsdatatypes-0.2.2`, unpacked |
| Licence | MIT OR Apache-2.0, the same terms as this workspace |
| Copyright | the Oxigraph developers |

The package on crates.io carries no licence files, so `LICENSE-MIT` and `LICENSE-APACHE`
here were taken from the root of the Oxigraph repository. `Cargo.toml` is the normalised
manifest cargo publishes; `Cargo.toml.orig` is the one it was normalised from and is not
read by cargo.

The manifest has one change: its `[lints.clippy]` table is removed. That table is
Oxigraph's workspace lint policy, 185 lints set to warn, written for the clippy of its day.
Under a newer clippy with `-D warnings` it fails on upstream's own tests (`unwrap` in a test
that returns `Result`) and names lints that have since been renamed or removed. Without it
the crate gets clippy's defaults, as the code around it does. `[lints.rust]` is kept.

## The change

`src/decimal.rs`: `Decimal::checked_mul` and `Decimal::checked_div`.

`Decimal` is a fixed-point number, an `i128` scaled by 10¹⁸. Upstream's `checked_mul`
returned `None` for any product it could not hold exactly. The product of two decimals
with 18 fractional digits has up to 36, so it failed even when the result was in range:

```sparql
SELECT ((1.3 * 31.583333333333333333) AS ?x) WHERE {}   # ?x unbound
SELECT ((2   * 31.583333333333333333) AS ?x) WHERE {}   # 63.166666666666666666
```

`checked_div` failed in the same way once the dividend was large enough that it could not
be shifted 18 places: `1000 / 31.583333333333333333` was an error.

Both now compute the exact result in 256 bits and round it once to 18 fractional digits,
half to even. They return `None` only for division by zero (FOAR0001) or a result outside
the range of `Decimal` (FOAR0002). [XPath F&O 3.1 §4.2](https://www.w3.org/TR/xpath-functions-31/#op.numeric)
allows this: an `xs:decimal` result with more digits than the implementation keeps "is
truncated or rounded in an implementation-defined manner", and FOAR0002 is for overflow.

Division used to truncate where it did not fail, so an inexact quotient can now differ in
its 18th fractional digit: `2 / 3` was `0.666666666666666666` and is now
`0.666666666666666667`. One rounding rule for both operations, and the one a reader
expects.

The tests `mul_rounds` and `div_rounds` in `src/decimal.rs` cover rounding, ties, signs,
products and quotients that do not fit in `i128` before scaling, and overflow. Every
upstream test passes unchanged. The arithmetic was also checked against Python's `decimal`
module on 200,000 random pairs of scaled values, from 0 to the extremes of `i128` and of
both signs, with no differences.

## Updating

Dropping this copy is right once an upstream release rounds instead of failing. Before
that, moving to a later 0.2.x means taking its `src/` and reapplying the change above,
which is self-contained: the two methods and the four helpers after `impl Decimal` in
`src/decimal.rs`, and the `[lints.clippy]` table removed from `Cargo.toml` again.
