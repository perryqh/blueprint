// blueprint reviewer — vanilla JS, no deps.
// Anchors comments to text via TextQuoteSelector (exact + prefix + suffix).

const slug = location.pathname.replace(/^\/b\//, '').split('/')[0];
document.getElementById('slug-display').textContent = slug;

let me = null;

async function refreshMe() {
  try {
    const r = await fetch('/api/me');
    me = r.ok ? await r.json() : null;
  } catch (_) {
    me = null;
  }
  renderAuthChip(me);
  // Drafts are scoped by user (Q2), so reload under the now-known identity.
  loadDrafts();
  renderDraftsBar();
}

function renderAuthChip(user) {
  const chip = document.getElementById('auth-chip');
  const legacyAuthorInput = document.getElementById('author');
  if (!chip) return;
  chip.innerHTML = '';
  chip.hidden = false;
  if (user) {
    if (legacyAuthorInput) legacyAuthorInput.style.display = 'none';
    const av = makeAvatar(document, user.login, user.avatar_url, false);
    av.style.width = '22px';
    av.style.height = '22px';
    chip.appendChild(av);
    const name = document.createElement('span');
    name.className = 'auth-chip-name';
    name.textContent = user.login;
    chip.appendChild(name);
    if (user.is_owner) {
      const pill = document.createElement('span');
      pill.className = 'role-pill role-owner';
      pill.textContent = 'owner';
      chip.appendChild(pill);
    }
    const out = document.createElement('a');
    out.href = '#';
    out.className = 'auth-chip-out';
    out.textContent = 'sign out';
    out.addEventListener('click', async e => {
      e.preventDefault();
      await fetch('/logout', { method: 'POST' });
      me = null;
      renderAuthChip(null);
      renderSidebar();
    });
    chip.appendChild(out);
  } else {
    if (legacyAuthorInput) legacyAuthorInput.style.display = '';
    const btn = document.createElement('a');
    btn.href = '/login';
    btn.className = 'login-btn';
    btn.textContent = 'Sign in with GitHub';
    chip.appendChild(btn);
  }
}

const authorInput = document.getElementById('author');
authorInput.value = localStorage.getItem('blueprint:author') || '';
authorInput.addEventListener('change', e => {
  localStorage.setItem('blueprint:author', e.target.value);
});

const blueprintFrame = document.getElementById('blueprint-frame');
blueprintFrame.src = `/api/blueprints/${slug}/raw`;

let allComments = [];
let pendingDraft = null;
// Staged drafts waiting to be batch-submitted. Each entry: {cid, body, selector, parentId, parentBody}.
// Persisted in localStorage keyed by (slug, oauthUserLogin|anon) so two reviewers
// on the same machine don't see each other's drafts. The agent never sees a
// draft until the user clicks "Submit all" — that's the whole point.
let drafts = [];
let lastTs = 0;
let drifted = new Set();
let pendingInNewVersion = new Set();
let pollTimer = null;
let lastBlueprintVersion = null;
let pendingBlueprintVersion = null;
let pendingUpdateCount = 0;
// Auto-scroll bookkeeping (Stage 4c)
let prevTopIds = [];
let prevProcessing = new Map(); // commentId → processing_by (or null)
let focusedThreadIdx = -1;      // j/k navigation
const COLLAPSED_KEY = 'blueprint:collapsed:' + slug;
const SHOW_RESOLVED_KEY = 'blueprint:show-resolved:' + slug;
let collapsedQuotes = new Set(JSON.parse(localStorage.getItem(COLLAPSED_KEY) || '[]'));
let showResolved = localStorage.getItem(SHOW_RESOLVED_KEY) === '1';

const PROCESSING_TTL_MS = 5 * 60 * 1000; // 5 minutes; rows older than this are treated as cleared

function saveCollapsed() {
  localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...collapsedQuotes]));
}

function saveShowResolved() {
  localStorage.setItem(SHOW_RESOLVED_KEY, showResolved ? '1' : '0');
}

// Hash-of-name → one of 8 curated CSS palette gradients. Same input always
// yields the same color across reloads and tabs.
const AVATAR_PALETTE_SIZE = 8;
function authorGradient(name) {
  let h = 0;
  for (const ch of name) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  const idx = (h % AVATAR_PALETTE_SIZE) + 1;
  return `var(--avatar-${idx})`;
}

function authorInitials(name) {
  const trimmed = (name || '?').trim();
  if (!trimmed) return '?';
  // Take first character of up to two whitespace-separated words.
  const parts = trimmed.split(/[\s._-]+/).filter(Boolean);
  if (parts.length === 0) return '?';
  if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
  return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase();
}

function makeAvatar(doc, name, avatarUrl, isAgent) {
  if (avatarUrl) {
    const img = doc.createElement('img');
    img.className = 'avatar avatar-img';
    img.src = avatarUrl;
    img.alt = name || '';
    img.referrerPolicy = 'no-referrer';
    return img;
  }
  const el = doc.createElement('span');
  el.className = 'avatar';
  el.textContent = authorInitials(name);
  // Agent flag is server-stamped via Identity::CliBearer — see src/auth.rs::is_agent.
  // Replaces the old `name.toLowerCase() === 'claude'` heuristic, which never
  // matched because the CLI posts as `--author 'Claude Code'`.
  if (isAgent) {
    el.style.background = 'var(--avatar-claude)';
  } else {
    el.style.background = authorGradient(name || 'anonymous');
  }
  return el;
}

/* ------------------------------------------------------------
 * Theme toggle — System / Light / Dark, persisted to localStorage.
 * Boot inline-script in <head> applies the saved value before paint.
 * ------------------------------------------------------------ */
