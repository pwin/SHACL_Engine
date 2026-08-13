// Differential test: the WASM module against the native CLI, over the whole
// W3C corpus.
//
// The engine is shared, so what this actually exercises is everything the WASM
// build does *differently* -- the sequential parse it takes with `parallel`
// off, the binding layer, and a wasm32 runtime where `getrandom` and the clock
// are supplied by the host rather than by std. Any of those changing a result
// would show up here as a disagreement on a real shapes graph.
//
// Comparison is on the semantic content of a report -- how many results, and
// the multiset of (component, severity, focus) -- rather than on serialised
// bytes. The two builds intern terms in a different order, so blank node
// labels in a report can legitimately differ; the findings cannot.
//
// A blank node is therefore compared as "a blank node" rather than by label.
// RDF says a label is local syntax carrying no identity, the two sides scope
// theirs differently, and the JS surface renders them differently again -- so
// requiring the text to match would fail on 36 documents where every finding
// is otherwise identical, which is noise rather than signal.
//
// Usage: node differential.js [--limit N]
const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const { Validator } = require('./pkg-node/shacl_wasm.js');

const ROOT = path.resolve(__dirname, '../..');
const CLI = path.join(ROOT, 'target/release/shacl.exe');
const SUITES = [path.join(ROOT, 'testsuite/shacl10'), path.join(ROOT, 'testsuite/shacl12')];

const limitArg = process.argv.indexOf('--limit');
const LIMIT = limitArg > -1 ? Number(process.argv[limitArg + 1]) : Infinity;

function walk(dir, out = []) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p, out);
    else if (e.name.endsWith('.ttl') && !e.name.startsWith('manifest')) out.push(p);
  }
  return out;
}

// A W3C test file is self-contained: it carries the shapes and the data it is
// meant to be validated against, which is why both sides can use it as both.
// `_:1_b1` from the N-Triples report, `1:b1` from the JS surface: the same
// node, spelled by two different layers.
const anon = (t) => (t && (t.startsWith('_:') || /^\d+[:_]b\d+$/.test(t)) ? '<blank>' : t);

function viaWasm(text) {
  const v = Validator.fromTurtle(text, 'http://example.org/test');
  const r = v.validateTurtle(text, 'http://example.org/test');
  return r.results.map((x) => `${x.component}|${x.severity}|${anon(x.focusNode)}|${x.path ?? ""}`).sort();
}

function viaCli(file) {
  // Exit 1 means "does not conform", which is the expected outcome for most of
  // this corpus, so it is a result rather than a failure. Anything else is a
  // real error and must not be read as an empty report.
  let out;
  try {
    out = execFileSync(CLI, ['-d', file, '-s', file, '-f', 'nt'], {
      encoding: 'utf8',
      maxBuffer: 1 << 28,
    });
  } catch (e) {
    if (e.status !== 1) throw e;
    out = e.stdout ?? '';
  }
  // Rebuild the same tuples from the RDF report.
  //
  // Only top-level results: those are what the JS `results` array exposes.
  // A constraint like `sh:memberShape` also emits nested results hung off
  // `sh:detail`, which are in the RDF report and not in the array, so counting
  // both sides' subjects would compare two different things.
  const topLevel = new Set();
  const byResult = new Map();
  const P = 'http://www.w3.org/ns/shacl#';
  for (const line of out.split('\n')) {
    const m = line.match(/^\S+ <([^>]+)> (\S+) \.$/);
    if (m && m[1] === `${P}result`) topLevel.add(m[2]);
  }
  for (const line of out.split('\n')) {
    const m = line.match(/^(\S+) <([^>]+)> (.+) \.$/);
    if (!m) continue;
    const [, subj, pred, objRaw] = m;
    if (!pred.startsWith(P)) continue;
    const key = pred.slice(P.length);
    if (!['sourceConstraintComponent', 'resultSeverity', 'focusNode', 'resultPath'].includes(key)) continue;
    // Match the JS surface's rendering: an IRI bare, and a literal as its
    // lexical form with the datatype and language tag dropped -- that is the
    // RDF/JS `.value` convention the bindings deliberately follow, so a
    // literal focus node reads `7` there and `"7"^^xsd:integer` here.
    let obj;
    if (objRaw.startsWith('<')) obj = objRaw.slice(1, -1);
    else if (objRaw.startsWith('"')) obj = objRaw.slice(1, objRaw.lastIndexOf('"'));
    else obj = objRaw;
    if (!byResult.has(subj)) byResult.set(subj, {});
    byResult.get(subj)[key] = obj;
  }
  return [...byResult.entries()]
    .filter(([subj]) => topLevel.has(subj))
    .map(([, r]) => r)
    .filter((r) => r.sourceConstraintComponent)
    .map((r) => `${r.sourceConstraintComponent}|${r.resultSeverity}|${anon(r.focusNode)}|${r.resultPath ?? ""}`)
    .sort();
}

const files = SUITES.filter(fs.existsSync).flatMap((d) => walk(d)).slice(0, LIMIT);
console.log(`comparing ${files.length} documents\n`);

let agree = 0;
let bothFailed = 0;
const disagree = [];
const onlyOneFailed = [];

for (const file of files) {
  const text = fs.readFileSync(file, 'utf8');
  let w, c, we, ce;
  try { w = viaWasm(text); } catch (e) { we = String(e).split('\n')[0]; }
  try { c = viaCli(file); } catch (e) { ce = String(e.stderr || e).split('\n')[0]; }

  if (we && ce) { bothFailed++; continue; }
  if (we || ce) {
    onlyOneFailed.push({ file: path.relative(ROOT, file), wasm: we ?? `${w.length} results`, cli: ce ?? `${c.length} results` });
    continue;
  }
  if (JSON.stringify(w) === JSON.stringify(c)) agree++;
  else disagree.push({ file: path.relative(ROOT, file), wasm: w.length, cli: c.length });
}

console.log(`agree            ${agree}`);
console.log(`both rejected    ${bothFailed}   (malformed or unsupported on purpose -- still agreement)`);
console.log(`one-sided error  ${onlyOneFailed.length}`);
console.log(`DISAGREE         ${disagree.length}`);

for (const d of onlyOneFailed.slice(0, 10)) console.log(`  one-sided ${d.file}\n     wasm: ${d.wasm}\n     cli:  ${d.cli}`);
for (const d of disagree.slice(0, 10)) console.log(`  differs   ${d.file}  wasm=${d.wasm} cli=${d.cli}`);

const bad = disagree.length + onlyOneFailed.length;
console.log(`\n${bad === 0 ? 'WASM AND NATIVE AGREE ON EVERY DOCUMENT' : bad + ' DIVERGENCE(S)'}`);
process.exit(bad === 0 ? 0 : 1);
