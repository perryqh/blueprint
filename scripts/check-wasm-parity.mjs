// Does the committed wasm artifact still match the committed source?
//
// `frontend/pkg/` is a build output that has to be checked in (rust-embed reads
// `frontend/` at compile time), which creates a failure mode nothing else
// catches: edit `anchor/src/`, forget to rebuild, and the Rust tests exercise the
// source while the JS tests and production exercise a stale artifact.
//
// The obvious guard — rebuild in CI and diff the bytes — does not work. wasm
// codegen is not reproducible across platforms: an ubuntu runner and a macOS
// laptop produce functionally identical modules with different bytes. (The
// wasm-bindgen *glue* is reproducible, so that stays a byte comparison; this
// script covers the `.wasm` itself.)
//
// So compare behaviour instead. Run a corpus through both the committed module
// and a freshly built one and require identical answers. That is immune to
// codegen noise while still catching the thing worth catching: an artifact whose
// behaviour has drifted from the source next to it.
//
// Usage: node scripts/check-wasm-parity.mjs <committed-pkg-dir> <fresh-pkg-dir>

import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const [committedDir, freshDir] = process.argv.slice(2);
if (!committedDir || !freshDir) {
  console.error('usage: check-wasm-parity.mjs <committed-pkg-dir> <fresh-pkg-dir>');
  process.exit(2);
}

async function load(dir) {
  const abs = resolve(dir);
  // Distinct file paths, so node instantiates these as independent modules with
  // their own wasm memory — otherwise the second import would be a cache hit and
  // we'd be comparing a module against itself.
  const mod = await import(join(abs, 'anchor.js'));
  await mod.default({ module_or_path: readFileSync(join(abs, 'anchor_bg.wasm')) });
  return mod;
}

const u16 = s => (s == null ? undefined
  : Uint16Array.from({ length: s.length }, (_, i) => s.charCodeAt(i)));

// Deterministic PRNG: a fixed corpus means a failure reproduces exactly, and
// nothing here may depend on wall-clock or unseeded randomness.
function mulberry32(a) {
  return () => {
    a = (a + 0x6D2B79F5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// Deliberately includes the shapes that distinguish a correct port from a
// plausible one: astral-plane characters (2 code units), a combining mark, and
// repeated tokens so disambiguation and the fallback path both get exercised.
const ALPHABET = ['a', 'b', ' ', '.', 'HIT', 'the ', '🎯', '🙂', 'é', 'x'];

function randomText(rnd, maxTokens) {
  let s = '';
  const n = 1 + Math.floor(rnd() * maxTokens);
  for (let i = 0; i < n; i++) s += ALPHABET[Math.floor(rnd() * ALPHABET.length)];
  return s;
}

function* cases() {
  // Hand-written cases first: the ones with known significance.
  const fixed = [
    ['alpha beta gamma', 'beta', 'alpha ', ' gamma'],
    ['the cat sat. the dog sat. done', 'sat', 'the dog ', undefined],
    ['one HIT two HIT three HIT four', 'HIT', 'ZZZZ ', ' QQQQ'], // fallback path
    ['one HIT two HIT three HIT four', 'HIT', 'three ', undefined],
    ['aa ZZ bb ZZ cc', 'ZZ', '', ' cc'],                          // empty prefix opts out
    ['nothing to see here', 'absent', 'no', 'pe'],                // no match
    ['', 'anything', undefined, undefined],
    ['short', 'much longer than the haystack', undefined, undefined],
    ['any text at all', '', 'ZZZZ', undefined],                   // empty quote
    ['café menu', 'café', undefined, undefined],        // normalization
    ['🎯 hit the TARGET 🎯', 'TARGET', undefined, undefined],

    // Self-overlapping quotes. These pin the scan's step size: advancing the
    // cursor by one code unit finds occurrence N+1 *inside* occurrence N, while
    // advancing by the match length skips it. The two only diverge when the
    // context also disagrees at the earlier position — a narrow enough
    // conjunction that 3000 random cases never produced one, so it has to be
    // written out or this check stays blind to a one-token edit in the scan loop.
    ['aaaa', 'aa', 'a', undefined],
    ['aaaa', 'aa', 'aa', undefined],
    ['aaaa', 'aa', undefined, 'a'],
    ['aaaaa', 'aaa', 'a', undefined],
    ['abababab', 'abab', 'ab', undefined],
    ['xaaay', 'aa', 'a', 'y'],
    ['🎯🎯🎯🎯', '🎯🎯', '🎯', undefined],
  ];
  for (const c of fixed) yield c;

  // A prefix that opens on an unpaired surrogate — the case that forces code
  // units across the boundary rather than a string.
  const lone = '🎯'.repeat(20) + 'aTARGET';
  yield [lone, 'TARGET', lone.slice(0, lone.indexOf('TARGET')).slice(-32), undefined];

  // Then breadth, seeded.
  const rnd = mulberry32(0x5EED);
  for (let i = 0; i < 3000; i++) {
    const text = randomText(rnd, 40);
    const quote = rnd() < 0.75
      ? // A substring that really occurs, sliced on code-unit boundaries so the
        // quote itself can begin mid-pair — a shape a real selection never
        // produces but the module must still answer consistently.
        text.slice(Math.floor(rnd() * text.length), Math.floor(rnd() * text.length) + 1 + Math.floor(rnd() * 6))
      : randomText(rnd, 3);
    const prefix = rnd() < 0.3 ? undefined : (rnd() < 0.15 ? '' : randomText(rnd, 20));
    const suffix = rnd() < 0.3 ? undefined : (rnd() < 0.15 ? '' : randomText(rnd, 20));
    yield [text, quote, prefix, suffix];
  }
}

function callAll(mod, text, quote, prefix, suffix) {
  const r = mod.resolveQuote(text, quote, u16(prefix), u16(suffix));
  if (!r) return null;
  const out = { start: r.start, end: r.end, fallback: r.fallback };
  r.free();
  return out;
}

const committed = await load(committedDir);
const fresh = await load(freshDir);

if (committed.contextUnits() !== fresh.contextUnits()) {
  console.error(`FAIL contextUnits: committed=${committed.contextUnits()} fresh=${fresh.contextUnits()}`);
  process.exit(1);
}

let checked = 0;
const mismatches = [];
for (const [text, quote, prefix, suffix] of cases()) {
  const a = callAll(committed, text, quote, prefix, suffix);
  const b = callAll(fresh, text, quote, prefix, suffix);
  checked++;
  if (JSON.stringify(a) !== JSON.stringify(b)) {
    mismatches.push({ text, quote, prefix, suffix, committed: a, fresh: b });
    if (mismatches.length >= 5) break;
  }
}

if (mismatches.length) {
  console.error(`FAIL: committed artifact disagrees with a fresh build on ${mismatches.length} of ${checked} case(s):\n`);
  for (const m of mismatches) console.error(JSON.stringify(m));
  console.error('\nfrontend/pkg is stale — rebuild it (see README) and commit the result.');
  process.exit(1);
}

console.log(`ok: committed artifact and a fresh build agree on all ${checked} cases`);
