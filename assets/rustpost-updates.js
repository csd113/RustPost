(() => {
  const script = document.currentScript;
  const progress = document.getElementById('update-progress');
  const form = document.getElementById('update-install');
  const finished = ['succeeded', 'rolled_back', 'failed', 'failed_manual_intervention'];
  let polling = script?.dataset.active === 'true';
  let observedJob = polling;
  let submitting = false;
  async function poll() {
    if (!polling) return;
    try {
      const response = await fetch('/admin/updates/status', { cache: 'no-store' });
      if (response.ok) {
        const status = await response.json();
        if (!finished.includes(status.phase) && status.phase !== 'idle') observedJob = true;
        if (finished.includes(status.phase) && (observedJob || !submitting)) {
          location.replace('/admin/updates');
          return;
        }
        if (progress) progress.textContent = status.message || 'Update in progress.';
      } else if (progress) progress.textContent = 'RustPost is restarting. Reconnecting…';
    } catch {
      if (progress) progress.textContent = 'RustPost is restarting. Reconnecting…';
    }
    setTimeout(poll, 3000);
  }
  form?.addEventListener('submit', async (event) => {
    event.preventDefault();
    if (submitting || polling) return;
    submitting = true;
    if (progress) progress.textContent = 'Starting the update. Reconnecting automatically after restart…';
    const button = form.querySelector('button[type="submit"]');
    if (button) button.disabled = true;
    // Keep this already loaded document across the outage. Same-origin POST
    // retains server-side auth/CSRF/password checks and ordinary no-JS fallback.
    try {
      const response = await fetch(form.action, {
        method: 'POST', body: new URLSearchParams(new FormData(form)), redirect: 'follow'
      });
      if (!response.ok) {
        const text = await response.text();
        const document = new DOMParser().parseFromString(text, 'text/html');
        if (progress) progress.textContent = document.querySelector('[role="alert"] p, .error-panel h1 + p')?.textContent.trim() || 'Update request rejected. Reload to try again.';
        submitting = false;
        if (button) button.disabled = false;
        return;
      }
    } catch {
      // The authenticated request may already have started the restart.
    } finally {
      form.querySelector('input[type="password"]').value = '';
    }
    submitting = false;
    polling = true;
    poll();
  });
  if (polling) setTimeout(poll, 3000);
})();
