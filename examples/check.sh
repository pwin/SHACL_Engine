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

# The cap counts only results that break conformance. Stopping on a warning
# while a violation sat unreached elsewhere reported `conforms: true` on a
# graph that does not conform -- the worst kind of wrong answer.
cat > /tmp/shacl-sev.$$.ttl <<'TTL'
@prefix ex: <http://example.org/ns#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
ex:WarnShape a sh:NodeShape ; sh:targetNode ex:a ;
  sh:property [ sh:path ex:w ; sh:minCount 1 ; sh:severity sh:Warning ] .
ex:ViolShape a sh:NodeShape ; sh:targetNode ex:b ;
  sh:property [ sh:path ex:v ; sh:minCount 1 ; sh:severity sh:Violation ] .
TTL
cat > /tmp/shacl-sevd.$$.ttl <<'TTL'
@prefix ex: <http://example.org/ns#> .
ex:a ex:x 1 .
ex:b ex:y 2 .
TTL
out=$("$SHACL" -d /tmp/shacl-sevd.$$.ttl -s /tmp/shacl-sev.$$.ttl --abort --quiet); rc=$?
check "--abort is severity-aware"      "conforms: false" "$out"
check "--abort keeps the exit status"  "1"               "$rc"
rm -f /tmp/shacl-sev.$$.ttl /tmp/shacl-sevd.$$.ttl

# pySHACL spells the format overrides with one dash and two letters.
out=$("$SHACL" -d person-valid.ttl -df ttl -s person-shapes.ttl -sf ttl --quiet)
check "pySHACL -df/-sf spelling"       "conforms: true"  "$out"

# --- the flags added in the CLI round-out
out=$("$SHACL" -d person-invalid.ttl -s person-shapes.ttl --abort | grep -c "ConstraintComponent")
check "--abort stops at one result"   "1"  "$out"
out=$("$SHACL" -d person-invalid.ttl -s person-shapes.ttl --max-results 3 | grep -c "ConstraintComponent")
check "--max-results caps the report" "3"  "$out"

# -o writes the same bytes the terminal would have seen.
"$SHACL" -d person-invalid.ttl -s person-shapes.ttl -f turtle -o /tmp/shacl-o.$$.ttl
piped=$("$SHACL" -d person-invalid.ttl -s person-shapes.ttl -f turtle)
[ "$piped" = "$(cat /tmp/shacl-o.$$.ttl)" ] && r=same || r=different
rm -f /tmp/shacl-o.$$.ttl
check "-o matches stdout"             "same"            "$r"

# Two documents that only validate once merged.
out=$("$SHACL" -d person-valid.ttl person-shapes.ttl --quiet)
check "repeated -d merges documents"  "conforms: true"  "$out"

# Reading the data graph from a pipe.
out=$(cat person-valid.ttl | "$SHACL" -d - --data-format ttl -s person-shapes.ttl --quiet)
check "reads the data graph from -"   "conforms: true"  "$out"

# The shapes here are well formed, so SHACL-SHACL should not object.
out=$("$SHACL" -d person-valid.ttl -s person-shapes.ttl -m --quiet)
check "--meta-shacl accepts them"     "conforms: true"  "$out"

# pySHACL spellings people will already have in their fingers.
out=$("$SHACL" -d person-valid.ttl --df ttl -s person-shapes.ttl --sf ttl -w --quiet)
check "pySHACL flag aliases"          "conforms: true"  "$out"

echo
if [ "$fails" -eq 0 ]; then
  echo "all checks passed"
else
  echo "$fails check(s) failed"
fi
exit $((fails > 0))
