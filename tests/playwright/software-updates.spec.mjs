import { test, expect } from '@playwright/test';
import { withLocalRuntime, login, adminUsername, adminPassword, assertNoHorizontalOverflow } from './configuration-admin.helpers.mjs';

test('admin software updates retain safe deployment and no-JS flows', async ({ page }) => {
  await withLocalRuntime(async ({ baseUrl }) => {
    await login(page, baseUrl, adminUsername, adminPassword);
    await page.goto(`${baseUrl}/admin`);
    await page.getByRole('link', { name: 'Software updates', exact: true }).click();
    await expect(page.getByRole('heading', { name: 'Software updates', exact: true })).toBeVisible();
    await expect(page.getByRole('heading', { name: 'Installed version' })).toBeVisible();
    await expect(page.getByText('v1.0.0', { exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Check for updates' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Install update', exact: true })).toHaveCount(0);
    await expect(page.getByRole('heading', { name: 'Pre-upgrade backups' })).toBeVisible();
    const check = page.locator('form[action="/admin/updates/check"]');
    await expect(check).toHaveAttribute('method', 'post');
    await expect(check.locator('input[name=csrf]')).toHaveAttribute('value', /.+/);
    const cookies = await page.context().cookies();
    const cookie = cookies.map(c => `${c.name}=${c.value}`).join('; ');
    const denied = await fetch(`${baseUrl}/admin/updates/check`, {
      method: 'POST', headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' }, body: 'csrf=forged',
    });
    expect(denied.status).toBe(403);
    expect((await fetch(`${baseUrl}/admin/updates/install`, { headers: { cookie } })).status).toBe(405);
    await assertNoHorizontalOverflow(page, 'desktop update page');
    await page.setViewportSize({ width: 375, height: 812 });
    await assertNoHorizontalOverflow(page, 'mobile update page');
    await page.screenshot({ path: `output/playwright/software-updates-${test.info().project.name}.png`, fullPage: true });
  });
});

test('anonymous users cannot access updater status or installation', async ({ page }) => {
  await withLocalRuntime(async ({ baseUrl }) => {
    for (const endpoint of ['/admin/updates', '/admin/updates/status']) {
      expect((await page.request.get(`${baseUrl}${endpoint}`)).status()).toBe(401);
    }
    expect((await page.request.post(`${baseUrl}/admin/updates/install`, {
      form: { csrf: 'bogus', approval: 'bogus', password: 'bogus' },
    })).status()).toBe(401);
  });
});

for (const outcome of ['succeeded', 'rolled_back']) {
  test(`install document survives an immediate outage and shows ${outcome}`, async ({ page }, testInfo) => {
    const { setupUpdateReview } = await import('./software-updates.helpers.mjs');
    await withLocalRuntime(async ({ baseUrl }) => {
      await login(page, baseUrl, adminUsername, adminPassword);
      const finish = await setupUpdateReview(page, baseUrl, outcome);
      await page.getByRole('button', { name: 'Check for updates' }).click();
      await expect(page.getByRole('heading', { name: 'Latest stable: v1.1.0' })).toBeVisible();
      await page.locator('#update-password').fill(adminPassword);
      await page.getByRole('button', { name: 'Install update', exact: true }).click();
      if (testInfo.project.use.javaScriptEnabled === false) {
        // An ordinary form can lose its navigation during restart. Manual
        // refresh recovers the durable result without any script execution.
        await page.goto(`${baseUrl}/admin/updates`);
        await expect(page.getByText('Verified pre-upgrade backup created.', { exact: true })).toBeVisible();
      } else {
        await expect(page.locator('#update-progress')).toHaveText('Verified pre-upgrade backup created.');
      }
      await page.screenshot({ path: `output/playwright/update-${outcome}-backup-${testInfo.project.name}.png`, fullPage: true });
      if (testInfo.project.use.javaScriptEnabled === false) {
        finish();
        await page.goto(`${baseUrl}/admin/updates`);
      }
      await expect(page.getByText(outcome === 'succeeded'
        ? 'RustPost successfully updated from v1.0.0 to v1.1.0.'
        : 'Upgrade to v1.1.0 failed. RustPost was restored to v1.0.0 using the pre-upgrade database backup.', { exact: true })).toBeVisible({ timeout: 20_000 });
      await expect(page).toHaveURL(`${baseUrl}/admin/updates`);
      await expect(page.getByText('Pre-upgrade database and configuration backup · Verified')).toBeVisible();
      await expect(page.getByRole('button', { name: 'Install update', exact: true })).toHaveCount(0);
      await page.screenshot({ path: `output/playwright/update-${outcome}-result-${testInfo.project.name}.png`, fullPage: true });
    });
  });
}