const THEME_KEY = 'blueprint:theme';
function currentTheme() {
  const v = localStorage.getItem(THEME_KEY);
  return v === 'light' || v === 'dark' ? v : 'system';
}
function resolvedTheme() {
  const t = currentTheme();
  if (t !== 'system') return t;
  return matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
}
function applyTheme(value) {
  if (value === 'system') {
    delete document.documentElement.dataset.theme;
    localStorage.removeItem(THEME_KEY);
  } else {
    document.documentElement.dataset.theme = value;
    localStorage.setItem(THEME_KEY, value);
  }
  for (const btn of document.querySelectorAll('.theme-toggle button')) {
    btn.setAttribute('aria-checked', btn.dataset.themeValue === value ? 'true' : 'false');
  }
  // Re-inject iframe styles to retheme highlight if needed.
  const doc = blueprintFrame.contentDocument;
  if (doc) {
    const existing = doc.getElementById('ps-injected-styles');
    if (existing) existing.remove();
    injectFrameStyles(doc);
  }
}
function bindThemeToggle() {
  const initial = currentTheme();
  for (const btn of document.querySelectorAll('.theme-toggle button')) {
    btn.setAttribute('aria-checked', btn.dataset.themeValue === initial ? 'true' : 'false');
    btn.addEventListener('click', () => applyTheme(btn.dataset.themeValue));
  }
  // Live-react to OS theme changes when set to System.
  matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
    if (currentTheme() === 'system') {
      const doc = blueprintFrame.contentDocument;
      if (doc) {
        const existing = doc.getElementById('ps-injected-styles');
        if (existing) existing.remove();
        injectFrameStyles(doc);
      }
    }
  });
}

// Defensive null-checks so an older cached reviewer.html (without these elements)
// doesn't throw and abort the rest of the script.
const collapseAllEl = document.getElementById('collapse-all');
if (collapseAllEl) {
  collapseAllEl.addEventListener('click', e => {
    e.preventDefault();
    for (const c of allComments) if (!c.parent_id) collapsedQuotes.add(c.selector.exact);
    saveCollapsed();
    renderSidebar();
  });
}
const expandAllEl = document.getElementById('expand-all');
if (expandAllEl) {
  expandAllEl.addEventListener('click', e => {
    e.preventDefault();
    collapsedQuotes.clear();
    saveCollapsed();
    renderSidebar();
  });
}

blueprintFrame.addEventListener('load', () => {
  setupFrameListeners();
  refreshComments();
  startPolling();
});

refreshMe();
bindThemeToggle();
bindShortcuts();
bindDraftsBar();

function bindDraftsBar() {
  const submit = document.getElementById('drafts-submit');
  const discard = document.getElementById('drafts-discard');
  if (submit) submit.addEventListener('click', submitAllDrafts);
  if (discard) discard.addEventListener('click', discardAllDrafts);
  // Surface any persisted drafts immediately (refreshMe will re-render after
  // identity resolves, which may change the key — that's fine, it's idempotent).
  loadDrafts();
  renderDraftsBar();
}

function setupFrameListeners() {
  const doc = blueprintFrame.contentDocument;
  if (!doc) return;
  doc.addEventListener('mouseup', onSelection);
  doc.addEventListener('keyup', e => {
    if (e.shiftKey) onSelection();
  });
  injectFrameStyles(doc);
}

function injectFrameStyles(doc) {
  // Propagate the chrome's resolved theme into the iframe root so plan HTML
  // can adapt via `:root[data-theme="dark"]` selectors. Done unconditionally
  // (even when our style tag is already injected) so toggling chrome theme
  // also retheme s the plan content if the plan author wrote dark-aware CSS.
  doc.documentElement.dataset.theme = resolvedTheme();
  if (doc.getElementById('ps-injected-styles')) return;
  // Iframe HTML is user-supplied and almost always a light document, so the
  // highlight stays yellow-family. Use a slightly more saturated yellow when
  // the parent is dark so the sidebar and iframe feel like one product.
  const dark = resolvedTheme() === 'dark';
  const bg = dark ? '#fde047' : '#fef08a';
  const bd = dark ? '#ca8a04' : '#fde047';
  const hoverBg = dark ? '#facc15' : '#fde68a';
  const hoverBd = dark ? '#a16207' : '#f59e0b';
  const activeBg = '#fcd34d';
  const activeBd = '#b45309';
  const style = doc.createElement('style');
  style.id = 'ps-injected-styles';
  style.textContent = `
    span[data-ps-hl], span[data-ps-hl] * {
      /* The highlight background is always a light yellow, so force dark text
         (and any nested colored text) to stay legible on it — plan HTML with
         white/light type would otherwise wash out. */
      color: #1c1917 !important;
      -webkit-text-fill-color: #1c1917 !important;
    }
    span[data-ps-hl] {
      background-color: ${bg} !important;
      box-shadow: 0 0 0 1px ${bd} !important;
      border-radius: 2px;
      cursor: pointer;
      transition: background-color 120ms;
    }
    span[data-ps-hl]:hover {
      background-color: ${hoverBg} !important;
      box-shadow: 0 0 0 1px ${hoverBd} !important;
    }
    span[data-ps-hl].ps-hl-active {
      background-color: ${activeBg} !important;
      box-shadow: 0 0 0 2px ${activeBd} !important;
    }
  `;
  doc.head.appendChild(style);
}

function onSelection() {
  const win = blueprintFrame.contentWindow;
  const sel = win.getSelection();
  if (!sel || sel.isCollapsed || sel.rangeCount === 0) return;
  const exact = sel.toString();
  if (!exact.trim() || exact.length > 500) return;
  const selector = captureSelector(sel);
  if (selector) showDraft(selector);
}

function captureSelector(sel) {
  const range = sel.getRangeAt(0);
  const body = blueprintFrame.contentDocument.body;
  if (!body.contains(range.startContainer)) return null;
  const exact = sel.toString();
  const before = textBefore(body, range.startContainer, range.startOffset);
  const full = wholeText(body);
  const start = before.length;
  const prefix = full.slice(Math.max(0, start - 32), start);
  const suffix = full.slice(start + exact.length, start + exact.length + 32);
  return { type: 'TextQuoteSelector', exact, prefix, suffix };
}

