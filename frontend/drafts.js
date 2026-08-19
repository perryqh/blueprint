// ---------- Batch drafts (Q1: hard-commit; Q2: scoped by user) ----------
//
// Staged drafts waiting to be batch-submitted. Each entry:
// {cid, body, selector, parentId, parentBody, createdAt}.
// Persisted in localStorage keyed by (slug, oauthUserLogin|anon) so two reviewers
// on the same machine don't see each other's drafts. The agent never sees a
// draft until the user clicks "Submit all" — that's the whole point.

import { truncate } from './dom.js';

// The store owns `drafts` so nothing else can reassign the array out from under
// the persist path. `identity` is a getter, not a value, because refreshMe()
// resolves the login *after* boot and the key has to follow it.
export function createDraftStore({ slug, identity, onChange }) {
  let drafts = [];

  function draftsKey() {
    const who = identity();
    return `blueprint:drafts:${slug}:${who && who.login ? who.login : 'anon'}`;
  }

  function load() {
    try {
      const raw = localStorage.getItem(draftsKey());
      drafts = raw ? JSON.parse(raw) : [];
    } catch (_) {
      drafts = [];
    }
    if (!Array.isArray(drafts)) drafts = [];
    return drafts;
  }

  function persist() {
    try {
      localStorage.setItem(draftsKey(), JSON.stringify(drafts));
    } catch (_) { /* localStorage full — silently lose the persist */ }
  }

  function newDraftId() {
    return 'd_' + Math.random().toString(36).slice(2, 10);
  }

  function add({ body, selector = null, parentId = null, parentBody = null }) {
    const trimmed = body.trim();
    if (!trimmed) return;
    drafts.push({
      cid: newDraftId(),
      body: trimmed,
      selector,
      parentId,
      parentBody,
      createdAt: Date.now(),
    });
    persist();
    onChange();
  }

  function remove(cid) {
    drafts = drafts.filter(d => d.cid !== cid);
    persist();
    onChange();
  }

  function clear() {
    drafts = [];
    persist();
    onChange();
  }

  return {
    all: () => drafts,
    count: () => drafts.length,
    draftsKey,
    load,
    persist,
    add,
    remove,
    clear,
  };
}

// Render the staged-drafts bar. Delegated: one listener on the list handles
// every tile's discard button, so re-rendering can't orphan a handler.
export function renderDraftsBar(store) {
  const bar = document.getElementById('drafts-bar');
  if (!bar) return;
  const list = document.getElementById('drafts-list');
  const count = document.getElementById('drafts-count');
  const submitBtn = document.getElementById('drafts-submit');
  const drafts = store.all();
  if (drafts.length === 0) {
    bar.hidden = true;
    return;
  }
  bar.hidden = false;
  count.textContent = `${drafts.length} draft${drafts.length === 1 ? '' : 's'} staged`;
  submitBtn.textContent = `Submit all ${drafts.length}`;
  list.textContent = '';
  for (const d of drafts) {
    const row = document.createElement('div');
    row.className = 'draft-tile';
    const pill = document.createElement('span');
    pill.className = 'draft-pill';
    pill.textContent = d.parentId ? 'DRAFT REPLY' : 'DRAFT';
    row.appendChild(pill);
    const bodyEl = document.createElement('div');
    bodyEl.className = 'draft-body';
    bodyEl.textContent = d.body;
    row.appendChild(bodyEl);
    const anchor = document.createElement('div');
    anchor.className = 'draft-anchor';
    if (d.parentId) {
      anchor.textContent = d.parentBody
        ? `↳ reply to "${truncate(d.parentBody, 60)}"`
        : '↳ reply';
    } else if (d.selector) {
      anchor.textContent = `on "${truncate(d.selector.exact, 60)}"`;
    }
    row.appendChild(anchor);
    const del = document.createElement('button');
    del.className = 'draft-del icon-btn';
    del.title = 'Discard this draft';
    del.setAttribute('aria-label', 'Discard draft');
    del.textContent = '×';
    del.dataset.action = 'discard-draft';
    del.dataset.cid = d.cid;
    row.appendChild(del);
    list.appendChild(row);
  }
}
