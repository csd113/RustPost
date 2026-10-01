import { test, expect } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { adminUsername, adminPassword, withLocalRuntime, login, assertNoHorizontalOverflow,
  stopProcess, startServer, waitForServer } from './configuration-admin.helpers.mjs';

const widths = [320, 375, 768, 1024, 1440, 1920];

test('complete configuration console preserves typed settings, validates, and distinguishes restart state', async ({ page }, testInfo) => {
  await withLocalRuntime(async ({ baseUrl, settingsPath, dataDir, server }) => {
    const initial = await readFile(settingsPath, 'utf8');
    await page.goto(`${baseUrl}/admin/deep-settings`);
    await expect(page.locator('#deep-settings-form')).toHaveCount(0);
    await login(page, baseUrl, adminUsername, adminPassword);
    await page.goto(`${baseUrl}/admin/deep-settings`);
    await expect(page.locator('.deep-settings-field')).toHaveCount(61);
    await expect(page.locator('.deep-settings-group')).toHaveCount(9);
    await expect(page.getByLabel('Administration', { exact: true }).getByRole('link', { name: 'Configuration' })).toHaveAttribute('aria-current', 'page');
    await expect(page.getByText('No environment or CLI value overrides apply', { exact: false })).toBeVisible();
    await expect(page.locator('body')).not.toContainText('fixture-private-value');
    await expect(page.locator('input[type=password]')).toHaveCount(0);
    await page.getByText('Deployment-managed settings', { exact: true }).click();
    await expect(page.locator('.deployment-settings')).toContainText('media.ffmpeg_path');
    await expect(page.locator('.deployment-settings')).toContainText('tor.data_dir');

    for (const width of widths) {
      await page.setViewportSize({ width, height: 900 });
      await assertNoHorizontalOverflow(page, `configuration ${width}px`);
      const clipped = await page.locator('#deep-settings-form input:not([type=hidden]), #deep-settings-form textarea, #deep-settings-form select').evaluateAll((controls) => controls.filter((control) => {
        const box = control.getBoundingClientRect();
        return box.left < 0 || box.right > window.innerWidth;
      }).map((control) => control.id));
      expect(clipped).toEqual([]);
      await page.screenshot({ path: `output/playwright/configuration-${testInfo.project.name}-${width}.png`, fullPage: false });
      for (const route of ['/admin', '/admin/health', '/admin/users', '/admin/media', '/admin/backups']) {
        await page.goto(`${baseUrl}${route}`);
        await expect(page.getByLabel('Administration', { exact: true })).toBeVisible();
        await assertNoHorizontalOverflow(page, `${route} ${width}px`);
      }
      await page.goto(`${baseUrl}/admin/deep-settings`);

    }
    await page.setViewportSize({ width: 1024, height: 900 });
    await page.getByLabel('Settings categories').getByRole('link', { name: 'Onion service', exact: true }).click();
    await expect(page).toHaveURL(/#settings-tor$/);
    await page.locator('#deep-site_name').focus();
    await page.keyboard.press(testInfo.project.name === 'webkit' ? 'Alt+Tab' : 'Tab');
    await expect(page.locator('#deep-registration_enabled')).toBeFocused();
    const focus = await page.locator('#deep-registration_enabled').evaluate((element) => getComputedStyle(element).outlineStyle);
    expect(focus).not.toBe('none');
    const labels = await page.locator('#deep-settings-form input:not([type=hidden]), #deep-settings-form textarea, #deep-settings-form select').evaluateAll((controls) => controls.every((control) => control.labels?.length && control.getAttribute('aria-describedby')));
    expect(labels).toBe(true);

    if (!testInfo.project.name.endsWith('no-js')) {
      await page.getByLabel('Find a setting').fill('rate limit');
      await expect(page.locator('.deep-settings-field:visible')).toHaveCount(6);
      await expect(page.locator('#settings-search-status')).toHaveText('6 settings found');
      await page.getByLabel('Settings categories').getByRole('link', { name: 'Site', exact: true }).click();
      await expect(page.locator('.deep-settings-field:visible')).toHaveCount(61);
    } else {
      await expect(page.locator('.settings-search')).toBeHidden();
    }

    // One changed field from every category plus boolean, enum, list and optional values.
    await page.locator('#deep-site_name').fill('Admin configured');
    await page.locator('#deep-deletion_grace_period_days').fill('7');
    await page.locator('#deep-max_text_chars').fill('320');
    await page.locator('#deep-vp9_deadline').selectOption('realtime');
    await page.locator('#deep-trusted_proxy_cidrs').fill('127.0.0.1/32\n::1/128');
    await page.locator('#deep-public_url').fill('https://example.org');
    await page.locator('#deep-posts_per_minute').fill('12');
    await page.locator('#deep-bootstrap_timeout_secs').fill('180');
    await page.locator('#deep-retention_keep_last').fill('12');
    await page.locator('#deep-create_admin_on_first_boot').check();
    await page.locator('#deep-nsfw_blur_enabled').uncheck();
    await page.locator('#deep-max_archive_entries').fill('0');
    await page.locator('#deep-settings-form button[type=submit]').click();
    await expect(page.getByRole('alert')).toContainText('Maximum archive entries');
    await expect(page.locator('#deep-site_name')).toHaveValue('Admin configured');
    await expect(page.locator('#deep-vp9_deadline')).toHaveValue('realtime');
    await expect(page.locator('#deep-nsfw_blur_enabled')).not.toBeChecked();
    expect(await readFile(settingsPath, 'utf8')).toBe(initial);
    await page.locator('#deep-max_archive_entries').fill('20000');
    await page.locator('#deep-settings-form button[type=submit]').click();
    await expect(page.getByRole('heading', { name: 'These settings are about to be changed' })).toBeVisible();
    expect(await readFile(settingsPath, 'utf8')).toBe(initial);
    await page.getByRole('button', { name: 'Confirm/Save', exact: true }).click();
    await expect(page.locator('.notice[role=status]')).toContainText('Restart required');
    await expect(page.locator('#deep-max_text_chars-help')).toContainText('Running value: 280');
    const saved = await readFile(settingsPath, 'utf8');
    for (const line of ['name = "Admin configured"', 'deletion_grace_period_days = 7', 'max_text_chars = 320', 'vp9_deadline = "realtime"',
      'posts_per_minute = 12', 'bootstrap_timeout_secs = 180', 'retention_keep_last = 12', 'create_admin_on_first_boot = true', 'nsfw_blur_enabled = false',
      'max_image_size = 52428801', 'private_fixture = "fixture-private-value"']) {
      expect(saved).toContain(line);
    }
    await page.reload();
    await expect(page.locator('#deep-site_name')).toHaveValue('Admin configured');
    await expect(page.locator('#deep-nsfw_blur_enabled')).not.toBeChecked();
    await page.locator('#deep-public_url').fill('');
    await page.locator('#deep-settings-form button[type=submit]').click();
    await page.getByRole('button', { name: 'Confirm/Save', exact: true }).click();
    expect(await readFile(settingsPath, 'utf8')).toContain('public_url = ""');

    await stopProcess(server);
    const restarted = startServer(dataDir);
    try {
      await waitForServer(baseUrl, restarted);
      await page.goto(`${baseUrl}/admin/deep-settings`);
      await expect(page.locator('#deep-max_text_chars-help')).not.toContainText('Restart pending');
      await expect(page.locator('.brand')).toContainText('Admin configured');
    } finally { await stopProcess(restarted); }
  }, { replacements: [
    ['max_image_size = 52428800', 'max_image_size = 52428801'],
    ['webp_quality = 82', 'webp_quality = 82\nprivate_fixture = "fixture-private-value"'],
  ] });
});
