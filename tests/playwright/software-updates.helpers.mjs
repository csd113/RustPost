// Local browser-only outage fixture. Actual download, migration and rollback
// are exercised by the Rust transaction tests; this controls UI network timing.
export async function setupUpdateReview(page, baseUrl, outcome) {
  await page.goto(`${baseUrl}/admin/updates`);
  const original = await page.content();
  const csrf = await page.locator('input[name="csrf"]').first().inputValue();
  let phase = 'idle';
  let polls = 0;
  const message = outcome === 'succeeded'
    ? 'RustPost successfully updated from v1.0.0 to v1.1.0.'
    : 'Upgrade to v1.1.0 failed. RustPost was restored to v1.0.0 using the pre-upgrade database backup.';
  const render = () => {
    const terminal = ['succeeded', 'rolled_back'].includes(phase);
    const form = terminal ? '' : `<section class="panel admin-card"><h2>Latest stable: v1.1.0</h2><p>Local test release; mocked availability and outage.</p><h3>Release notes</h3><p>Test successful restart and automatic rollback.</p><form id="update-install" method="post" action="/admin/updates/install"><input type="hidden" name="csrf" value="${csrf}"><input type="hidden" name="approval" value="test-only"><label for="update-password">Current administrator password</label><input id="update-password" type="password" name="password" required><button type="submit">Install update</button></form></section><script src="/assets/rustpost-updates.js" data-active="false" defer></script>`;
    let html = original.replace('</main>', `${form}<section class="panel"><h2>Last update attempt</h2><p>${terminal ? message : 'Local test ready'}</p>${terminal ? '<p>Pre-upgrade database and configuration backup · Verified</p>' : ''}</section></main>`);
    if (phase === 'succeeded') html = html.replace('<p>v1.0.0</p>', '<p>v1.1.0</p>');
    if (!terminal && phase !== 'idle') html = html.replace('<p>Local test ready</p>', '<p>Verified pre-upgrade backup created.</p>');
    return html;
  };
  await page.route(`${baseUrl}/admin/updates`, route => route.fulfill({ contentType: 'text/html', body: render() }));
  await page.route(`${baseUrl}/admin/updates/check`, route => route.fulfill({ contentType: 'text/html', body: render() }));
  await page.route(`${baseUrl}/admin/updates/install`, async route => {
    const submitted = new URLSearchParams(route.request().postData());
    if (submitted.get('csrf') !== csrf || !submitted.get('password')) {
      await route.fulfill({ status: 403, contentType: 'text/html', body: '<main>Invalid authorization.</main>' });
      return;
    }
    phase = 'backing_up';
    // Simulate the server stopping before its installation redirect arrives.
    if (route.request().isNavigationRequest()) {
      await route.fulfill({ status: 503, contentType: 'text/html', body: '<main>RustPost is restarting. Reload the updates page after reconnecting.</main>' });
    } else {
      await route.abort('connectionreset');
    }
  });
  await page.route(`${baseUrl}/admin/updates/status`, async route => {
    polls += 1;
    if (polls === 2) { await route.abort('connectionrefused'); return; }
    if (polls >= 4) phase = outcome;
    await route.fulfill({ json: {
      phase, job: 'local-test-job', message: polls < 2 ? 'Verified pre-upgrade backup created.' : polls < 4 ? 'Health-checking the restarted application.' : message,
    } });
  });
  await page.goto(`${baseUrl}/admin/updates`);
  return () => { phase = outcome; };
}
