import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import net from "node:net";
import { chromium, firefox, webkit } from "playwright";

const PASSWORD = "very secure password";
const TEST_ONION_ADDRESS = "abcdefghijklmnopqrstuvwxyz234567abcdefghijklmnopqrstuvwx.onion";
const tinyPng = Buffer.from(
  "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c63f8cfc00000040001fea7699d160000000049454e44ae426082",
  "hex",
);

async function main() {
  const server = await startRustPost();
  try {
    await jsChromiumFlow(server.baseUrl);
    await noJsFirefoxFlow(server.baseUrl);
    await webKitFlow(server.baseUrl);
  } finally {
    server.stop();
  }
}

async function startRustPost() {
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "rustpost-pw-"));
  const port = await freePort();
  runCargo(["run", "--quiet", "--bin", "rustpost-cli", "--", "--data-dir", dataDir, "init"]);
  const settingsPath = path.join(dataDir, "settings.toml");
  const settings = fs
    .readFileSync(settingsPath, "utf8")
    .replace("port = 8080", `port = ${port}`)
    .replace('display_onion_address = ""', `display_onion_address = "${TEST_ONION_ADDRESS}"`);
  fs.writeFileSync(settingsPath, settings);
  runCargo([
    "run",
    "--quiet",
    "--bin",
    "rustpost-cli",
    "--",
    "--data-dir",
    dataDir,
    "create-admin",
    "siteowner",
    PASSWORD,
  ]);
  const child = spawn(
    "cargo",
    ["run", "--quiet", "--bin", "rustpost-cli", "--", "--data-dir", dataDir, "serve"],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  const baseUrl = `http://127.0.0.1:${port}`;
  await waitForServer(baseUrl, child);
  return {
    baseUrl,
    stop() {
      child.kill("SIGTERM");
      fs.rmSync(dataDir, { recursive: true, force: true });
    },
  };
}

async function jsChromiumFlow(baseUrl) {
  const browser = await chromium.launch();
  try {
    const user = await browser.newContext();
    const page = await user.newPage();
    await installClipboardProbe(page);
    await register(page, baseUrl, "alicepw");
    await expectOnionHeaderWithJs(page, baseUrl);
    await createMediaPost(page, "alice flagged media", true);
    await expectBlurred(page);
    await page.locator(".nsfw-show").first().click();
    await expectUnblurred(page);

    await page.goto(`${baseUrl}/settings`);
    await page.locator("#nsfw_blur_enabled").uncheck();
    await page.getByRole("button", { name: "Save settings" }).click();
    await page.goto(`${baseUrl}/home`);
    await assertNoBlurredMedia(page);
    await page.goto(`${baseUrl}/settings`);
    await page.locator("#nsfw_blur_enabled").check();
    await page.getByRole("button", { name: "Save settings" }).click();
    await page.goto(`${baseUrl}/home`);
    await expectBlurred(page);

    const admin = await browser.newContext();
    const adminPage = await admin.newPage();
    await login(adminPage, baseUrl, "siteowner", PASSWORD);
    await createMediaPost(adminPage, "admin toggle media", false);
    await adminPage.getByRole("button", { name: "Mark NSFW" }).first().click();
    await expectPostBlurred(adminPage, "admin toggle media");
    await adminPage.getByRole("button", { name: "Unmark NSFW" }).first().click();
    await assertPostNotBlurred(adminPage, "admin toggle media");

    await setGlobalBlur(adminPage, false);
    await page.goto(`${baseUrl}/home`);
    await assertNoBlurredMedia(page);
    await setGlobalBlur(adminPage, true);
    const anonymous = await browser.newContext();
    const anonPage = await anonymous.newPage();
    await anonPage.goto(`${baseUrl}/home`);
    await expectBlurred(anonPage);

    const mobile = await browser.newContext({
      viewport: { width: 390, height: 844 },
      isMobile: true,
    });
    const mobilePage = await mobile.newPage();
    await mobilePage.goto(`${baseUrl}/home`);
    await expectMobileOnionLayout(mobilePage);

    await user.close();
    await admin.close();
    await anonymous.close();
    await mobile.close();
  } finally {
    await browser.close();
  }
}

async function noJsFirefoxFlow(baseUrl) {
  const browser = await firefox.launch();
  try {
    const context = await browser.newContext({ javaScriptEnabled: false });
    const page = await context.newPage();
    await register(page, baseUrl, "toruserpw");
    await expectOnionHeaderWithoutJs(page, baseUrl);
    await createMediaPost(page, "no js flagged media", true);
    await expectBlurred(page);
    await page.locator(".nsfw-show").first().click();
    await expectUnblurred(page);

    const adminContext = await browser.newContext({ javaScriptEnabled: false });
    const adminPage = await adminContext.newPage();
    await login(adminPage, baseUrl, "siteowner", PASSWORD);
    await createMediaPost(adminPage, "no js admin media", false);
    await adminPage.getByRole("button", { name: "Mark NSFW" }).first().click();
    await expectPostBlurred(adminPage, "no js admin media");
    await adminPage.getByRole("button", { name: "Unmark NSFW" }).first().click();
    await assertPostNotBlurred(adminPage, "no js admin media");

    await context.close();
    await adminContext.close();
  } finally {
    await browser.close();
  }
}

async function webKitFlow(baseUrl) {
  const browser = await webkit.launch();
  try {
    const context = await browser.newContext();
    const page = await context.newPage();
    await installClipboardProbe(page);
    await register(page, baseUrl, "webkitpw");
    await expectOnionHeaderWithJs(page, baseUrl);
    await createMediaPost(page, "webkit flagged media", true);
    await expectBlurred(page);
    await page.locator(".nsfw-show").first().click();
    await expectUnblurred(page);
    await context.close();
  } finally {
    await browser.close();
  }
}

