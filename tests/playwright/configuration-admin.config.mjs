import { defineConfig } from '@playwright/test';
export default defineConfig({
  testDir: '.',
  testMatch: /configuration-admin\.spec\.mjs$/,
  timeout: 120_000,
  workers: 1,
  outputDir: '../../output/playwright/configuration-results',
  reporter: [['list']],
  use: { trace: 'retain-on-failure', screenshot: 'only-on-failure' },
  projects: [
    { name: 'chromium', use: { browserName: 'chromium' } },
    { name: 'chromium-no-js', use: { browserName: 'chromium', javaScriptEnabled: false } },
    { name: 'firefox-no-js', use: { browserName: 'firefox', javaScriptEnabled: false, ...(process.env.RUSTPOST_FIREFOX_EXECUTABLE ? { launchOptions: { executablePath: process.env.RUSTPOST_FIREFOX_EXECUTABLE } } : {}) } },
    { name: 'webkit', use: { browserName: 'webkit' } },
  ],
});