function wholeText(root) {
  const walker = root.ownerDocument.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  let s = '';
  while (walker.nextNode()) s += walker.currentNode.textContent;
  return s;
}

function textBefore(root, node, offset) {
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

function showDraft(selector) {
  pendingDraft = { selector };
  document.getElementById('draft-quote').textContent = `"${selector.exact}"`;
  document.getElementById('draft-body').value = '';
  document.getElementById('draft').hidden = false;
  document.getElementById('draft-body').focus();
}

// ---------- Batch drafts (Q1: hard-commit; Q2: scoped by user) ----------

function draftsKey() {
  const who = (me && me.login) ? me.login : 'anon';
  return `blueprint:drafts:${slug}:${who}`;
}

function loadDrafts() {
  try {
    const raw = localStorage.getItem(draftsKey());
    drafts = raw ? JSON.parse(raw) : [];
  } catch (_) {
    drafts = [];
  }
  if (!Array.isArray(drafts)) drafts = [];
}

function persistDrafts() {
  try {
    localStorage.setItem(draftsKey(), JSON.stringify(drafts));
  } catch (_) { /* localStorage full — silently lose the persist */ }
}

function newDraftId() {
  return 'd_' + Math.random().toString(36).slice(2, 10);
}

function addToBatch({ body, selector = null, parentId = null, parentBody = null }) {
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
  persistDrafts();
  renderDraftsBar();
}

function removeDraft(cid) {
  drafts = drafts.filter(d => d.cid !== cid);
  persistDrafts();
  renderDraftsBar();
}

function discardAllDrafts() {
  if (drafts.length === 0) return;
  if (!confirm(`Discard ${drafts.length} draft${drafts.length === 1 ? '' : 's'}?`)) return;
  drafts = [];
  persistDrafts();
  renderDraftsBar();
}

async function submitAllDrafts() {
  if (drafts.length === 0) return;
  const submitBtn = document.getElementById('drafts-submit');
  setLoading(submitBtn, true);
  const author = (me && me.login) || (authorInput.value.trim() || 'anonymous');
  const payload = drafts.map(d => {
    const o = { author, body: d.body };
    if (d.selector) o.selector = d.selector;
    if (d.parentId) o.parent_id = d.parentId;
    // Server requires a selector. For replies (where the draft has none),
    // synthesize one from the parent's body so the wire shape is well-formed —
    // the server inherits the real selector from the parent anyway via the
    // /replies route logic, but the /batch endpoint takes the selector as-is.
    // Reuse the parent's selector if we have it via allComments.
    if (!o.selector && d.parentId) {
      const parent = allComments.find(c => c.id === d.parentId);
      if (parent) o.selector = parent.selector;
    }
    return o;
  });
  try {
    const resp = await fetch(`/api/blueprints/${slug}/comments/batch`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload),
    });
    if (!resp.ok) {
      const text = await resp.text();
      showToast(`Submit failed: ${resp.status} ${text}`, 'error');
      return;
    }
    drafts = [];
    persistDrafts();
    renderDraftsBar();
    showToast(`Submitted ${payload.length} comment${payload.length === 1 ? '' : 's'}`, 'success');
    refreshComments();
  } catch (e) {
    showToast(`Submit failed: ${e.message || e}`, 'error');
  } finally {
    setLoading(submitBtn, false);
  }
}

function renderDraftsBar() {
  const bar = document.getElementById('drafts-bar');
  if (!bar) return;
  const list = document.getElementById('drafts-list');
  const count = document.getElementById('drafts-count');
  const submitBtn = document.getElementById('drafts-submit');
  if (drafts.length === 0) {
    bar.hidden = true;
    return;
  }
  bar.hidden = false;
  count.textContent = `${drafts.length} draft${drafts.length === 1 ? '' : 's'} staged`;
  submitBtn.textContent = `Submit all ${drafts.length}`;
  list.innerHTML = '';
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
    del.addEventListener('click', () => removeDraft(d.cid));
    row.appendChild(del);
    list.appendChild(row);
  }
}

function truncate(s, n) {
  if (!s) return '';
  return s.length > n ? s.slice(0, n - 1) + '…' : s;
}

document.getElementById('draft-cancel').addEventListener('click', () => {
  pendingDraft = null;
  document.getElementById('draft').hidden = true;
});

document.getElementById('draft-submit').addEventListener('click', submitDraft);
document.getElementById('draft-body').addEventListener('keydown', e => {
  if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
    e.preventDefault();
    submitDraft();
  } else if (e.key === 'Escape') {
    e.preventDefault();
    document.getElementById('draft-cancel').click();
  }
});

// Stage the composer's contents as a draft. Doesn't POST — the agent only sees
// comments after "Submit all" in the drafts bar. See `addToBatch` for the why.
function submitDraft() {
  if (!pendingDraft) return;
  const body = document.getElementById('draft-body').value.trim();
  if (!body) return;
  // Persist authorInput so the legacy un-OAuth flow keeps its name across reloads.
  const author = (authorInput.value.trim() || 'anonymous');
  localStorage.setItem('blueprint:author', author);
  addToBatch({ body, selector: pendingDraft.selector });
  pendingDraft = null;
  document.getElementById('draft').hidden = true;
  showToast('Draft staged — submit when ready', 'info');
}

document.getElementById('finish-btn').addEventListener('click', async (e) => {
  if (drafts.length > 0) {
    const n = drafts.length;
    const yes = confirm(
      `You have ${n} unsent draft${n === 1 ? '' : 's'}. ` +
        `Finish anyway? (Drafts will be kept locally; they're not lost.)`
    );
    if (!yes) return;
  }
  const btn = e.currentTarget;
  setLoading(btn, true);
  try {
    const r = await fetch(`/api/blueprints/${slug}/finish`, { method: 'POST' });
    if (r.ok) showToast('Marked review complete', 'success');
    else showToast(`Finish failed: ${r.status}`, 'error');
  } finally {
    setLoading(btn, false);
  }
});

