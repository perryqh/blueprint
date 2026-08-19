// Text anchoring — maps a TextQuoteSelector (exact + prefix + suffix) onto a
// live DOM and wraps the match in highlight spans.
//
// The whole file works on the *flattened* text of a subtree (`wholeText`), not
// on element structure, because a user selection routinely straddles inline
// elements (`<em>`, `<code>`, a `<a>` mid-sentence). Offsets are therefore
// indices into that flattened string, and `highlightQuote` walks the text nodes
// a second time to convert an offset pair back into a Range.

const CONTEXT_CHARS = 32;

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
  const prefix = full.slice(Math.max(0, start - CONTEXT_CHARS), start);
  const suffix = full.slice(start + exact.length, start + exact.length + CONTEXT_CHARS);
  return { type: 'TextQuoteSelector', exact, prefix, suffix };
}

// Resolve `selector` against `doc` and wrap every matching run in a highlight
// span. Returns false when the quote no longer exists — the caller renders that
// as "drifted".
//
// `onClick` is invoked with the quote when a highlight span is clicked; passed
// in rather than imported so this module stays free of app state.
export function highlightQuote(doc, selector, quote, onClick) {
  const root = doc.body;
  if (!root) return false;
  const text = wholeText(root);
  const bestIdx = findQuoteIndex(text, selector, quote);
  if (bestIdx === -1) return false;
  const walker = doc.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let consumed = 0;
  let startNode = null, startOffset = 0, endNode = null, endOffset = 0;
  const targetStart = bestIdx;
  const targetEnd = bestIdx + quote.length;
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
  if (!startNode || !endNode) return false;
  try {
    const range = doc.createRange();
    range.setStart(startNode, startOffset);
    range.setEnd(endNode, endOffset);
    wrapRange(range, doc, quote, onClick);
    return true;
  } catch (e) {
    return false;
  }
}

// Pick which occurrence of `quote` the selector meant. Walks every hit and
// keeps the first whose surrounding text agrees with prefix/suffix.
//
// Known-wrong fallback: when no occurrence matches the context we return the
// first `indexOf` hit anyway rather than failing. That silently anchors to the
// wrong paragraph, but the alternative — reporting drift on text that plainly
// still exists — tested worse, so the wrong-but-visible anchor stays.
//
// Note both context checks are skipped when the corresponding field is falsy,
// which covers `undefined` (the server omits absent context via
// skip_serializing_if) and `''` (a selection at the very start of the document
// has no prefix). Either way disambiguation is off and the first hit wins.
export function findQuoteIndex(text, selector, quote) {
  let searchFrom = 0;
  while (true) {
    const found = text.indexOf(quote, searchFrom);
    if (found === -1) break;
    const before = text.slice(Math.max(0, found - CONTEXT_CHARS), found);
    const after = text.slice(found + quote.length, found + quote.length + CONTEXT_CHARS);
    const prefixOK = !selector.prefix
      || before.endsWith(selector.prefix.slice(-Math.min(CONTEXT_CHARS, selector.prefix.length)));
    const suffixOK = !selector.suffix
      || after.startsWith(selector.suffix.slice(0, Math.min(CONTEXT_CHARS, selector.suffix.length)));
    if (prefixOK && suffixOK) return found;
    searchFrom = found + 1;
  }
  return text.indexOf(quote);
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
