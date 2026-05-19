import { test, expect, type Locator, type Page } from '@playwright/test';
import {
  adminPassword,
  adminUser,
  assertNoQaFailures,
  createPost,
  expectNotErrorPage,
  fixturePath,
  goBackToHealthyPage,
  goForwardToHealthyPage,
  installQaGuards,
  login,
  logout,
  openPostThread,
  register,
  uniqueName,
} from './helpers';

const password = 'very secure password';

test.beforeEach(async ({ page }, testInfo) => {
  await installQaGuards(page, testInfo);
});

test.afterEach(async ({ page }) => {
  assertNoQaFailures(page);
});

function postArticle(page: Page, text: string): Locator {
  return page.locator('article[data-post-id]').filter({ hasText: text }).first();
}

async function clickAndStayHealthy(page: Page, control: Locator): Promise<void> {
  await control.click();
  await page.waitForLoadState('domcontentloaded');
  await expectNotErrorPage(page);
}

async function registerFresh(page: Page, prefix: string): Promise<string> {
  const username = uniqueName(prefix);
  await register(page, username, password);
  await expectNotErrorPage(page);
  return username;
}

test('register, login, and logout survive restored browser history', async ({ page }) => {
  const user = uniqueName('auth');

  await page.goto('/register');
  await expectNotErrorPage(page);
  await page.goto('/login');
  await goBackToHealthyPage(page);
  await page.getByLabel('Username').fill(user);
  await page.getByLabel('Password', { exact: true }).fill(password);
  await page.getByLabel('Confirm password').fill(password);
  await Promise.all([
    page.waitForURL(/\/home/),
    page.getByRole('button', { name: 'Create account' }).click(),
  ]);
  await expectNotErrorPage(page);

  await page.goto('/settings');
  await expect(page.getByRole('heading', { name: 'Account settings' })).toBeVisible();
  await page.goto('/search');
  await goBackToHealthyPage(page);
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Log out' }));
  await expect(page.getByRole('link', { name: 'Log in' })).toBeVisible();

  await page.goto('/login');
  await goBackToHealthyPage(page);
  await goForwardToHealthyPage(page);
  await page.getByLabel('Username').fill(user);
  await page.getByLabel('Password', { exact: true }).fill(password);
  await Promise.all([
    page.waitForURL(/\/home/),
    page.getByRole('button', { name: 'Log in' }).click(),
  ]);
  await expectNotErrorPage(page);
});

test('composer, reply, and profile settings forms survive restored browser history', async ({ page }) => {
  const user = await registerFresh(page, 'forms');
  const postText = `history composer ${uniqueName('post')}`;

  await page.goto('/home');
  await page.goto('/search');
  await goBackToHealthyPage(page);
  await page.getByLabel('What is happening?').fill(postText);
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Post', exact: true }));
  await expect(postArticle(page, postText)).toBeVisible();
  const postId = await postArticle(page, postText).getAttribute('data-post-id');
  expect(postId).not.toBeNull();

  const replyText = `history reply ${uniqueName('reply')}`;
  await openPostThread(page, postId!);
  await goBackToHealthyPage(page);
  await goForwardToHealthyPage(page);
  await page.getByLabel('What is happening?').fill(replyText);
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Post', exact: true }));
  await expect(page.getByText(replyText)).toBeVisible();
  await expect(page).toHaveURL(/\/posts\/\d+/);

  await page.goto('/settings');
  await page.getByLabel('Display name').fill('History Profile');
  await page.getByLabel('Bio').fill('First profile update from a history test.');
  await page.getByLabel('Website').fill(`https://example.com/${user}`);
  await page.getByLabel('Profile picture').setInputFiles(fixturePath('tiny.png'));
  await page.getByLabel('Banner').setInputFiles(fixturePath('tiny.png'));
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Save settings' }));
  await expect(page.getByLabel('Display name')).toHaveValue('History Profile');

  await goBackToHealthyPage(page);
  await expect(page.getByRole('heading', { name: 'Account settings' })).toBeVisible();
  await page.getByLabel('Bio').fill('Second profile update from a restored page.');
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Save settings' }));
  await expect(page.getByLabel('Bio')).toHaveValue('Second profile update from a restored page.');
});

