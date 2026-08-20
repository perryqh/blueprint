// blueprint reviewer — vanilla JS, no deps, no build step.
// Anchors comments to text via TextQuoteSelector (exact + prefix + suffix).
//
// Loaded as `<script type="module">`; the pieces with a testable core live in
// sibling modules (anchor.js, poll.js, drafts.js, render.js) and this file is
// the wiring: DOM lookups, app state, and the delegated event handlers.

import {
  initAnchoring,
  captureSelector,
  highlightQuote,
  clearHighlights,
} from './anchor.js';
import {
  createPoller,
  commentsEqual,
} from './poll.js';
import {
  createDraftStore,
  renderDraftsBar,
} from './drafts.js';
import {
  renderEmptyState,
  renderCount,
  renderBatchIndicator,
  renderGroup,
  groupComments,
} from './render.js';
import { showToast, setLoading } from './toast.js';
import { makeAvatar, scrollBehavior, timeAgo, PROCESSING_TTL_MS } from './dom.js';

const slug = location.pathname.replace(/^\/b\//, '').split('/')[0];
document.getElementById('slug-display').textContent = slug;

let me = null;
let allComments = [];
let pendingDraft = null;
let lastTs = 0;
let drifted = new Set();
// Quotes that resolved, but only by falling back to the first occurrence because
// no occurrence agreed with the recorded context — so the highlight is probably
// on the wrong paragraph. Kept separate from `drifted`: the text is still there,
// which is a different thing to tell the reviewer.
let misanchored = new Set();
let pendingInNewVersion = new Set();
let lastBlueprintVersion = null;
let pendingBlueprintVersion = null;
let pendingUpdateCount = 0;
// Auto-scroll bookkeeping (Stage 4c)
let prevTopIds = new Set();
let prevProcessing = new Map(); // commentId → processing_by (or null)
let focusedThreadIdx = -1;      // j/k navigation
// Monotonic request id. Every /comments response carries the id it was issued
// with; a response older than the newest one we've already applied is dropped
// rather than allowed to overwrite `allComments`/`lastTs` with stale data.
let requestSeq = 0;
let appliedSeq = 0;
// Last rendered batch_processing payload, kept as a JSON-comparable signature
// so the sidebar poll doesn't tear down + rebuild the indicator when nothing
// actually changed.
let lastBatchProcessingKey = null;
// Last finish stamp rendered on the button, so repeat polls don't rebuild it.
let lastFinishedAt = null;

const COLLAPSED_KEY = 'blueprint:collapsed:' + slug;
const SHOW_RESOLVED_KEY = 'blueprint:show-resolved:' + slug;
let collapsedQuotes = new Set(JSON.parse(localStorage.getItem(COLLAPSED_KEY) || '[]'));
let showResolved = localStorage.getItem(SHOW_RESOLVED_KEY) === '1';

function saveCollapsed() {
  localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...collapsedQuotes]));
}

function saveShowResolved() {
  localStorage.setItem(SHOW_RESOLVED_KEY, showResolved ? '1' : '0');
}

const authorInput = document.getElementById('author');
authorInput.value = localStorage.getItem('blueprint:author') || '';
authorInput.addEventListener('change', e => {
  localStorage.setItem('blueprint:author', e.target.value);
});

const blueprintFrame = document.getElementById('blueprint-frame');
blueprintFrame.src = `/api/blueprints/${slug}/raw`;

const drafts = createDraftStore({
  slug,
  identity: () => me,
  onChange: () => renderDraftsBar(drafts),
});

/* ------------------------------------------------------------
 * Identity
 * ------------------------------------------------------------ */

async function refreshMe() {
  // A transient network failure must NOT downgrade a signed-in owner to
  // anonymous: `draftsKey()` is scoped by login, so flipping to `anon` hides
  // the user's staged drafts and re-persists them under the wrong key. Only a
  // clean 401/403 is proof of being logged out; anything else means "couldn't
  // tell", and we keep the identity we already had.
  let next = me;
  try {
    const r = await fetch('/api/me');
    if (r.ok) {
      next = await r.json();
    } else if (r.status === 401 || r.status === 403) {
      next = null;
    }
    // Any other status (500, 502, a proxy error page) leaves `next` untouched.
  } catch (_) {
    // Network error — same reasoning, hold the previous identity.
  }
  me = next;
  renderAuthChip(me);
  // Drafts are scoped by user (Q2), so reload under the now-known identity.
  drafts.load();
  renderDraftsBar(drafts);
}

