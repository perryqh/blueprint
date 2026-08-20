// Tests for the DOM half of text anchoring (frontend/anchor.js).
//
// Anchoring is the load-bearing piece of the whole product: get it wrong and a
// comment either lands on the wrong paragraph or is reported as "drifted" when
// the text is plainly still there.
//
// Choosing *which* occurrence a selector meant now lives in the
// `blueprint-anchor` crate, so the table-driven cases that used to live here —
// duplicate quotes, window edges, surrogate pairs, the known-wrong fallback —
// are `#[test]`s in anchor/src/lib.rs. What remains is what needs a DOM:
// flattening text across elements, mapping offsets back onto text nodes, and
// building the highlight Ranges. Those exercise the real wasm module rather than
// a stand-in, so this file also covers the JS↔wasm boundary.

import { beforeAll, describe, expect, it } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import {
  initAnchoring,
  highlightQuote,
  wholeText,
  textBefore,
  captureSelector,
} from '../frontend/anchor.js';

// The wasm glue defaults to `fetch`ing its sibling .wasm relative to its own
// module URL, which node has no answer for — so hand it the bytes directly.
// Same artifact the browser loads, so the boundary under test is the real one.
// Resolved from the vitest root rather than `import.meta.url`: the jsdom
// environment reports module URLs as http://, which `readFileSync` rejects.
beforeAll(async () => {
  await initAnchoring(readFileSync(resolve(process.cwd(), 'frontend/pkg/anchor_bg.wasm')));
});

// Build a document whose body is `html`. jsdom is the environment (see
// vitest.config.js), so `document.implementation` gives us isolated docs that
// don't interfere with each other across cases.
function docFrom(html) {
  const doc = document.implementation.createHTMLDocument('t');
  doc.body.innerHTML = html;
  return doc;
}

function sel({ exact, prefix, suffix }) {
  const s = { type: 'TextQuoteSelector', exact };
  if (prefix !== undefined) s.prefix = prefix;
  if (suffix !== undefined) s.suffix = suffix;
  return s;
}

// The text covered by the highlight spans, concatenated in document order.
function highlightedText(doc) {
  return [...doc.querySelectorAll('span[data-ps-hl]')]
    .map(s => s.textContent)
    .join('');
}

// Anchored and context-confirmed — the ordinary success case.
const CONFIDENT = { anchored: true, confident: true };

describe('wholeText / textBefore', () => {
  it('flattens text across element boundaries in document order', () => {
    const doc = docFrom('<p>Hello <em>brave</em> <strong>new</strong> world</p>');
    expect(wholeText(doc.body)).toBe('Hello brave new world');
  });

  it('textBefore counts every preceding text node plus the partial one', () => {
    const doc = docFrom('<p>ab<em>cd</em>ef</p>');
    const em = doc.querySelector('em');
    // Offset 1 inside "cd" → "ab" + "c".
    expect(textBefore(doc.body, em.firstChild, 1)).toBe('abc');
    // Offset 0 of the same node → just the preceding nodes.
    expect(textBefore(doc.body, em.firstChild, 0)).toBe('ab');
  });

  it('textBefore at a text-node boundary is unambiguous from either side', () => {
    const doc = docFrom('<p>ab<em>cd</em>ef</p>');
    const p = doc.querySelector('p');
    const ab = p.firstChild;
    const cd = doc.querySelector('em').firstChild;
    // The offset "2" is expressible as the end of `ab` or the start of `cd`;
    // both must flatten to the same index, or captureSelector and
    // highlightQuote would disagree about where a selection began.
    expect(textBefore(doc.body, ab, 2)).toBe('ab');
    expect(textBefore(doc.body, cd, 0)).toBe('ab');
  });
});

describe('captureSelector', () => {
  it('records 32 code units of context on each side', () => {
    const doc = docFrom(`<p>${'a'.repeat(40)}QUOTE${'b'.repeat(40)}</p>`);
    const textNode = doc.querySelector('p').firstChild;
    const range = doc.createRange();
    const start = wholeText(doc.body).indexOf('QUOTE');
    range.setStart(textNode, start);
    range.setEnd(textNode, start + 5);
    const fakeSel = { getRangeAt: () => range, toString: () => 'QUOTE' };
    const s = captureSelector(fakeSel, doc.body);
    // The width comes from the Rust side via `contextUnits()`, so this also
    // asserts capture and comparison still agree about it.
    expect(s).toEqual({
      type: 'TextQuoteSelector',
      exact: 'QUOTE',
      prefix: 'a'.repeat(32),
      suffix: 'b'.repeat(32),
    });
  });

  it('returns null when the selection start escaped the anchoring root', () => {
    const doc = docFrom('<p>inside</p>');
    const other = docFrom('<p>outside</p>');
    const range = other.createRange();
    range.selectNodeContents(other.querySelector('p').firstChild);
    const fakeSel = { getRangeAt: () => range, toString: () => 'outside' };
    expect(captureSelector(fakeSel, doc.body)).toBeNull();
  });

  it('yields an empty prefix for a selection at the very start', () => {
    const doc = docFrom('<p>start here</p>');
    const textNode = doc.querySelector('p').firstChild;
    const range = doc.createRange();
    range.setStart(textNode, 0);
    range.setEnd(textNode, 5);
    const fakeSel = { getRangeAt: () => range, toString: () => 'start' };
    // Real and reachable, and it silently turns off prefix disambiguation for
    // the resulting selector — see the Rust cases for what that then does.
    expect(captureSelector(fakeSel, doc.body).prefix).toBe('');
  });
});

