// Sidebar rendering. Every interactive element carries a `data-action` and the
// ids it needs; a single delegated listener in app.js reads them. Nothing here
// attaches a listener, which is what makes the "rebuild the subtree on every
// poll" strategy safe — there are no handlers to leak and no focused node whose
// listeners die mid-typing.

import { makeAvatar, makeSparkleSvg, strongPrefixed, timeAgo, PROCESSING_TTL_MS } from './dom.js';

export function renderEmptyState(kind) {
  const wrap = document.createElement('div');
  wrap.className = 'empty-state' + (kind === 'all-resolved' ? ' done' : '');
  const icon = document.createElement('div');
  icon.className = 'empty-icon';
  icon.textContent = kind === 'cold' ? '+' : '✓';
  wrap.appendChild(icon);
  const title = document.createElement('div');
  title.className = 'empty-title';
  title.textContent = kind === 'cold' ? 'No comments yet.' : 'All threads resolved.';
  wrap.appendChild(title);
  const body = document.createElement('div');
  body.className = 'empty-body';
  if (kind === 'cold') {
    body.appendChild(document.createTextNode('Select any text in the plan and press '));
    const kbd = document.createElement('kbd');
    kbd.textContent = '⌘+Enter';
    body.appendChild(kbd);
    body.appendChild(document.createTextNode(' to leave the first one.'));
  } else {
    body.appendChild(document.createTextNode('Toggle '));
    const em = document.createElement('em');
    em.textContent = 'show resolved';
    body.appendChild(em);
    body.appendChild(document.createTextNode(' above to see them again.'));
  }
  wrap.appendChild(body);
  return wrap;
}

// Sidebar header count. Numbers get a span.n so the type scale can emphasize
// them while keeping the unit labels muted.
export function renderCount(countEl, { total, threadCount }) {
  if (!countEl) return;
  countEl.textContent = '';
  if (total === 0) {
    countEl.textContent = 'no comments';
    return;
  }
  const threads = document.createElement('span');
  threads.className = 'n';
  threads.textContent = String(threadCount);
  countEl.appendChild(threads);
  countEl.appendChild(
    document.createTextNode(` thread${threadCount === 1 ? '' : 's'} · `)
  );
  const comments = document.createElement('span');
  comments.className = 'n';
  comments.textContent = String(total);
  countEl.appendChild(comments);
  countEl.appendChild(
    document.createTextNode(` comment${total === 1 ? '' : 's'}`)
  );
}

export function countAllReplies(id, byParent) {
  const direct = byParent.get(id) || [];
  return direct.length + direct.reduce((n, r) => n + countAllReplies(r.id, byParent), 0);
}

/* Slug-level "Claude is working on N comments" pill — server `batch_processing`
   field, set by `blueprint batch-processing start` and cleared on the last
   reply or after PROCESSING_TTL_MS. */
export function renderBatchIndicator(el, bp) {
  if (!el) return;
  const active = bp && (Date.now() - bp.started_at) < PROCESSING_TTL_MS;
  if (!active) {
    el.hidden = true;
    el.textContent = '';
    return;
  }
  const noun = bp.count === 1 ? 'comment' : 'comments';
  el.textContent = '';
  el.appendChild(makeSparkleSvg());
  const msg = document.createElement('span');
  msg.appendChild(strongPrefixed(bp.author, ` is working on ${bp.count} ${noun}`));
  el.appendChild(msg);
  el.hidden = false;
}

