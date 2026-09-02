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
  let steady = 0;
  for (const n of [1000, 10000, 100000]) {
    const data = people(n);
    const triples = n * 4;

    // Two runs, because the first at a new size is not measuring the engine.
    // wasm32 has one linear memory that `memory.grow` may have to relocate,
    // so the run that first needs ~300 MB pays for building it -- around 13
    // of the 17 seconds below. A caller validating one document per process
    // does pay that; one reusing a Validator, which is what the API is shaped
    // for, pays it once. Both numbers are reported rather than averaged,
    // since they answer different questions.
    const once = () => {
      const t0 = process.hrtime.bigint();
      const r = v.validateTurtle(data, BASE);
      return [Number(process.hrtime.bigint() - t0) / 1e6, r];
    };
    const [coldMs] = once();
    const [warmMs, r] = once();

    const rate = Math.round(triples / (warmMs / 1000));
    // Every tenth instance breaks both the datatype and the pattern.
    const expected = Math.ceil(n / 10) * 2;
    const heap = Math.round(process.memoryUsage().rss / 1048576);
    console.log(
      `  ${String(n).padStart(6)} instances  ${String(triples).padStart(7)} triples  ` +
      `cold ${coldMs.toFixed(0).padStart(6)} ms  warm ${warmMs.toFixed(0).padStart(6)} ms  ` +
      `${String(rate).padStart(7)} triples/s (warm)  rss ${heap} MB`
    );
    check(`  ${n}: ${expected} findings`, r.length, expected);
    steady = rate;
  }
  // Warm throughput at the largest size. Native manages roughly ten times
  // this; the gap is wasm32 with no threads and no mimalloc, not an
  // algorithmic difference -- `differential.js` shows the two agree on every
  // W3C document, and `scaling.rs` pins the curve as linear on both.
  check('warm throughput is not pathological (>50k triples/s)', steady > 50000, true);
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
    const r = validateTurtle(data, shapes, BASE);
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


console.log('\n== SHACL-AF rules through the WASM API ==');
{
  const shapes = `
    @prefix sh: <http://www.w3.org/ns/shacl#> .
    @prefix ex: <http://example.org/> .
    @prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
    ex:PersonShape a sh:NodeShape ; sh:targetClass ex:Person ;
      sh:rule [ a sh:TripleRule ;
        sh:subject sh:this ; sh:predicate rdf:type ; sh:object ex:Agent ] .
    ex:AgentShape a sh:NodeShape ; sh:targetClass ex:Agent ;
      sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:message "an Agent needs a name" ] .
  `;
  const data = `
    @prefix ex: <http://example.org/> .
    ex:alice a ex:Person ; ex:name "Alice" .
    ex:bob   a ex:Person .
  `;
  const v = Validator.fromTurtle(shapes, BASE);
  check('off by default: a rule changes the report, so it is asked for',
    v.validateTurtle(data, BASE).conforms, true);
  const r = v.validateTurtle(data, BASE, 'rules');
  // pySHACL 0.40.1 on the same input: Conforms False, one result on ex:bob.
  check('inference "rules" infers before validating', r.length, 1);
  check('and the finding is on ex:bob', r.results[0].focusNode, 'http://example.org/bob');

  // A transitive rule needs more than the single pass the spec defines.
  const trans = `
    @prefix sh: <http://www.w3.org/ns/shacl#> .
    @prefix ex: <http://example.org/> .
    ex:S a sh:NodeShape ; sh:targetSubjectsOf ex:sub ;
      sh:rule [ a sh:TripleRule ; sh:subject sh:this ; sh:predicate ex:sub ;
                sh:object [ sh:path ( ex:sub ex:sub ) ] ] .
    ex:Cap a sh:NodeShape ; sh:targetNode ex:a ;
      sh:property [ sh:path ex:sub ; sh:maxCount 2 ] .
  `;
  const chain = '@prefix ex: <http://example.org/> . ex:a ex:sub ex:b . ex:b ex:sub ex:c . ex:c ex:sub ex:d .';
  const tv = Validator.fromTurtle(trans, BASE);
  check('one pass stays within maxCount 2', tv.validateTurtle(chain, BASE, 'rules').conforms, true);
  check('iterating closes the chain and breaks it', tv.validateTurtle(chain, BASE, 'rules-iterated').conforms, false);

  let threw = false;
  try { v.validateTurtle(data, BASE, 'magic'); } catch (e) { threw = String(e).includes('rules-iterated'); }
  check('an unknown mode is an error, not a silent none', threw, true);
}

// --------------------------------------------- one-shot argument order
//
// The one-shot took (shapes, data) until 0.2.0 and now takes (data, shapes),
// matching the Python binding, the CLI and the Rust API. Getting it backwards
// was a silent fault, not a loud one: the data compiled as a shapes graph,
// declared no shapes, and validating against no shapes conforms — so the
// caller was told the graph was valid when nothing had been checked. These
// pin both halves: the new order works, and the old order fails loudly.
{
  console.log('\n== one-shot argument order ==');
  const shapes = `
    @prefix sh: <http://www.w3.org/ns/shacl#> .
    @prefix ex: <http://example.org/> .
    ex:S a sh:NodeShape ; sh:targetClass ex:Person ;
      sh:property [ sh:path ex:name ; sh:maxCount 1 ] .
  `;
  const data = '@prefix ex: <http://example.org/> . ex:a a ex:Person ; ex:name "A", "B" .';

  check('(data, shapes) finds the violation', validateTurtle(data, shapes, BASE).length, 1);

  let threw = false;
  try {
    validateTurtle(shapes, data, BASE);
  } catch (e) {
    threw = String(e).includes('declares no shapes');
  }
  check('(shapes, data) throws rather than reporting conformance', threw, true);

  // A self-describing document carries its own shapes, as it does for the
  // Python binding and the CLI.
  check('shapes may be omitted', validateTurtle(shapes + data, null, BASE).length, 1);
}

console.log(`\n${failures === 0 ? 'ALL CHECKS PASSED' : failures + ' CHECK(S) FAILED'}`);
process.exit(failures === 0 ? 0 : 1);