test('own-post like and bookmark toggles stay on valid pages after Back and Forward', async ({ page }) => {
  await registerFresh(page, 'own');
  const postText = `own action ${uniqueName('post')}`;
  await createPost(page, postText);
  await expectNotErrorPage(page);
  const article = postArticle(page, postText);

  await page.goto('/search');
  await goBackToHealthyPage(page);
  await expect(article).toBeVisible();
  await clickAndStayHealthy(page, article.getByRole('button', { name: 'Like' }));
  await expect(article.getByRole('button', { name: 'Unlike' })).toBeVisible();

  await goBackToHealthyPage(page);
  await goForwardToHealthyPage(page);
  await clickAndStayHealthy(page, article.locator('[data-action-kind="like"]'));
  await expect(article.getByRole('button', { name: 'Like' })).toBeVisible();

  await page.goto('/search');
  await goBackToHealthyPage(page);
  await expect(article).toBeVisible();
  await clickAndStayHealthy(page, article.getByRole('button', { name: 'Bookmark' }));
  await expect(article.getByRole('button', { name: 'Unbookmark' })).toBeVisible();

  await goBackToHealthyPage(page);
  await goForwardToHealthyPage(page);
  await clickAndStayHealthy(page, article.locator('[data-action-kind="bookmark"]'));
  await expect(article.getByRole('button', { name: 'Bookmark' })).toBeVisible();
});

test('repost, follow, block, unblock, and mute keep profile context after history restores', async ({ page }) => {
  const alice = await registerFresh(page, 'alice');
  const postText = `social action ${uniqueName('post')}`;
  await createPost(page, postText);
  await logout(page);

  await registerFresh(page, 'bob');
  await page.goto(`/users/${alice}`);
  await expectNotErrorPage(page);
  const article = postArticle(page, postText);

  await page.goto('/search');
  await goBackToHealthyPage(page);
  await expect(article).toBeVisible();
  await clickAndStayHealthy(page, article.getByRole('button', { name: 'Repost' }));
  await expect(article.getByRole('button', { name: 'Unrepost' })).toBeVisible();

  await goBackToHealthyPage(page);
  await goForwardToHealthyPage(page);
  await clickAndStayHealthy(page, article.locator('[data-action-kind="repost"]'));
  await expect(article.getByRole('button', { name: 'Repost' })).toBeVisible();

  await page.goto('/search');
  await goBackToHealthyPage(page);
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Follow this account' }));
  await expect(page).toHaveURL(new RegExp(`/users/${alice}`));
  await expect(page.getByRole('button', { name: 'Unfollow this account' })).toBeVisible();

  await goBackToHealthyPage(page);
  await goForwardToHealthyPage(page);
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Unfollow this account' }));
  await expect(page).toHaveURL(new RegExp(`/users/${alice}`));
  await expect(page.getByRole('button', { name: 'Follow this account' })).toBeVisible();

  await page.goto('/search');
  await goBackToHealthyPage(page);
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Block this account' }));
  await expect(page).toHaveURL(new RegExp(`/users/${alice}`));

  await page.goto('/settings');
  await page.goto('/home');
  await goBackToHealthyPage(page);
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Unblock this account' }));
  await expect(page).toHaveURL(/\/settings/);

  await page.goto(`/users/${alice}`);
  await page.goto('/search');
  await goBackToHealthyPage(page);
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Mute this account' }));
  await expect(page).toHaveURL(new RegExp(`/users/${alice}`));
});

test('delete and notification forms remain healthy from restored pages', async ({ page }) => {
  await registerFresh(page, 'delete');
  const postText = `delete action ${uniqueName('post')}`;
  await createPost(page, postText);
  await expectNotErrorPage(page);

  await postArticle(page, postText).getByRole('link', { name: 'Delete', exact: true }).click();
  await expect(page.getByRole('heading', { name: 'Delete post?' })).toBeVisible();
  await goBackToHealthyPage(page);
  await goForwardToHealthyPage(page);
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Confirm delete' }));
  await expect(page).toHaveURL(/\/home/);
  await expect(page.getByText(postText)).toHaveCount(0);

  await page.goto('/notifications');
  await page.goto('/home');
  await goBackToHealthyPage(page);
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Mark all read' }));
  await expect(page).toHaveURL(/\/notifications/);
});

test('admin suspend and backup forms survive restored admin pages', async ({ page }) => {
  const managedUser = await registerFresh(page, 'managed');
  await logout(page);
  await login(page, adminUser, adminPassword);
  await expectNotErrorPage(page);

  await page.goto('/admin/users');
  await page.goto('/admin');
  await goBackToHealthyPage(page);
  const managedRow = page.locator('tr').filter({ hasText: managedUser }).first();
  await clickAndStayHealthy(page, managedRow.getByRole('button', { name: 'Suspend' }));
  await expect(managedRow.getByRole('button', { name: 'Unsuspend' })).toBeVisible();

  await goBackToHealthyPage(page);
  await goForwardToHealthyPage(page);
  await clickAndStayHealthy(page, managedRow.getByRole('button', { name: 'Unsuspend' }));
  await expect(managedRow.getByRole('button', { name: 'Suspend' })).toBeVisible();

  await page.goto('/admin/backups');
  await page.goto('/admin');
  await goBackToHealthyPage(page);
  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Create backup' }));
  await expect(page.getByText('Backup created:')).toBeVisible();
});

