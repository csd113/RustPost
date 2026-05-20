import { test, expect, type Locator, type Page } from '@playwright/test';
import {
  assertNoQaFailures,
  createPost,
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

function resultArticle(page: Page, text: string): Locator {
  return page.locator('article[data-post-id]').filter({ hasText: text }).first();
}

async function submitSearch(page: Page, query: string): Promise<void> {
  await page.getByLabel(/Search RustPost/).fill(query);
  await Promise.all([
    page.waitForURL(/\/search\?q=/),
    page.getByRole('button', { name: 'Search' }).click(),
  ]);
  await expectNotErrorPage(page);
}

test('logged-out search page loads and shows the empty prompt', async ({ page }) => {
  const response = await page.goto('/search');

  expect(response?.status()).toBeLessThan(400);
  await expect(page.getByRole('heading', { name: 'Search', exact: true })).toBeVisible();
  await expect(page.getByPlaceholder('Search posts, @users, or #tags')).toBeVisible();
  await expect(page.getByText('Search for posts, usernames, mentions, or hashtags.')).toBeVisible();
});

test('search states, result cards, and actions work while logged in', async ({ page }) => {
  const username = uniqueName('searchalice');
  const token = uniqueName('needle');
  const postText = `Searchable ${token} post about #rust mentions @${username}`;

  await register(page, username, password);
  await page.goto('/search');
  await expect(page.getByRole('heading', { name: 'Search', exact: true })).toBeVisible();
  await expect(page.getByText('Search for posts, usernames, mentions, or hashtags.')).toBeVisible();

  const postId = await createPost(page, postText);

  await page.goto('/search');
  await submitSearch(page, token);
  await expect(page.getByRole('searchbox', { name: /Search RustPost/ })).toHaveValue(token);
  await expect(page.getByRole('heading', { name: `1 result for "${token}"` })).toBeVisible();
  await expect(resultArticle(page, postText)).toBeVisible();

  await resultArticle(page, postText).getByRole('button', { name: 'Like' }).click();
  await expect(page).toHaveURL(new RegExp(`/search\\?q=${token}$`));
  await expect(resultArticle(page, postText).getByRole('button', { name: 'Unlike' })).toBeVisible();

  await Promise.all([
    page.waitForURL(new RegExp(`/posts/${postId}$`)),
    resultArticle(page, postText).locator('.text').click(),
  ]);
  await expect(page.locator(`article[data-post-id="${postId}"]`)).toBeVisible();

  await page.goto('/search');
  await submitSearch(page, '#rust');
  await expect(resultArticle(page, postText)).toBeVisible();

  await page.goto('/search');
  await submitSearch(page, `@${username}`);
  await expect(page.getByLabel('People').getByRole('link', { name: username, exact: true })).toBeVisible();
  await expect(resultArticle(page, postText)).toBeVisible();

  await page.goto('/search');
  await submitSearch(page, 'NEAR OR !!!');
  await expectNotErrorPage(page);

  const missing = uniqueName('missing');
  await page.goto('/search');
  await submitSearch(page, missing);
  await expect(page.getByRole('heading', { name: 'No results found' })).toBeVisible();
  await expect(page.getByText(`No matches for "${missing}".`)).toBeVisible();
});

test('mobile viewport keeps the search input and results readable', async ({ page }) => {
  const username = uniqueName('searchmobile');
  const token = uniqueName('mobile');
  const postText = `Mobile search result ${token} #rust`;

  await page.setViewportSize({ width: 390, height: 844 });
  await register(page, username, password);
  await createPost(page, postText);

  await page.goto('/search');
  await submitSearch(page, token);

  const input = page.getByRole('searchbox', { name: /Search RustPost/ });
  const article = resultArticle(page, postText);
  await expect(input).toBeVisible();
  await expect(article).toBeVisible();

  const inputBox = await input.boundingBox();
  const articleBox = await article.boundingBox();
  expect(inputBox?.width).toBeLessThanOrEqual(390);
  expect(articleBox?.width).toBeLessThanOrEqual(390);

  await logout(page);
});