export function renderComment(c, replies, byParent, ctx) {
  const wrap = document.createElement('div');
  // Role-based outline: owner gets a solid accent border, guest a dashed muted
  // border. Both live in styles.css. Plain `user` (incl. logged-in non-owner)
  // renders without an extra class.
  wrap.className = 'comment'
    + (c.role === 'owner' ? ' is-owner' : '')
    + (c.role === 'guest' ? ' is-guest' : '');
  const author = document.createElement('div');
  author.className = 'author' + (c.is_agent ? ' claude' : '');
  author.appendChild(makeAvatar(document, c.author, c.author_avatar_url, c.is_agent));
  const nameSpan = document.createElement('span');
  nameSpan.className = 'a-name';
  nameSpan.textContent = c.author;
  author.appendChild(nameSpan);
  // Owner / guest pills next to the author name. `user` (no pill) keeps the
  // sidebar quiet for the common case.
  if (c.role === 'owner') {
    const pill = document.createElement('span');
    pill.className = 'role-pill role-owner';
    pill.textContent = 'owner';
    author.appendChild(pill);
  } else if (c.role === 'guest') {
    const pill = document.createElement('span');
    pill.className = 'role-pill role-guest';
    pill.textContent = 'guest';
    author.appendChild(pill);
  }
  const ts = document.createElement('span');
  ts.className = 'ts';
  ts.dataset.ts = String(c.created_at);
  ts.textContent = timeAgo(c.created_at);
  ts.title = new Date(c.created_at).toLocaleString();
  author.appendChild(ts);
  // Version badge: this comment was authored against an older snapshot than
  // the one currently rendered in the iframe. Link to that exact snapshot
  // (served with a no-store sandbox) so the reviewer can see the text it
  // anchored to — the plan may have since edited that passage away.
  if (
    c.blueprint_version != null &&
    ctx.currentVersion != null &&
    c.blueprint_version < ctx.currentVersion
  ) {
    const vb = document.createElement('a');
    vb.className = 'version-badge';
    vb.textContent = `on v${c.blueprint_version}`;
    vb.href = `/api/blueprints/${ctx.slug}/raw?version=${c.blueprint_version}`;
    vb.target = '_blank';
    vb.rel = 'noopener';
    vb.title = `Authored against version ${c.blueprint_version}; you're viewing v${ctx.currentVersion}. Open that snapshot in a new tab.`;
    author.appendChild(vb);
  }
  wrap.appendChild(author);
  const body = document.createElement('div');
  body.className = 'body';
  body.textContent = c.body;
  wrap.appendChild(body);

  // "Claude is replying…" indicator — only on the comment that's being processed,
  // and only within the 5-min TTL window.
  if (c.processing_by && c.processing_started_at) {
    const age = Date.now() - c.processing_started_at;
    if (age >= 0 && age < PROCESSING_TTL_MS) {
      const working = document.createElement('div');
      working.className = 'working-indicator';
      const av = makeAvatar(document, c.processing_by, null, false);
      av.style.width = '16px';
      av.style.height = '16px';
      av.style.fontSize = '8px';
      working.appendChild(av);
      const msg = document.createElement('span');
      msg.appendChild(strongPrefixed(c.processing_by, ' is replying'));
      working.appendChild(msg);
      working.appendChild(makeSparkleSvg());
      wrap.appendChild(working);
    }
  }

  for (const r of replies) {
    const rep = document.createElement('div');
    rep.className = 'reply';
    rep.appendChild(renderComment(r, byParent.get(r.id) || [], byParent, ctx));
    wrap.appendChild(rep);
  }
  const form = document.createElement('div');
  form.className = 'reply-form';
  const inp = document.createElement('input');
  inp.placeholder = 'stage reply…';
  inp.setAttribute('aria-label', `Reply to ${c.author}`);
  inp.dataset.replyFor = c.id;
  const btn = document.createElement('button');
  btn.type = 'button';
  btn.textContent = 'Add to batch';
  btn.dataset.action = 'stage-reply';
  btn.dataset.commentId = c.id;
  form.appendChild(inp);
  form.appendChild(btn);
  wrap.appendChild(form);
  return wrap;
}

// Group comments into per-quote threads and hand back both the visible set and
// the resolved tally the header toggle needs.
export function groupComments(allComments, { showResolved }) {
  const tops = allComments.filter(c => !c.parent_id);
  const replies = allComments.filter(c => c.parent_id);
  const byParent = new Map();
  for (const r of replies) {
    if (!byParent.has(r.parent_id)) byParent.set(r.parent_id, []);
    byParent.get(r.parent_id).push(r);
  }
  const byQuote = new Map();
  for (const c of tops) {
    const k = c.selector.exact;
    if (!byQuote.has(k)) byQuote.set(k, []);
    byQuote.get(k).push(c);
  }
  // Partition top-level threads by whether their head comment is resolved.
  // A thread is "resolved" iff its first top-level comment has resolved=true.
  const visibleQuotes = [];
  let resolvedCount = 0;
  for (const [quote, cs] of byQuote) {
    if (isThreadResolved(cs)) {
      resolvedCount++;
      if (!showResolved) continue;
    }
    visibleQuotes.push([quote, cs]);
  }
  return { byParent, byQuote, visibleQuotes, resolvedCount };
}

export function isThreadResolved(cs) {
  return cs.length > 0 && cs[0].resolved;
}

