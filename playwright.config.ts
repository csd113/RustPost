import { defineConfig, devices } from '@playwright/test';

const port = Number(process.env.RUSTPOST_E2E_PORT ?? '18080');
const baseURL = process.env.RUSTPOST_E2E_BASE_URL ?? `http://127.0.0.1:${port}`;

export default defineConfig({
  testDir: './tests/e2e',
  outputDir: './output/playwright/test-results',
  timeout: 45_000,
  expect: {
    timeout: 5_000,
  },
  fullyParallel: false,
  workers: 1,
  reporter: [
    ['list'],
    ['html', { outputFolder: 'output/playwright/report', open: 'never' }],
  ],
  use: {
    baseURL,
    actionTimeout: 10_000,
    navigationTimeout: 15_000,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure',
  },
  webServer: {
    command: `RUSTPOST_E2E_PORT=${port} ./tests/e2e/start-rustpost.sh`,
    url: baseURL,
    reuseExistingServer: false,
    timeout: 120_000,
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
      testIgnore: /mobile\.spec\.ts/,
    },
    {
      name: 'mobile-chrome',
      use: { ...devices['Pixel 7'] },
      testMatch: /mobile\.spec\.ts/,
    },
  ],
});