async function refreshComments() {
  const r = await fetch(`/api/blueprints/${slug}/comments`);
  if (!r.ok) return;
  const { comments, server_ts, blueprint_version, batch_processing } = await r.json();
  lastTs = server_ts;
  allComments = comments;
  renderBatchIndicator(batch_processing ?? null);
  if (lastBlueprintVersion === null) lastBlueprintVersion = blueprint_version;
  // Seed auto-scroll bookkeeping so the first poll doesn't mistake existing
  // comments for new arrivals.
  prevTopIds = comments.filter(c => !c.parent_id).map(c => c.id);
  prevProcessing = new Map();
  for (const c of comments) prevProcessing.set(c.id, c.processing_by || null);
  applyHighlights();
  renderSidebar();
}

async function pollOnce() {
  try {
    // Full-fetch every poll (no `since=` filter) so that state changes on existing
    // comments — processing flags going on/off, resolve toggles — actually propagate.
    const r = await fetch(`/api/blueprints/${slug}/comments`);
    if (!r.ok) return;
    const { comments, server_ts, blueprint_version, batch_processing } = await r.json();
    lastTs = server_ts;
    renderBatchIndicator(batch_processing ?? null);

    // If the server has a newer plan version than what we've loaded, show a banner
    // instead of auto-reloading (preserves scroll position and reading state).
    // The first poll might race with refreshComments() — if lastBlueprintVersion is still
    // null, treat this poll as the baseline-setter rather than a "newer version" trigger.
    const effectiveLoaded = pendingBlueprintVersion ?? lastBlueprintVersion;
    if (effectiveLoaded === null) {
      lastBlueprintVersion = blueprint_version;
    } else if (blueprint_version !== effectiveLoaded) {
      pendingBlueprintVersion = blueprint_version;
      pendingUpdateCount += 1;
      showUpdateBanner();
    }

    // Skip re-render when nothing changed. Re-renders destroy input fields, so a
    // user mid-typing in a reply input loses focus on every poll without this guard.
    if (commentsEqual(allComments, comments)) return;

    // Diff for auto-scroll decisions BEFORE we mutate allComments.
    const scrollList = document.getElementById('comments-list');
    const wasNearBottom = scrollList
      ? (scrollList.scrollHeight - scrollList.scrollTop - scrollList.clientHeight) < 80
      : false;
    const newTopIds = comments.filter(c => !c.parent_id).map(c => c.id);
    const addedTops = newTopIds.filter(id => !prevTopIds.includes(id));
    // Find a comment whose "Claude is replying" flag JUST cleared and now has a new reply.
    let claudeRepliedTo = null;
    const newReplyByParent = new Map();
    for (const c of comments) {
      if (c.parent_id && !prevTopIds.concat([...prevProcessing.keys()]).includes(c.id)) {
        newReplyByParent.set(c.parent_id, c.id);
      }
    }
    for (const c of comments) {
      const wasProcessing = prevProcessing.get(c.id);
      const isProcessing = c.processing_by || null;
      if (wasProcessing && !isProcessing && newReplyByParent.has(c.id)) {
        claudeRepliedTo = c.id;
        break;
      }
    }

    // Genuine change — re-render, but preserve any focused input + its cursor.
    preserveFocus(() => {
      allComments = comments;
      applyHighlights();
      renderSidebar();
    });

    // Auto-scroll (Stage 4c). Two cases:
    //   1. Claude just answered → always scroll the parent into view.
    //   2. New top-level comment arrived while we were tracking the bottom → scroll.
    if (claudeRepliedTo) {
      scrollSidebarToComment(claudeRepliedTo);
    } else if (addedTops.length > 0 && wasNearBottom) {
      scrollSidebarToComment(addedTops[addedTops.length - 1]);
    }

    // Update bookkeeping.
    prevTopIds = newTopIds;
    prevProcessing = new Map();
    for (const c of comments) {
      prevProcessing.set(c.id, c.processing_by || null);
    }
  } catch (e) {
    // network blip; ignore
  }
}

function scrollSidebarToComment(commentId) {
  const c = allComments.find(x => x.id === commentId);
  if (!c) return;
  // Walk to the top-level ancestor — that's the .group we render.
  let head = c;
  while (head.parent_id) {
    const parent = allComments.find(x => x.id === head.parent_id);
    if (!parent) break;
    head = parent;
  }
  const quote = head.selector ? head.selector.exact : null;
  if (!quote) return;
  const list = document.getElementById('comments-list');
  if (!list) return;
  const group = list.querySelector(`.group[data-quote="${CSS.escape(quote)}"]`);
  if (!group) return;
  group.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
}

function commentsEqual(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    const x = a[i], y = b[i];
    if (x.id !== y.id) return false;
    if (x.body !== y.body) return false;
    if (x.resolved !== y.resolved) return false;
    if ((x.processing_by ?? null) !== (y.processing_by ?? null)) return false;
    if ((x.processing_started_at ?? null) !== (y.processing_started_at ?? null)) return false;
    if ((x.author_avatar_url ?? null) !== (y.author_avatar_url ?? null)) return false;
  }
  return true;
}

