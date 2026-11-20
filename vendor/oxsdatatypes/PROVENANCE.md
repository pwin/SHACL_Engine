# Provenance

This is **oxsdatatypes 0.2.4** from the [Oxigraph](https://github.com/oxigraph/oxigraph)
project, as published on [crates.io](https://crates.io/crates/oxsdatatypes/0.2.4), with
the one change below. The workspace `Cargo.toml` puts it in place of the registry crate with
`[patch.crates-io]`, so `spareval` and every crate here resolve this copy.

| | |
|---|---|
| Upstream | https://github.com/oxigraph/oxigraph/tree/main/lib/oxsdatatypes |
| Taken from | the crates.io package `oxsdatatypes-0.2.4`, unpacked |
| Licence | MIT OR Apache-2.0, the same terms as this workspace |
| Copyright | the Oxigraph developers |

The package on crates.io carries no licence files, so `LICENSE-MIT` and `LICENSE-APACHE`
here were taken from the root of the Oxigraph repository. `Cargo.toml` is the normalised
manifest cargo publishes; `Cargo.toml.orig` is the one it was normalised from and is not
read by cargo.

The manifest has three changes. Its `[lints.clippy]` table is removed. That table is
Oxigraph's workspace lint policy, about 200 lints set to warn, written for the clippy of its
day. Under a newer clippy with `-D warnings` it fails on upstream's own tests (`unwrap` in a
test that returns `Result`) and names lints that have since been renamed or removed. Without
it the crate gets clippy's defaults, as the code around it does. `[lints.rust]` is kept. And
its `resolver = "2"` line is removed: cargo ignores a resolver set by a package that is not
the workspace root, and says so on every command, because this workspace uses resolver 3.

And the `custom-now` feature is removed from `[features]`. It exists upstream so that a host
without a clock -- zkvm, no-std -- can supply the time itself: turning it on makes the crate
declare `unsafe extern "Rust" fn custom_ox_now()` and leaves the embedder to define it.
Nothing in this workspace is such a host, and nothing should define that symbol. Because this
copy is a workspace *member* rather than only a patch, `cargo test --workspace --all-features`
turned the feature on and the build then failed at the linker with `unresolved external symbol
custom_ox_now` -- which reads as a broken toolchain and is a feature nobody asked for. The
`cfg` blocks that test for it are left in place, so the source still differs from upstream in
`decimal.rs` alone; `[lints.rust]` declares the now-unknown `cfg` value so each of them does
not warn.

Membership is deliberate and worth keeping: it is what runs this copy's own 82 tests here, and
those tests are the only thing guarding the `decimal.rs` change below. `vendor/spareval` is
excluded instead and has its tests run in HOLOS's workspace.

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
be shifted 18 places (`1000 / 31.583333333333333333`), and when the dividend was 0 and the
divisor was not whole (`0 / 1.3`).

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
products and quotients that do not fit in `i128` before scaling, and overflow. One upstream
assertion in `mul` is changed, because it asserted the failure:
`Decimal::STEP.checked_mul(0.1)`, which is 10⁻¹⁹, was `None` and is now 0. Every other
upstream test passes unchanged. The arithmetic was also checked against Python's `decimal`
module on 200,000 random pairs of scaled values, from 0 to the extremes of `i128` and of
both signs, with no differences.

This `src/decimal.rs` is the same file as the one in HOLOS's `vendor/oxsdatatypes`
(pwin/triplestore 0.26.2), so the two engines do the same arithmetic.

## Updating

Dropping this copy is right once an upstream release rounds instead of failing. Before
that, moving to a later 0.2.x means taking its package and reapplying the change above,
which is self-contained in `src/decimal.rs`: the two methods, the four helpers after
`impl Decimal` (`wide_mul`, `wide_div`, `round_half_even`, `with_sign`), the
`std::cmp::Ordering` import, the two tests and the one assertion in `mul`; and the two lines
of `Cargo.toml` above removed again.

`spareval` pins `oxsdatatypes` to an exact version, so this copy's version has to be the
one the locked `spareval` asks for: `spareval` 0.2.9 asks for `=0.2.4`. Otherwise cargo
ignores the patch with a warning and builds the registry crate.

This copy was 0.2.2, under `spareval` 0.2.6, until the move to 0.2.4. The move took the
Oxigraph crates `spareval` 0.2.9 needs: `spargebra` 0.4.8, `sparopt` 0.3.9 and their
family, which fix arithmetic and filter scoping that 0.2.6 answered wrongly.
