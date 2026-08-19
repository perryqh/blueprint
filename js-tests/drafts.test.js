// Tests for the staged-drafts store (frontend/drafts.js). The key risk here is
// the localStorage key: it's scoped by login, so anything that changes the
// perceived identity moves the drafts to a different key.

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { createDraftStore, renderDraftsBar } from '../frontend/drafts.js';

function store(identity = () => null, onChange = () => {}) {
  return createDraftStore({ slug: 'my-plan', identity, onChange });
}

beforeEach(() => {
  localStorage.clear();
});

describe('draftsKey', () => {
  it('scopes by slug and login', () => {
    expect(store(() => ({ login: 'perry' })).draftsKey())
      .toBe('blueprint:drafts:my-plan:perry');
  });

  it('falls back to anon when signed out', () => {
    expect(store(() => null).draftsKey()).toBe('blueprint:drafts:my-plan:anon');
  });

  // This is the failure mode behind the refreshMe() fix: if a transient blip
  // downgrades the identity to null, the key moves and the user's staged drafts
  // vanish from the UI while re-persisting under the wrong key.
  it('a changed identity points at a different bucket', () => {
    let me = { login: 'perry' };
    const s = store(() => me);
    s.add({ body: 'signed-in draft' });
    expect(s.count()).toBe(1);
    me = null;                     // the downgrade refreshMe must now prevent
    s.load();
    expect(s.count()).toBe(0);     // drafts are "gone" from the user's view
    me = { login: 'perry' };       // and come back when identity is restored
    s.load();
    expect(s.count()).toBe(1);
  });
});

describe('add / remove / clear', () => {
  it('trims the body and ignores empty submissions', () => {
    const s = store();
    s.add({ body: '   ' });
    expect(s.count()).toBe(0);
    s.add({ body: '  hello  ' });
    expect(s.all()[0].body).toBe('hello');
  });

  it('assigns a distinct cid per draft and removes by it', () => {
    const s = store();
    s.add({ body: 'one' });
    s.add({ body: 'two' });
    const [a, b] = s.all();
    expect(a.cid).not.toBe(b.cid);
    s.remove(a.cid);
    expect(s.all().map(d => d.body)).toEqual(['two']);
  });

  it('round-trips through localStorage', () => {
    const s = store();
    s.add({ body: 'persisted', selector: { exact: 'q' } });
    const fresh = store();
    fresh.load();
    expect(fresh.all()).toHaveLength(1);
    expect(fresh.all()[0].selector.exact).toBe('q');
  });

  it('survives corrupt stored JSON', () => {
    localStorage.setItem('blueprint:drafts:my-plan:anon', '{not json');
    const s = store();
    expect(s.load()).toEqual([]);
  });

  it('survives stored JSON of the wrong shape', () => {
    // An older build could have written something non-array here.
    localStorage.setItem('blueprint:drafts:my-plan:anon', '{"a":1}');
    const s = store();
    expect(s.load()).toEqual([]);
  });

  it('notifies on every mutation so the bar can re-render', () => {
    const onChange = vi.fn();
    const s = store(() => null, onChange);
    s.add({ body: 'x' });
    s.remove(s.all()[0].cid);
    s.clear();
    expect(onChange).toHaveBeenCalledTimes(3);
  });
});

describe('renderDraftsBar', () => {
  beforeEach(() => {
    document.body.innerHTML = `
      <div id="drafts-bar" hidden>
        <span id="drafts-count"></span>
        <button id="drafts-submit"></button>
        <div id="drafts-list"></div>
      </div>`;
  });

  it('stays hidden with no drafts', () => {
    const s = store();
    renderDraftsBar(s);
    expect(document.getElementById('drafts-bar').hidden).toBe(true);
  });

  it('renders one tile per draft with a discard action', () => {
    const s = store();
    s.add({ body: 'first', selector: { exact: 'some quote' } });
    s.add({ body: 'second', parentId: 'c1', parentBody: 'parent text' });
    renderDraftsBar(s);
    expect(document.getElementById('drafts-bar').hidden).toBe(false);
    expect(document.getElementById('drafts-count').textContent).toBe('2 drafts staged');
    expect(document.getElementById('drafts-submit').textContent).toBe('Submit all 2');
    const tiles = document.querySelectorAll('.draft-tile');
    expect(tiles.length).toBe(2);
    expect(tiles[0].querySelector('.draft-pill').textContent).toBe('DRAFT');
    expect(tiles[1].querySelector('.draft-pill').textContent).toBe('DRAFT REPLY');
    expect(tiles[0].querySelector('.draft-anchor').textContent).toBe('on "some quote"');
    expect(tiles[1].querySelector('.draft-anchor').textContent).toBe('↳ reply to "parent text"');
    // Delegation contract: the cid travels on the button, not in a closure.
    const del = tiles[0].querySelector('[data-action="discard-draft"]');
    expect(del.dataset.cid).toBe(s.all()[0].cid);
  });

  it('singularises the count for one draft', () => {
    const s = store();
    s.add({ body: 'only' });
    renderDraftsBar(s);
    expect(document.getElementById('drafts-count').textContent).toBe('1 draft staged');
  });

  it('renders draft bodies as text, never as markup', () => {
    const s = store();
    s.add({ body: '<img src=x onerror=alert(1)>' });
    renderDraftsBar(s);
    const body = document.querySelector('.draft-body');
    expect(body.querySelector('img')).toBeNull();
    expect(body.textContent).toBe('<img src=x onerror=alert(1)>');
  });
});
