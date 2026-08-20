// Text anchoring — maps a TextQuoteSelector (exact + prefix + suffix) onto a
// live DOM and wraps the match in highlight spans.
//
// The whole file works on the *flattened* text of a subtree (`wholeText`), not
// on element structure, because a user selection routinely straddles inline
// elements (`<em>`, `<code>`, a `<a>` mid-sentence). Offsets are therefore
// indices into that flattened string, and `highlightQuote` walks the text nodes
// a second time to convert an offset pair back into a Range.
//
// Deciding *which* occurrence of a quote the selector meant no longer lives
// here: it's `resolveQuote` in the blueprint-anchor crate, compiled to wasm, so
// the daemon and the browser share one implementation instead of keeping two
// copies in step by hand. What stays is everything that genuinely needs a DOM —
// TreeWalker mapping, Range construction, span wrapping.

import init, { resolveQuote, contextUnits } from './pkg/anchor.js';

let ready = null;

// Load the wasm module. Idempotent, so every entry point can await it without
// coordinating; `app.js` does so once at boot, before the first render.
//
// `wasm` is an escape hatch for tests, which have no fetch-able URL and pass the
// bytes straight in. Left undefined, the glue fetches `anchor_bg.wasm` relative
// to its own module URL — /static/pkg/ in the daemon.
export function initAnchoring(wasm) {
  ready ??= init(wasm === undefined ? undefined : { module_or_path: wasm });
  return ready;
}

// Context has to cross into wasm as UTF-16 code units, not as a string.
//
// The context window is a fixed number of code units, so it can bisect a
// surrogate pair and leave a lone surrogate at the edge of the recorded prefix —
// which has no UTF-8 encoding, so a string boundary would silently rewrite it to
// U+FFFD and the prefix would stop matching. Empty and absent both mean "no
// context recorded", which disables that half of disambiguation.
function codeUnits(s) {
  if (!s) return undefined;
  const a = new Uint16Array(s.length);
  for (let i = 0; i < s.length; i++) a[i] = s.charCodeAt(i);
  return a;
}

// Flattened text of `root`, in document order. Must stay in lockstep with the
// walker in `highlightQuote` — both use SHOW_TEXT with no filter, so the same
// nodes contribute in the same order and offsets are comparable.
export function wholeText(root) {
  const walker = root.ownerDocument.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let s = '';
  while (walker.nextNode()) s += walker.currentNode.textContent;
  return s;
}

// Length of the flattened text preceding (node, offset). Used to turn a live
// selection boundary into a `wholeText` index.
export function textBefore(root, node, offset) {
  const walker = root.ownerDocument.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let s = '';
  while (walker.nextNode()) {
    const n = walker.currentNode;
    if (n === node) {
      s += n.textContent.slice(0, offset);
      return s;
    }
    s += n.textContent;
  }
  return s;
}

// Build a selector from a live Selection. `body` is the anchoring root; a
// selection whose start escaped it (possible when the user drags out of the
// iframe) is unanchorable, so bail rather than record a bogus offset.
export function captureSelector(sel, body) {
  const range = sel.getRangeAt(0);
  if (!body.contains(range.startContainer)) return null;
  const exact = sel.toString();
  const before = textBefore(body, range.startContainer, range.startOffset);
  const full = wholeText(body);
  const start = before.length;
  // Window width comes from the Rust side rather than a local constant: capture
  // and comparison have to agree on it, and two copies of a "32" are exactly how
  // they'd drift apart for long prefixes.
  const win = contextUnits();
  const prefix = full.slice(Math.max(0, start - win), start);
  const suffix = full.slice(start + exact.length, start + exact.length + win);
  return { type: 'TextQuoteSelector', exact, prefix, suffix };
}

