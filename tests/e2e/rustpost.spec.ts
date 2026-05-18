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
  const settings = readFileSync(settingsPath, "utf8")
    .replace('name = "RustPost"', 'name = "RustPost Test"')
    .replace("port = 8080", `port = ${port}`)
    .replace("max_image_size = 52428800", "max_image_size = 60")
    .replace("account_creations_per_ip_per_day = 3", "account_creations_per_ip_per_day = 20");
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
  await expect(page).toHaveTitle(/RustPost Test/);
  await expect(page.getByRole("link", { name: "RustPost Test" })).toBeVisible();
  await page.getByRole("button", { name: "Post", exact: true }).click();
  await expect(page.getByText("post text or media is required")).toBeVisible();
  await page.goto(`${baseUrl}/home`);
  await page.getByLabel("What is happening?").fill("Alice desktop post");
  await page.getByRole("button", { name: "Post", exact: true }).click();
  await expect(page).toHaveURL(`${baseUrl}/home#post-1`);
  await expect(page.getByText("Alice desktop post")).toBeVisible();
  await expect(page.locator("article.post").filter({ hasText: "Alice desktop post" }).first().getByRole("button", { name: "Repost unavailable for your own post" })).toBeDisabled();
  await page.getByRole("button", { name: "Log out" }).click();

  await register(page, "bob", "very secure password");
  await login(page, "bob", "very secure password");
  await page.goto(`${baseUrl}/home`);
  await expect(page.getByText("Alice desktop post")).toBeVisible();
  const post = page.locator("article.post").filter({ hasText: "Alice desktop post" }).first();
  await post.getByRole("link", { name: /Open thread/ }).click();
  await page.getByLabel("What is happening?").fill("Bob replies from the browser");
  await page.getByRole("button", { name: "Post", exact: true }).click();
  await expect(page.getByText("Bob replies from the browser")).toBeVisible();
  await page.goto(`${baseUrl}/home`);
  await expect(page.locator("article.post").filter({ hasText: "Bob replies from the browser" })).toHaveCount(0);
  await page.goto(`${baseUrl}/posts/1`);
  const reply = page.locator("article.post").filter({ hasText: "Bob replies from the browser" });
  await expect(reply).toHaveCount(1);
  await expect(reply.getByRole("link", { name: /Open thread/ })).toHaveCount(0);

  await page.goto(`${baseUrl}/home`);
  await page.locator("article.post").filter({ hasText: "Alice desktop post" }).first().getByRole("button", { name: "Like" }).click();
  await expect(page).toHaveURL(`${baseUrl}/home#post-1`);
  await expect(page.getByText("1 likes")).toBeVisible();
  await page.locator("article.post").filter({ hasText: "Alice desktop post" }).first().getByRole("button", { name: "Bookmark" }).click();
  await page.getByRole("link", { name: "Bookmarks" }).click();
  await expect(page.getByText("Alice desktop post")).toBeVisible();
  await page.getByRole("link", { name: "Open thread" }).first().click();
  await page.locator("article.post").filter({ hasText: "Alice desktop post" }).first().getByRole("button", { name: "Repost" }).click();
  await expect(page).toHaveURL(`${baseUrl}/posts/1#post-1`);
  await page.goto(`${baseUrl}/home`);
  await expect(page.getByText("bob reposted")).toBeVisible();

  await page.getByRole("button", { name: "Log out" }).click();
  await login(page, "alice", "very secure password");
  const repostOfOwnPost = page.locator("article.post").filter({ hasText: "bob reposted" }).filter({ hasText: "Alice desktop post" }).first();
  await expect(repostOfOwnPost.getByRole("button", { name: "Repost unavailable for your own post" })).toBeDisabled();
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
  await page.goto(`${baseUrl}/home`);
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

  await page.goto(`${baseUrl}/home`);
  await page.getByLabel("What is happening?").fill("Mobile layout post");
  await page.getByRole("button", { name: "Post", exact: true }).click();
  await expect(page.getByText("Mobile layout post")).toBeVisible();
  await expect(page.locator("article.post").first()).toBeVisible();
  await expect(page.getByRole("link", { name: "Profile" })).toBeVisible();
  await page.getByRole("link", { name: "Profile" }).click();
  await expect(page).toHaveURL(`${baseUrl}/users/carol`);
  await page.locator("section.profile").getByRole("link", { name: "Settings" }).click();
  await expect(page.getByRole("heading", { name: "Account settings" })).toBeVisible();
  await page.goto(`${baseUrl}/search`);
  await expect(page.getByRole("heading", { name: "Search" })).toBeVisible();

  mkdirSync("output/playwright", { recursive: true });
  await page.screenshot({ path: join("output/playwright", `${testInfo.project.name}-local.png`), fullPage: true });
});