function renderAuthChip(user) {
  const chip = document.getElementById('auth-chip');
  const legacyAuthorField = document.getElementById('author-field');
  if (!chip) return;
  chip.textContent = '';
  chip.hidden = false;
  if (user) {
    if (legacyAuthorField) legacyAuthorField.hidden = true;
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
    const out = document.createElement('button');
    out.type = 'button';
    out.className = 'auth-chip-out';
    out.textContent = 'sign out';
    out.addEventListener('click', async () => {
      await fetch('/logout', { method: 'POST' });
      me = null;
      renderAuthChip(null);
      renderSidebar();
    });
    chip.appendChild(out);
  } else {
    if (legacyAuthorField) legacyAuthorField.hidden = false;
    const btn = document.createElement('a');
    btn.href = '/login';
    btn.className = 'login-btn';
    btn.textContent = 'Sign in with GitHub';
    chip.appendChild(btn);
  }
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
  reinjectFrameStyles();
}
function reinjectFrameStyles() {
  const doc = blueprintFrame.contentDocument;
  if (!doc) return;
  const existing = doc.getElementById('ps-injected-styles');
  if (existing) existing.remove();
  injectFrameStyles(doc);
}
function bindThemeToggle() {
  const initial = currentTheme();
  for (const btn of document.querySelectorAll('.theme-toggle button')) {
    btn.setAttribute('aria-checked', btn.dataset.themeValue === initial ? 'true' : 'false');
    btn.addEventListener('click', () => applyTheme(btn.dataset.themeValue));
  }
  // Live-react to OS theme changes when set to System.
  matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
    if (currentTheme() === 'system') reinjectFrameStyles();
  });
}

// Defensive null-checks so an older cached reviewer.html (without these elements)
// doesn't throw and abort the rest of the script.
const collapseAllEl = document.getElementById('collapse-all');
if (collapseAllEl) {
  collapseAllEl.addEventListener('click', e => {
    e.preventDefault();
    collapseAll();
  });
}
const expandAllEl = document.getElementById('expand-all');
if (expandAllEl) {
  expandAllEl.addEventListener('click', e => {
    e.preventDefault();
    expandAll();
  });
}

function collapseAll() {
  for (const c of allComments) if (!c.parent_id) collapsedQuotes.add(c.selector.exact);
  saveCollapsed();
  renderSidebar();
}

function expandAll() {
  collapsedQuotes.clear();
  saveCollapsed();
  renderSidebar();
}

// Kicked off at module load so the wasm fetch overlaps the iframe's own load
// rather than starting after it.
const anchoringReady = initAnchoring().catch(e => {
  // Without the module nothing can anchor, and silently rendering every comment
  // as "drifted" would look like the plan changed. Say what actually happened.
  console.error('anchoring module failed to load', e);
  showToast('Anchoring unavailable — highlights are disabled', 'error');
  throw e;
});

blueprintFrame.addEventListener('load', async () => {
  // Resolution runs through wasm, so nothing can highlight until it's
  // instantiated. Everything downstream of the first render waits on this.
  try {
    await anchoringReady;
  } catch {
    return;
  }
  setupFrameListeners();
  refreshComments();
  poller.start();
  loadVersions();
});

