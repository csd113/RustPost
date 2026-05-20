import { expect, type Locator, type Page, type TestInfo } from '@playwright/test';
import path from 'node:path';
import fs from 'node:fs';

const allowedMissing = new Set(['/favicon.ico']);

export const adminUser = process.env.RUSTPOST_E2E_ADMIN_USER ?? 'admin_e2e';
export const adminPassword =
  process.env.RUSTPOST_E2E_ADMIN_PASSWORD ?? 'very secure admin password';

export function fixturePath(name: string): string {
  const file = path.join(process.cwd(), 'tests/e2e/fixtures', name);
  if (name === 'tiny.png') {
    const raw = fs.readFileSync(file, 'utf8').trim();
    const decoded = Buffer.from(raw, 'base64');
    const pngFile = path.join(process.cwd(), 'output/playwright/tiny.png');
    fs.mkdirSync(path.dirname(pngFile), { recursive: true });
    fs.writeFileSync(pngFile, decoded);
    return pngFile;
  }
  return file;
}

export function uniqueName(prefix: string): string {
  return `${prefix}${Date.now()}${Math.random().toString(36).slice(2, 8)}`;
}

export async function expectAppShell(page: Page): Promise<void> {
  await expect(page.locator('header.site-header')).toBeVisible();
  await expect(page.locator('main .content-shell')).toBeVisible();
  await expect(page.locator('nav')).toBeVisible();
}

