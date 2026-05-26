import path from 'node:path';
import { defineConfig, mergeConfig } from 'vitest/config';
import viteConfig from './vite.config';

// Vitest re-uses the Vite config (plugins, alias, define) but layers a
// `test` block on top. Tests live OUTSIDE the UI package, under
// `tests/ui/` at the repo root, so the SPA bundle stays free of test
// code and the `tests/` umbrella mirrors where Rust tests live.
//
// Because the test files live above the package root, vite's bare-
// import resolver needs help finding `@testing-library/*` and friends
// in `ui/node_modules/`. The `resolve.modules` block below tells vite
// to fall back to that directory for any unresolved bare specifier.
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
    resolve: {
      // Falls back to ui/node_modules when a bare import is resolved
      // from a test file outside the package root. Vite walks parent
      // directories from the importing file by default; explicitly
      // pointing at our package's node_modules makes resolution
      // deterministic.
      modules: [path.resolve(__dirname, 'node_modules')]
    },
    test: {
      // Anchor module resolution to the `ui/` package.
      root: __dirname,
      // jsdom is the right default: the reducer tests are pure but
      // component tests need DOM globals.
      environment: 'jsdom',
      // Discover tests in the umbrella directory at the repo root.
      // We restrict the glob to direct children of `tests/ui/` so the
      // `tests/ui/node_modules` symlink (used to make Vite's bare-
      // import resolver see this package's deps) does not pull in
      // type-test fixtures shipped by libraries like
      // `@testing-library/jest-dom`.
      include: ['../tests/ui/*.{test,spec}.{ts,tsx}'],
      exclude: [
        '**/node_modules/**',
        '**/dist/**',
        '**/.git/**',
        '**/{tmp,target}/**'
      ],
      globals: true,
      setupFiles: ['./src/__test_setup.ts'],
      restoreMocks: true,
      clearMocks: true
    }
  })
);
