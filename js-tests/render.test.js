// Tests for the sidebar renderers (frontend/render.js). Two things worth
// pinning down: the DOM-construction rewrite must not reintroduce markup
// injection, and every control must carry the data-action attributes the
// delegated listener dispatches on.

import { beforeEach, describe, expect, it } from 'vitest';
import {
  groupComments,
  renderBatchIndicator,
  renderCount,
  renderEmptyState,
  renderGroup,
  isThreadResolved,
  countAllReplies,
} from '../frontend/render.js';

function comment(over = {}) {
  return {
    id: 'c1',
    author: 'perry',
    body: 'body text',
    created_at: Date.now(),
    selector: { exact: 'quoted text' },
    parent_id: null,
    resolved: false,
    ...over,
  };
}

const emptyState = () => ({
  drifted: new Set(),
  pendingInNewVersion: new Set(),
  collapsedQuotes: new Set(),
  focusedThreadIdx: -1,
  byParent: new Map(),
});

const ctx = { slug: 'my-plan', currentVersion: 3 };

describe('renderCount', () => {
  const cases = [
    { total: 0, threadCount: 0, text: 'no comments' },
    { total: 1, threadCount: 1, text: '1 thread · 1 comment' },
    { total: 5, threadCount: 2, text: '2 threads · 5 comments' },
  ];
  for (const c of cases) {
    it(`renders "${c.text}"`, () => {
      const el = document.createElement('span');
      renderCount(el, c);
      expect(el.textContent).toBe(c.text);
    });
  }

  it('wraps the numbers in span.n for the type scale', () => {
    const el = document.createElement('span');
    renderCount(el, { total: 5, threadCount: 2 });
    expect([...el.querySelectorAll('span.n')].map(s => s.textContent)).toEqual(['2', '5']);
  });

  it('clears previous content instead of appending', () => {
    const el = document.createElement('span');
    renderCount(el, { total: 5, threadCount: 2 });
    renderCount(el, { total: 1, threadCount: 1 });
    expect(el.textContent).toBe('1 thread · 1 comment');
  });
});

describe('renderEmptyState', () => {
  it('builds the cold state with a real <kbd>, not markup', () => {
    const el = renderEmptyState('cold');
    expect(el.className).toBe('empty-state');
    expect(el.querySelector('.empty-title').textContent).toBe('No comments yet.');
    expect(el.querySelector('kbd').textContent).toBe('⌘+Enter');
    // The old version used innerHTML for this; assert the tag really exists as
    // an element rather than as escaped text.
    expect(el.querySelector('.empty-body').textContent).toContain('Select any text');
  });

  it('builds the all-resolved state with a real <em>', () => {
    const el = renderEmptyState('all-resolved');
    expect(el.className).toBe('empty-state done');
    expect(el.querySelector('em').textContent).toBe('show resolved');
  });
});

describe('groupComments', () => {
  it('groups top-level comments by quote and nests replies', () => {
    const all = [
      comment({ id: 'a', selector: { exact: 'q1' } }),
      comment({ id: 'b', selector: { exact: 'q2' } }),
      comment({ id: 'r1', parent_id: 'a' }),
    ];
    const { byQuote, byParent, visibleQuotes } = groupComments(all, { showResolved: false });
    expect([...byQuote.keys()]).toEqual(['q1', 'q2']);
    expect(byParent.get('a').map(c => c.id)).toEqual(['r1']);
    expect(visibleQuotes.length).toBe(2);
  });

  it('hides resolved threads unless showResolved, and always counts them', () => {
    const all = [
      comment({ id: 'a', selector: { exact: 'q1' }, resolved: true }),
      comment({ id: 'b', selector: { exact: 'q2' } }),
    ];
    const hidden = groupComments(all, { showResolved: false });
    expect(hidden.visibleQuotes.map(([q]) => q)).toEqual(['q2']);
    expect(hidden.resolvedCount).toBe(1);
    const shown = groupComments(all, { showResolved: true });
    expect(shown.visibleQuotes.map(([q]) => q)).toEqual(['q1', 'q2']);
    expect(shown.resolvedCount).toBe(1);
  });

  it('treats a thread as resolved based on its head comment only', () => {
    expect(isThreadResolved([comment({ resolved: true }), comment({ resolved: false })])).toBe(true);
    expect(isThreadResolved([comment({ resolved: false }), comment({ resolved: true })])).toBe(false);
    expect(isThreadResolved([])).toBe(false);
  });
});

describe('countAllReplies', () => {
  it('counts nested replies recursively', () => {
    const byParent = new Map([
      ['a', [comment({ id: 'r1' }), comment({ id: 'r2' })]],
      ['r1', [comment({ id: 'r3' })]],
      ['r3', [comment({ id: 'r4' })]],
    ]);
    expect(countAllReplies('a', byParent)).toBe(4);
    expect(countAllReplies('nope', byParent)).toBe(0);
  });
});

