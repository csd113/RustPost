import { expect } from '@playwright/test';
import { spawn } from 'node:child_process';
import { mkdtemp, readFile, writeFile, rm } from 'node:fs/promises';
import net from 'node:net';
import path from 'node:path';
import { tmpdir } from 'node:os';

export const adminUsername = 'siteowner';
export const adminPassword = 'very secure password';
const binary = path.join(process.cwd(), 'target', 'debug', process.platform === 'win32' ? 'rustpost-cli.exe' : 'rustpost-cli');

async function run(dataDir, args) {
  return new Promise((resolve, reject) => {
    const child = spawn(binary, ['--data-dir', dataDir, ...args]);
    let error = '';
    child.stdout.resume();
    child.stderr.on('data', chunk => { error += chunk; });
    child.on('error', reject);
    child.on('close', code => code === 0 ? resolve() : reject(new Error(error)));
  });
}
async function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const port = server.address().port;
      server.close(() => resolve(port));
    });
  });
}
export function startServer(dataDir) {
  const child = spawn(binary, ['--data-dir', dataDir, 'serve'], { stdio: ['ignore', 'pipe', 'pipe'] });
  child.stdout.resume();
  child.stderr.resume();
  return child;
}
export async function stopProcess(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  child.kill('SIGTERM');
  await new Promise(resolve => child.once('close', resolve));
}
export async function waitForServer(baseUrl, child) {
  for (let i = 0; i < 200; i++) {
    expect(child.exitCode).toBeNull();
    try { if ((await fetch(`${baseUrl}/home`)).ok) return; } catch { /* listener starting */ }
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error('Server did not become ready');
}
export async function withLocalRuntime(callback, options = {}) {
  const dataDir = await mkdtemp(path.join(tmpdir(), 'rustpost-configuration-'));
  let server;
  try {
    const port = await freePort();
    const baseUrl = `http://127.0.0.1:${port}`;
    await run(dataDir, ['init']);
    const settingsPath = path.join(dataDir, 'settings.toml');
    let raw = (await readFile(settingsPath, 'utf8')).replace('port = 8080', `port = ${port}`)
      .replace('create_admin_on_first_boot = true', 'create_admin_on_first_boot = false');
    for (const [from, to] of options.replacements || []) {
      expect(raw).toContain(from);
      raw = raw.replace(from, to);
    }
    await writeFile(settingsPath, raw);
    await run(dataDir, ['create-admin', adminUsername, adminPassword]);
    server = startServer(dataDir);
    await waitForServer(baseUrl, server);
    await callback({ dataDir, baseUrl, settingsPath, server });
  } finally {
    if (server) await stopProcess(server);
    await rm(dataDir, { recursive: true, force: true });
  }
}
export async function login(page, baseUrl, username, password) {
  await page.goto(`${baseUrl}/login`);
  await page.locator('#username').fill(username);
  await page.locator('#password').fill(password);
  await page.locator('button.auth-submit').click();
  await page.waitForURL(/\/(home|onboarding)$/);
  if (new URL(page.url()).pathname === '/onboarding') {
    await page.locator('button[name=intent][value=skip]').click();
    await page.waitForURL(/\/home$/);
  }
}
export async function assertNoHorizontalOverflow(page, label) {
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth);
  expect(overflow, label).toBe(false);
}
