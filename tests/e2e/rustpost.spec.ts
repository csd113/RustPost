import { test, expect, type Page } from '@playwright/test';
import fs from 'node:fs';
import path from 'node:path';
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

const SECOND_PNG_BASE64 =
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=';

function writeGeneratedFixture(name: string, data: Buffer | string): string {
  const file = path.join(process.cwd(), 'output/playwright/generated-fixtures', name);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, data);
  return file;
}

function generatedPng(name: string, base64 = SECOND_PNG_BASE64): string {
  const bytes = Buffer.from(base64, 'base64');
  return writeGeneratedFixture(name, Buffer.concat([bytes, Buffer.from(name)]));
}

async function expectReachableImage(page: Page, src: string) {
  const response = await page.request.get(src);
  expect(response.status(), `${src} should be reachable`).toBe(200);
  expect(response.headers()['content-type'] ?? '', `${src} content type`).toMatch(/^image\//);
}

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
  await expect(page.getByRole('heading', { name: 'Log in' })).toBeVisible();
  await expect(page.getByText('No account with that username.')).toBeVisible();

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
  await expect(page.locator(`article[data-post-id="${postId}"]`)).toBeVisible();
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
  await expect(page.getByRole('heading', { name: 'Create account' })).toBeVisible();
  await expect(page.getByText('That username is already taken.')).toBeVisible();

  await page.goto('/login');
  await page.getByLabel('Username').fill(user);
  await page.getByLabel('Password', { exact: true }).fill('not the password');
  const badPassword = await Promise.all([
    page.waitForResponse((response) => response.url().includes('/login') && response.request().method() === 'POST'),
    page.getByRole('button', { name: 'Log in' }).click(),
  ]).then(([response]) => response);
  expect(badPassword.status()).toBe(401);
  await expect(page.getByRole('heading', { name: 'Log in' })).toBeVisible();
  await expect(page.getByText('The password is incorrect.')).toBeVisible();
  await expect(page.getByText('No account with that username.')).not.toBeVisible();

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

test('profile-picture thumbnails are used for compact avatars with original images preserved for full profile UI', async ({ page }) => {
  const user = `pfp_${Date.now()}`;
  const password = 'very secure password';
  const firstPicture = fixturePath('tiny.png');
  const secondPicture = generatedPng('replacement-profile-picture.png');

  await register(page, user, password);
  await page.goto('/settings');
  await page.getByLabel('Display name').fill('Profile Thumb QA');
  await page.getByLabel('Profile picture', { exact: true }).setInputFiles(firstPicture);
  await Promise.all([
    page.waitForURL('/settings'),
    page.getByRole('button', { name: 'Save settings' }).click(),
  ]);

  const settingsPicture = page.locator('img.profile-picture').first();
  await expect(settingsPicture).toBeVisible();
  const originalSrc = await settingsPicture.getAttribute('src');
  expect(originalSrc).toMatch(/^\/uploads\/(?:images|originals)\//);
  expect(originalSrc).not.toContain('/uploads/thumbs/');
  await expectReachableImage(page, originalSrc!);

  await page.goto(`/users/${user}`);
  const fullProfileSrc = await page.locator('section.profile img.profile-picture').getAttribute('src');
  expect(fullProfileSrc).toBe(originalSrc);

  const postText = `Profile thumbnail check ${Date.now()}`;
  await createPost(page, postText);
  const article = page.locator('article[data-post-id]').filter({ hasText: postText }).first();
  const compactAvatar = article.locator('img.post-avatar');
  await expect(compactAvatar).toBeVisible();
  const compactSrc = await compactAvatar.getAttribute('src');
  expect(compactSrc).toMatch(/^\/uploads\/(?:thumbs|images|originals)\//);
  await expectReachableImage(page, compactSrc!);
  if (compactSrc!.includes('/uploads/thumbs/')) {
    expect(compactSrc).toMatch(/^\/uploads\/thumbs\/\d+-profile\.webp$/);
    const thumbnail = await page.request.get(compactSrc!);
    expect(thumbnail.headers()['content-type'] ?? '').toMatch(/^image\/webp\b/);
  } else {
    expect(compactSrc).toBe(originalSrc);
  }

  await page.goto('/settings');
  await page.getByLabel('Profile picture', { exact: true }).setInputFiles(secondPicture);
  await Promise.all([
    page.waitForURL('/settings'),
    page.getByRole('button', { name: 'Save settings' }).click(),
  ]);
  const replacementOriginalSrc = await page.locator('img.profile-picture').first().getAttribute('src');
  expect(replacementOriginalSrc).toMatch(/^\/uploads\/(?:images|originals)\//);
  expect(replacementOriginalSrc).not.toBe(originalSrc);
  expect(replacementOriginalSrc).not.toContain('/uploads/thumbs/');
  await expectReachableImage(page, replacementOriginalSrc!);

  await page.goto('/home');
  const replacedCompactSrc = await article.locator('img.post-avatar').getAttribute('src');
  expect(replacedCompactSrc).not.toBe(compactSrc);
  expect(replacedCompactSrc).not.toBe(originalSrc);
  await expectReachableImage(page, replacedCompactSrc!);
  if (replacedCompactSrc!.includes('/uploads/thumbs/')) {
    expect(replacedCompactSrc).toMatch(/^\/uploads\/thumbs\/\d+-profile\.webp$/);
    const replacementThumbnail = await page.request.get(replacedCompactSrc!);
    expect(replacementThumbnail.headers()['content-type'] ?? '').toMatch(/^image\/webp\b/);
  } else {
    expect(replacedCompactSrc).toBe(replacementOriginalSrc);
  }

  await page.goto('/settings');
  await page.getByLabel('Delete profile picture').check();
  await Promise.all([
    page.waitForURL('/settings'),
    page.getByRole('button', { name: 'Save settings' }).click(),
  ]);
  await expect(page.locator('img.profile-picture')).toHaveCount(0);

  await page.goto('/home');
  const placeholderArticle = page.locator('article[data-post-id]').filter({ hasText: postText }).first();
  await expect(placeholderArticle.locator('span.post-avatar.placeholder')).toBeVisible();
  await expect(placeholderArticle.locator('img.post-avatar')).toHaveCount(0);
});

test('custom favicon can be uploaded, replaced, reset, and rejects unsupported uploads', async ({ page }) => {
  const firstFavicon = fixturePath('tiny.png');
  const secondFavicon = generatedPng('replacement-favicon.png');
  const invalidFavicon = writeGeneratedFixture('invalid-favicon.txt', 'not a favicon');

  const home = await page.goto('/home');
  expect(home?.status()).toBeLessThan(400);
  const faviconLink = page.locator('link[rel="icon"]');
  await expect(faviconLink).toHaveAttribute('href', '/favicon.ico');

  let favicon = await page.request.get('/favicon.ico');
  expect(favicon.status()).toBe(200);
  expect(favicon.headers()['content-type']).toMatch(/^image\/x-icon\b/);
  expect(favicon.headers()['cache-control']).toBe('public, max-age=3600');
  expect(favicon.headers()['x-content-type-options']).toBe('nosniff');
  const defaultBytes = await favicon.body();

  await login(page, adminUser, adminPassword);
  await page.goto('/admin');
  await page.getByLabel('Upload favicon').setInputFiles(firstFavicon);
  await Promise.all([
    page.waitForURL('/admin'),
    page.getByRole('button', { name: 'Save favicon' }).click(),
  ]);
  await expect(page.getByText('Custom favicon configured')).toBeVisible();

  favicon = await page.request.get('/favicon.ico');
  expect(favicon.status()).toBe(200);
  expect(favicon.headers()['content-type']).toMatch(/^image\/png\b/);
  expect(favicon.headers()['cache-control']).toBe('public, max-age=3600');
  expect(favicon.headers()['x-content-type-options']).toBe('nosniff');
  const firstBytes = await favicon.body();
  expect(firstBytes.equals(defaultBytes)).toBe(false);

  await page.getByLabel('Upload favicon').setInputFiles(secondFavicon);
  await Promise.all([
    page.waitForURL('/admin'),
    page.getByRole('button', { name: 'Save favicon' }).click(),
  ]);
  favicon = await page.request.get('/favicon.ico');
  const replacementBytes = await favicon.body();
  expect(favicon.headers()['content-type']).toMatch(/^image\/png\b/);
  expect(replacementBytes.equals(firstBytes)).toBe(false);

  await page.getByLabel('Upload favicon').setInputFiles(invalidFavicon);
  const invalidUpload = await Promise.all([
    page.waitForResponse((response) => response.url().includes('/admin/favicon') && response.request().method() === 'POST'),
    page.getByRole('button', { name: 'Save favicon' }).click(),
  ]).then(([response]) => response);
  expect(invalidUpload.status()).toBe(400);
  await expect(page.getByRole('heading', { name: 'Check the form' })).toBeVisible();
  await expect(page.getByText('unsupported favicon type; upload .ico, .png, or .svg')).toBeVisible();
  favicon = await page.request.get('/favicon.ico');
  expect((await favicon.body()).equals(replacementBytes)).toBe(true);

  await page.goto('/admin');
  await page.getByRole('button', { name: 'Reset to the built-in favicon' }).click();
  await expect(page).toHaveURL(/\/admin/);
  await expect(page.getByText('Using built-in default favicon')).toBeVisible();
  favicon = await page.request.get('/favicon.ico');
  expect(favicon.headers()['content-type']).toMatch(/^image\/x-icon\b/);
  expect((await favicon.body()).equals(defaultBytes)).toBe(true);
});