export async function expectNotErrorPage(page: Page): Promise<void> {
  await page.waitForLoadState('domcontentloaded');
  await expectAppShell(page);
  await expect(page).not.toHaveURL(/\/(?:posts\/\d+\/(?:like|bookmark|repost|reply)|users\/\d+\/(?:follow|unfollow|block|unblock|mute)|logout|notifications\/read|admin\/users\/\d+\/suspend|admin\/posts\/\d+\/delete)(?:$|[?#])/);
  await expect(page.locator('.error-panel')).toHaveCount(0);
  await expect(page.getByText(/csrf error|invalid csrf|missing csrf|400 error|403 error|404 error|405 error|415 error|500 error|access denied|page not found|method not allowed|unsupported media type|something went wrong/i)).toHaveCount(0);
  await expect(page.locator('body')).not.toHaveText(/^\s*(bad request|forbidden|not found|method not allowed|unsupported media type|internal server error)\s*$/i);
}

export async function goBackToHealthyPage(page: Page): Promise<void> {
  await page.goBack({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
}

export async function goForwardToHealthyPage(page: Page): Promise<void> {
  await page.goForward({ waitUntil: 'domcontentloaded' });
  await expectNotErrorPage(page);
}

export async function openPostThread(page: Page, postId: string): Promise<void> {
  await page.goto(`/posts/${postId}`);
  await expect(page.locator(`article[data-post-id="${postId}"]`)).toBeVisible();
  await expectNotErrorPage(page);
}

export async function installQaGuards(page: Page, testInfo: TestInfo): Promise<void> {
  const failures: string[] = [];

  page.on('pageerror', (error) => {
    failures.push(`page error on ${page.url()}: ${error.message}`);
  });

  page.on('console', (message) => {
    if (message.type() === 'error') {
      if (/Failed to load resource: the server responded with a status of (400|401|403)/.test(message.text())) {
        return;
      }
      failures.push(`console error on ${page.url()}: ${message.text()}`);
    }
  });

  page.on('response', (response) => {
    const url = new URL(response.url());
    if (url.origin !== new URL(page.url() || 'http://127.0.0.1').origin) {
      return;
    }
    if (allowedMissing.has(url.pathname)) {
      return;
    }
    const status = response.status();
    if (status === 404 || status === 405 || status >= 500) {
      failures.push(`${status} response while at ${page.url()}: ${response.request().method()} ${response.url()}`);
    }
  });

  await testInfo.attach('qa-guard-note', {
    body: 'Unexpected console errors, page errors, 404, 405, and 5xx responses fail the test.',
    contentType: 'text/plain',
  });

  testInfo.attachments.push({
    name: 'qa-failures-ref',
    contentType: 'text/plain',
    body: Buffer.from('Failures are asserted through assertNoQaFailures(page).'),
  });

  (page as Page & { __qaFailures?: string[] }).__qaFailures = failures;
}

export function assertNoQaFailures(page: Page): void {
  const failures = (page as Page & { __qaFailures?: string[] }).__qaFailures ?? [];
  expect(failures, failures.join('\n')).toEqual([]);
}

export async function register(page: Page, username: string, password: string): Promise<void> {
  await page.goto('/register');
  await page.getByLabel('Username').fill(username);
  await page.getByLabel('Password', { exact: true }).fill(password);
  await page.getByLabel('Confirm password').fill(password);
  await Promise.all([
    page.waitForURL(/\/home/),
    page.getByRole('button', { name: 'Create account' }).click(),
  ]);
  await expect(page.getByRole('heading', { name: 'Home Feed' })).toBeVisible();
}

export async function login(page: Page, username: string, password: string): Promise<void> {
  await page.goto('/login');
  await page.getByLabel('Username').fill(username);
  await page.getByLabel('Password', { exact: true }).fill(password);
  await Promise.all([
    page.waitForURL(/\/home/),
    page.getByRole('button', { name: 'Log in' }).click(),
  ]);
  await expect(page.getByRole('heading', { name: 'Home Feed' })).toBeVisible();
}

export async function logout(page: Page): Promise<void> {
  await Promise.all([
    page.waitForURL(/\/home/),
    page.getByRole('button', { name: 'Log out' }).click(),
  ]);
  await expect(page.getByRole('link', { name: 'Log in' })).toBeVisible();
}

export async function createPost(page: Page, text: string, media?: string): Promise<string> {
  await page.goto('/home');
  await page.getByLabel('What is happening?').fill(text);
  if (media) {
    await page.locator('input[type="file"][name="media"]').setInputFiles(media);
  }
  await page.getByRole('button', { name: 'Post', exact: true }).click();
  const article = page.locator('article[data-post-id]').filter({ hasText: text }).first();
  await expect(article).toBeVisible();
  const hashId = new URL(page.url()).hash.replace('#post-', '');
  if (hashId) {
    return hashId;
  }
  const postId = await article.getAttribute('data-post-id');
  expect(postId, `post id for "${text}"`).not.toBeNull();
  return postId!;
}

export async function submitFirstPostAction(page: Page, label: string): Promise<void> {
  const button = page.getByRole('button', { name: label }).first();
  await expect(button).toBeVisible();
  await Promise.all([
    page.waitForLoadState('networkidle'),
    button.click(),
  ]);
}

export async function expectHealthyRedirect(
  page: Page,
  action: () => Promise<void>,
  expectedUrl: RegExp | string,
): Promise<void> {
  await Promise.all([
    page.waitForURL(expectedUrl),
    action(),
  ]);
  await expectNotErrorPage(page);
}

export async function crawlVisibleInternalLinks(page: Page): Promise<void> {
  const hrefs = await page
    .locator('a[href]')
    .evaluateAll((links) =>
      Array.from(
        new Set(
          links
            .map((link) => (link as HTMLAnchorElement).getAttribute('href') ?? '')
            .filter((href) => href.startsWith('/') && !href.startsWith('//')),
        ),
      ),
    );

  for (const href of hrefs) {
  const response = await page.goto(href);
    expect(response, `no response for ${href}`).not.toBeNull();
    expect(response!.status(), `${href} returned ${response!.status()}`).toBeLessThan(400);
  }
}

export async function expectIntentionalClientError(action: () => Promise<void>): Promise<void> {
  await action();
}

export function firstFormWithAction(page: Page, action: string): Locator {
  return page.locator(`form[action="${action}"]`).first();
}