async function installClipboardProbe(page) {
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        async writeText(text) {
          window.__rustpostCopiedText = text;
        },
      },
    });
  });
}

async function expectOnionHeaderWithJs(page, baseUrl) {
  await page.goto(`${baseUrl}/home`);
  const indicator = page.getByTestId("tor-header-indicator");
  await indicator.waitFor();
  const box = await indicator.boundingBox();
  assert.ok(box, "Tor header indicator should have a visible box");
  assert.ok(box.y < 180, `Tor header indicator should be near the top, got y=${box.y}`);
  await indicator.locator("summary").click();
  await expectFullOnionAddress(page);
  await page.getByTestId("tor-copy-button").click();
  assert.equal(await page.evaluate(() => window.__rustpostCopiedText), TEST_ONION_ADDRESS);
  await page.getByRole("button", { name: "Copied" }).waitFor();
}

async function expectOnionHeaderWithoutJs(page, baseUrl) {
  await page.goto(`${baseUrl}/home`);
  const indicator = page.getByTestId("tor-header-indicator");
  await indicator.waitFor();
  await indicator.locator("summary").click();
  await expectFullOnionAddress(page);
  assert.equal(await page.getByTestId("tor-copy-button").innerText(), "Copy");
}

async function expectFullOnionAddress(page) {
  const fullAddress = page.getByTestId("tor-full-address");
  await fullAddress.waitFor();
  assert.equal(await fullAddress.innerText(), TEST_ONION_ADDRESS);
}

async function expectMobileOnionLayout(page) {
  const indicator = page.getByTestId("tor-header-indicator");
  await indicator.waitFor();
  const viewport = page.viewportSize();
  const box = await indicator.boundingBox();
  assert.ok(box, "mobile Tor header indicator should have a visible box");
  assert.ok(box.y < 180, `mobile Tor header indicator should be near the top, got y=${box.y}`);
  assert.ok(box.x >= 0, `mobile Tor header indicator should not overflow left, got x=${box.x}`);
  assert.ok(
    box.x + box.width <= viewport.width,
    `mobile Tor header indicator should fit viewport, got right=${box.x + box.width}`,
  );
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  assert.ok(overflow <= 1, `mobile layout should not overflow horizontally, got ${overflow}px`);
  await indicator.locator("summary").click();
  await expectFullOnionAddress(page);
}

async function register(page, baseUrl, username) {
  await page.goto(`${baseUrl}/register`);
  await page.locator("#username").fill(username);
  await page.locator("#password").fill(PASSWORD);
  await page.locator("#confirm_password").fill(PASSWORD);
  await page.getByRole("button", { name: "Create account" }).click();
  await page.waitForURL("**/home");
}

async function login(page, baseUrl, username, password) {
  await page.goto(`${baseUrl}/login`);
  await page.locator("#username").fill(username);
  await page.locator("#password").fill(password);
  await page.getByRole("button", { name: "Log in" }).click();
  await page.waitForURL("**/home");
}

async function createMediaPost(page, text, nsfw) {
  await page.goto(new URL("/home", page.url()).toString());
  await page.locator("#post-text").fill(text);
  if (nsfw) {
    await page.locator("#post-nsfw").check();
  }
  await page.locator("#media").setInputFiles({
    name: `${text.replaceAll(" ", "-")}.png`,
    mimeType: "image/png",
    buffer: tinyPng,
  });
  await page.getByRole("button", { name: "Post", exact: true }).click();
  await page.getByText(text).waitFor();
}

async function setGlobalBlur(page, enabled) {
  await page.goto(new URL("/admin/deep-settings", page.url()).toString());
  await page.locator("#deep-nsfw_blur_enabled").selectOption(enabled ? "true" : "false");
  await page.getByRole("button", { name: "Save" }).click();
  await page.getByRole("button", { name: "Confirm/Save" }).click();
  await page.getByText("Settings saved successfully").waitFor();
}

async function expectBlurred(page) {
  await page.locator(".nsfw-media").first().waitFor();
  const filter = await page.locator(".nsfw-media-frame img").first().evaluate((node) => getComputedStyle(node).filter);
  assert.notEqual(filter, "none");
}

async function expectUnblurred(page) {
  const filter = await page.locator(".nsfw-media-frame img").first().evaluate((node) => getComputedStyle(node).filter);
  assert.equal(filter, "none");
}

async function assertNoBlurredMedia(page) {
  await page.waitForLoadState("domcontentloaded");
  assert.equal(await page.locator(".nsfw-media").count(), 0);
}

async function expectPostBlurred(page, text) {
  const post = page.locator("article", { hasText: text }).first();
  await post.locator(".nsfw-media").first().waitFor();
}

async function assertPostNotBlurred(page, text) {
  const post = page.locator("article", { hasText: text }).first();
  assert.equal(await post.locator(".nsfw-media").count(), 0);
}

function runCargo(args) {
  const result = spawnSync("cargo", args, { encoding: "utf8" });
  if (result.status !== 0) {
    throw new Error(`cargo ${args.join(" ")} failed\n${result.stderr}\n${result.stdout}`);
  }
}

async function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close(() => resolve(address.port));
    });
    server.on("error", reject);
  });
}

async function waitForServer(baseUrl, child) {
  let stderr = "";
  child.stderr.on("data", (chunk) => {
    stderr += chunk.toString();
  });
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(`RustPost exited early\n${stderr}`);
    }
    try {
      const response = await fetch(`${baseUrl}/home`);
      if (response.ok) {
        return;
      }
    } catch (_err) {
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
  }
  throw new Error(`RustPost did not start\n${stderr}`);
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