// Re-renders destroy the focused element. Capture identity + value + caret first,
// run the re-render, then restore on whatever DOM node took the destroyed one's
// place (matched by id or by `data-reply-for=<comment-id>` for per-comment inputs).
function preserveFocus(fn) {
  const focused = document.activeElement;
  const inSidebar = focused && focused.matches('input, textarea')
    && focused.closest('.sidebar, #draft, .comments');
  if (!inSidebar) {
    fn();
    return;
  }
  const snap = {
    id: focused.id || null,
    replyFor: focused.dataset.replyFor || null,
    value: focused.value,
    start: focused.selectionStart,
    end: focused.selectionEnd,
  };
  fn();
  let target = null;
  if (snap.id) target = document.getElementById(snap.id);
  if (!target && snap.replyFor) {
    target = document.querySelector(`[data-reply-for="${CSS.escape(snap.replyFor)}"]`);
  }
  if (target) {
    if ('value' in target) target.value = snap.value;
    target.focus();
    try { target.setSelectionRange(snap.start, snap.end); } catch (_) {}
  }
}

function reloadPlan() {
  blueprintFrame.src = `/api/blueprints/${slug}/raw?v=${lastBlueprintVersion}`;
}

function showUpdateBanner() {
  const banner = document.getElementById('update-banner');
  if (!banner) return;
  const msg = banner.querySelector('.msg');
  msg.textContent = pendingUpdateCount === 1
    ? 'Plan updated'
    : `Plan updated · ${pendingUpdateCount} changes`;
  banner.hidden = false;
}

function hideUpdateBanner() {
  const banner = document.getElementById('update-banner');
  if (banner) banner.hidden = true;
}

function acceptPendingUpdate() {
  // Always hide and reload — even if state is wonky, the user clicked
  // a "Refresh" button and expects something to happen.
  if (pendingBlueprintVersion !== null) {
    lastBlueprintVersion = pendingBlueprintVersion;
    pendingBlueprintVersion = null;
  }
  pendingUpdateCount = 0;
  hideUpdateBanner();
  reloadPlan();
}

document.getElementById('apply-update').addEventListener('click', acceptPendingUpdate);
document.getElementById('dismiss-update').addEventListener('click', () => {
  // Keep pendingBlueprintVersion set so we still treat the iframe as stale for anchoring,
  // and so the next update bumps the count when the banner re-appears.
  hideUpdateBanner();
});

function startPolling() {
  if (pollTimer) clearInterval(pollTimer);
  pollTimer = setInterval(pollOnce, 1500);
}

function renderSidebar() {
  const list = document.getElementById('comments-list');
  list.innerHTML = '';
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
  const isThreadResolved = (cs) => cs.length > 0 && cs[0].resolved;
  const visibleQuotes = [];
  let resolvedCount = 0;
  for (const [quote, cs] of byQuote) {
    if (isThreadResolved(cs)) {
      resolvedCount++;
      if (!showResolved) continue;
    }
    visibleQuotes.push([quote, cs]);
  }

  // Update the sidebar header count (total comments incl replies). Numbers
  // get a span.n so the type scale can emphasize them while keeping the unit
  // labels muted.
  const countEl = document.getElementById('comments-count');
  if (countEl) {
    const total = allComments.length;
    const threadCount = byQuote.size;
    if (total === 0) {
      countEl.textContent = 'no comments';
    } else {
      countEl.innerHTML = `<span class="n">${threadCount}</span> thread${threadCount === 1 ? '' : 's'} · <span class="n">${total}</span> comment${total === 1 ? '' : 's'}`;
    }
  }
  // Maintain a "show/hide resolved" toggle when there's anything to toggle.
  syncResolvedToggle(resolvedCount);

  if (visibleQuotes.length === 0) {
    list.appendChild(renderEmptyState(byQuote.size === 0 ? 'cold' : 'all-resolved'));
    // Drop j/k focus when there's nothing to focus.
    focusedThreadIdx = -1;
    return;
  }

  // Clamp focus index to the visible set.
  if (focusedThreadIdx >= visibleQuotes.length) focusedThreadIdx = visibleQuotes.length - 1;
  if (focusedThreadIdx < -1) focusedThreadIdx = -1;

  let visibleIdx = 0;
  for (const [quote, cs] of visibleQuotes) {
    const resolved = isThreadResolved(cs);
    const headId = cs[0].id;
    const isPending = pendingInNewVersion.has(quote);
    const isDrifted = drifted.has(quote);
    // Resolved threads default to collapsed (overridable per-thread).
    const isCollapsed = collapsedQuotes.has(quote) || (resolved && !collapsedQuotes.has('!' + quote));
    const group = document.createElement('div');
    group.className = 'group'
      + (isPending ? ' pending' : isDrifted ? ' drifted' : '')
      + (resolved ? ' resolved' : '')
      + (isCollapsed ? ' collapsed' : '')
      + (visibleIdx === focusedThreadIdx ? ' focused' : '');
    group.dataset.quote = quote;
    group.dataset.visibleIdx = String(visibleIdx);
    visibleIdx++;
    const q = document.createElement('div');
    q.className = 'quote';
    q.textContent = `"${quote}"`;
    // Click the quote bar to toggle collapse.
    q.addEventListener('click', e => {
      e.stopPropagation();
      if (resolved) {
        // Resolved threads use the opposite default: clicking expands them.
        if (collapsedQuotes.has('!' + quote)) collapsedQuotes.delete('!' + quote);
        else collapsedQuotes.add('!' + quote);
      } else {
        if (collapsedQuotes.has(quote)) collapsedQuotes.delete(quote);
        else collapsedQuotes.add(quote);
      }
      saveCollapsed();
      renderSidebar();
    });
    // Click elsewhere in the group: scroll to highlight, or auto-refresh if pending.
    group.addEventListener('click', e => {
      if (e.target.closest('input, textarea, button, a')) return;
      if (e.target.closest('.quote')) return;  // handled by quote toggle
      if (isPending) {
        acceptPendingUpdate();
        return;
      }
      if (!isDrifted) scrollFrameToQuote(quote);
    });
    if (resolved) {
      const tag = document.createElement('span');
      tag.className = 'resolved-tag';
      tag.textContent = 'resolved';
      q.appendChild(tag);
    } else if (isPending) {
      const tag = document.createElement('span');
      tag.className = 'pending-tag';
      tag.textContent = 'refresh to see';
      q.appendChild(tag);
    } else if (isDrifted) {
      const tag = document.createElement('span');
      tag.className = 'drift-tag';
      tag.textContent = 'drifted';
      q.appendChild(tag);
    }
    // Reply count badge (only visible when collapsed via CSS).
    const replyCount = cs.reduce((n, c) => n + countAllReplies(c.id, byParent), 0);
    const threadSize = cs.length + replyCount;
    const badge = document.createElement('span');
    badge.className = 'collapse-count';
    badge.textContent = `${threadSize} message${threadSize === 1 ? '' : 's'}`;
    q.appendChild(badge);
    // Resolve link in the quote bar (only on un-resolved threads).
    if (!resolved && !isPending && !isDrifted) {
      const resolveBtn = document.createElement('a');
      resolveBtn.href = '#';
      resolveBtn.className = 'resolve-link';
      resolveBtn.textContent = 'resolve';
      resolveBtn.addEventListener('click', async e => {
        e.preventDefault();
        e.stopPropagation();
        await setResolved(headId, true);
      });
      q.appendChild(resolveBtn);
    } else if (resolved) {
      const resolveBtn = document.createElement('a');
      resolveBtn.href = '#';
      resolveBtn.className = 'resolve-link';
      resolveBtn.textContent = 'reopen';
      resolveBtn.addEventListener('click', async e => {
        e.preventDefault();
        e.stopPropagation();
        await setResolved(headId, false);
      });
      q.appendChild(resolveBtn);
    }
    group.appendChild(q);
    for (const c of cs) {
      group.appendChild(renderComment(c, byParent.get(c.id) || [], byParent));
    }
    list.appendChild(group);
  }
}

