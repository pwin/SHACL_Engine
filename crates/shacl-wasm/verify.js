// Verifies the WASM build against real shapes -- including the exact ones that
// defeat the JS `shacl-engine` in the Ontology Development Suite extension:
//   * shapes/data.ttl and shapes/efficiency.ttl crash it outright
//     ("Tried to bind variable ?this in a GROUP BY operator")
//   * it silently drops sh:severity declared inside an sh:sparql block
const fs = require('node:fs');
const path = require('node:path');
const { Validator } = require('./pkg-node/shacl_wasm.js');

const REGISTRY = 'C:/repos/consolidated_ontology_suite_webapp/resources/checks-registry';
const BASE = 'http://example.org/';

let failures = 0;
function check(label, actual, expected) {
  const ok = JSON.stringify(actual) === JSON.stringify(expected);
  if (!ok) failures++;
  console.log(`  ${ok ? 'PASS' : 'FAIL'}  ${label}`);
  if (!ok) console.log(`        expected ${JSON.stringify(expected)}\n        actual   ${JSON.stringify(actual)}`);
}

// ---------------------------------------------------------------- sanity
console.log('\n== 1. basic sanity: minCount violation ==');
{
  const shapes = `
    @prefix sh: <http://www.w3.org/ns/shacl#> .
    @prefix ex: <http://example.org/> .
    ex:PersonShape a sh:NodeShape ;
      sh:targetClass ex:Person ;
      sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:severity sh:Warning ] .
  `;
  const data = `
    @prefix ex: <http://example.org/> .
    ex:alice a ex:Person ; ex:name "Alice" .
    ex:bob   a ex:Person .
  `;
  const v = Validator.fromTurtle(shapes, BASE);
  const r = v.validateTurtle(data, BASE);
  const res = r.results;
  check('one finding', res.length, 1);
  check('focus node is ex:bob', res[0].focusNode, 'http://example.org/bob');
  check('path is ex:name', res[0].path, 'http://example.org/name');
  check('severity honours sh:Warning on the property shape', res[0].severity, 'http://www.w3.org/ns/shacl#Warning');
  check('conforms (no Violation-severity results)', r.conforms, true);
}

// ------------------------------------- the two shapes that crash shacl-engine
console.log('\n== 2. shapes that crash the JS shacl-engine ==');
for (const file of ['data.ttl', 'efficiency.ttl', 'structural.ttl', 'style.ttl', 'logical.ttl', 'quality.ttl']) {
  const shapes = fs.readFileSync(path.join(REGISTRY, 'shapes', file), 'utf8');
  try {
    const v = Validator.fromTurtle(shapes, BASE);
    console.log(`  PASS  ${file} compiled (${v.shapeCount} shapes)`);
  } catch (e) {
    failures++;
    console.log(`  FAIL  ${file}: ${e}`);
  }
}

// ------------------------------------------- severity inside an sh:sparql block
console.log('\n== 3. sh:severity declared inside sh:sparql (dropped by shacl-engine) ==');
{
  // STY-003 declares sh:Info inside its sh:sparql block; a label with no
  // language tag is exactly what it targets.
  const shapes = fs.readFileSync(path.join(REGISTRY, 'shapes', 'style.ttl'), 'utf8');
  const data = `
    @prefix ex: <http://example.org/demo#> .
    @prefix owl: <http://www.w3.org/2002/07/owl#> .
    @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
    ex:Dog a owl:Class ; rdfs:label "Dog" .
  `;
  const v = Validator.fromTurtle(shapes, BASE);
  const r = v.validateTurtle(data, BASE);
  const sty003 = r.results.filter((x) => (x.sourceShape || '').endsWith('STY-003'));
  check('STY-003 fired on the untagged label', sty003.length > 0, true);
  if (sty003.length) {
    check('severity is Info, as the shape declares', sty003[0].severity, 'http://www.w3.org/ns/shacl#Info');
  }
  console.log(`        (all severities seen: ${JSON.stringify([...new Set(r.results.map((x) => x.severity))])})`);
}

// ----------------------------------------------------------- report as turtle
console.log('\n== 4. report serialises to Turtle ==');
{
  const shapes = `
    @prefix sh: <http://www.w3.org/ns/shacl#> .
    @prefix ex: <http://example.org/> .
    ex:S a sh:NodeShape ; sh:targetClass ex:T ; sh:property [ sh:path ex:p ; sh:minCount 1 ] .
  `;
  const v = Validator.fromTurtle(shapes, BASE);
  const ttl = v.validateTurtle('@prefix ex: <http://example.org/> . ex:x a ex:T .', BASE).toTurtle();
  check('turtle mentions sh:ValidationReport', ttl.includes('ValidationReport'), true);
  check('turtle mentions the focus node', ttl.includes('http://example.org/x'), true);
}

console.log(`\n${failures === 0 ? 'ALL CHECKS PASSED' : failures + ' CHECK(S) FAILED'}`);
process.exit(failures === 0 ? 0 : 1);
