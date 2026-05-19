import { test, expect } from '@playwright/test';
import {
  adminPassword,
  adminUser,
  assertNoQaFailures,
  createPost,
  crawlVisibleInternalLinks,
  fixturePath,
  firstFormWithAction,
  installQaGuards,
  login,
  logout,
  register,
  submitFirstPostAction,
} from './helpers';

test.beforeEach(async ({ page }, testInfo) => {
  await installQaGuards(page, testInfo);
});

test.afterEach(async ({ page }) => {
  assertNoQaFailures(page);
});

test('logged-out navigation, registration validation, and public links are healthy', async ({ page }) => {
  for (const url of ['/', '/home', '/local', '/login', '/register', '/search']) {
    const response = await page.goto(url);
    expect(response?.status(), `${url} should render or redirect cleanly`).toBeLessThan(400);
  }

  await page.goto('/register');
  await page.getByLabel('Username').fill('bad_user_name');
  await page.getByLabel('Password', { exact: true }).fill('valid password one');
  await page.getByLabel('Confirm password').fill('valid password two');
  const badRegister = await Promise.all([
    page.waitForResponse((response) => response.url().includes('/register') && response.request().method() === 'POST'),
    page.getByRole('button', { name: 'Create account' }).click(),
  ]).then(([response]) => response);
  expect(badRegister.status()).toBe(400);
  await expect(page.getByRole('heading', { name: 'Check the form' })).toBeVisible();

  await page.goto('/login');
  await page.getByLabel('Username').fill('missing-user');
  await page.getByLabel('Password', { exact: true }).fill('not the password');
  const badLogin = await Promise.all([
    page.waitForResponse((response) => response.url().includes('/login') && response.request().method() === 'POST'),
    page.getByRole('button', { name: 'Log in' }).click(),
  ]).then(([response]) => response);
  expect(badLogin.status()).toBe(401);
  await expect(page.getByRole('heading', { name: 'Authentication required' })).toBeVisible();

  await page.goto('/home');
  await crawlVisibleInternalLinks(page);
});