// Build one thread group. The quote bar is a real <button> so collapse/expand
// is reachable by keyboard and announced with its expanded state — it used to
// be a <div> with a click handler, i.e. invisible to assistive tech.
export function renderGroup({ quote, cs, visibleIdx, state, ctx }) {
  const { byParent } = state;
  const resolved = isThreadResolved(cs);
  const isPending = state.pendingInNewVersion.has(quote);
  const isDrifted = state.drifted.has(quote);
  // Anchored, but on an occurrence the recorded context didn't confirm. Distinct
  // from drift: the quote is still in the document, it's the *placement* that's
  // in doubt. `?? false` keeps callers that predate the flag working.
  const isMisanchored = state.misanchored?.has(quote) ?? false;
  // Resolved threads default to collapsed (overridable per-thread).
  const isCollapsed = state.collapsedQuotes.has(quote)
    || (resolved && !state.collapsedQuotes.has('!' + quote));
  const group = document.createElement('div');
  group.className = 'group'
    + (isPending ? ' pending' : isDrifted ? ' drifted' : isMisanchored ? ' misanchored' : '')
    + (resolved ? ' resolved' : '')
    + (isCollapsed ? ' collapsed' : '')
    + (visibleIdx === state.focusedThreadIdx ? ' focused' : '');
  group.dataset.quote = quote;
  group.dataset.visibleIdx = String(visibleIdx);
  group.dataset.headId = cs[0].id;
  if (isPending) group.dataset.pending = '1';
  if (isDrifted) group.dataset.drifted = '1';
  // j/k navigation moves real focus here, so the group needs to be focusable
  // without becoming a tab stop of its own.
  group.tabIndex = -1;
  group.id = `group-${cs[0].id}`;

  // `.quote` stays the visual bar, but it's now a plain flex container: the
  // collapse affordance is a real <button> inside it, and `resolve` is a
  // sibling button. Interactive content can't nest, so the two can't be merged.
  const q = document.createElement('div');
  q.className = 'quote';

  const toggle = document.createElement('button');
  toggle.type = 'button';
  toggle.className = 'quote-toggle';
  toggle.dataset.action = 'toggle-collapse';
  toggle.setAttribute('aria-expanded', isCollapsed ? 'false' : 'true');
  toggle.setAttribute('aria-controls', group.id);
  const quoteText = document.createElement('span');
  quoteText.className = 'quote-text';
  quoteText.textContent = `"${quote}"`;
  toggle.appendChild(quoteText);

  if (resolved) {
    const tag = document.createElement('span');
    tag.className = 'resolved-tag';
    tag.textContent = 'resolved';
    toggle.appendChild(tag);
  } else if (isPending) {
    const tag = document.createElement('span');
    tag.className = 'pending-tag';
    tag.textContent = 'refresh to see';
    toggle.appendChild(tag);
  } else if (isDrifted) {
    const tag = document.createElement('span');
    tag.className = 'drift-tag';
    tag.textContent = 'drifted';
    toggle.appendChild(tag);
  } else if (isMisanchored) {
    const tag = document.createElement('span');
    tag.className = 'misanchor-tag';
    tag.textContent = 'may be misplaced';
    tag.title = 'The quoted text still exists, but the surrounding text has '
      + 'changed, so this highlight may be on the wrong occurrence.';
    toggle.appendChild(tag);
  }
  // Reply count badge (only visible when collapsed via CSS).
  const replyCount = cs.reduce((n, c) => n + countAllReplies(c.id, byParent), 0);
  const threadSize = cs.length + replyCount;
  const badge = document.createElement('span');
  badge.className = 'collapse-count';
  badge.textContent = `${threadSize} message${threadSize === 1 ? '' : 's'}`;
  toggle.appendChild(badge);
  q.appendChild(toggle);

  // Resolve is offered on every thread: un-resolved ones get `resolve`,
  // resolved ones get `reopen`. Pending/drifted threads can't be resolved
  // because their anchor text isn't on screen to judge.
  if (resolved || (!isPending && !isDrifted)) {
    const resolveBtn = document.createElement('button');
    resolveBtn.type = 'button';
    resolveBtn.className = 'resolve-link';
    resolveBtn.textContent = resolved ? 'reopen' : 'resolve';
    resolveBtn.dataset.action = 'set-resolved';
    resolveBtn.dataset.commentId = cs[0].id;
    resolveBtn.dataset.resolved = resolved ? 'false' : 'true';
    q.appendChild(resolveBtn);
  }
  group.appendChild(q);

  for (const c of cs) {
    group.appendChild(renderComment(c, byParent.get(c.id) || [], byParent, ctx));
  }
  return group;
}