describe('renderGroup', () => {
  it('makes the collapse affordance a real button with aria-expanded', () => {
    const g = renderGroup({
      quote: 'quoted text',
      cs: [comment()],
      visibleIdx: 0,
      state: emptyState(),
      ctx,
    });
    const toggle = g.querySelector('[data-action="toggle-collapse"]');
    expect(toggle.tagName).toBe('BUTTON');
    expect(toggle.getAttribute('aria-expanded')).toBe('true');
    expect(toggle.getAttribute('aria-controls')).toBe(g.id);
    expect(g.querySelector('.quote-text').textContent).toBe('"quoted text"');
  });

  it('reports aria-expanded=false when collapsed', () => {
    const state = emptyState();
    state.collapsedQuotes.add('quoted text');
    const g = renderGroup({ quote: 'quoted text', cs: [comment()], visibleIdx: 0, state, ctx });
    expect(g.className).toContain('collapsed');
    expect(g.querySelector('[data-action="toggle-collapse"]').getAttribute('aria-expanded'))
      .toBe('false');
  });

  it('defaults resolved threads to collapsed, overridable with the ! key', () => {
    const state = emptyState();
    const cs = [comment({ resolved: true })];
    let g = renderGroup({ quote: 'quoted text', cs, visibleIdx: 0, state, ctx });
    expect(g.className).toContain('collapsed');
    state.collapsedQuotes.add('!quoted text');
    g = renderGroup({ quote: 'quoted text', cs, visibleIdx: 0, state, ctx });
    expect(g.className).not.toContain('collapsed');
  });

  it('offers resolve on open threads and reopen on resolved ones', () => {
    const open = renderGroup({
      quote: 'q', cs: [comment()], visibleIdx: 0, state: emptyState(), ctx,
    });
    const openBtn = open.querySelector('[data-action="set-resolved"]');
    expect(openBtn.tagName).toBe('BUTTON');
    expect(openBtn.textContent).toBe('resolve');
    expect(openBtn.dataset.resolved).toBe('true');
    expect(openBtn.dataset.commentId).toBe('c1');

    const done = renderGroup({
      quote: 'q', cs: [comment({ resolved: true })], visibleIdx: 0, state: emptyState(), ctx,
    });
    const doneBtn = done.querySelector('[data-action="set-resolved"]');
    expect(doneBtn.textContent).toBe('reopen');
    expect(doneBtn.dataset.resolved).toBe('false');
  });

  it('withholds resolve on drifted and pending threads', () => {
    for (const key of ['drifted', 'pendingInNewVersion']) {
      const state = emptyState();
      state[key].add('q');
      const g = renderGroup({ quote: 'q', cs: [comment()], visibleIdx: 0, state, ctx });
      expect(g.querySelector('[data-action="set-resolved"]')).toBeNull();
      // The delegated click handler reads these flags off the dataset.
      expect(g.dataset.pending || g.dataset.drifted).toBe('1');
    }
  });

  it('is focusable for j/k without joining the tab order', () => {
    const g = renderGroup({
      quote: 'q', cs: [comment()], visibleIdx: 0, state: emptyState(), ctx,
    });
    expect(g.tabIndex).toBe(-1);
  });

  it('tags the reply control with the comment id for delegation', () => {
    const g = renderGroup({
      quote: 'q', cs: [comment({ id: 'c9' })], visibleIdx: 0, state: emptyState(), ctx,
    });
    const btn = g.querySelector('[data-action="stage-reply"]');
    expect(btn.dataset.commentId).toBe('c9');
    expect(btn.type).toBe('button');
    const input = g.querySelector('input[data-reply-for="c9"]');
    expect(input.getAttribute('aria-label')).toBe('Reply to perry');
  });

  it('renders comment bodies and authors as text, never as markup', () => {
    const g = renderGroup({
      quote: 'q',
      cs: [comment({ author: '<b>x</b>', body: '<script>alert(1)</script>' })],
      visibleIdx: 0,
      state: emptyState(),
      ctx,
    });
    expect(g.querySelector('script')).toBeNull();
    expect(g.querySelector('.body').textContent).toBe('<script>alert(1)</script>');
    expect(g.querySelector('.a-name').textContent).toBe('<b>x</b>');
    expect(g.querySelector('.a-name b')).toBeNull();
  });

  it('shows a version badge only for comments older than the loaded version', () => {
    const older = renderGroup({
      quote: 'q', cs: [comment({ blueprint_version: 1 })], visibleIdx: 0,
      state: emptyState(), ctx,
    });
    expect(older.querySelector('.version-badge').textContent).toBe('on v1');
    const current = renderGroup({
      quote: 'q', cs: [comment({ blueprint_version: 3 })], visibleIdx: 0,
      state: emptyState(), ctx,
    });
    expect(current.querySelector('.version-badge')).toBeNull();
  });

  it('counts the whole thread in the collapse badge', () => {
    const state = emptyState();
    state.byParent = new Map([['c1', [comment({ id: 'r1' })]], ['r1', [comment({ id: 'r2' })]]]);
    const g = renderGroup({ quote: 'q', cs: [comment()], visibleIdx: 0, state, ctx });
    // 1 head + 2 nested replies.
    expect(g.querySelector('.collapse-count').textContent).toBe('3 messages');
  });
});

describe('renderBatchIndicator', () => {
  let el;
  beforeEach(() => {
    el = document.createElement('div');
  });

  it('hides and empties when there is nothing in flight', () => {
    renderBatchIndicator(el, null);
    expect(el.hidden).toBe(true);
    expect(el.textContent).toBe('');
  });

  it('hides a stale entry past the TTL', () => {
    renderBatchIndicator(el, { author: 'Claude', count: 2, started_at: Date.now() - 10 * 60 * 1000 });
    expect(el.hidden).toBe(true);
  });

  it('renders the author in a <strong> without markup injection', () => {
    renderBatchIndicator(el, { author: '<i>Claude</i>', count: 2, started_at: Date.now() });
    expect(el.hidden).toBe(false);
    expect(el.querySelector('strong').textContent).toBe('<i>Claude</i>');
    expect(el.querySelector('i')).toBeNull();
    expect(el.textContent).toContain('is working on 2 comments');
    expect(el.querySelector('svg.sparkle')).not.toBeNull();
  });

  it('singularises a single comment', () => {
    renderBatchIndicator(el, { author: 'Claude', count: 1, started_at: Date.now() });
    expect(el.textContent).toContain('is working on 1 comment');
  });
});
