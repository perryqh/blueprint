/* ============================================================
 * Toast notifications (Stage 4a)
 * ============================================================ */

export function showToast(msg, kind = 'info') {
  const stack = document.getElementById('toast-stack');
  if (!stack) return null;
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
  return toast;
}

export function dismissToast(toast) {
  if (!toast || !toast.parentNode) return;
  toast.classList.add('exiting');
  toast.addEventListener('animationend', () => {
    if (toast.parentNode) toast.parentNode.removeChild(toast);
  }, { once: true });
}

// setStatus is kept as a thin wrapper for any legacy callers; new code routes
// through showToast directly.
export function setStatus(msg, isError = false) {
  showToast(msg, isError ? 'error' : 'info');
}

/* ============================================================
 * Loading state for buttons (Stage 4b)
 * ============================================================ */
export function setLoading(btn, on) {
  if (!btn) return;
  if (on) {
    btn.dataset.loading = '1';
    btn.disabled = true;
  } else {
    delete btn.dataset.loading;
    btn.disabled = false;
  }
}
