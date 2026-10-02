import configuration from './configuration-admin.config.mjs';
import { defineConfig } from '@playwright/test';
export default defineConfig({ ...configuration,
  testMatch: /software-updates\.spec\.mjs$/,
  outputDir: '../../output/playwright/software-update-results',
});
