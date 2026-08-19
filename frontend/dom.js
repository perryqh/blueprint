// Small shared DOM/formatting helpers. No app state — everything here is a
// pure function of its arguments so the render modules can be exercised in
// isolation.

export const PROCESSING_TTL_MS = 5 * 60 * 1000; // 5 minutes; rows older than this are treated as cleared

// Honour the OS "reduce motion" setting for our programmatic scrolls. Smooth
// scrolling is a vestibular trigger, and unlike CSS transitions a JS
// scrollIntoView isn't covered by a media query in the stylesheet.
export function scrollBehavior() {
  return matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth';
}

// Hash-of-name → one of 8 curated CSS palette gradients. Same input always
// yields the same color across reloads and tabs.
const AVATAR_PALETTE_SIZE = 8;
export function authorGradient(name) {
  let h = 0;
  for (const ch of name) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  const idx = (h % AVATAR_PALETTE_SIZE) + 1;
  return `var(--avatar-${idx})`;
}

export function authorInitials(name) {
  const trimmed = (name || '?').trim();
  if (!trimmed) return '?';
  // Take first character of up to two whitespace-separated words.
  const parts = trimmed.split(/[\s._-]+/).filter(Boolean);
  if (parts.length === 0) return '?';
  if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
  return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase();
}

export function makeAvatar(doc, name, avatarUrl, isAgent) {
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

export function truncate(s, n) {
  if (!s) return '';
  return s.length > n ? s.slice(0, n - 1) + '…' : s;
}

export function timeAgo(ms) {
  const dt = (Date.now() - ms) / 1000;
  if (dt < 60) return Math.max(1, Math.floor(dt)) + 's ago';
  if (dt < 3600) return Math.floor(dt / 60) + 'm ago';
  if (dt < 86400) return Math.floor(dt / 3600) + 'h ago';
  return Math.floor(dt / 86400) + 'd ago';
}

/* The rotating ✦ sparkle used by both the per-comment "X is replying" pill
   in renderComment and the slug-level batch-processing indicator. SVG is
   identical in both spots; consolidating prevents the two from drifting. */
export function makeSparkleSvg() {
  const sparkle = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  sparkle.setAttribute('class', 'sparkle sparkle--spin');
  sparkle.setAttribute('viewBox', '0 0 24 24');
  sparkle.setAttribute('aria-hidden', 'true');
  const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  path.setAttribute('d', 'M 12 2 L 13.5 10.5 L 22 12 L 13.5 13.5 L 12 22 L 10.5 13.5 L 2 12 L 10.5 10.5 Z');
  sparkle.appendChild(path);
  return sparkle;
}

// `<strong>name</strong> is <rest>` as DOM nodes. Both call sites used to build
// this with innerHTML + escapeHtml; a helper keeps the escaping question from
// coming up at all.
export function strongPrefixed(name, rest) {
  const frag = document.createDocumentFragment();
  const strong = document.createElement('strong');
  strong.textContent = name;
  frag.appendChild(strong);
  frag.appendChild(document.createTextNode(rest));
  return frag;
}
