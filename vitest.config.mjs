import { defineConfig } from 'vitest/config';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  test: {
    environment: 'jsdom',
    setupFiles: ['tests/frontend/setup.ts'],
    include: ['tests/frontend/**/*.test.{ts,tsx}'],
    coverage: {
      provider: 'v8',
      include: ['packages/client/src/**/*.{ts,tsx}', 'packages/ui/src/**/*.{ts,tsx}', 'apps/desktop/src/**/*.{ts,tsx}'],
      exclude: ['**/types.ts', '**/index.ts'],
      reporter: ['text', 'json-summary', 'html'],
      thresholds: { perFile: true, lines: 80, statements: 80, functions: 80, branches: 80 }
    }
  }
});
