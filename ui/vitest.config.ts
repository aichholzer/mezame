import path from 'node:path';
import { defineConfig, mergeConfig } from 'vitest/config';
import viteConfig from './vite.config';

// Vitest re-uses the Vite config (plugins, alias, define) but layers a
// `test` block on top. Tests live OUTSIDE the UI package, under
// `tests/ui/` at the repo root, so the SPA bundle stays free of test
// code and the `tests/` umbrella mirrors where Rust tests live.
export default mergeConfig(
  viteConfig,
  defineConfig({
    // Vite's dev server refuses to serve files outside the project
    // root by default; vitest inherits that. Tests and their setup
    // live one level up, so explicitly allow the repo root.
    server: {
      fs: {
        allow: [path.resolve(__dirname, '..')]
      }
    },
    test: {
      // Anchor module resolution to the `ui/` package so imports of
      // node_modules and the `@/` alias work when the test files live
      // outside the package root.
      root: __dirname,
      // jsdom is the right default: the reducer tests are pure but
      // component tests need DOM globals, and there is no per-test
      // env override in vitest 4 that is cheaper than running every
      // test in jsdom.
      environment: 'jsdom',
      // Discover tests in the umbrella directory at the repo root.
      include: ['../tests/ui/**/*.{test,spec}.{ts,tsx}'],
      // Source files in `ui/src/` get pulled in via imports as needed.
      // Make `globals: true` so describe/it/expect resolve without
      // `import { describe, it, expect } from 'vitest'` in every file.
      globals: true,
      setupFiles: ['./src/__test_setup.ts'],
      restoreMocks: true,
      clearMocks: true
    }
  })
);
