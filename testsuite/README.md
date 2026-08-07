# Vendored W3C test suites

These directories are copied verbatim from the W3C [`w3c/data-shapes`][repo]
repository and are **not** part of this project's own source.

| Directory | Upstream path | Covers |
| --- | --- | --- |
| `shacl10/` | `data-shapes-test-suite/` | SHACL 1.0 (2017 Recommendation) |
| `shacl12/` | `shacl12-test-suite/` | SHACL 1.2 (core, node expressions, SPARQL, rules) |

They are vendored rather than pulled in as a submodule so that a clone builds
and tests without network access, and so a suite update lands as a reviewable
commit rather than silently changing what conformance means.

Only the `tests/` trees and their READMEs are kept; upstream's HTML report
scaffolding is omitted.

## Refreshing

```sh
git clone --depth 1 https://github.com/w3c/data-shapes.git /tmp/ds
rm -rf testsuite/shacl10 testsuite/shacl12
cp -r /tmp/ds/data-shapes-test-suite testsuite/shacl10
cp -r /tmp/ds/shacl12-test-suite     testsuite/shacl12
rm -rf testsuite/shacl10/javascripts testsuite/shacl10/stylesheets testsuite/shacl10/reports
```

Then re-run `cargo test -p shacl --test w3c -- --nocapture` and update the
baseline in `crates/shacl/tests/w3c.rs` if the suite's size changed.

## Licence

The test suites are published by the W3C under the [W3C Software and Document
Licence][licence]. Copyright remains with the W3C and the document authors.

[repo]: https://github.com/w3c/data-shapes
[licence]: https://www.w3.org/copyright/software-license/
