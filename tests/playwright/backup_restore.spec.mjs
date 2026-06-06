import { test, expect } from "@playwright/test";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import net from "node:net";

const repoRoot = process.cwd();
const adminUsername = "siteowner";
const adminPassword = "very secure password";
const tinyPng = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==",
  "base64",
);

test.describe.configure({ mode: "serial" });

function commandOutput(command, args, code, stdout, stderr) {
  return [`${command} ${args.join(" ")} exited with ${code}`, stdout.trim(), stderr.trim()]
    .filter(Boolean)
    .join("\n");
}

function runCommand(command, args) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, args, { cwd: repoRoot });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk) => {
      stderr += chunk;
    });
    child.on("error", reject);
    child.on("close", (code) => {
      if (code === 0) {
        resolve({ stdout, stderr });
      } else {
        reject(new Error(commandOutput(command, args, code, stdout, stderr)));
      }
    });
  });
}

function freePort() {
  return new Promise((resolve, reject) => {
    const listener = net.createServer();
    listener.on("error", reject);
    listener.listen(0, "127.0.0.1", () => {
      const address = listener.address();
      listener.close(() => resolve(address.port));
    });
  });
}

async function configureDataDir(dataDir, port, replacements = []) {
  await runCommand("cargo", [
    "run",
    "--quiet",
    "--bin",
    "rustpost-cli",
    "--",
    "--data-dir",
    dataDir,
    "init",
  ]);
  const settingsPath = path.join(dataDir, "settings.toml");
  let settings = await readFile(settingsPath, "utf8");
  settings = settings
    .replace("port = 8080", `port = ${port}`)
    .replace("create_admin_on_first_boot = true", "create_admin_on_first_boot = false")
    .replace("account_creations_per_ip_per_day = 3", "account_creations_per_ip_per_day = 50");
  for (const [from, to] of replacements) {
    settings = settings.replace(from, to);
  }
  await writeFile(settingsPath, settings);
  await runCommand("cargo", [
    "run",
    "--quiet",
    "--bin",
    "rustpost-cli",
    "--",
    "--data-dir",
    dataDir,
    "create-admin",
    adminUsername,
    adminPassword,
  ]);
}