// Resolve `selector` against `doc` and wrap every matching run in a highlight
// span.
//
// Returns `{ anchored, confident }`:
//   anchored  — false when the quote no longer exists anywhere, or the Range
//               couldn't be built. The caller renders that as "drifted".
//   confident — false when the quote was found but *no* occurrence agreed with
//               the recorded prefix/suffix, so this is merely the first hit and
//               probably the wrong paragraph. Only meaningful when anchored.
//
// The second field is what the old bare-index return could not express: a
// context-confirmed match and a blind fallback were indistinguishable, so a
// comment silently attached to text the reviewer never selected.
//
// `onClick` is invoked with the quote when a highlight span is clicked; passed
// in rather than imported so this module stays free of app state.
export function highlightQuote(doc, selector, quote, onClick) {
  const miss = { anchored: false, confident: true };
  const root = doc.body;
  if (!root) return miss;
  const text = wholeText(root);
  const hit = resolveQuote(text, quote, codeUnits(selector.prefix), codeUnits(selector.suffix));
  if (!hit) return miss;
  // Read the fields out and release the wasm-side object rather than waiting for
  // the glue's FinalizationRegistry — this runs once per comment per render.
  const targetStart = hit.start;
  const targetEnd = hit.end;
  const confident = !hit.fallback;
  hit.free();
  const found = { anchored: true, confident };
  const walker = doc.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let consumed = 0;
  let startNode = null, startOffset = 0, endNode = null, endOffset = 0;
  while (walker.nextNode()) {
    const n = walker.currentNode;
    const len = n.textContent.length;
    if (startNode === null && consumed + len > targetStart) {
      startNode = n;
      startOffset = targetStart - consumed;
    }
    if (consumed + len >= targetEnd) {
      endNode = n;
      endOffset = targetEnd - consumed;
      break;
    }
    consumed += len;
  }
  if (!startNode || !endNode) return miss;
  try {
    const range = doc.createRange();
    range.setStart(startNode, startOffset);
    range.setEnd(endNode, endOffset);
    wrapRange(range, doc, quote, onClick);
    return found;
  } catch (e) {
    return miss;
  }
}

// Wrap `range` in one or more highlight spans. `surroundContents` is tried
// first because a single-text-node match produces exactly one clean span; it
// throws on ranges that partially select an element, hence the split-per-node
// fallback below.
export function wrapRange(range, doc, quote, onClick) {
  if (range.startContainer === range.endContainer && range.startContainer.nodeType === Node.TEXT_NODE) {
    const span = mkHighlight(doc, quote, onClick);
    try {
      range.surroundContents(span);
      return;
    } catch (e) {
      // fall through to multi-node path
    }
  }
  // Snapshot the node list before mutating: we replace each text node with
  // three new ones, which would otherwise perturb a live walker mid-iteration.
  const nodes = [];
  const iter = doc.createTreeWalker(range.commonAncestorContainer, NodeFilter.SHOW_TEXT, {
    acceptNode(n) {
      return range.intersectsNode(n) ? NodeFilter.FILTER_ACCEPT : NodeFilter.FILTER_REJECT;
    }
  });
  let n;
  while ((n = iter.nextNode())) nodes.push(n);
  for (const node of nodes) {
    const start = node === range.startContainer ? range.startOffset : 0;
    const end = node === range.endContainer ? range.endOffset : node.textContent.length;
    if (start >= end) continue;
    const before = node.textContent.slice(0, start);
    const mid = node.textContent.slice(start, end);
    const after = node.textContent.slice(end);
    const parent = node.parentNode;
    if (!parent) continue;
    const beforeNode = doc.createTextNode(before);
    const midSpan = mkHighlight(doc, quote, onClick);
    midSpan.textContent = mid;
    const afterNode = doc.createTextNode(after);
    parent.insertBefore(beforeNode, node);
    parent.insertBefore(midSpan, node);
    parent.insertBefore(afterNode, node);
    parent.removeChild(node);
  }
}

function mkHighlight(doc, quote, onClick) {
  const span = doc.createElement('span');
  span.setAttribute('data-ps-hl', '1');
  span.dataset.psQuote = quote;
  span.title = `(annotated) ${quote} — click to see comments`;
  // Keyboard-reachable: the highlight is a real control (it jumps the sidebar
  // to the thread), so it needs a tab stop and Enter/Space, not just a click.
  span.setAttribute('role', 'button');
  span.tabIndex = 0;
  span.setAttribute('aria-label', `Comments on "${quote}"`);
  if (onClick) {
    span.addEventListener('click', e => {
      e.stopPropagation();
      onClick(quote);
    });
    span.addEventListener('keydown', e => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        e.stopPropagation();
        onClick(quote);
      }
    });
  }
  return span;
}

// Undo every highlight span in `doc`, restoring the original text nodes.
// `normalize()` re-merges the three-way split from `wrapRange` so repeated
// apply/clear cycles don't fragment the document indefinitely.
export function clearHighlights(doc) {
  doc.querySelectorAll('span[data-ps-hl]').forEach(span => {
    const parent = span.parentNode;
    while (span.firstChild) parent.insertBefore(span.firstChild, span);
    parent.removeChild(span);
    parent.normalize();
  });
}