test.describe('no JavaScript fallback', () => {
  test.use({ javaScriptEnabled: false });

test('account, posting, thread, profile, search, and social actions work', async ({ page }) => {
  const alice = `alice_${Date.now()}`;
  const bob = `bob_${Date.now()}`;
  const password = 'very secure password';

  await register(page, alice, password);
  const postText = `Hello from Playwright #qa @${adminUser}`;
  const postId = await createPost(page, postText, fixturePath('tiny.png'));

  await page.goto(`/posts/${postId}`);
  await expect(page.getByRole('heading', { name: 'Thread' })).toBeVisible();
  await page.getByLabel('What is happening?').fill('Reply from the same account');
  await Promise.all([
    page.waitForURL(/\/posts\/\d+#reply-\d+/),
    page.getByRole('button', { name: 'Post', exact: true }).click(),
  ]);
  await expect(page.getByText('Reply from the same account')).toBeVisible();

  await submitFirstPostAction(page, 'Like');
  await expect(page.getByRole('button', { name: 'Unlike' }).first()).toBeVisible();
  await submitFirstPostAction(page, 'Unlike');
  await expect(page.getByRole('button', { name: 'Like' }).first()).toBeVisible();
  await submitFirstPostAction(page, 'Bookmark');

  await page.goto('/bookmarks');
  await expect(page.getByText(postText)).toBeVisible();

  await page.goto('/search');
  await page.getByLabel(/Search RustPost/).fill('#qa');
  await Promise.all([
    page.waitForURL(/\/search\?q=%23qa/),
    page.getByRole('button', { name: 'Search' }).click(),
  ]);
  await expect(page.getByText(postText)).toBeVisible();

  await page.goto('/tags/qa');
  await expect(page.getByText(postText)).toBeVisible();

  await page.goto('/settings');
  await page.getByLabel('Display name').fill('Alice QA');
  await page.getByLabel('Bio').fill('Testing the full account settings form.');
  await page.getByLabel('Website').fill('https://example.com/alice');
  await page.getByLabel('Profile picture').setInputFiles(fixturePath('tiny.png'));
  await page.getByLabel('Banner').setInputFiles(fixturePath('tiny.png'));
  await Promise.all([
    page.waitForURL('/settings'),
    page.getByRole('button', { name: 'Save settings' }).click(),
  ]);
  await expect(page.locator('input[name="display_name"]')).toHaveValue('Alice QA');

  await logout(page);

  await register(page, bob, password);
  await page.goto(`/users/${alice}`);
  await submitFirstPostAction(page, 'Repost');
  await expect(page.getByRole('button', { name: 'Unrepost' }).first()).toBeVisible();
  await page.getByRole('button', { name: 'Follow this account' }).click();
  await expect(page.getByRole('button', { name: 'Unfollow this account' })).toBeVisible();
  await page.getByRole('button', { name: 'Unfollow this account' }).click();
  await expect(page.getByRole('button', { name: 'Follow this account' })).toBeVisible();
  await page.getByRole('button', { name: 'Block this account' }).click();
  await page.goto('/settings');
  await expect(page.getByRole('button', { name: 'Unblock this account' })).toBeVisible();
  await page.getByRole('button', { name: 'Unblock this account' }).click();

  await page.goto(`/users/${alice}`);
  await page.getByRole('button', { name: 'Mute this account' }).click();
  await expect(page).toHaveURL(/\/users\/alice_/);
});
});

test('authenticated crawl, admin pages, notifications, delete, and backup forms are healthy', async ({ page }) => {
  const user = `charlie_${Date.now()}`;
  const password = 'very secure password';
  await register(page, user, password);
  const postId = await createPost(page, 'Post that will be deleted');

  await page.goto('/notifications');
  await page.getByRole('button', { name: 'Mark all read' }).click();
  await expect(page).toHaveURL(/\/notifications/);

  await page.goto(`/posts/${postId}/delete?return_to=/home%23post-${postId}`);
  await expect(page.getByRole('heading', { name: 'Delete post?' })).toBeVisible();
  await page.getByRole('link', { name: 'Cancel' }).click();
  await expect(page).toHaveURL(new RegExp(`/home#post-${postId}`));
  await page.goto(`/posts/${postId}/delete?return_to=/home%23post-${postId}`);
  await page.getByRole('button', { name: 'Confirm delete' }).click();
  await expect(page).toHaveURL(/\/home/);

  await logout(page);
  await login(page, adminUser, adminPassword);

  for (const url of ['/admin', '/admin/health', '/admin/users', '/admin/media', '/admin/backups']) {
    const response = await page.goto(url);
    expect(response?.status(), `${url} should be reachable for admin`).toBeLessThan(400);
  }

  await page.goto('/admin/backups');
  await page.getByRole('button', { name: 'Create backup' }).click();
  await expect(page.getByText('Backup created:')).toBeVisible();

  await page.goto('/home');
  await crawlVisibleInternalLinks(page);
});

test('normal UI forms post to real routes and forbidden duplicates are intentional errors', async ({ page }) => {
  const user = `dupe_${Date.now()}`;
  const password = 'very secure password';
  await register(page, user, password);
  await logout(page);

  await page.goto('/register');
  await page.getByLabel('Username').fill(user);
  await page.getByLabel('Password', { exact: true }).fill(password);
  await page.getByLabel('Confirm password').fill(password);
  const duplicate = await Promise.all([
    page.waitForResponse((response) => response.url().includes('/register') && response.request().method() === 'POST'),
    page.getByRole('button', { name: 'Create account' }).click(),
  ]).then(([response]) => response);
  expect(duplicate.status()).toBe(400);
  await expect(page.getByRole('heading', { name: 'Check the form' })).toBeVisible();

  await login(page, user, password);
  await page.goto('/home');
  await expect(firstFormWithAction(page, '/posts')).toBeVisible();
  await page.getByLabel('What is happening?').fill('');
  const emptyPost = await Promise.all([
    page.waitForResponse((response) => response.url().includes('/posts') && response.request().method() === 'POST'),
    page.getByRole('button', { name: 'Post', exact: true }).click(),
  ]).then(([response]) => response);
  expect(emptyPost.status()).toBe(400);
  await expect(page.getByRole('heading', { name: 'Check the form' })).toBeVisible();
});