test("registration confirms passwords and password toggles are accessible", async ({ page }) => {
  await page.goto(`${baseUrl}/register`);
  await page.getByLabel("Username").fill("dana");
  await page.getByLabel("Password", { exact: true }).fill("very secure password");
  await page.getByLabel("Confirm password").fill("different password");
  await page.getByRole("button", { name: "Create account" }).click();
  await expect(page.getByText("passwords do not match")).toBeVisible();

  const missingConfirm = await page.request.post(`${baseUrl}/register`, {
    form: { username: "erin", password: "very secure password" }
  });
  expect(missingConfirm.status()).toBe(400);
  await expect(missingConfirm.text()).resolves.toContain("please confirm your password");

  await page.goto(`${baseUrl}/register`);
  await page.getByLabel("Username").fill("frank");
  await page.getByLabel("Password", { exact: true }).fill("very secure password");
  await page.getByLabel("Confirm password").fill("very secure password");
  const passwordInput = page.locator("#password");
  await expect(passwordInput).toHaveAttribute("type", "password");
  await page.getByRole("button", { name: "Show password", exact: true }).click();
  await expect(passwordInput).toHaveAttribute("type", "text");
  await page.getByRole("button", { name: "Create account" }).click();
  await expect(page).toHaveURL(`${baseUrl}/home`);

  await page.getByRole("button", { name: "Log out" }).click();
  await page.goto(`${baseUrl}/login`);
  await page.getByLabel("Password", { exact: true }).fill("very secure password");
  const loginPassword = page.locator("#password");
  await page.getByRole("button", { name: "Show password" }).click();
  await expect(loginPassword).toHaveAttribute("type", "text");
});

test("profile picture upload errors are friendly validation failures", async ({ page }) => {
  await register(page, "grace", "very secure password");
  await page.getByRole("link", { name: "Profile" }).click();
  await page.locator("section.profile").getByRole("link", { name: "Settings" }).click();

  await page.setInputFiles("#profile_picture", {
    name: "avatar.gif",
    mimeType: "image/gif",
    buffer: Buffer.from("R0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw==", "base64")
  });
  await page.getByRole("button", { name: "Save settings" }).click();
  await expect(page).toHaveURL(`${baseUrl}/settings`);
  await expect(page.locator("img.profile-picture")).toBeVisible();

  await page.setInputFiles("#profile_picture", {
    name: "avatar.txt",
    mimeType: "text/plain",
    buffer: Buffer.from("not an image")
  });
  await page.getByRole("button", { name: "Save settings" }).click();
  await expect(page.getByText("unsupported media type")).toBeVisible();
  await expect(page.getByText("500 error")).toHaveCount(0);

  await page.goto(`${baseUrl}/settings`);
  await page.setInputFiles("#profile_picture", {
    name: "large.png",
    mimeType: "image/png",
    buffer: Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=", "base64")
  });
  await page.getByRole("button", { name: "Save settings" }).click();
  await expect(page.getByText("image exceeds maximum size")).toBeVisible();
  await expect(page.getByText("500 error")).toHaveCount(0);
});

test("blocked users can be reviewed and unblocked, and mute does not error", async ({ page }) => {
  await register(page, "heidi", "very secure password");
  await page.getByRole("button", { name: "Log out" }).click();

  await register(page, "ivan", "very secure password");
  await page.goto(`${baseUrl}/users/heidi`);
  await page.getByRole("button", { name: "Block" }).click();
  await expect(page).toHaveURL(`${baseUrl}/home`);

  await page.getByRole("link", { name: "Profile" }).click();
  await page.locator("section.profile").getByRole("link", { name: "Settings" }).click();
  await expect(page.getByRole("heading", { name: "Blocked users" })).toBeVisible();
  await expect(page.getByText("@heidi")).toBeVisible();
  await page.getByRole("button", { name: "Unblock" }).click();
  await expect(page).toHaveURL(`${baseUrl}/settings`);
  await expect(page.getByText("No blocked users")).toBeVisible();

  await page.goto(`${baseUrl}/users/heidi`);
  await page.getByRole("button", { name: "Mute" }).click();
  await expect(page).toHaveURL(`${baseUrl}/home`);
  await expect(page.getByText("500 error")).toHaveCount(0);
});

async function register(page: Page, username: string, password: string) {
  await page.goto(`${baseUrl}/register`);
  await page.getByLabel("Username").fill(username);
  await page.getByLabel("Password", { exact: true }).fill(password);
  await page.getByLabel("Confirm password").fill(password);
  await page.getByRole("button", { name: "Create account" }).click();
  await expect(page).toHaveURL(`${baseUrl}/home`);
}

async function login(page: Page, username: string, password: string) {
  await page.goto(`${baseUrl}/login`);
  await page.getByLabel("Username").fill(username);
  await page.getByLabel("Password", { exact: true }).fill(password);
  await page.getByRole("button", { name: "Log in" }).click();
  await expect(page).toHaveURL(`${baseUrl}/home`);
}

async function waitForServer(url: string) {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`${url}/home`);
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
