import { test, expect } from '@playwright/test';
import {
  assertNoQaFailures,
  createPost,
  installQaGuards,
  register,
} from './helpers';

test.beforeEach(async ({ page }, testInfo) => {
  await installQaGuards(page, testInfo);
});

test.afterEach(async ({ page }) => {
  assertNoQaFailures(page);
});

test('mobile smoke covers core pages and forms', async ({ page }) => {
  const user = `mobile_${Date.now()}`;
  const password = 'very secure password';

  await page.goto('/home');
  await expect(page.getByRole('heading', { name: 'Home Feed' })).toBeVisible();
  await page.goto('/search');
  await expect(page.getByRole('heading', { name: 'Search' })).toBeVisible();

  await register(page, user, password);
  const postId = await createPost(page, 'Mobile viewport post #mobile');

  for (const url of ['/home', `/posts/${postId}`, `/users/${user}`, '/settings', '/notifications']) {
    const response = await page.goto(url);
    expect(response?.status(), `${url} should render on mobile`).toBeLessThan(400);
    await expect(page.locator('body')).toBeVisible();
  }

  await page.goto('/settings');
  await page.getByLabel('Bio').fill('Updated from mobile smoke.');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.getByLabel('Bio')).toHaveValue('Updated from mobile smoke.');
});
