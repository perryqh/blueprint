// mermaid-init.js — initialize Mermaid with a theme that tracks the chrome's
// resolved Sys/Light/Dark setting. Re-renders every <pre class="mermaid"> when
// the parent toggles `data-theme`, so diagrams retheme in lockstep with the
// rest of the page (no parent ↔ iframe messaging required — the reviewer
// chrome's `injectFrameStyles` already propagates `data-theme` into the
// iframe root, see frontend/app.js).
//
// Loads after mermaid.js (deferred), so `window.mermaid` is available.

(function () {
  if (!window.mermaid) {
    console.warn('mermaid-init: window.mermaid not present');
    return;
  }
  const root = document.documentElement;

  function resolvedTheme() {
    const t = root.dataset.theme;
    if (t === 'dark' || t === 'light') return t;
    return matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  }

  function readVar(name, fallback) {
    const v = getComputedStyle(root).getPropertyValue(name).trim();
    return v || fallback;
  }

  // Snapshot the original Mermaid source of every block before the first
  // render, so a theme toggle can re-render from the source rather than
  // re-parsing the already-rendered SVG.
  for (const el of document.querySelectorAll('pre.mermaid, .mermaid:not([data-mermaid-src])')) {
    if (!el.dataset.mermaidSrc) {
      el.dataset.mermaidSrc = el.textContent;
    }
  }

  function render() {
    const dark = resolvedTheme() === 'dark';
    window.mermaid.initialize({
      startOnLoad: false,
      theme: dark ? 'dark' : 'default',
      themeVariables: {
        // Bridge Mermaid's named slots to our CSS palette so light/dark
        // diagrams feel continuous with the rest of the page.
        background: readVar('--bg', dark ? '#0d1117' : '#ffffff'),
        primaryColor: readVar('--node-bg', dark ? '#0c2d4d' : '#ddf4ff'),
        primaryBorderColor: readVar('--node-stroke', dark ? '#58a6ff' : '#0969da'),
        primaryTextColor: readVar('--fg', dark ? '#e6edf3' : '#1f2328'),
        lineColor: readVar('--muted', dark ? '#8b949e' : '#57606a'),
        secondaryColor: readVar('--bg-soft', dark ? '#161b22' : '#f6f8fa'),
        tertiaryColor: readVar('--bg-soft', dark ? '#161b22' : '#f6f8fa'),
        fontFamily: '-apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, sans-serif',
      },
    });

    // Reset each block to its original Mermaid source and let mermaid.run()
    // re-process them. Idempotent — second theme toggle behaves the same.
    for (const el of document.querySelectorAll('[data-mermaid-src]')) {
      el.removeAttribute('data-processed');
      el.innerHTML = el.dataset.mermaidSrc;
      el.classList.add('mermaid');
    }
    window.mermaid.run({ querySelector: '.mermaid' }).catch(e => {
      console.warn('mermaid render failed:', e);
    });
  }

  render();

  // Chrome theme toggles mutate `data-theme` on this document's root (the
  // iframe doc). Re-render when that changes; ignore unrelated mutations.
  new MutationObserver((muts) => {
    if (muts.some(m => m.attributeName === 'data-theme')) render();
  }).observe(root, { attributes: true, attributeFilter: ['data-theme'] });

  // Also react to OS-level preference changes when the chrome is on "System".
  matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
    if (!root.dataset.theme) render();
  });
})();
