// Tests for the polling loop (frontend/poll.js). The interesting behaviour is
// all in the scheduling: no overlap, backoff on failure, a sticky banner after
// a run of failures, and nothing at all while the tab is hidden.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { commentsEqual, createPoller, FAILURES_BEFORE_BANNER } from '../frontend/poll.js';

function comment(over = {}) {
  return {
    id: 'c1',
    body: 'hi',
    resolved: false,
    processing_by: null,
    processing_started_at: null,
    author_avatar_url: null,
    ...over,
  };
}

describe('commentsEqual', () => {
  const cases = [
    { name: 'identical lists', a: [comment()], b: [comment()], equal: true },
    { name: 'different length', a: [comment()], b: [], equal: false },
    { name: 'body edited', a: [comment()], b: [comment({ body: 'yo' })], equal: false },
    { name: 'resolve toggled', a: [comment()], b: [comment({ resolved: true })], equal: false },
    {
      name: 'processing flag raised',
      a: [comment()],
      b: [comment({ processing_by: 'Claude' })],
      equal: false,
    },
    {
      name: 'avatar url appeared',
      a: [comment()],
      b: [comment({ author_avatar_url: 'http://x/y.png' })],
      equal: false,
    },
    {
      name: 'undefined and null are the same absence',
      a: [comment({ processing_by: undefined })],
      b: [comment({ processing_by: null })],
      equal: true,
    },
    {
      name: 'ids reordered',
      a: [comment({ id: 'a' }), comment({ id: 'b' })],
      b: [comment({ id: 'b' }), comment({ id: 'a' })],
      equal: false,
    },
  ];

  for (const c of cases) {
    it(c.name, () => {
      expect(commentsEqual(c.a, c.b)).toBe(c.equal);
    });
  }
});