function renderEmptyState(kind) {
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
  body.innerHTML = kind === 'cold'
    ? 'Select any text in the plan and press <kbd>⌘+Enter</kbd> to leave the first one.'
    : 'Toggle <em>show resolved</em> above to see them again.';
  wrap.appendChild(body);
  return wrap;
}

async function setResolved(commentId, resolved) {
  const r = await fetch(`/api/blueprints/${slug}/comments/${commentId}/resolve`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ resolved }),
  });
  if (r.ok) refreshComments();
}

function syncResolvedToggle(resolvedCount) {
  const header = document.querySelector('.sidebar-header');
  if (!header) return;
  let toggle = header.querySelector('#toggle-resolved');
  if (resolvedCount === 0) {
    if (toggle) toggle.remove();
    return;
  }
  if (!toggle) {
    toggle = document.createElement('a');
    toggle.id = 'toggle-resolved';
    toggle.href = '#';
    toggle.style.marginLeft = '4px';
    toggle.addEventListener('click', e => {
      e.preventDefault();
      showResolved = !showResolved;
      saveShowResolved();
      renderSidebar();
    });
    const sep = document.createElement('span');
    sep.className = 'sep';
    sep.textContent = '·';
    header.appendChild(sep);
    header.appendChild(toggle);
  }
  toggle.textContent = showResolved
    ? `hide resolved (${resolvedCount})`
    : `show resolved (${resolvedCount})`;
}

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

// Live-tick timestamps: every 30s, walk all [data-ts] elements and refresh.
setInterval(() => {
  for (const el of document.querySelectorAll('[data-ts]')) {
    const t = Number(el.dataset.ts);
    if (Number.isFinite(t) && t > 0) el.textContent = timeAgo(t);
  }
}, 30 * 1000);

function countAllReplies(id, byParent) {
  const direct = byParent.get(id) || [];
  return direct.length + direct.reduce((n, r) => n + countAllReplies(r.id, byParent), 0);
}

function scrollFrameToQuote(quote) {
  // Auto-expand the matching sidebar group if it's collapsed — the user clearly wants to see it.
  if (collapsedQuotes.has(quote)) {
    collapsedQuotes.delete(quote);
    saveCollapsed();
    renderSidebar();
  }
  const doc = blueprintFrame.contentDocument;
  if (!doc) return;
  const spans = doc.querySelectorAll(`span[data-ps-hl][data-ps-quote="${CSS.escape(quote)}"]`);
  if (spans.length === 0) return;
  spans[0].scrollIntoView({ behavior: 'smooth', block: 'center' });
  spans.forEach(s => {
    s.classList.remove('ps-hl-active');
    void s.offsetWidth;
    s.classList.add('ps-hl-active');
    setTimeout(() => s.classList.remove('ps-hl-active'), 1500);
  });
}

/* The rotating ✦ sparkle used by both the per-comment "X is replying" pill
   in renderComment and the slug-level batch-processing indicator. SVG is
   identical in both spots; consolidating prevents the two from drifting. */
function makeSparkleSvg() {
  const sparkle = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  sparkle.setAttribute('class', 'sparkle sparkle--spin');
  sparkle.setAttribute('viewBox', '0 0 24 24');
  sparkle.setAttribute('aria-hidden', 'true');
  const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  path.setAttribute('d', 'M 12 2 L 13.5 10.5 L 22 12 L 13.5 13.5 L 12 22 L 10.5 13.5 L 2 12 L 10.5 10.5 Z');
  sparkle.appendChild(path);
  return sparkle;
}

// Last rendered batch_processing payload, kept as a JSON-comparable signature
// so the 1.5s sidebar poll doesn't tear down + rebuild the indicator when
// nothing actually changed.
let lastBatchProcessingKey = null;

/* Slug-level "Claude is working on N comments" pill — server `batch_processing`
   field, set by `blueprint batch-processing start` and cleared on the last
   reply or after PROCESSING_TTL_MS. */
