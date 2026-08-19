// Comment polling. This 1.5s loop is the *only* thing that surfaces new
// comments, resolve toggles, processing flags and the finish stamp, so its
// failure modes are user-visible in a way a background refresh usually isn't.

import { showToast, dismissToast } from './toast.js';

export const POLL_BASE_MS = 1500;
export const POLL_MAX_MS = 30000;
// The daemon exits on its own (POST /api/shutdown-if-empty), so a few failed
// polls is routine rather than exceptional. Wait for a run of them before
// shouting, otherwise every laptop-sleep produces a scary banner.
export const FAILURES_BEFORE_BANNER = 3;

export function commentsEqual(a, b) {
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

// Self-scheduling timeout chain rather than setInterval: a poll that outlives
// its interval (a big /comments response on a slow link) used to overlap the
// next one, and since the handler assigns `allComments = comments` wholesale
// the slower of two in-flight responses could win. One-at-a-time scheduling
// plus the request-id guard in `fetchComments` closes that.
export function createPoller({ fetchOnce, baseMs = POLL_BASE_MS, maxMs = POLL_MAX_MS }) {
  let timer = null;
  let failures = 0;
  let banner = null;
  let running = false;
  let inFlight = false;

  function delay() {
    if (failures === 0) return baseMs;
    // 1.5s, 3s, 6s, 12s, 24s, 30s…
    return Math.min(maxMs, baseMs * Math.pow(2, failures - 1));
  }

  function showDisconnected() {
    // Reuse the toast stack's error kind — it already persists until dismissed,
    // which is exactly the "sticky until resolved" behaviour a banner needs.
    if (banner && banner.parentNode) return;
    banner = showToast('Disconnected — retrying', 'error');
  }

  function clearDisconnected() {
    if (banner) {
      dismissToast(banner);
      banner = null;
    }
  }

  async function tick() {
    timer = null;
    if (!running || inFlight) return;
    // A hidden tab has no reason to poll; `document.hidden` is re-checked here
    // (not just in the visibility handler) so a tick already queued when the
    // tab was hidden doesn't sneak a fetch through.
    if (typeof document !== 'undefined' && document.hidden) return;
    inFlight = true;
    let ok = false;
    try {
      ok = await fetchOnce();
    } catch (_) {
      ok = false;
    } finally {
      inFlight = false;
    }
    if (ok) {
      failures = 0;
      clearDisconnected();
    } else {
      failures++;
      if (failures >= FAILURES_BEFORE_BANNER) showDisconnected();
    }
    schedule();
  }

  function schedule() {
    if (!running) return;
    if (typeof document !== 'undefined' && document.hidden) return;
    if (timer !== null) return;
    timer = setTimeout(tick, delay());
  }

  // Drop the pending timer without ending the loop — this is the "paused while
  // hidden" state, which `pollNow` can resume from.
  function cancelTimer() {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  }

  // Shut the loop down for good. Kept distinct from cancelTimer so a later
  // visibilitychange can't silently restart a poller the caller stopped.
  function stop() {
    running = false;
    cancelTimer();
  }

  function start() {
    running = true;
    cancelTimer();
    schedule();
  }

  // Resume with an *immediate* poll: the user just came back to the tab and the
  // data on screen is as stale as the time they were away.
  function pollNow() {
    if (!running) return;
    cancelTimer();
    tick();
  }

  function onVisibilityChange() {
    if (document.hidden) {
      cancelTimer();
    } else {
      pollNow();
    }
  }

  return {
    start,
    stop,
    pollNow,
    onVisibilityChange,
    failureCount: () => failures,
    currentDelay: delay,
  };
}