describe('highlightQuote', () => {
  it('wraps a unique quote inside a single text node', () => {
    const doc = docFrom('<p>alpha beta gamma</p>');
    expect(highlightQuote(doc, sel({ exact: 'beta', prefix: 'alpha ' }), 'beta')).toEqual(CONFIDENT);
    const spans = doc.querySelectorAll('span[data-ps-hl]');
    expect(spans.length).toBe(1);
    expect(spans[0].textContent).toBe('beta');
    expect(spans[0].dataset.psQuote).toBe('beta');
    // Flattened text must be unchanged — highlighting is presentational only.
    expect(wholeText(doc.body)).toBe('alpha beta gamma');
  });

  it('spans element boundaries across <em>/<strong>', () => {
    const doc = docFrom('<p>Hello <em>brave</em> <strong>new</strong> world</p>');
    const quote = 'brave new';
    expect(highlightQuote(doc, sel({ exact: quote, prefix: 'Hello ' }), quote)).toEqual(CONFIDENT);
    // One span per contributing text node: "brave", " ", "new".
    expect(highlightedText(doc)).toBe(quote);
    expect(doc.querySelectorAll('span[data-ps-hl]').length).toBeGreaterThan(1);
    // The <em>/<strong> structure survives — spans go inside them, not around.
    expect(doc.querySelector('em span[data-ps-hl]')).not.toBeNull();
    expect(doc.querySelector('strong span[data-ps-hl]')).not.toBeNull();
    expect(wholeText(doc.body)).toBe('Hello brave new world');
  });

  it('anchors when the offset lands exactly on a text-node boundary', () => {
    // The quote starts precisely where the <em> text node starts and ends
    // precisely where it ends, so start/endOffset are 0 and node length —
    // the boundary conditions in the walker's `consumed + len` arithmetic.
    const doc = docFrom('<p>ab<em>cd</em>ef</p>');
    expect(highlightQuote(doc, sel({ exact: 'cd', prefix: 'ab', suffix: 'ef' }), 'cd'))
      .toEqual(CONFIDENT);
    expect(highlightedText(doc)).toBe('cd');
    expect(wholeText(doc.body)).toBe('abcdef');
  });

  it('anchors a quote ending exactly at the document end', () => {
    const doc = docFrom('<p>lead tail</p>');
    expect(highlightQuote(doc, sel({ exact: 'tail', prefix: 'lead ', suffix: '' }), 'tail'))
      .toEqual(CONFIDENT);
    expect(highlightedText(doc)).toBe('tail');
  });

  it('reports not-anchored and wraps nothing when the quote is gone', () => {
    const doc = docFrom('<p>the plan changed</p>');
    const r = highlightQuote(doc, sel({ exact: 'deleted sentence' }), 'deleted sentence');
    expect(r.anchored).toBe(false);
    expect(doc.querySelectorAll('span[data-ps-hl]').length).toBe(0);
  });

  it('picks the context-matching occurrence among duplicates', () => {
    const doc = docFrom('<p>the cat sat. the dog sat. done</p>');
    expect(highlightQuote(doc, sel({ exact: 'sat', prefix: 'the dog ' }), 'sat'))
      .toEqual(CONFIDENT);
    const span = doc.querySelector('span[data-ps-hl]');
    // The highlighted "sat" must be the one after "dog", not after "cat".
    expect(span.previousSibling.textContent.endsWith('the dog ')).toBe(true);
  });

  // The headline case from issue #6, now observable. The quote is still present
  // but no occurrence agrees with the recorded context, so the highlight lands on
  // the first hit — the wrong one. It still anchors (that trade-off is
  // deliberate), but `confident: false` is what lets the sidebar say so instead
  // of presenting it as a clean match.
  it('flags a fallback match as not confident', () => {
    const doc = docFrom('<p>one HIT two HIT three HIT four</p>');
    const r = highlightQuote(doc, sel({ exact: 'HIT', prefix: 'ZZZZ ', suffix: ' QQQQ' }), 'HIT');
    expect(r).toEqual({ anchored: true, confident: false });
    // And it really did take the first occurrence.
    const span = doc.querySelector('span[data-ps-hl]');
    expect(span.previousSibling.textContent).toBe('one ');
  });

  it('invokes onClick with the quote when a highlight is activated', () => {
    const doc = docFrom('<p>alpha beta gamma</p>');
    const seen = [];
    highlightQuote(doc, sel({ exact: 'beta' }), 'beta', q => seen.push(q));
    const span = doc.querySelector('span[data-ps-hl]');
    // Keyboard-reachable, per the a11y fix: role, tab stop, and Enter support.
    expect(span.getAttribute('role')).toBe('button');
    expect(span.tabIndex).toBe(0);
    // A document from createHTMLDocument has no defaultView, so use the
    // environment's own event constructors — dispatchEvent accepts them.
    span.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    span.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    expect(seen).toEqual(['beta', 'beta']);
  });

  // Astral-plane text end to end through the wasm boundary: the offsets the
  // module returns are UTF-16 code units, which is what the TreeWalker's
  // `textContent.length` arithmetic counts. A byte- or char-based port would
  // return a plausible number here and highlight the wrong span.
  it('anchors correctly when the document contains emoji before the quote', () => {
    const doc = docFrom('<p>🎯🎯🎯 aim at TARGET now</p>');
    expect(highlightQuote(doc, sel({ exact: 'TARGET', prefix: 'aim at ' }), 'TARGET'))
      .toEqual(CONFIDENT);
    expect(highlightedText(doc)).toBe('TARGET');
    expect(wholeText(doc.body)).toBe('🎯🎯🎯 aim at TARGET now');
  });
});
