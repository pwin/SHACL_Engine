#!/usr/bin/env bash
# Builds the release artefacts into dist/.
#
#   scripts/release.sh
#
# Produces the CLI for the host platform and a Python wheel. Cross-compiling is
# deliberately not attempted here: wheels for other platforms belong in CI,
# where each one can be built and tested natively.
set -euo pipefail

cd "$(dirname "$0")/.."
DIST="dist"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

# The host triple decides the artefact names, so a download is unambiguous
# about what it will run on.
TARGET="$(rustc -vV | sed -n 's/^host: //p')"
EXE=""
case "$TARGET" in
  *windows*) EXE=".exe" ;;
esac

echo "shacl $VERSION for $TARGET"
rm -rf "$DIST"
mkdir -p "$DIST"

echo
echo "==> tests"
cargo test --workspace --quiet

echo
echo "==> cli"
cargo build --release -p shacl-cli
cp "target/release/shacl$EXE" "$DIST/shacl-$VERSION-$TARGET$EXE"

echo
echo "==> wheel"
# Set MATURIN to point at the executable if it is not on PATH — under Git Bash
# a Windows-style PATH entry such as `C:/venv/Scripts` is not searched, so a
# maturin installed into a virtualenv will not be found by name.
MATURIN="${MATURIN:-maturin}"
if "$MATURIN" --version >/dev/null 2>&1; then
  (cd crates/shacl-python && "$MATURIN" build --release --out "../../$DIST")
else
  echo "  maturin not found; skipping the wheel."
  echo "  pip install maturin, then re-run — or set MATURIN=/path/to/maturin."
fi

echo
echo "==> checksums"
(cd "$DIST" && sha256sum ./* > SHA256SUMS && cat SHA256SUMS)

echo
echo "==> smoke test"
"$DIST/shacl-$VERSION-$TARGET$EXE" --version

echo
ls -la "$DIST"
