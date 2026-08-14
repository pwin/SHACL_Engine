const fs=require('fs'),path=require('path');
const {Validator}=require(path.join(__dirname,'pkg-node/shacl_wasm.js'));
const f=path.join(__dirname,'../../testsuite/shacl12/tests/sparql/rules/rectangle-prefixes.ttl');
const ttl=fs.readFileSync(f,'utf8');
try{
  const v=Validator.fromTurtle(ttl);
  const r=v.validateTurtle(ttl);
  console.log('WASM: built OK; conforms =', r.conforms, '; results =', r.results.length);
}catch(e){ console.log('WASM threw:', String(e).slice(0,200)); }
