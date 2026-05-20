import { test, expect, type Locator, type Page } from '@playwright/test';
import {
  assertNoQaFailures,
  createPost,
  expectHealthyRedirect,
  expectNotErrorPage,
  installQaGuards,
  logout,
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
  await page.waitForLoadState('networkidle');
  await expectNotErrorPage(page);
}

async function registerFresh(page: Page, prefix: string): Promise<string> {
  const username = uniqueName(prefix);
  await register(page, username, password);
  await expectNotErrorPage(page);
  return username;
}

test('normal post actions preserve feed, thread, anchors, and profile context', async ({ page }) => {
  const alice = await registerFresh(page, 'navalice');
  const postText = `navigation source ${uniqueName('post')}`;
  const postId = await createPost(page, postText);
  const homePostUrl = new RegExp(`/home#post-${postId}$`);
  await expect(page).toHaveURL(homePostUrl);

  const homeArticle = postArticle(page, postText);
  await clickAndStayHealthy(page, homeArticle.getByRole('button', { name: 'Like' }));
  await expect(page).toHaveURL(homePostUrl);
  await expect(homeArticle.getByRole('button', { name: 'Unlike' })).toBeVisible();

  await clickAndStayHealthy(page, homeArticle.getByRole('button', { name: 'Bookmark' }));
  await expect(page).toHaveURL(homePostUrl);
  await expect(homeArticle.getByRole('button', { name: 'Unbookmark' })).toBeVisible();

  await page.goto(`/posts/${postId}`);
  await expect(page.locator(`article[data-post-id="${postId}"]`)).toBeVisible();
  await expectNotErrorPage(page);
  const threadUrl = new RegExp(`/posts/${postId}$`);
  const threadArticle = postArticle(page, postText);

  await clickAndStayHealthy(page, threadArticle.getByRole('button', { name: 'Unlike' }));
  await expect(page).toHaveURL(threadUrl);
  await clickAndStayHealthy(page, threadArticle.getByRole('button', { name: 'Unbookmark' }));
  await expect(page).toHaveURL(threadUrl);

  const replyText = `anchored reply ${uniqueName('reply')}`;
  await page.getByLabel('What is happening?').fill(replyText);
  await Promise.all([
    page.waitForURL(/\/posts\/\d+#reply-\d+$/),
    page.getByRole('button', { name: 'Post', exact: true }).click(),
  ]);
  await expectNotErrorPage(page);
  const replyId = new URL(page.url()).hash.replace('#reply-', '');
  expect(replyId, 'reply id in URL fragment').toMatch(/^\d+$/);
  await expect(page.locator(`#reply-${replyId}`)).toBeAttached();
  await expect(postArticle(page, replyText)).toBeVisible();

  await postArticle(page, replyText).getByRole('link', { name: 'Delete' }).click();
  await expect(page.getByRole('heading', { name: 'Delete post?' })).toBeVisible();
  await expectHealthyRedirect(
    page,
    () => page.getByRole('button', { name: 'Confirm delete' }).click(),
    new RegExp(`/posts/${postId}#post-${postId}$`),
  );
  await expect(page.getByText(replyText)).toHaveCount(0);

  await logout(page);
  await registerFresh(page, 'navbob');
  await page.goto(`/users/${alice}`);
  await expectNotErrorPage(page);
  const profileUrl = new RegExp(`/users/${alice}$`);
  const profileArticle = postArticle(page, postText);

  await clickAndStayHealthy(page, profileArticle.getByRole('button', { name: 'Repost' }));
  await expect(page).toHaveURL(profileUrl);
  await expect(profileArticle.getByRole('button', { name: 'Unrepost' })).toBeVisible();

  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Follow this account' }));
  await expect(page).toHaveURL(profileUrl);
  await expect(page.getByRole('button', { name: 'Unfollow this account' })).toBeVisible();

  await expectHealthyRedirect(
    page,
    () => page.getByRole('button', { name: 'Block this account' }).click(),
    profileUrl,
  );
  await page.goto('/settings');
  await expectHealthyRedirect(
    page,
    () => page.getByRole('button', { name: 'Unblock this account' }).click(),
    /\/settings$/,
  );

  await page.goto(`/users/${alice}`);
  await expectHealthyRedirect(
    page,
    () => page.getByRole('button', { name: 'Mute this account' }).click(),
    profileUrl,
  );
});

test('post cards open threads except for the current root post inside its own thread', async ({ page }) => {
  await registerFresh(page, 'threadnav');
  const postText = `thread navigation ${uniqueName('post')}`;
  const postId = await createPost(page, postText);
  const homeArticle = postArticle(page, postText);

  await Promise.all([
    page.waitForURL(new RegExp(`/posts/${postId}$`)),
    homeArticle.locator('.text').click(),
  ]);
  await expectNotErrorPage(page);
  await expect(page.getByRole('heading', { name: 'Thread' })).toHaveCount(0);
  const backLink = page.getByRole('link', { name: 'Back' });
  await expect(backLink).toHaveAttribute('href', '/home');

  const rootArticle = postArticle(page, postText);
  const backBox = await backLink.boundingBox();
  const rootBox = await rootArticle.boundingBox();
  expect(backBox, 'back control should be visible').not.toBeNull();
  expect(rootBox, 'root post should be visible').not.toBeNull();
  expect(backBox!.y).toBeLessThan(rootBox!.y);
  expect(backBox!.x).toBeLessThan(rootBox!.x + 80);

  await Promise.all([
    page.waitForURL(new RegExp(`/home#post-${postId}$`)),
    backLink.click(),
  ]);
  await expect(postArticle(page, postText)).toBeVisible();
  await Promise.all([
    page.waitForURL(new RegExp(`/posts/${postId}$`)),
    postArticle(page, postText).locator('.text').click(),
  ]);
  await expectNotErrorPage(page);

  const reopenedRootArticle = postArticle(page, postText);
  await expect(reopenedRootArticle).not.toHaveAttribute('data-card-href', `/posts/${postId}`);
  await page.evaluate(() => {
    (window as Window & { __threadRootClickMarker?: string }).__threadRootClickMarker = 'alive';
  });
  const threadUrl = page.url();
  await reopenedRootArticle.locator('.text').click();
  await page.waitForTimeout(300);
  expect(page.url()).toBe(threadUrl);
  await expect
    .poll(() =>
      page.evaluate(() => (window as Window & { __threadRootClickMarker?: string }).__threadRootClickMarker),
    )
    .toBe('alive');

  await page.goto('/home');
  await Promise.all([
    page.waitForURL(new RegExp(`/posts/${postId}$`)),
    postArticle(page, postText).locator('.text').click(),
  ]);
  await expectNotErrorPage(page);
});

test('Back and Forward restore current like, bookmark, repost, follow, reply, and settings state', async ({ page }) => {
  const alice = await registerFresh(page, 'histalice');
  const postText = `history state ${uniqueName('post')}`;
  const postId = await createPost(page, postText);

  await page.goto('/search');
  await page.goBack({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
  const article = postArticle(page, postText);

  await clickAndStayHealthy(page, article.getByRole('button', { name: 'Like' }));
  await page.goBack({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
  await page.goForward({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
  await expect(postArticle(page, postText).getByRole('button', { name: 'Unlike' })).toBeVisible();

  await clickAndStayHealthy(page, postArticle(page, postText).getByRole('button', { name: 'Bookmark' }));
  await page.goBack({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
  await page.goForward({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
  await expect(postArticle(page, postText).getByRole('button', { name: 'Unbookmark' })).toBeVisible();

  await page.goto(`/posts/${postId}`);
  const replyText = `history visible reply ${uniqueName('reply')}`;
  await page.getByLabel('What is happening?').fill(replyText);
  await Promise.all([
    page.waitForURL(/\/posts\/\d+#reply-\d+$/),
    page.getByRole('button', { name: 'Post', exact: true }).click(),
  ]);
  await page.goBack({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
  await page.goForward({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
  await expect(page.getByText(replyText)).toBeVisible();
  await expect(postArticle(page, postText).locator('[data-count="replies"]')).toHaveText('1 replies');

  await page.goto('/settings');
  await page.getByLabel('Display name').fill('History Current');
  await expectHealthyRedirect(page, () => page.getByRole('button', { name: 'Save settings' }).click(), /\/settings$/);
  await page.goto('/search');
  await page.goBack({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
  await expect(page.getByLabel('Display name')).toHaveValue('History Current');

  await logout(page);
  await registerFresh(page, 'histbob');
  await page.goto(`/users/${alice}`);
  await clickAndStayHealthy(page, postArticle(page, postText).getByRole('button', { name: 'Repost' }));
  await page.goto('/search');
  await page.goBack({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
  await expect(postArticle(page, postText).getByRole('button', { name: 'Unrepost' })).toBeVisible();

  await clickAndStayHealthy(page, page.getByRole('button', { name: 'Follow this account' }));
  await page.goto('/search');
  await page.goBack({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
  await expect(page.getByRole('button', { name: 'Unfollow this account' })).toBeVisible();
});

test('rapid clicks and duplicate submits do not create duplicate records or raw errors', async ({ page }) => {
  await registerFresh(page, 'dupealice');

  const postText = `double post ${uniqueName('post')}`;
  await page.goto('/home');
  await page.getByLabel('What is happening?').fill(postText);
  await page.getByRole('button', { name: 'Post', exact: true }).dblclick();
  await expect(postArticle(page, postText)).toHaveCount(1);
  await expectNotErrorPage(page);

  const postId = await postArticle(page, postText).getAttribute('data-post-id');
  expect(postId).not.toBeNull();
  await page.goto(`/posts/${postId}`);
  const replyText = `double reply ${uniqueName('reply')}`;
  await page.getByLabel('What is happening?').fill(replyText);
  await page.getByRole('button', { name: 'Post', exact: true }).dblclick();
  await expect(postArticle(page, replyText)).toHaveCount(1);
  await expectNotErrorPage(page);

  await postArticle(page, replyText).getByRole('link', { name: 'Delete' }).click();
  await expect(page.getByRole('heading', { name: 'Delete post?' })).toBeVisible();
  const deletePostId = new URL(page.url()).pathname.match(/\d+/)?.[0];
  expect(deletePostId).toBeDefined();
  const deleteForm = page.locator('main form').first();
  const csrf = await deleteForm.locator('input[name="csrf"]').inputValue();
  const returnTo = await deleteForm.locator('input[name="return_to"]').inputValue();
  const firstDelete = await page.request.post(`/posts/${deletePostId}/delete`, {
    form: { csrf, return_to: returnTo },
  });
  expect(firstDelete.status(), 'first delete should land on a valid page').toBeLessThan(400);
  const secondDelete = await page.request.post(`/posts/${deletePostId}/delete`, {
    form: { csrf, return_to: returnTo },
  });
  expect(secondDelete.status(), 'replayed delete should land on a valid page').toBeLessThan(400);

  await page.goto('/home');
  const actionArticle = postArticle(page, postText);
  await actionArticle.getByRole('button', { name: 'Like' }).dblclick();
  await expectNotErrorPage(page);
  await expect(actionArticle.locator('[data-action-kind="like"]')).toBeEnabled();

  await actionArticle.getByRole('button', { name: 'Bookmark' }).dblclick();
  await expectNotErrorPage(page);
  await expect(actionArticle.locator('[data-action-kind="bookmark"]')).toBeEnabled();

  await logout(page);
  const bob = await registerFresh(page, 'dupebob');
  await logout(page);
  await page.goto('/login');
  await page.getByLabel('Username').fill(bob);
  await page.getByLabel('Password', { exact: true }).fill(password);
  await Promise.all([
    page.waitForURL(/\/home/),
    page.getByRole('button', { name: 'Log in' }).dblclick(),
  ]);
  await expectNotErrorPage(page);

  await logout(page);
  const registerUser = uniqueName('duperegister');
  await page.goto('/register');
  await page.getByLabel('Username').fill(registerUser);
  await page.getByLabel('Password', { exact: true }).fill(password);
  await page.getByLabel('Confirm password').fill(password);
  await Promise.all([
    page.waitForURL(/\/home/),
    page.getByRole('button', { name: 'Create account' }).dblclick(),
  ]);
  await expectNotErrorPage(page);
});

test.describe('no JavaScript navigation parity', () => {
  test.use({ javaScriptEnabled: false });

  test('core no-JS forms redirect to valid contextual pages', async ({ page }) => {
    const alice = await registerFresh(page, 'nojsnavalice');
    const postText = `nojs navigation ${uniqueName('post')}`;

    await page.goto('/home');
    await page.getByLabel('What is happening?').fill(postText);
    await expectHealthyRedirect(
      page,
      () => page.getByRole('button', { name: 'Post', exact: true }).click(),
      /\/home#post-\d+$/,
    );
    const postId = new URL(page.url()).hash.replace('#post-', '');
    await expect(postArticle(page, postText)).toBeVisible();

    await expectHealthyRedirect(
      page,
      () => postArticle(page, postText).getByRole('button', { name: 'Like' }).click(),
      new RegExp(`/home#post-${postId}$`),
    );
    await expect(postArticle(page, postText).getByRole('button', { name: 'Unlike' })).toBeVisible();

    await expectHealthyRedirect(
      page,
      () => postArticle(page, postText).getByRole('button', { name: 'Bookmark' }).click(),
      new RegExp(`/home#post-${postId}$`),
    );

    await page.goto(`/posts/${postId}`);
    const replyText = `nojs anchored reply ${uniqueName('reply')}`;
    await page.getByLabel('What is happening?').fill(replyText);
    await expectHealthyRedirect(
      page,
      () => page.getByRole('button', { name: 'Post', exact: true }).click(),
      /\/posts\/\d+#reply-\d+$/,
    );
    await expect(page.getByText(replyText)).toBeVisible();

    await logout(page);
    await registerFresh(page, 'nojsnavbob');
    await page.goto(`/users/${alice}`);
    await expectHealthyRedirect(
      page,
      () => page.getByRole('button', { name: 'Follow this account' }).click(),
      new RegExp(`/users/${alice}$`),
    );
    await expect(page.getByRole('button', { name: 'Unfollow this account' })).toBeVisible();

    await expectHealthyRedirect(
      page,
      () => postArticle(page, postText).getByRole('button', { name: 'Repost' }).click(),
      new RegExp(`/users/${alice}#post-${postId}$`),
    );
    await expect(postArticle(page, postText).getByRole('button', { name: 'Unrepost' })).toBeVisible();

    await logout(page);
    await expect(page.getByRole('link', { name: 'Log in' })).toBeVisible();
  });
});