function renderBatchIndicator(bp) {
  const el = document.getElementById('batch-indicator');
  if (!el) return;
  const active = bp && (Date.now() - bp.started_at) < PROCESSING_TTL_MS;
  const key = active ? `${bp.author}|${bp.count}|${bp.started_at}` : null;
  if (key === lastBatchProcessingKey) return;
  lastBatchProcessingKey = key;
  if (!active) {
    el.hidden = true;
    el.innerHTML = '';
    return;
  }
  const noun = bp.count === 1 ? 'comment' : 'comments';
  el.innerHTML = '';
  el.appendChild(makeSparkleSvg());
  const msg = document.createElement('span');
  msg.innerHTML = `<strong>${escapeHtml(bp.author)}</strong> is working on ${bp.count} ${noun}`;
  el.appendChild(msg);
  el.hidden = false;
}

function renderComment(c, replies, byParent) {
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
    lastBlueprintVersion != null &&
    c.blueprint_version < lastBlueprintVersion
  ) {
    const vb = document.createElement('a');
    vb.className = 'version-badge';
    vb.textContent = `on v${c.blueprint_version}`;
    vb.href = `/api/blueprints/${slug}/raw?version=${c.blueprint_version}`;
    vb.target = '_blank';
    vb.rel = 'noopener';
    vb.title = `Authored against version ${c.blueprint_version}; you're viewing v${lastBlueprintVersion}. Open that snapshot in a new tab.`;
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
      msg.innerHTML = `<strong>${escapeHtml(c.processing_by)}</strong> is replying`;
      working.appendChild(msg);
      working.appendChild(makeSparkleSvg());
      wrap.appendChild(working);
    }
  }

  for (const r of replies) {
    const rep = document.createElement('div');
    rep.className = 'reply';
    rep.appendChild(renderComment(r, byParent.get(r.id) || [], byParent));
    wrap.appendChild(rep);
  }
  const form = document.createElement('div');
  form.className = 'reply-form';
  const inp = document.createElement('input');
  inp.placeholder = 'stage reply…';
  inp.dataset.replyFor = c.id;  // preserveFocus uses this to restore typing across re-renders
  const btn = document.createElement('button');
  btn.textContent = 'Add to batch';
  btn.addEventListener('click', () => {
    const replyBody = inp.value.trim();
    if (!replyBody) return;
    addToBatch({
      body: replyBody,
      parentId: c.id,
      parentBody: c.body,
    });
    inp.value = '';
    showToast('Reply staged — submit when ready', 'info');
  });
  inp.addEventListener('keydown', e => {
    if (e.key === 'Enter') btn.click();
  });
  form.appendChild(inp);
  form.appendChild(btn);
  wrap.appendChild(form);
  return wrap;
}

function timeAgo(ms) {
  const dt = (Date.now() - ms) / 1000;
  if (dt < 60) return Math.max(1, Math.floor(dt)) + 's ago';
  if (dt < 3600) return Math.floor(dt / 60) + 'm ago';
  if (dt < 86400) return Math.floor(dt / 3600) + 'h ago';
  return Math.floor(dt / 86400) + 'd ago';
}

function applyHighlights() {
  const doc = blueprintFrame.contentDocument;
  if (!doc) return;
  doc.querySelectorAll('span[data-ps-hl]').forEach(span => {
    const parent = span.parentNode;
    while (span.firstChild) parent.insertBefore(span.firstChild, span);
    parent.removeChild(span);
    parent.normalize();
  });
  drifted = new Set();
  pendingInNewVersion = new Set();
  const tops = allComments.filter(c => !c.parent_id);
  const seen = new Set();
  const hasPendingUpdate = pendingBlueprintVersion !== null;
  for (const c of tops) {
    const q = c.selector.exact;
    if (seen.has(q)) continue;
    seen.add(q);
    if (!highlightQuote(doc, c.selector, q)) {
      // If there's a pending plan update, the anchor may exist in the next version
      // the user hasn't loaded yet. Render distinctly so they don't mistake it for drift.
      if (hasPendingUpdate) {
        pendingInNewVersion.add(q);
      } else {
        drifted.add(q);
      }
    }
  }
}

function highlightQuote(doc, selector, quote) {
  const root = doc.body;
  if (!root) return false;
  const text = wholeText(root);
  let bestIdx = -1;
  let searchFrom = 0;
  while (true) {
    const found = text.indexOf(quote, searchFrom);
    if (found === -1) break;
    const before = text.slice(Math.max(0, found - 32), found);
    const after = text.slice(found + quote.length, found + quote.length + 32);
    const prefixOK = !selector.prefix || before.endsWith(selector.prefix.slice(-Math.min(32, selector.prefix.length)));
    const suffixOK = !selector.suffix || after.startsWith(selector.suffix.slice(0, Math.min(32, selector.suffix.length)));
    if (prefixOK && suffixOK) {
      bestIdx = found;
      break;
    }
    searchFrom = found + 1;
  }
  if (bestIdx === -1) bestIdx = text.indexOf(quote);
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
    wrapRange(range, doc, quote);
    return true;
  } catch (e) {
    return false;
  }
}

function wrapRange(range, doc, quote) {
  if (range.startContainer === range.endContainer && range.startContainer.nodeType === Node.TEXT_NODE) {
    const span = mkHighlight(doc, quote);
    try {
      range.surroundContents(span);
      return;
    } catch (e) {
      // fall through to multi-node path
    }
  }
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
    const midSpan = mkHighlight(doc, quote);
    midSpan.textContent = mid;
    const afterNode = doc.createTextNode(after);
    parent.insertBefore(beforeNode, node);
    parent.insertBefore(midSpan, node);
    parent.insertBefore(afterNode, node);
    parent.removeChild(node);
  }
}

function mkHighlight(doc, quote) {
  const span = doc.createElement('span');
  span.setAttribute('data-ps-hl', '1');
  span.dataset.psQuote = quote;
  span.title = `(annotated) ${quote} — click to see comments`;
  span.addEventListener('click', e => {
    e.stopPropagation();
    focusQuote(quote);
  });
  return span;
}

