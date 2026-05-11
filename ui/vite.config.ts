import path from 'node:path';
import { existsSync, readFileSync } from 'node:fs';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// Dev: Vite runs on :5173 with HMR and proxies /ws and /state to the Rust
// binary on :9510. Release builds emit to ui/dist and get baked into the
// binary via rust-embed.

// Surface the version string in `ui/package.json` to the UI at build
// time. Keep the one canonical source; `Cargo.toml` mirrors it by hand
// until we decide on a script.
const pkg = JSON.parse(readFileSync(path.resolve(__dirname, 'package.json'), 'utf8')) as { version: string };

// Build id: written by `build.rs` so the Rust binary and the JS bundle
// share the exact same token. In dev mode (no `build.rs` run) we fall
// back to a timestamp so the comparison is effectively skipped (the
// server will have a different value, but in dev you have Vite HMR
// anyway so the reload gate is irrelevant).
const buildIdFile = path.resolve(__dirname, '.build-id');
const buildId = existsSync(buildIdFile)
  ? readFileSync(buildIdFile, 'utf8').trim()
  : Date.now().toString(36);

export default defineConfig({
  plugins: [react(), tailwindcss()],
  define: {
    __MEZAME_VERSION__: JSON.stringify(pkg.version),
    __MEZAME_BUILD_ID__: JSON.stringify(buildId)
  },
  resolve: {
    alias: {
      '@': path.resolve(__dirname, 'src')
    }
  },
  server: {
    port: 5173,
    strictPort: true,
    proxy: {
      '/ws': {
        target: 'ws://127.0.0.1:9510',
        ws: true,
        changeOrigin: true
      },
      '/state': 'http://127.0.0.1:9510',
      '/legacy': 'http://127.0.0.1:9510'
    }
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    target: 'es2022'
  }
});
