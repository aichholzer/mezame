import path from 'node:path';
import { readFileSync } from 'node:fs';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// Dev: Vite runs on :5173 with HMR and proxies /ws and /state to the Rust
// binary on :7842. Release builds emit to ui/dist and get baked into the
// binary via rust-embed.

// Surface the version string in `ui/package.json` to the UI at build
// time. Keep the one canonical source; `Cargo.toml` mirrors it by hand
// until we decide on a script.
const pkg = JSON.parse(readFileSync(path.resolve(__dirname, 'package.json'), 'utf8')) as { version: string };

export default defineConfig({
  plugins: [react(), tailwindcss()],
  define: {
    __OKIRO_VERSION__: JSON.stringify(pkg.version)
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
        target: 'ws://127.0.0.1:7842',
        ws: true,
        changeOrigin: true
      },
      '/state': 'http://127.0.0.1:7842',
      '/legacy': 'http://127.0.0.1:7842'
    }
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    target: 'es2022'
  }
});
