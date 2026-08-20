// Tests for the text-anchoring algorithm (frontend/anchor.js).
//
// Anchoring is the load-bearing piece of the whole product: get it wrong and a
// comment either lands on the wrong paragraph or is reported as "drifted" when
// the text is plainly still there.
//
// `findQuoteIndex` is driven from a case table in vectors/anchor-cases.json —
// data rather than code, so the cases read as a spec. `highlightQuote` is tested
// against real jsdom documents so the text-node walking and Range construction
// are exercised too.

import { describe, expect, it } from 'vitest';
import corpus from './vectors/anchor-cases.json';
import {
  findQuoteIndex,
  highlightQuote,
  wholeText,
  textBefore,
  captureSelector,
} from '../frontend/anchor.js';

const cases = corpus.cases;

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

describe('findQuoteIndex', () => {
  // Cases come from vectors/anchor-cases.json. Keeping them as data means the
  // table reads as a specification of the algorithm — including the offsets it
  // gets deliberately wrong — instead of being buried in assertion syntax, and
  // a new case costs one JSON object rather than a new test.
  it.each(cases.map(c => [c.name, c]))('%s', (_name, c) => {
    expect(findQuoteIndex(c.text, sel(c), c.exact)).toBe(c.index ?? -1);
  });

  it('covers the whole corpus', () => {
    // Guards against `it.each` silently receiving an empty array. A
    // table-driven suite whose table failed to load reports success while
    // testing nothing, which is worse than having no suite.
    expect(cases.length).toBeGreaterThanOrEqual(25);
  });

  // Two things the case table cannot carry, kept here as hand-written cases.

  // JSON cannot hold an unpaired surrogate, so this one only exists in code.
  //
  // Getting a real split takes care, and the version of this test that shipped
  // before did not manage it: a 32-unit window over 2-unit emoji always lands on
  // a pair boundary, because both are even. `'z' + 16 emoji` yields a perfectly
  // well-formed prefix, so the old assertion passed without exercising the case
  // it described. A split needs an odd count of 1-unit characters after the run —
  // 20 emoji plus `'a'` puts the cut at offset 9, inside emoji #5.
  it('a prefix starting on a lone surrogate still anchors', () => {
    const text = '🎯'.repeat(20) + 'aTARGET';
    const start = text.indexOf('TARGET');
    expect(start).toBe(41);
    const recorded = text.slice(0, start).slice(-32);
    expect(recorded.length).toBe(32);
    // Assert the premise, so this can't quietly stop testing what it claims.
    const first = recorded.charCodeAt(0);
    expect(first).toBeGreaterThanOrEqual(0xDC00);
    expect(first).toBeLessThanOrEqual(0xDFFF);
    expect(findQuoteIndex(text, sel({ exact: 'TARGET', prefix: recorded }), 'TARGET'))
      .toBe(start);
  });

  // The table asserts that an empty quote resolves to nothing. What it cannot
  // assert is that the call *terminates*: `indexOf('', n)` clamps to `length`
  // instead of returning -1, so without the guard the scan cursor stops
  // advancing the result and the loop hangs the tab. The timeout turns that into
  // a legible failure rather than a wedged test run.
  it('an empty quote returns -1 rather than spinning forever', { timeout: 1000 }, () => {
    expect(findQuoteIndex('any text at all', sel({ exact: '', prefix: 'ZZZZ' }), '')).toBe(-1);
    expect(findQuoteIndex('any text at all', sel({ exact: '' }), '')).toBe(-1);
    expect(findQuoteIndex('', sel({ exact: '' }), '')).toBe(-1);
  });
});

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
  it('records 32 chars of context on each side', () => {
    const doc = docFrom(`<p>${'a'.repeat(40)}QUOTE${'b'.repeat(40)}</p>`);
    const textNode = doc.querySelector('p').firstChild;
    const range = doc.createRange();
    const start = wholeText(doc.body).indexOf('QUOTE');
    range.setStart(textNode, start);
    range.setEnd(textNode, start + 5);
    const fakeSel = { getRangeAt: () => range, toString: () => 'QUOTE' };
    const s = captureSelector(fakeSel, doc.body);
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
    // Real, reachable, and it silently turns off prefix disambiguation for the
    // resulting selector — the corpus has the cases for what that then does.
    expect(captureSelector(fakeSel, doc.body).prefix).toBe('');
  });
});

describe('highlightQuote', () => {
  it('wraps a unique quote inside a single text node', () => {
    const doc = docFrom('<p>alpha beta gamma</p>');
    expect(highlightQuote(doc, sel({ exact: 'beta', prefix: 'alpha ' }), 'beta')).toBe(true);
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
    expect(highlightQuote(doc, sel({ exact: quote, prefix: 'Hello ' }), quote)).toBe(true);
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
    expect(highlightQuote(doc, sel({ exact: 'cd', prefix: 'ab', suffix: 'ef' }), 'cd')).toBe(true);
    expect(highlightedText(doc)).toBe('cd');
    expect(wholeText(doc.body)).toBe('abcdef');
  });

  it('anchors a quote ending exactly at the document end', () => {
    const doc = docFrom('<p>lead tail</p>');
    expect(highlightQuote(doc, sel({ exact: 'tail', prefix: 'lead ', suffix: '' }), 'tail')).toBe(true);
    expect(highlightedText(doc)).toBe('tail');
  });

  it('returns false and wraps nothing when the quote is gone', () => {
    const doc = docFrom('<p>the plan changed</p>');
    expect(highlightQuote(doc, sel({ exact: 'deleted sentence' }), 'deleted sentence')).toBe(false);
    expect(doc.querySelectorAll('span[data-ps-hl]').length).toBe(0);
  });

  it('picks the context-matching occurrence among duplicates', () => {
    const doc = docFrom('<p>the cat sat. the dog sat. done</p>');
    expect(highlightQuote(doc, sel({ exact: 'sat', prefix: 'the dog ' }), 'sat')).toBe(true);
    const span = doc.querySelector('span[data-ps-hl]');
    // The highlighted "sat" must be the one after "dog", not after "cat".
    expect(span.previousSibling.textContent.endsWith('the dog ')).toBe(true);
  });

  // Astral-plane text end to end through the DOM path: the offsets the resolver
  // returns are UTF-16 code units, which is what the TreeWalker's
  // `textContent.length` arithmetic counts. An implementation using bytes or
  // code points would return a plausible number and highlight the wrong span.
  it('anchors correctly when the document contains emoji before the quote', () => {
    const doc = docFrom('<p>🎯🎯🎯 aim at TARGET now</p>');
    expect(highlightQuote(doc, sel({ exact: 'TARGET', prefix: 'aim at ' }), 'TARGET')).toBe(true);
    expect(highlightedText(doc)).toBe('TARGET');
    expect(wholeText(doc.body)).toBe('🎯🎯🎯 aim at TARGET now');
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
});
