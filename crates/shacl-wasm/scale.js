// Scale and runtime-hazard checks for the WASM build.
//
// The engine is shared with the native build, so correctness is established by
// the Rust suite. What cannot be established there is what only wasm32 has:
// a 32-bit address space with no swap, a clock supplied by the host rather
// than by std, and `getrandom` wired to the JS crypto API. Each of those has
// already been a build-breaking or panicking difference at least once.
//
// Usage: node scale.js
const { Validator, validateTurtle } = require('./pkg-node/shacl_wasm.js');

const BASE = 'http://example.org/';
let failures = 0;
function check(label, actual, expected) {
  const ok = JSON.stringify(actual) === JSON.stringify(expected);
  if (!ok) failures++;
  console.log(`  ${ok ? 'PASS' : 'FAIL'}  ${label}${ok ? '' : `  (expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)})`}`);
}

const SHAPES = `
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <http://example.org/> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .
ex:PersonShape a sh:NodeShape ;
  sh:targetClass ex:Person ;
  sh:property [ sh:path ex:name  ; sh:minCount 1 ; sh:datatype xsd:string ] ;
  sh:property [ sh:path ex:age   ; sh:maxCount 1 ; sh:datatype xsd:integer ] ;
  sh:property [ sh:path ex:email ; sh:pattern "^[^@]+@[^@]+$" ] .
`;

// One instance in ten is invalid, so the count is a real assertion rather than
// a check that nothing was found.
function people(n) {
  const out = ['@prefix ex: <http://example.org/> .', '@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .'];
  for (let i = 0; i < n; i++) {
    const bad = i % 10 === 0;
    out.push(`ex:p${i} a ex:Person ; ex:name "P${i}" ; ex:age ${bad ? `"old"` : i % 90} ; ex:email "${bad ? 'nope' : `p${i}@example.org`}" .`);
  }
  return out.join('\n');
}

const mem = () => {
  const m = require('./pkg-node/shacl_wasm_bg.wasm');
  return 0;
};

console.log('== scale: one compiled shapes graph, growing data ==');
{
  const v = Validator.fromTurtle(SHAPES, BASE);
  console.log(`  compiled ${v.shapeCount} shapes`);
  let lastRate = 0;
  for (const n of [1000, 10000, 100000]) {
    const data = people(n);
    const triples = n * 4;
    const t0 = process.hrtime.bigint();
    const r = v.validateTurtle(data, BASE);
    const ms = Number(process.hrtime.bigint() - t0) / 1e6;
    const rate = Math.round(triples / (ms / 1000));
    // Every tenth instance breaks both the datatype and the pattern.
    const expected = Math.ceil(n / 10) * 2;
    const heap = Math.round(process.memoryUsage().rss / 1048576);
    console.log(`  ${String(n).padStart(6)} instances  ${String(triples).padStart(7)} triples  ${ms.toFixed(0).padStart(6)} ms  ${String(rate).padStart(8)} triples/s  rss ${heap} MB`);
    check(`  ${n}: ${expected} findings`, r.length, expected);
    lastRate = rate;
  }
  check('throughput is not pathological (>50k triples/s)', lastRate > 50000, true);
}

console.log('\n== reuse: a compiled Validator survives many runs ==');
{
  const v = Validator.fromTurtle(SHAPES, BASE);
  const data = people(500);
  const counts = new Set();
  for (let i = 0; i < 50; i++) counts.add(v.validateTurtle(data, BASE).length);
  check('50 runs give one stable answer', [...counts], [100]);
}

console.log('\n== wasm32 hazard: the clock (SPARQL NOW()) ==');
{
  // `NOW()` reaches oxsdatatypes' clock. On wasm32 that is `std::time::SystemTime`
  // unless the `js` feature switches it to Date.now(); without it this panics
  // with "time not implemented on this platform".
  const shapes = `
    @prefix sh: <http://www.w3.org/ns/shacl#> .
    @prefix ex: <http://example.org/> .
    ex:S a sh:NodeShape ; sh:targetClass ex:T ;
      sh:sparql [ sh:message "now" ; sh:select """
        SELECT $this ?value WHERE { $this <http://example.org/p> ?value . FILTER(NOW() > "2000-01-01T00:00:00Z"^^<http://www.w3.org/2001/XMLSchema#dateTime>) }
      """ ] .
  `;
  const data = '@prefix ex: <http://example.org/> . ex:x a ex:T ; ex:p 1 .';
  try {
    const r = validateTurtle(shapes, data, BASE);
    check('NOW() evaluates instead of panicking', r.length, 1);
  } catch (e) {
    failures++;
    console.log(`  FAIL  NOW() threw: ${e}`);
  }
}

console.log('\n== wasm32 hazard: blank nodes (getrandom via oxrdf) ==');
{
  const shapes = `
    @prefix sh: <http://www.w3.org/ns/shacl#> .
    @prefix ex: <http://example.org/> .
    ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:p ;
      sh:property [ sh:path ex:p ; sh:datatype <http://www.w3.org/2001/XMLSchema#integer> ] .
  `;
  const data = '@prefix ex: <http://example.org/> . [] ex:p "not an int" . [] ex:p "also not" .';
  const r = validateTurtle(shapes, data, BASE);
  check('blank node focus nodes validate', r.length, 2);
  check('and are reported as blank nodes', r.results.every((x) => x.focusNode.startsWith('_:')), true);
}

console.log('\n== the two blank-node bugs, checked against this built artefact ==');
{
  // 0.1.3 cross-joined blank node focus nodes under sh:sparql (N -> N^2).
  const crossJoin = `
    @prefix sh: <http://www.w3.org/ns/shacl#> .
    @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
    @prefix ex: <http://example.org/> .
    ex:S a sh:NodeShape ; sh:targetSubjectsOf rdfs:label ;
      sh:sparql [ sh:message "m" ; sh:select """SELECT $this ?value WHERE { $this <http://example.org/p> ?value }""" ] .
  `;
  const blanks = (n) => '@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n@prefix ex: <http://example.org/> .\n' +
    Array.from({ length: n }, (_, i) => `[] rdfs:label "b${i}" ; ex:p ${i} .`).join('\n');
  const v = Validator.fromTurtle(crossJoin, BASE);
  for (const n of [2, 3, 4, 60]) check(`N=${n} gives ${n}, not ${n * n}`, v.validateTurtle(blanks(n), BASE).length, n);

  // 0.1.4 made isIRI($this) true for a blank node, so shapes that exclude
  // anonymous class expressions stopped excluding them.
  const filtered = (f) => `
    @prefix sh: <http://www.w3.org/ns/shacl#> .
    @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
    @prefix ex: <http://example.org/> .
    ex:S a sh:NodeShape ; sh:targetSubjectsOf rdfs:label ;
      sh:sparql [ sh:message "m" ; sh:select """SELECT $this ?value WHERE { $this <http://example.org/p> ?value . FILTER(${f}) }""" ] .
  `;
  const mixed = blanks(3) + '\n' + Array.from({ length: 2 }, (_, i) => `ex:n${i} rdfs:label "n${i}" ; ex:p ${i} .`).join('\n');
  check('isIRI($this) keeps only the 2 named', validateTurtle(filtered('isIRI($this)'), mixed, BASE).length, 2);
  check('isBlank($this) keeps only the 3 blank', validateTurtle(filtered('isBlank($this)'), mixed, BASE).length, 3);
}

console.log(`\n${failures === 0 ? 'ALL CHECKS PASSED' : failures + ' CHECK(S) FAILED'}`);
process.exit(failures === 0 ? 0 : 1);