test.describe('no JavaScript fallback', () => {
  test.use({ javaScriptEnabled: false });

  test('composer, reply, like, bookmark, and logout forms work from restored pages without JavaScript', async ({ page }) => {
    await registerFresh(page, 'nojs');

    const firstPost = `nojs first ${uniqueName('post')}`;
    await page.goto('/home');
    await page.getByLabel('What is happening?').fill(firstPost);
    await Promise.all([
      page.waitForURL(/\/home#post-\d+/),
      page.getByRole('button', { name: 'Post', exact: true }).click(),
    ]);
    await expectNotErrorPage(page);
    await expect(page.getByText(firstPost)).toBeVisible();

    const secondPost = `nojs restored composer ${uniqueName('post')}`;
    await goBackToHealthyPage(page);
    await page.getByLabel('What is happening?').fill(secondPost);
    await Promise.all([
      page.waitForURL(/\/home#post-\d+/),
      page.getByRole('button', { name: 'Post', exact: true }).click(),
    ]);
    await expectNotErrorPage(page);
    await expect(page.getByText(secondPost)).toBeVisible();
    const secondId = await postArticle(page, secondPost).getAttribute('data-post-id');
    expect(secondId).not.toBeNull();

    await clickAndStayHealthy(page, postArticle(page, secondPost).getByRole('button', { name: 'Like' }));
    await expect(postArticle(page, secondPost).getByRole('button', { name: 'Unlike' })).toBeVisible();
    await goBackToHealthyPage(page);
    await goForwardToHealthyPage(page);
    await clickAndStayHealthy(page, postArticle(page, secondPost).getByRole('button', { name: 'Unlike' }));
    await expect(postArticle(page, secondPost).getByRole('button', { name: 'Like' })).toBeVisible();

    await clickAndStayHealthy(page, postArticle(page, secondPost).getByRole('button', { name: 'Bookmark' }));
    await expect(postArticle(page, secondPost).getByRole('button', { name: 'Unbookmark' })).toBeVisible();
    await goBackToHealthyPage(page);
    await goForwardToHealthyPage(page);
    await clickAndStayHealthy(page, postArticle(page, secondPost).getByRole('button', { name: 'Unbookmark' }));
    await expect(postArticle(page, secondPost).getByRole('button', { name: 'Bookmark' })).toBeVisible();

    const replyText = `nojs reply ${uniqueName('reply')}`;
    await openPostThread(page, secondId!);
    await goBackToHealthyPage(page);
    await goForwardToHealthyPage(page);
    await page.getByLabel('What is happening?').fill(replyText);
    await Promise.all([
      page.waitForURL(/\/posts\/\d+#reply-\d+/),
      page.getByRole('button', { name: 'Post', exact: true }).click(),
    ]);
    await expectNotErrorPage(page);
    await expect(page.getByText(replyText)).toBeVisible();

    await page.goto('/settings');
    await page.goto('/search');
    await goBackToHealthyPage(page);
    await clickAndStayHealthy(page, page.getByRole('button', { name: 'Log out' }));
    await expect(page.getByRole('link', { name: 'Log in' })).toBeVisible();
  });
});

test('CSRF rejects missing, invalid, and cross-session tokens', async ({ page, browser }) => {
  const alice = await registerFresh(page, 'negativea');
  const postId = await createPost(page, `negative csrf ${uniqueName('post')}`);
  await page.goto('/home');
  const aliceToken = await page.locator('input[name="csrf"]').first().inputValue();

  const missing = await page.request.post('/posts', {
    multipart: {
      text: 'missing csrf should not post',
    },
  });
  expect(missing.status()).toBe(403);

  const invalid = await page.request.post('/posts', {
    multipart: {
      csrf: 'not-a-valid-csrf-token',
      text: 'invalid csrf should not post',
    },
  });
  expect(invalid.status()).toBe(403);

  const context = await browser.newContext({ baseURL: page.url().replace(/\/home.*/, '') });
  const bobPage = await context.newPage();
  await installQaGuards(bobPage, test.info());
  await register(bobPage, uniqueName('negativeb'), password);
  await expectNotErrorPage(bobPage);
  const crossSession = await bobPage.request.post(`/posts/${postId}/like`, {
    form: { csrf: aliceToken },
    headers: { referer: `/users/${alice}` },
  });
  expect(crossSession.status()).toBe(403);
  await context.close();
});
