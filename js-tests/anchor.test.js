// Table-driven tests for the text-anchoring algorithm (frontend/anchor.js).
//
// Anchoring is the load-bearing piece of the whole product: get it wrong and a
// comment either lands on the wrong paragraph or is reported as "drifted" when
// the text is plainly still there. It had zero coverage in either language
// before this file.
//
// `findQuoteIndex` is tested directly on strings (it's pure), and
// `highlightQuote` is tested against real jsdom documents so the text-node
// walking and Range construction are exercised too.

import { describe, expect, it } from 'vitest';
import {
  findQuoteIndex,
  highlightQuote,
  wholeText,
  textBefore,
  captureSelector,
} from '../frontend/anchor.js';

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
  // Each case: the haystack, the selector, and the offset we expect to win.
  const cases = [
    {
      name: 'unique quote — context is redundant but must not break the match',
      text: 'alpha beta gamma',
      selector: { exact: 'beta', prefix: 'alpha ', suffix: ' gamma' },
      expected: 6,
    },
    {
      name: 'unique quote with no context at all',
      text: 'alpha beta gamma',
      selector: { exact: 'beta' },
      expected: 6,
    },
    {
      name: 'duplicate quotes — prefix disambiguates to the second',
      text: 'the cat sat. the dog sat. done',
      selector: { exact: 'sat', prefix: 'the dog ' },
      expected: 21,
    },
    {
      name: 'duplicate quotes — suffix disambiguates to the second',
      text: 'x: value one. x: value two.',
      selector: { exact: 'x: value', suffix: ' two.' },
      expected: 14,
    },
    {
      name: 'duplicate quotes — prefix+suffix together pick the middle one',
      text: 'a ref b. c ref d. e ref f.',
      selector: { exact: 'ref', prefix: 'c ', suffix: ' d.' },
      expected: 11,
    },
    {
      name: 'prefix longer than the 32-char window still matches on its tail',
      // Only the last 32 chars of the prefix are compared, because that's all
      // `captureSelector` recorded and all the haystack slice provides.
      text: 'x'.repeat(50) + 'TARGET tail',
      selector: { exact: 'TARGET', prefix: 'y'.repeat(20) + 'x'.repeat(32) },
      expected: 50,
    },
    {
      name: 'no match — quote is simply gone',
      text: 'nothing to see here',
      selector: { exact: 'absent', prefix: 'no', suffix: 'pe' },
      expected: -1,
    },
  ];

  for (const c of cases) {
    it(c.name, () => {
      expect(findQuoteIndex(c.text, sel(c.selector), c.selector.exact)).toBe(c.expected);
    });
  }

  // KNOWN-WRONG BEHAVIOUR, asserted deliberately so a future change is visible.
  //
  // When several occurrences exist and *none* of them agrees with the recorded
  // prefix/suffix (e.g. the plan was edited around the quote), the loop falls
  // through and `return text.indexOf(quote)` hands back the FIRST occurrence.
  // That is very likely the wrong one — the user's comment was anchored to some
  // other instance. The alternative (returning -1 and flagging drift) was judged
  // worse in practice, so the wrong-but-visible anchor is intentional. If this
  // test starts failing, someone changed that trade-off on purpose.
  it('duplicate quotes with FAILING disambiguation falls back to the first hit (known-wrong)', () => {
    const text = 'one HIT two HIT three HIT four';
    // Context that matches none of the three occurrences.
    const s = sel({ exact: 'HIT', prefix: 'ZZZZ ', suffix: ' QQQQ' });
    expect(findQuoteIndex(text, s, 'HIT')).toBe(4); // first occurrence
    // For contrast: correct disambiguation would have picked the third.
    expect(text.indexOf('HIT', 18)).toBe(22);
  });

  // The Rust test harness (tests/e2e.rs, tests/concurrent.rs) builds nearly
  // every selector as {type, exact} with no context, and src/selector.rs uses
  // `skip_serializing_if = "Option::is_none"` so the fields come back *absent*
  // rather than empty. Either way `!selector.prefix` is truthy and
  // disambiguation is skipped entirely — so the whole prefix/suffix branch that
  // this algorithm exists for is never exercised by the Rust suite.
  it('empty-string prefix disables prefix disambiguation (so first hit wins)', () => {
    const text = 'aa ZZ bb ZZ cc';
    // A selection at offset 0 legitimately has prefix '', which is falsy — the
    // same code path as "no prefix recorded".
    const s = sel({ exact: 'ZZ', prefix: '', suffix: ' bb' });
    expect(findQuoteIndex(text, s, 'ZZ')).toBe(3);
    // Suffix alone still works; it's only the empty field that opts out.
    const s2 = sel({ exact: 'ZZ', prefix: '', suffix: ' cc' });
    expect(findQuoteIndex(text, s2, 'ZZ')).toBe(9);
  });

  it('undefined prefix (server omitted it) behaves the same as empty', () => {
    const text = 'aa ZZ bb ZZ cc';
    const s = sel({ exact: 'ZZ', suffix: ' cc' });
    expect(s.prefix).toBeUndefined();
    expect(findQuoteIndex(text, s, 'ZZ')).toBe(9);
  });

  // Multi-byte characters make the 32-char context window a *code-unit* window,
  // not a character one: JS string indices are UTF-16 code units, so an emoji
  // (surrogate pair) counts as 2 and can be sliced in half at the boundary.
  describe('multi-byte characters near the 32-char window edge', () => {
    it('astral-plane prefix is compared by code units and still matches', () => {
      // 16 emoji = 32 UTF-16 code units, exactly filling the window.
      const emoji = '🎯'.repeat(16);
      const text = `lead ${emoji}TARGET trail`;
      const start = text.indexOf('TARGET');
      const fullPrefix = text.slice(0, start);
      expect(fullPrefix.length).toBeGreaterThan(32);
      // captureSelector would have recorded only the last 32 code units.
      const recorded = fullPrefix.slice(-32);
      expect(findQuoteIndex(text, sel({ exact: 'TARGET', prefix: recorded }), 'TARGET'))
        .toBe(start);
    });

    it('a surrogate pair split by the window edge still anchors', () => {
      // 33 code units of prefix means the 32-unit window starts mid-emoji,
      // leaving a lone low surrogate at the front of both the recorded prefix
      // and the haystack slice. They're compared as equal code units, so
      // endsWith still succeeds — the match must not be lost to mojibake.
      const text = 'z' + '🎯'.repeat(16) + 'TARGET';
      const start = text.indexOf('TARGET');
      expect(start).toBe(33);
      const recorded = text.slice(0, start).slice(-32);
      expect(recorded.length).toBe(32);
      expect(findQuoteIndex(text, sel({ exact: 'TARGET', prefix: recorded }), 'TARGET'))
        .toBe(start);
    });

    it('combining marks are matched as written, not normalized', () => {
      // 'é' as e + U+0301 is two code units and does NOT equal precomposed 'é'.
      // The algorithm does no Unicode normalization, so a selector recorded in
      // one form won't match text stored in the other. Asserted so the
      // limitation is documented rather than discovered.
      const decomposed = 'café menu';
      const precomposed = 'café menu';
      expect(findQuoteIndex(decomposed, sel({ exact: 'café' }), 'café')).toBe(-1);
      expect(findQuoteIndex(precomposed, sel({ exact: 'café' }), 'café')).toBe(0);
    });
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
    // This is the '' case above: real, reachable, and it silently turns off
    // prefix disambiguation for the resulting selector.
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