function focusQuote(quote) {
  // Auto-expand the matching group so the comment is visible after scrolling.
  if (collapsedQuotes.has(quote)) {
    collapsedQuotes.delete(quote);
    saveCollapsed();
    renderSidebar();
  }
  const list = document.getElementById('comments-list');
  const g = list.querySelector(`.group[data-quote="${CSS.escape(quote)}"]`);
  if (!g) return;
  g.scrollIntoView({ behavior: 'smooth', block: 'center' });
  g.classList.remove('flash');
  // force reflow so the animation restarts even on repeat clicks
  void g.offsetWidth;
  g.classList.add('flash');

  // Pulse the highlight spans in the iframe too
  const doc = blueprintFrame.contentDocument;
  if (doc) {
    const spans = doc.querySelectorAll(`span[data-ps-hl][data-ps-quote="${CSS.escape(quote)}"]`);
    spans.forEach(s => {
      s.classList.remove('ps-hl-active');
      void s.offsetWidth;
      s.classList.add('ps-hl-active');
      setTimeout(() => s.classList.remove('ps-hl-active'), 1500);
    });
  }
}

// setStatus is kept as a thin wrapper for any legacy callers; new code routes
// through showToast directly.
function setStatus(msg, isError = false) {
  showToast(msg, isError ? 'error' : 'info');
}

/* ============================================================
 * Toast notifications (Stage 4a)
 * ============================================================ */
function showToast(msg, kind = 'info') {
  const stack = document.getElementById('toast-stack');
  if (!stack) return;
  const toast = document.createElement('div');
  toast.className = 'toast ' + kind;
  const text = document.createElement('div');
  text.className = 'toast-msg';
  text.textContent = msg;
  toast.appendChild(text);
  const close = document.createElement('button');
  close.className = 'toast-close';
  close.setAttribute('aria-label', 'Dismiss');
  close.textContent = '×';
  close.addEventListener('click', () => dismissToast(toast));
  toast.appendChild(close);
  stack.appendChild(toast);
  // Errors persist until dismissed; others auto-dismiss.
  if (kind !== 'error') {
    setTimeout(() => dismissToast(toast), 4000);
  }
}
function dismissToast(toast) {
  if (!toast || !toast.parentNode) return;
  toast.classList.add('exiting');
  toast.addEventListener('animationend', () => {
    if (toast.parentNode) toast.parentNode.removeChild(toast);
  }, { once: true });
}

/* ============================================================
 * Loading state for buttons (Stage 4b)
 * ============================================================ */
function setLoading(btn, on) {
  if (!btn) return;
  if (on) {
    btn.dataset.loading = '1';
    btn.disabled = true;
  } else {
    delete btn.dataset.loading;
    btn.disabled = false;
  }
}

/* ============================================================
 * Shortcuts dialog + global keyboard handler (Stages 4e, 4g, 4h)
 * ============================================================ */
function bindShortcuts() {
  const dlg = document.getElementById('shortcuts-dialog');
  const helpBtn = document.getElementById('help-btn');
  const closeBtn = document.getElementById('shortcuts-close');
  if (helpBtn && dlg) {
    helpBtn.addEventListener('click', () => openShortcuts());
  }
  if (closeBtn && dlg) {
    closeBtn.addEventListener('click', () => dlg.close());
  }
  document.addEventListener('keydown', onGlobalKeydown);
}
function openShortcuts() {
  const dlg = document.getElementById('shortcuts-dialog');
  if (!dlg) return;
  if (typeof dlg.showModal === 'function' && !dlg.open) dlg.showModal();
  else dlg.setAttribute('open', '');
}
function isTypingTarget(el) {
  if (!el) return false;
  const tag = el.tagName;
  return tag === 'INPUT' || tag === 'TEXTAREA' || el.isContentEditable;
}
function onGlobalKeydown(e) {
  if (isTypingTarget(e.target)) return;
  // Don't fight modifier-based shortcuts (Ctrl+R reload, etc).
  if (e.metaKey || e.ctrlKey || e.altKey) return;
  // Skip when the shortcuts dialog is open — its native Esc handler runs.
  const dlg = document.getElementById('shortcuts-dialog');
  if (dlg && dlg.open) return;
  switch (e.key) {
    case '?':
      e.preventDefault();
      openShortcuts();
      break;
    case 'j':
      e.preventDefault();
      moveFocusedThread(1);
      break;
    case 'k':
      e.preventDefault();
      moveFocusedThread(-1);
      break;
    case 'r':
      e.preventDefault();
      resolveFocusedThread();
      break;
    case 'e':
      e.preventDefault();
      collapsedQuotes.clear();
      saveCollapsed();
      renderSidebar();
      break;
    case 'c':
      e.preventDefault();
      for (const c of allComments) if (!c.parent_id) collapsedQuotes.add(c.selector.exact);
      saveCollapsed();
      renderSidebar();
      break;
  }
}
function moveFocusedThread(delta) {
  const list = document.getElementById('comments-list');
  if (!list) return;
  const groups = list.querySelectorAll('.group');
  if (groups.length === 0) return;
  if (focusedThreadIdx === -1) {
    focusedThreadIdx = delta > 0 ? 0 : groups.length - 1;
  } else {
    focusedThreadIdx = (focusedThreadIdx + delta + groups.length) % groups.length;
  }
  for (const g of groups) g.classList.remove('focused');
  const target = groups[focusedThreadIdx];
  if (target) {
    target.classList.add('focused');
    target.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
  }
}
function resolveFocusedThread() {
  const list = document.getElementById('comments-list');
  if (!list) return;
  const groups = list.querySelectorAll('.group');
  const target = groups[focusedThreadIdx];
  if (!target) return;
  const quote = target.dataset.quote;
  const head = allComments.find(c => !c.parent_id && c.selector && c.selector.exact === quote);
  if (!head) return;
  setResolved(head.id, !head.resolved);
}
