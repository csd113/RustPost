import { test, expect } from '@playwright/test';
import {
  assertNoQaFailures,
  createPost,
  expectNotErrorPage,
  installQaGuards,
  logout,
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
  const postText = 'Mobile viewport post #mobile';
  const postId = await createPost(page, postText);
  const article = page.locator('article[data-post-id]').filter({ hasText: postText }).first();

  await article.getByRole('button', { name: 'Like' }).click();
  await expect(article.getByRole('button', { name: 'Unlike' })).toBeVisible();
  await article.getByRole('button', { name: 'Bookmark' }).click();
  await expect(article.getByRole('button', { name: 'Unbookmark' })).toBeVisible();

  await page.goto('/search');
  await page.goBack({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
  await page.goForward({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);

  for (const url of ['/home', `/posts/${postId}`, `/users/${user}`, '/settings', '/notifications']) {
    const response = await page.goto(url);
    expect(response?.status(), `${url} should render on mobile`).toBeLessThan(400);
    await expect(page.locator('body')).toBeVisible();
  }

  await page.goto(`/posts/${postId}`);
  const replyText = `Mobile reply ${Date.now()}`;
  await page.getByLabel('What is happening?').fill(replyText);
  await Promise.all([
    page.waitForURL(/\/posts\/\d+#reply-\d+/),
    page.getByRole('button', { name: 'Post', exact: true }).click(),
  ]);
  await expect(page.getByText(replyText)).toBeVisible();
  await page.goBack({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
  await page.goForward({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);

  await page.goto('/settings');
  await page.getByLabel('Bio').fill('Updated from mobile smoke.');
  await page.getByRole('button', { name: 'Save settings' }).click();
  await expect(page.getByLabel('Bio')).toHaveValue('Updated from mobile smoke.');

  await logout(page);
  const follower = `mobile_follower_${Date.now()}`;
  await register(page, follower, password);
  await page.goto(`/users/${user}`);
  await page.getByRole('button', { name: 'Follow this account' }).click();
  await expect(page.getByRole('button', { name: 'Unfollow this account' })).toBeVisible();
  await logout(page);
  await page.goto('/login');
  await expect(page.getByRole('heading', { name: 'Log in' })).toBeVisible();
});
