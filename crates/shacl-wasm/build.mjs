#!/usr/bin/env node
// Builds the npm package(s) for shacl-wasm.
//
// Wraps `wasm-pack build` to fix up two things it cannot express itself:
//
//   * The LICENSE files. wasm-pack copies them into the output directory, but
//     the package.json it generates has an explicit `files` array, and npm's
//     "always include a licence" rule does not match the suffixed
//     `LICENSE-APACHE`/`LICENSE-MIT` names this dual-licensed project uses --
//     verified with `npm pack --dry-run`, which listed 4 files and no licence
//     until this script started adding them. Shipping a package whose
//     package.json claims `"license": "MIT OR Apache-2.0"` while carrying
//     neither licence text is not acceptable, so they go in explicitly.
//
//   * `repository`, which wasm-pack warns about on every build.
//
// Usage:
//   node build.mjs            # both targets (nodejs + bundler)
//   node build.mjs nodejs     # just one
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const REPOSITORY = 'https://github.com/pwin/SHACL_Engine';
const LICENSES = ['LICENSE-APACHE', 'LICENSE-MIT'];

// `nodejs` for require()-based consumers (the VS Code extension host, CLIs);
// `bundler` for webpack/vite/rollup, which want ESM plus a separate .wasm.
const TARGETS = {
  nodejs: 'pkg-node',
  bundler: 'pkg-bundler',
};

const requested = process.argv.slice(2);
const targets = requested.length ? requested : Object.keys(TARGETS);

for (const target of targets) {
  const outDir = TARGETS[target];
  if (!outDir) {
    console.error(`unknown target ${target}: expected one of ${Object.keys(TARGETS).join(', ')}`);
    process.exit(1);
  }

  console.log(`\n=== wasm-pack build --target ${target} --out-dir ${outDir} ===`);
  execFileSync('wasm-pack', ['build', '--target', target, '--out-dir', outDir, '--release'], {
    cwd: here,
    stdio: 'inherit',
  });

  const pkgPath = join(here, outDir, 'package.json');
  const pkg = JSON.parse(readFileSync(pkgPath, 'utf8'));

  const missing = LICENSES.filter((f) => !existsSync(join(here, outDir, f)));
  if (missing.length) {
    console.error(`  licence file(s) missing from ${outDir}: ${missing.join(', ')}`);
    process.exit(1);
  }
  pkg.files = [...new Set([...(pkg.files ?? []), ...LICENSES])];
  pkg.repository = { type: 'git', url: `git+${REPOSITORY}.git` };
  pkg.homepage = REPOSITORY;

  writeFileSync(pkgPath, `${JSON.stringify(pkg, null, 2)}\n`, 'utf8');
  console.log(`  patched ${outDir}/package.json: +licences, +repository`);
}