function startServer(dataDir) {
  const child = spawn(
    "cargo",
    ["run", "--quiet", "--bin", "rustpost-cli", "--", "--data-dir", dataDir, "serve"],
    {
      cwd: repoRoot,
      detached: process.platform !== "win32",
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  child.stdout.resume();
  child.stderr.resume();
  return child;
}

async function waitForServer(baseUrl, child) {
  const deadline = Date.now() + 90_000;
  while (Date.now() < deadline) {
    expect(child.exitCode).toBeNull();
    try {
      const response = await fetch(`${baseUrl}/home`, { redirect: "manual" });
      if (response.status < 500) {
        return;
      }
    } catch (_error) {
      // Retry while cargo builds and the server binds its listener.
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  throw new Error("RustPost server did not become ready in time");
}

async function stopProcess(child) {
  if (!child || child.exitCode !== null) {
    return;
  }
  try {
    if (process.platform === "win32") {
      child.kill("SIGTERM");
    } else {
      process.kill(-child.pid, "SIGTERM");
    }
  } catch (error) {
    if (error.code === "ESRCH") {
      return;
    }
    throw error;
  }
  await new Promise((resolve) => {
    const timeout = setTimeout(resolve, 5_000);
    child.once("close", () => {
      clearTimeout(timeout);
      resolve();
    });
  });
}

async function withRuntime(testPrefix, callback, replacements = []) {
  const dataDir = await mkdtemp(path.join(tmpdir(), testPrefix));
  const port = await freePort();
  const baseUrl = `http://127.0.0.1:${port}`;
  let server;
  await configureDataDir(dataDir, port, replacements);
  try {
    server = startServer(dataDir);
    await waitForServer(baseUrl, server);
    await callback({ dataDir, baseUrl, server });
  } finally {
    await stopProcess(server);
    await rm(dataDir, { recursive: true, force: true });
  }
}

async function login(page, baseUrl) {
  await page.goto(`${baseUrl}/login`);
  await page.fill("#username", adminUsername);
  await page.fill("#password", adminPassword);
  await Promise.all([page.waitForURL("**/home"), page.locator("button.auth-submit").click()]);
}

async function createPost(page, baseUrl, text, withMedia = false) {
  await page.goto(`${baseUrl}/home`);
  await page.fill("#post-text", text);
  if (withMedia) {
    await page.setInputFiles("#post-media", {
      name: "backup.png",
      mimeType: "image/png",
      buffer: tinyPng,
    });
  }
  await Promise.all([
    page.waitForURL(/\/home#post-\d+$/),
    page.locator('form[action="/posts"] button[type="submit"]').click(),
  ]);
}

async function latestBackup(dataDir, prefix = "rustpost-") {
  const backups = (await readdir(path.join(dataDir, "backups")))
    .filter((name) => name.startsWith(prefix) && name.endsWith(".tar"))
    .sort();
  expect(backups.length).toBeGreaterThan(0);
  return path.join(dataDir, "backups", backups.at(-1));
}

async function waitForAutomaticBackup(dataDir) {
  const deadline = Date.now() + 90_000;
  while (Date.now() < deadline) {
    const backups = (await readdir(path.join(dataDir, "backups"))).filter((name) =>
      name.startsWith("rustpost-auto-") && name.endsWith(".tar"),
    );
    if (backups.length > 0) {
      return backups;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error("automatic backup was not created");
}

test("manual backup can be created and downloaded", async ({ page }) => {
  await withRuntime("rustpost-backup-manual-", async ({ baseUrl }) => {
    await login(page, baseUrl);
    await createPost(page, baseUrl, "manual backup includes posts and media", true);
    await page.goto(`${baseUrl}/admin/backups`);
    await expect(page.getByRole("heading", { name: "Backups", exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Create backup" }).click();
    await expect(page.getByText("Backup created:")).toBeVisible();
    const download = await Promise.all([
      page.waitForEvent("download"),
      page.getByRole("link", { name: "Download" }).first().click(),
    ]).then(([download]) => download);
    expect(download.suggestedFilename()).toMatch(/^rustpost-.*\.tar$/);
  });
});

test("fresh runtime restore preserves users posts settings media and admin access", async ({ page }) => {
  const sourceDir = await mkdtemp(path.join(tmpdir(), "rustpost-backup-source-"));
  const sourcePort = await freePort();
  const sourceUrl = `http://127.0.0.1:${sourcePort}`;
  let sourceServer;
  let restoredServer;
  const restoredDir = await mkdtemp(path.join(tmpdir(), "rustpost-backup-restored-"));
  try {
    await configureDataDir(sourceDir, sourcePort, [
      ["name = \"RustPost\"", "name = \"Restored RustPost\""],
      ["anonymous_mode_enabled = false", "anonymous_mode_enabled = true"],
      ["nsfw_blur_enabled = true", "nsfw_blur_enabled = false"],
    ]);
    sourceServer = startServer(sourceDir);
    await waitForServer(sourceUrl, sourceServer);
    await login(page, sourceUrl);
    await createPost(page, sourceUrl, "fresh restore keeps this media post", true);
    await runCommand("cargo", [
      "run",
      "--quiet",
      "--bin",
      "rustpost-cli",
      "--",
      "--data-dir",
      sourceDir,
      "backup",
    ]);
    const archive = await latestBackup(sourceDir);
    await stopProcess(sourceServer);
    sourceServer = null;

    const restoredPort = await freePort();
    await configureDataDir(restoredDir, restoredPort);
    await runCommand("cargo", [
      "run",
      "--quiet",
      "--bin",
      "rustpost-cli",
      "--",
      "--data-dir",
      restoredDir,
      "restore",
      archive,
    ]);
    restoredServer = startServer(restoredDir);
    const restoredUrl = sourceUrl;
    await waitForServer(restoredUrl, restoredServer);
    await login(page, restoredUrl);
    await expect(page.getByText("fresh restore keeps this media post")).toBeVisible();
    await expect(page.locator('img[src^="/uploads/"], video[src^="/uploads/"]').first()).toBeVisible();
    await page.goto(`${restoredUrl}/admin/health`);
    await expect(page.getByText("Anonymous mode")).toBeVisible();
    await expect(
      page.locator("dt", { hasText: "Anonymous mode" }).locator("xpath=following-sibling::dd[1]"),
    ).toHaveText("true");
  } finally {
    await stopProcess(sourceServer);
    await stopProcess(restoredServer);
    await rm(sourceDir, { recursive: true, force: true });
    await rm(restoredDir, { recursive: true, force: true });
  }
});

test("admin restore upload replaces populated runtime after restart", async ({ page }) => {
  await withRuntime("rustpost-backup-populated-", async ({ dataDir, baseUrl, server }) => {
    await login(page, baseUrl);
    await createPost(page, baseUrl, "state captured before restore", true);
    await runCommand("cargo", [
      "run",
      "--quiet",
      "--bin",
      "rustpost-cli",
      "--",
      "--data-dir",
      dataDir,
      "backup",
    ]);
    const archive = await latestBackup(dataDir);
    await createPost(page, baseUrl, "state that should disappear after restore");
    await page.goto(`${baseUrl}/admin/backups`);
    await page.setInputFiles("#backup-upload", archive);
    await page.fill("#restore-confirm", "RESTORE");
    await page.getByRole("button", { name: "Restore backup" }).click();
    await expect(page.getByText("Restore completed.")).toBeVisible();
    await stopProcess(server);
    const restarted = startServer(dataDir);
    try {
      await waitForServer(baseUrl, restarted);
      await login(page, baseUrl);
      await expect(page.getByText("state captured before restore")).toBeVisible();
      await expect(page.getByText("state that should disappear after restore")).toHaveCount(0);
    } finally {
      await stopProcess(restarted);
    }
  });
});

test("scheduler creates automatic backup and retention prunes old automatic archives", async ({ page }) => {
  test.setTimeout(130_000);
  await withRuntime(
    "rustpost-backup-scheduler-",
    async ({ dataDir, baseUrl }) => {
      await login(page, baseUrl);
      await createPost(page, baseUrl, "scheduler backup state");
      await waitForAutomaticBackup(dataDir);
      await writeFile(path.join(dataDir, "backups", "rustpost-auto-20000101T000000000000Z.tar"), "old");
      await writeFile(path.join(dataDir, "backups", "rustpost-auto-20000101T000001000000Z.tar"), "older");
      await page.goto(`${baseUrl}/admin/backups`);
      await page.getByLabel("Keep newest automatic backups").fill("1");
      await page.getByRole("button", { name: "Save backup settings" }).click();
      await expect(page.getByText("Backup settings saved.")).toBeVisible();
      const deadline = Date.now() + 90_000;
      while (Date.now() < deadline) {
        const oldExists =
          existsSync(path.join(dataDir, "backups", "rustpost-auto-20000101T000000000000Z.tar")) ||
          existsSync(path.join(dataDir, "backups", "rustpost-auto-20000101T000001000000Z.tar"));
        if (!oldExists) {
          return;
        }
        await new Promise((resolve) => setTimeout(resolve, 500));
      }
      throw new Error("automatic retention did not prune old archives");
    },
    [
      ["automatic_enabled = false", "automatic_enabled = true"],
      ["automatic_interval_minutes = 1440", "automatic_interval_minutes = 1"],
      ["retention_keep_last = 10", "retention_keep_last = 2"],
    ],
  );
});

test("malicious restore upload is rejected without replacing live state", async ({ page }) => {
  await withRuntime("rustpost-backup-malicious-", async ({ dataDir, baseUrl }) => {
    const malicious = path.join(dataDir, "tmp", "malicious.tar");
    await writeFile(malicious, maliciousTraversalTar());
    await login(page, baseUrl);
    await createPost(page, baseUrl, "live state survives malicious restore");
    await page.goto(`${baseUrl}/admin/backups`);
    await page.setInputFiles("#backup-upload", malicious);
    await page.fill("#restore-confirm", "RESTORE");
    await page.getByRole("button", { name: "Restore backup" }).click();
    await expect(page.getByText("Restore failed")).toBeVisible();
    await page.goto(`${baseUrl}/home`);
    await expect(page.getByText("live state survives malicious restore")).toBeVisible();
  });
});

test("backup page works without JavaScript in Firefox", async ({ browserName, browser }) => {
  test.skip(browserName !== "firefox", "Firefox no-JS coverage");
  const context = await browser.newContext({ javaScriptEnabled: false });
  const page = await context.newPage();
  page.setDefaultTimeout(20_000);
  await withRuntime("rustpost-backup-no-js-", async ({ baseUrl }) => {
    await login(page, baseUrl);
    await page.goto(`${baseUrl}/admin/backups`);
    await expect(page.getByRole("heading", { name: "Backups", exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Create backup" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Restore backup" })).toBeVisible();
  });
  await context.close();
});

test("backup admin page has a WebKit smoke path", async ({ browserName, page }) => {
  test.skip(browserName !== "webkit", "WebKit admin smoke");
  await withRuntime("rustpost-backup-webkit-", async ({ baseUrl }) => {
    await login(page, baseUrl);
    await page.goto(`${baseUrl}/admin`);
    await page.getByRole("link", { name: "Backups" }).click();
    await expect(page.getByRole("heading", { name: "Backups", exact: true })).toBeVisible();
  });
});

function maliciousTraversalTar() {
  const chunks = [
    tarEntry("manifest.toml", "0", Buffer.from("format_version = 1\n")),
    tarEntry("../settings.toml", "0", Buffer.from("owned")),
    Buffer.alloc(1024),
  ];
  return Buffer.concat(chunks);
}

function tarEntry(name, typeflag, body) {
  const header = Buffer.alloc(512, 0);
  header.write(name, 0, "utf8");
  header.write("0000600\0", 100, "ascii");
  header.write("0000000\0", 108, "ascii");
  header.write("0000000\0", 116, "ascii");
  header.write(body.length.toString(8).padStart(11, "0") + "\0", 124, "ascii");
  header.write("00000000000\0", 136, "ascii");
  header.fill(" ", 148, 156);
  header.write(typeflag, 156, "ascii");
  header.write("ustar\0", 257, "ascii");
  header.write("00", 263, "ascii");
  let checksum = 0;
  for (const byte of header) {
    checksum += byte;
  }
  header.write(checksum.toString(8).padStart(6, "0") + "\0 ", 148, "ascii");
  const padding = Buffer.alloc((512 - (body.length % 512)) % 512, 0);
  return Buffer.concat([header, body, padding]);
}