// Populate the header version dropdown. Runs on initial load and after each
// update reload (the iframe 'load' fires both times). Historical versions open
// as sandboxed snapshots in a new tab — consistent with the comment "on vN"
// badge — rather than swapping the live review iframe.
async function loadVersions() {
  const menu = document.getElementById('version-menu');
  const summary = document.getElementById('version-current');
  const list = document.getElementById('version-list');
  if (!menu || !summary || !list) return;
  try {
    const r = await fetch(`/api/blueprints/${slug}/versions`);
    if (!r.ok) return;
    const { current, versions } = await r.json();
    summary.textContent = `v${current}`;
    // A dropdown only earns its place once there's history to browse.
    if (!Array.isArray(versions) || versions.length <= 1) {
      menu.hidden = true;
      return;
    }
    menu.hidden = false;
    list.textContent = '';
    for (const v of [...versions].sort((a, b) => b - a)) {
      const a = document.createElement('a');
      a.className = 'version-item' + (v === current ? ' is-current' : '');
      a.href = `/api/blueprints/${slug}/raw?version=${v}`;
      a.target = '_blank';
      a.rel = 'noopener';
      a.textContent = v === current ? `v${v} · current` : `v${v}`;
      a.addEventListener('click', () => { menu.open = false; });
      list.appendChild(a);
    }
  } catch {
    /* dropdown is a convenience; a fetch failure just leaves it hidden */
  }
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
  // also rethemes the plan content if the plan author wrote dark-aware CSS.
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
    span[data-ps-hl]:focus-visible {
      outline: 2px solid ${activeBd} !important;
      outline-offset: 1px;
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
  const body = blueprintFrame.contentDocument.body;
  const selector = captureSelector(sel, body);
  if (selector) showDraft(selector);
}

function showDraft(selector) {
  pendingDraft = { selector };
  document.getElementById('draft-quote').textContent = `"${selector.exact}"`;
  document.getElementById('draft-body').value = '';
  document.getElementById('draft').hidden = false;
  document.getElementById('draft-body').focus();
}

/* ------------------------------------------------------------
 * Drafts bar
 * ------------------------------------------------------------ */

function bindDraftsBar() {
  const submit = document.getElementById('drafts-submit');
  const discard = document.getElementById('drafts-discard');
  const list = document.getElementById('drafts-list');
  if (submit) submit.addEventListener('click', submitAllDrafts);
  if (discard) discard.addEventListener('click', discardAllDrafts);
  // One delegated listener for every tile's discard button — the list is
  // rebuilt wholesale on each change, so per-tile listeners would be churn.
  if (list) {
    list.addEventListener('click', e => {
      const btn = e.target.closest('[data-action="discard-draft"]');
      if (!btn) return;
      drafts.remove(btn.dataset.cid);
    });
  }
  // Surface any persisted drafts immediately (refreshMe will re-render after
  // identity resolves, which may change the key — that's fine, it's idempotent).
  drafts.load();
  renderDraftsBar(drafts);
}

function discardAllDrafts() {
  const n = drafts.count();
  if (n === 0) return;
  if (!confirm(`Discard ${n} draft${n === 1 ? '' : 's'}?`)) return;
  drafts.clear();
}

async function submitAllDrafts() {
  const staged = drafts.all();
  if (staged.length === 0) return;
  const submitBtn = document.getElementById('drafts-submit');
  setLoading(submitBtn, true);
  const author = (me && me.login) || (authorInput.value.trim() || 'anonymous');
  const payload = staged.map(d => {
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
    drafts.clear();
    showToast(`Submitted ${payload.length} comment${payload.length === 1 ? '' : 's'}`, 'success');
    refreshComments();
  } catch (e) {
    showToast(`Submit failed: ${e.message || e}`, 'error');
  } finally {
    setLoading(submitBtn, false);
  }
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
// comments after "Submit all" in the drafts bar. See `drafts.add` for the why.
function submitDraft() {
  if (!pendingDraft) return;
  const body = document.getElementById('draft-body').value.trim();
  if (!body) return;
  // Persist authorInput so the legacy un-OAuth flow keeps its name across reloads.
  const author = (authorInput.value.trim() || 'anonymous');
  localStorage.setItem('blueprint:author', author);
  drafts.add({ body, selector: pendingDraft.selector });
  pendingDraft = null;
  document.getElementById('draft').hidden = true;
  showToast('Draft staged — submit when ready', 'info');
}

document.getElementById('finish-btn').addEventListener('click', async (e) => {
  const n = drafts.count();
  if (n > 0) {
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
    if (r.ok) {
      const { finished_at } = await r.json();
      renderFinishedState(finished_at);
      showToast('Review finished — Claude has been told to wrap up', 'success');
    } else {
      showToast(`Finish failed: ${r.status}`, 'error');
    }
  } finally {
    setLoading(btn, false);
  }
});

// Reflect the server's persisted finish stamp on the button. Called on every
// poll, so the state survives a reload and shows up in a second tab. The button
// stays clickable on purpose: if the reviewer keeps going after finishing,
// they need to be able to end the next round too.
function renderFinishedState(finishedAt) {
  if (finishedAt === lastFinishedAt) return;
  lastFinishedAt = finishedAt;
  const btn = document.getElementById('finish-btn');
  if (!btn) return;
  if (!finishedAt) {
    btn.classList.remove('is-finished');
    btn.textContent = 'Finish Review';
    btn.title = '';
    return;
  }
  const when = new Date(finishedAt).toLocaleTimeString([], {
    hour: 'numeric',
    minute: '2-digit',
  });
  btn.classList.add('is-finished');
  btn.textContent = `Finished ${when}`;
  btn.title = 'Review marked finished. Click again to end another round.';
}

/* ------------------------------------------------------------
 * Comment fetching
 * ------------------------------------------------------------ */

// Single fetch path for both the initial load and the poll. Returns true on a
// successful round-trip so the poller can reset its backoff; throwing or
// returning false both count as a failure.
//
// `seq` guards the writes: two responses can still be in flight across a
// refreshComments()/poll overlap, and the loser must not clobber the winner.
async function fetchComments({ isInitial }) {
  const seq = ++requestSeq;
  // Full-fetch every poll (no `since=` filter) so that state changes on existing
  // comments — processing flags going on/off, resolve toggles — actually propagate.
  const r = await fetch(`/api/blueprints/${slug}/comments`);
  if (!r.ok) return false;
  const payload = await r.json();
  if (seq < appliedSeq) return true;   // a newer response already landed
  appliedSeq = seq;
  applyComments(payload, { isInitial });
  return true;
}

function applyComments(payload, { isInitial }) {
  const { comments, server_ts, blueprint_version, batch_processing, finished_at } = payload;
  lastTs = server_ts;
  syncBatchIndicator(batch_processing ?? null);
  renderFinishedState(finished_at ?? null);

  if (isInitial) {
    allComments = comments;
    if (lastBlueprintVersion === null) lastBlueprintVersion = blueprint_version;
    // Seed auto-scroll bookkeeping so the first poll doesn't mistake existing
    // comments for new arrivals.
    rememberComments(comments);
    applyHighlights();
    renderSidebar();
    return;
  }

  // If the server has a newer plan version than what we've loaded, show a banner
  // instead of auto-reloading (preserves scroll position and reading state).
  // The first poll might race with the initial load — if lastBlueprintVersion is
  // still null, treat this poll as the baseline-setter rather than a
  // "newer version" trigger.
  const effectiveLoaded = pendingBlueprintVersion ?? lastBlueprintVersion;
  if (effectiveLoaded === null || effectiveLoaded === undefined) {
    lastBlueprintVersion = blueprint_version;
  } else if (blueprint_version !== effectiveLoaded) {
    pendingBlueprintVersion = blueprint_version;
    pendingUpdateCount += 1;
    showUpdateBanner();
  }

  // Skip re-render when nothing changed. Cheap guard that also keeps the
  // sidebar's scroll position from being disturbed on every idle poll.
  if (commentsEqual(allComments, comments)) return;

  // Diff for auto-scroll decisions BEFORE we mutate allComments.
  const scrollList = document.getElementById('comments-list');
  const wasNearBottom = scrollList
    ? (scrollList.scrollHeight - scrollList.scrollTop - scrollList.clientHeight) < 80
    : false;
  const newTopIds = comments.filter(c => !c.parent_id).map(c => c.id);
  const addedTops = newTopIds.filter(id => !prevTopIds.has(id));
  // Find a comment whose "Claude is replying" flag JUST cleared and now has a new reply.
  let claudeRepliedTo = null;
  const newReplyByParent = new Map();
  for (const c of comments) {
    // `prevProcessing` is keyed by every id we saw last round (not just tops),
    // so a miss there means "this comment is new" — no array building needed.
    if (c.parent_id && !prevProcessing.has(c.id)) {
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

  allComments = comments;
  applyHighlights();
  renderSidebar();

  // Auto-scroll (Stage 4c). Two cases:
  //   1. Claude just answered → always scroll the parent into view.
  //   2. New top-level comment arrived while we were tracking the bottom → scroll.
  if (claudeRepliedTo) {
    scrollSidebarToComment(claudeRepliedTo);
  } else if (addedTops.length > 0 && wasNearBottom) {
    scrollSidebarToComment(addedTops[addedTops.length - 1]);
  }

  rememberComments(comments);
}

function rememberComments(comments) {
  prevTopIds = new Set();
  prevProcessing = new Map();
  for (const c of comments) {
    if (!c.parent_id) prevTopIds.add(c.id);
    prevProcessing.set(c.id, c.processing_by || null);
  }
}

async function refreshComments() {
  try {
    await fetchComments({ isInitial: true });
  } catch (_) {
    /* the poller owns failure reporting; a failed initial load just retries */
  }
}

const poller = createPoller({
  fetchOnce: () => fetchComments({ isInitial: false }),
});

// A backgrounded tab used to keep fetching the full comment list every 1.5s
// forever (2,400 requests/hour) with nobody looking at it. Stop while hidden,
// and poll immediately on return since the view is as stale as the absence.
document.addEventListener('visibilitychange', () => poller.onVisibilityChange());

function syncBatchIndicator(bp) {
  const el = document.getElementById('batch-indicator');
  const active = bp && (Date.now() - bp.started_at) < PROCESSING_TTL_MS;
  const key = active ? `${bp.author}|${bp.count}|${bp.started_at}` : null;
  if (key === lastBatchProcessingKey) return;
  lastBatchProcessingKey = key;
  renderBatchIndicator(el, bp);
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
  group.scrollIntoView({ behavior: scrollBehavior(), block: 'nearest' });
}

/* ------------------------------------------------------------
 * Plan update banner
 * ------------------------------------------------------------ */

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

/* ------------------------------------------------------------
 * Sidebar
 * ------------------------------------------------------------ */

function renderSidebar() {
  const list = document.getElementById('comments-list');
  list.textContent = '';
  const { byParent, byQuote, visibleQuotes, resolvedCount } =
    groupComments(allComments, { showResolved });

  renderCount(document.getElementById('comments-count'), {
    total: allComments.length,
    threadCount: byQuote.size,
  });
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

  const state = {
    byParent,
    drifted,
    misanchored,
    pendingInNewVersion,
    collapsedQuotes,
    focusedThreadIdx,
  };
  const ctx = { slug, currentVersion: lastBlueprintVersion };
  let visibleIdx = 0;
  for (const [quote, cs] of visibleQuotes) {
    list.appendChild(renderGroup({ quote, cs, visibleIdx, state, ctx }));
    visibleIdx++;
  }
}

// One delegated listener for the whole comment list. Previously every quote
// bar, group, resolve link, reply input and reply button got its own handler,
// which the next render destroyed along with the focused node — that teardown
// is what `preserveFocus` existed to paper over. With delegation the listener
// outlives every re-render, so mid-typing focus is no longer collateral damage.
function bindCommentsList() {
  const list = document.getElementById('comments-list');
  if (!list) return;

  list.addEventListener('click', e => {
    const actionEl = e.target.closest('[data-action]');
    const group = e.target.closest('.group');
    if (actionEl && list.contains(actionEl)) {
      const { action } = actionEl.dataset;
      if (action === 'toggle-collapse') {
        toggleCollapse(group, actionEl);
        return;
      }
      if (action === 'set-resolved') {
        setResolved(actionEl.dataset.commentId, actionEl.dataset.resolved === 'true');
        return;
      }
      if (action === 'stage-reply') {
        stageReply(actionEl);
        return;
      }
    }
    // Click elsewhere in the group: scroll to the highlight, or accept a
    // pending plan update if the anchor only exists in the newer version.
    if (!group) return;
    if (e.target.closest('input, textarea, button, a')) return;
    if (group.dataset.pending) {
      acceptPendingUpdate();
      return;
    }
    if (!group.dataset.drifted) scrollFrameToQuote(group.dataset.quote);
  });

  // Enter in a reply input stages it, matching the button next to it.
  list.addEventListener('keydown', e => {
    if (e.key !== 'Enter') return;
    const input = e.target.closest('input[data-reply-for]');
    if (!input) return;
    e.preventDefault();
    const btn = input.parentElement.querySelector('[data-action="stage-reply"]');
    if (btn) stageReply(btn);
  });
}

function toggleCollapse(group, toggleEl) {
  if (!group) return;
  const quote = group.dataset.quote;
  const resolved = group.classList.contains('resolved');
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
  // Re-focus the equivalent toggle in the rebuilt subtree so keyboard users
  // don't get dumped back to the top of the document.
  const again = document.querySelector(
    `.group[data-quote="${CSS.escape(quote)}"] [data-action="toggle-collapse"]`
  );
  if (again && toggleEl === document.activeElement) again.focus();
}

function stageReply(btn) {
  const commentId = btn.dataset.commentId;
  const input = btn.parentElement.querySelector(`input[data-reply-for="${CSS.escape(commentId)}"]`);
  if (!input) return;
  const replyBody = input.value.trim();
  if (!replyBody) return;
  const parent = allComments.find(c => c.id === commentId);
  drafts.add({
    body: replyBody,
    parentId: commentId,
    parentBody: parent ? parent.body : null,
  });
  input.value = '';
  showToast('Reply staged — submit when ready', 'info');
}

async function setResolved(commentId, resolved) {
  try {
    const r = await fetch(`/api/blueprints/${slug}/comments/${commentId}/resolve`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ resolved }),
    });
    if (!r.ok) {
      // Silence here meant the click looked like a no-op and users clicked
      // again; say so instead.
      showToast(`Couldn't ${resolved ? 'resolve' : 'reopen'} thread: ${r.status}`, 'error');
      return;
    }
    refreshComments();
  } catch (e) {
    showToast(`Couldn't ${resolved ? 'resolve' : 'reopen'} thread: ${e.message || e}`, 'error');
  }
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
    toggle = document.createElement('button');
    toggle.type = 'button';
    toggle.id = 'toggle-resolved';
    toggle.className = 'linkish';
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

// Live-tick timestamps: every 30s, walk all [data-ts] elements and refresh.
setInterval(() => {
  for (const el of document.querySelectorAll('[data-ts]')) {
    const t = Number(el.dataset.ts);
    if (Number.isFinite(t) && t > 0) el.textContent = timeAgo(t);
  }
}, 30 * 1000);

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
  spans[0].scrollIntoView({ behavior: scrollBehavior(), block: 'center' });
  pulseSpans(spans);
}

function pulseSpans(spans) {
  spans.forEach(s => {
    s.classList.remove('ps-hl-active');
    void s.offsetWidth;
    s.classList.add('ps-hl-active');
    setTimeout(() => s.classList.remove('ps-hl-active'), 1500);
  });
}

/* ------------------------------------------------------------
 * Highlights
 * ------------------------------------------------------------ */

function applyHighlights() {
  const doc = blueprintFrame.contentDocument;
  if (!doc) return;
  clearHighlights(doc);
  drifted = new Set();
  misanchored = new Set();
  pendingInNewVersion = new Set();
  const tops = allComments.filter(c => !c.parent_id);
  const seen = new Set();
  const hasPendingUpdate = pendingBlueprintVersion !== null;
  for (const c of tops) {
    const q = c.selector.exact;
    if (seen.has(q)) continue;
    seen.add(q);
    const { anchored, confident } = highlightQuote(doc, c.selector, q, focusQuote);
    if (!anchored) {
      // If there's a pending plan update, the anchor may exist in the next version
      // the user hasn't loaded yet. Render distinctly so they don't mistake it for drift.
      if (hasPendingUpdate) {
        pendingInNewVersion.add(q);
      } else {
        drifted.add(q);
      }
    } else if (!confident) {
      misanchored.add(q);
    }
  }
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
  g.scrollIntoView({ behavior: scrollBehavior(), block: 'center' });
  g.classList.remove('flash');
  // force reflow so the animation restarts even on repeat clicks
  void g.offsetWidth;
  g.classList.add('flash');

  // Pulse the highlight spans in the iframe too
  const doc = blueprintFrame.contentDocument;
  if (doc) {
    pulseSpans(doc.querySelectorAll(`span[data-ps-hl][data-ps-quote="${CSS.escape(quote)}"]`));
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
      expandAll();
      break;
    case 'c':
      e.preventDefault();
      collapseAll();
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
  if (!target) return;
  target.classList.add('focused');
  // Move real DOM focus, not just the class. The `.focused` outline alone is
  // invisible to a screen reader — it announces nothing when j/k moves, which
  // makes the shortcut useless without sight. The group has tabIndex -1 so it
  // can take focus programmatically without joining the tab order.
  target.focus({ preventScroll: true });
  target.scrollIntoView({ behavior: scrollBehavior(), block: 'nearest' });
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

/* ------------------------------------------------------------
 * Boot
 * ------------------------------------------------------------ */
refreshMe();
bindThemeToggle();
bindShortcuts();
bindDraftsBar();
bindCommentsList();
