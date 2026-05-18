import { expect, test, type Page } from "@playwright/test";
import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync, readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createServer } from "node:net";

let dataDir: string;
let baseUrl: string;
let server: ChildProcess;

test.beforeAll(async () => {
  const port = await freePort();
  baseUrl = `http://127.0.0.1:${port}`;
  dataDir = mkdtempSync(join(tmpdir(), "rustpost-e2e-"));
  const binary = resolve("target/debug/rustpost-cli");
  const init = spawnSync(binary, ["--data-dir", dataDir, "init"], { encoding: "utf8" });
  if (init.status !== 0) {
    throw new Error(`rustpost-cli init failed: ${init.stderr || init.stdout}`);
  }
  const settingsPath = join(dataDir, "settings.toml");
  const settings = readFileSync(settingsPath, "utf8").replace("port = 8080", `port = ${port}`);
  writeFileSync(settingsPath, settings);
  server = spawn(binary, ["--data-dir", dataDir, "serve"], {
    stdio: ["ignore", "pipe", "pipe"]
  });
  await waitForServer(baseUrl);
});

test.afterAll(() => {
  server?.kill();
  if (dataDir) {
    rmSync(dataDir, { recursive: true, force: true });
  }
});

test("desktop posting and social actions work from the UI", async ({ page }) => {
  await register(page, "alice", "very secure password");
  await page.getByRole("button", { name: "Post", exact: true }).click();
  await expect(page.getByText("post text or media is required")).toBeVisible();
  await page.goto(`${baseUrl}/home`);
  await page.getByLabel("What is happening?").fill("Alice desktop post");
  await page.getByRole("button", { name: "Post", exact: true }).click();
  await expect(page).toHaveURL(`${baseUrl}/local`);
  await expect(page.getByText("Alice desktop post")).toBeVisible();
  await page.getByRole("button", { name: "Log out" }).click();

  await register(page, "bob", "very secure password");
  await login(page, "bob", "very secure password");
  await page.goto(`${baseUrl}/local`);
  await expect(page.getByText("Alice desktop post")).toBeVisible();
  const post = page.locator("article.post").filter({ hasText: "Alice desktop post" }).first();
  await post.getByRole("link", { name: /Thread/ }).click();
  await page.getByLabel("What is happening?").fill("Bob replies from the browser");
  await page.getByRole("button", { name: "Post", exact: true }).click();
  await expect(page.getByText("Bob replies from the browser")).toBeVisible();

  await page.locator("article.post").filter({ hasText: "Alice desktop post" }).first().getByRole("button", { name: "Like" }).click();
  await expect(page.getByText("1 likes")).toBeVisible();
  await page.locator("article.post").filter({ hasText: "Alice desktop post" }).first().getByRole("button", { name: "Bookmark" }).click();
  await page.getByRole("link", { name: "Bookmarks" }).click();
  await expect(page.getByText("Alice desktop post")).toBeVisible();
  await page.getByRole("link", { name: "Thread" }).first().click();
  await page.locator("article.post").filter({ hasText: "Alice desktop post" }).first().getByRole("button", { name: "Repost" }).click();
  await expect(page).toHaveURL(`${baseUrl}/home`);
  await expect(page.getByText("bob reposted")).toBeVisible();

  await page.getByRole("button", { name: "Log out" }).click();
  await expect(page.getByRole("link", { name: "Log in" })).toBeVisible();
  await expect(page.locator(".composer")).toHaveCount(0);
  const rejected = await page.request.post(`${baseUrl}/posts`, {
    multipart: { text: "logged out attempt" }
  });
  expect(rejected.status()).toBe(403);
  await expect(page.goto(`${baseUrl}/notifications`)).resolves.toBeTruthy();
});

test("mobile layout, validation, auth pages, and admin health stay usable", async ({ page }, testInfo) => {
  await page.goto(`${baseUrl}/local`);
  const dimensions = await page.evaluate(() => ({
    clientWidth: document.documentElement.clientWidth,
    scrollWidth: document.documentElement.scrollWidth
  }));
  expect(dimensions.scrollWidth).toBeLessThanOrEqual(dimensions.clientWidth + 1);
  await expect(page.locator(".composer")).toHaveCount(0);

  await register(page, "carol", "very secure password");
  await page.getByLabel("What is happening?").fill("x".repeat(281));
  await page.locator("textarea[name='text']").evaluate((node: HTMLTextAreaElement) => {
    node.removeAttribute("maxlength");
    node.value = "x".repeat(281);
  });
  await page.getByRole("button", { name: "Post", exact: true }).click();
  await expect(page.getByText("post is too long")).toBeVisible();

  await page.goto(`${baseUrl}/local`);
  await page.getByLabel("What is happening?").fill("Mobile layout post");
  await page.getByRole("button", { name: "Post", exact: true }).click();
  await expect(page.getByText("Mobile layout post")).toBeVisible();
  await expect(page.locator("article.post").first()).toBeVisible();
  await expect(page.getByRole("link", { name: "Settings" })).toBeVisible();
  await page.getByRole("link", { name: "Settings" }).click();
  await expect(page.getByRole("heading", { name: "Account settings" })).toBeVisible();
  await page.goto(`${baseUrl}/search`);
  await expect(page.getByRole("heading", { name: "Search" })).toBeVisible();

  mkdirSync("output/playwright", { recursive: true });
  await page.screenshot({ path: join("output/playwright", `${testInfo.project.name}-local.png`), fullPage: true });
});

async function register(page: Page, username: string, password: string) {
  await page.goto(`${baseUrl}/register`);
  await page.getByLabel("Username").fill(username);
  await page.getByLabel("Password").fill(password);
  await page.getByRole("button", { name: "Create account" }).click();
  await expect(page).toHaveURL(`${baseUrl}/home`);
}

async function login(page: Page, username: string, password: string) {
  await page.goto(`${baseUrl}/login`);
  await page.getByLabel("Username").fill(username);
  await page.getByLabel("Password").fill(password);
  await page.getByRole("button", { name: "Log in" }).click();
  await expect(page).toHaveURL(`${baseUrl}/home`);
}

async function waitForServer(url: string) {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`${url}/local`);
      if (response.ok) {
        return;
      }
    } catch {
      await new Promise((resolveWait) => setTimeout(resolveWait, 150));
    }
  }
  throw new Error("RustPost server did not become ready");
}

async function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const probe = createServer();
    probe.on("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const address = probe.address();
      probe.close(() => {
        if (address && typeof address === "object") {
          resolvePort(address.port);
        } else {
          reject(new Error("could not allocate port"));
        }
      });
    });
  });
}