describe('createPoller', () => {
  let hidden;

  beforeEach(() => {
    vi.useFakeTimers();
    hidden = false;
    // jsdom's document.hidden is read-only, so shadow it.
    Object.defineProperty(document, 'hidden', {
      configurable: true,
      get: () => hidden,
    });
    // The banner uses the real toast stack; give it one to attach to.
    document.body.innerHTML = '<div id="toast-stack"></div>';
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  const toasts = () => document.querySelectorAll('#toast-stack .toast');

  it('polls on the base interval while healthy', async () => {
    const fetchOnce = vi.fn().mockResolvedValue(true);
    const p = createPoller({ fetchOnce, baseMs: 1000, maxMs: 8000 });
    p.start();
    expect(fetchOnce).not.toHaveBeenCalled();   // scheduled, not immediate
    await vi.advanceTimersByTimeAsync(1000);
    expect(fetchOnce).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1000);
    expect(fetchOnce).toHaveBeenCalledTimes(2);
    p.stop();
  });

  it('never overlaps two in-flight fetches', async () => {
    let active = 0;
    let maxActive = 0;
    const fetchOnce = vi.fn(async () => {
      active++;
      maxActive = Math.max(maxActive, active);
      // Outlive the poll interval, which is what used to cause overlap.
      await new Promise(r => setTimeout(r, 5000));
      active--;
      return true;
    });
    const p = createPoller({ fetchOnce, baseMs: 1000, maxMs: 8000 });
    p.start();
    await vi.advanceTimersByTimeAsync(20000);
    expect(maxActive).toBe(1);
    p.stop();
  });

  it('backs off exponentially on failure and caps out', async () => {
    const fetchOnce = vi.fn().mockResolvedValue(false);
    const p = createPoller({ fetchOnce, baseMs: 1000, maxMs: 8000 });
    p.start();
    const delays = [];
    for (let i = 0; i < 6; i++) {
      delays.push(p.currentDelay());
      await vi.advanceTimersByTimeAsync(p.currentDelay());
    }
    // 1000 (first, no failures yet), then 1000, 2000, 4000, 8000, capped 8000.
    expect(delays).toEqual([1000, 1000, 2000, 4000, 8000, 8000]);
    p.stop();
  });

  it('resets the backoff after a success', async () => {
    const fetchOnce = vi.fn()
      .mockResolvedValueOnce(false)
      .mockResolvedValueOnce(false)
      .mockResolvedValue(true);
    const p = createPoller({ fetchOnce, baseMs: 1000, maxMs: 8000 });
    p.start();
    await vi.advanceTimersByTimeAsync(1000);   // fail 1
    await vi.advanceTimersByTimeAsync(1000);   // fail 2
    expect(p.failureCount()).toBe(2);
    await vi.advanceTimersByTimeAsync(2000);   // success
    expect(p.failureCount()).toBe(0);
    expect(p.currentDelay()).toBe(1000);
    p.stop();
  });

  it('shows a sticky banner only after a run of failures, and clears it on success', async () => {
    const fetchOnce = vi.fn().mockResolvedValue(false);
    const p = createPoller({ fetchOnce, baseMs: 1000, maxMs: 8000 });
    p.start();
    // One short of the threshold: still quiet, because a lone failed poll is
    // routine (the daemon shuts itself down when idle).
    for (let i = 0; i < FAILURES_BEFORE_BANNER - 1; i++) {
      await vi.advanceTimersByTimeAsync(p.currentDelay());
    }
    expect(toasts().length).toBe(0);
    await vi.advanceTimersByTimeAsync(p.currentDelay());
    expect(toasts().length).toBe(1);
    expect(toasts()[0].textContent).toContain('Disconnected');
    expect(toasts()[0].className).toContain('error');

    // Further failures must not stack duplicate banners.
    await vi.advanceTimersByTimeAsync(p.currentDelay());
    await vi.advanceTimersByTimeAsync(p.currentDelay());
    expect(toasts().length).toBe(1);

    // Recovery starts the dismiss animation.
    fetchOnce.mockResolvedValue(true);
    await vi.advanceTimersByTimeAsync(p.currentDelay());
    expect(toasts()[0].className).toContain('exiting');
    p.stop();
  });

  it('treats a thrown fetch as a failure', async () => {
    const fetchOnce = vi.fn().mockRejectedValue(new Error('offline'));
    const p = createPoller({ fetchOnce, baseMs: 1000, maxMs: 8000 });
    p.start();
    await vi.advanceTimersByTimeAsync(1000);
    expect(p.failureCount()).toBe(1);
    p.stop();
  });

  it('stops polling while the tab is hidden and resumes immediately on return', async () => {
    const fetchOnce = vi.fn().mockResolvedValue(true);
    const p = createPoller({ fetchOnce, baseMs: 1000, maxMs: 8000 });
    p.start();
    await vi.advanceTimersByTimeAsync(1000);
    expect(fetchOnce).toHaveBeenCalledTimes(1);

    hidden = true;
    p.onVisibilityChange();
    // A backgrounded tab used to keep fetching forever; now: nothing.
    await vi.advanceTimersByTimeAsync(60000);
    expect(fetchOnce).toHaveBeenCalledTimes(1);

    hidden = false;
    p.onVisibilityChange();
    // Immediate, not after another full interval — the view is as stale as the
    // time the user was away.
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchOnce).toHaveBeenCalledTimes(2);
    p.stop();
  });

  it('stop() prevents any further polling', async () => {
    const fetchOnce = vi.fn().mockResolvedValue(true);
    const p = createPoller({ fetchOnce, baseMs: 1000, maxMs: 8000 });
    p.start();
    await vi.advanceTimersByTimeAsync(1000);
    p.stop();
    await vi.advanceTimersByTimeAsync(60000);
    expect(fetchOnce).toHaveBeenCalledTimes(1);
  });

  it('stop() is final — a later visibilitychange must not resurrect the loop', async () => {
    const fetchOnce = vi.fn().mockResolvedValue(true);
    const p = createPoller({ fetchOnce, baseMs: 1000, maxMs: 8000 });
    p.start();
    await vi.advanceTimersByTimeAsync(1000);
    p.stop();
    // Tab goes away and comes back. "Paused because hidden" and "stopped by the
    // caller" are different states; only the former resumes.
    hidden = true;
    p.onVisibilityChange();
    hidden = false;
    p.onVisibilityChange();
    await vi.advanceTimersByTimeAsync(60000);
    expect(fetchOnce).toHaveBeenCalledTimes(1);
  });
});
