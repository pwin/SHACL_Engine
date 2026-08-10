#!/usr/bin/env bash
# Runs the examples and checks the CLI behaves as documented.
#
#   examples/check.sh [path-to-shacl]
#
# Defaults to target/release/shacl, so it works against a source build; pass a
# path to check a downloaded release binary instead.
#
# Exit status is 0 only if every expectation holds, so this is usable as a
# smoke test for a build you did not make yourself.
set -uo pipefail
cd "$(dirname "$0")"

SHACL="${1:-../target/release/shacl}"
[ -x "$SHACL" ] || SHACL="$SHACL.exe"
if ! "$SHACL" --version >/dev/null 2>&1; then
  echo "cannot run $SHACL — build it with: cargo build --release -p shacl-cli" >&2
  exit 2
fi
echo "checking $("$SHACL" --version)"

fails=0
check() { # check <description> <expected> <actual>
  if [ "$2" = "$3" ]; then
    printf '  ok    %s\n' "$1"
  else
    printf '  FAIL  %s\n        expected %s, got %s\n' "$1" "$2" "$3"
    fails=$((fails + 1))
  fi
}

# --- conforming data
out=$("$SHACL" -d person-valid.ttl -s person-shapes.ttl --quiet); rc=$?
check "valid data conforms"        "conforms: true" "$out"
check "valid data exits 0"         "0"              "$rc"

# --- non-conforming data
out=$("$SHACL" -d person-invalid.ttl -s person-shapes.ttl --quiet); rc=$?
check "invalid data does not conform" "conforms: false" "$out"
# A non-conforming graph is a normal outcome, not an error: 1 rather than 2.
check "invalid data exits 1"          "1"               "$rc"

n=$("$SHACL" -d person-invalid.ttl -s person-shapes.ttl | grep -c 'ConstraintComponent')
check "one violation per subject"     "10"              "$n"

# Every subject in person-invalid.ttl names the component it should trip.
for c in MinCount MaxCount MinLength Datatype MaxInclusive Pattern In Class; do
  got=$("$SHACL" -d person-invalid.ttl -s person-shapes.ttl | grep -c "${c}ConstraintComponent" || true)
  [ "$got" -gt 0 ] && r=present || r=missing
  check "reports ${c}ConstraintComponent" "present" "$r"
done

# --- shapes read from the data graph itself
out=$("$SHACL" -d self-contained.ttl --quiet)
check "self-contained document"       "conforms: false" "$out"

# --- the RDF report
rdf=$("$SHACL" -d person-invalid.ttl -s person-shapes.ttl -f turtle)
for want in "sh:ValidationReport" "sh:conforms false" "sh:resultSeverity"; do
  case "$rdf" in *"$want"*) r=present ;; *) r=missing ;; esac
  check "turtle report contains $want" "present" "$r"
done

# The report must be a graph another tool can read, so feed it back in.
echo "$rdf" > /tmp/shacl-report.$$.ttl
out=$("$SHACL" -d /tmp/shacl-report.$$.ttl --quiet); rc=$?
rm -f /tmp/shacl-report.$$.ttl
check "report parses as RDF"          "conforms: true"  "$out"

echo
if [ "$fails" -eq 0 ]; then
  echo "all checks passed"
else
  echo "$fails check(s) failed"
fi
exit $((fails > 0))
