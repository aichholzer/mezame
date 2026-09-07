// A page load contacts only Mezame. The two font families used to come
// from Google, which sent every viewer's address to two third-party origins
// on every load and stood in the way of a same-origin content security
// policy; they now ship in the bundle from Fontsource.

import { readFileSync } from 'node:fs';
import path from 'node:path';

const ui = (rel: string) => readFileSync(path.resolve(__dirname, '../../ui', rel), 'utf8');

describe('bundle origins', () => {
  it('loads nothing from Google in the HTML shell', () => {
    const html = ui('index.html');
    for (const remote of ['fonts.googleapis.com', 'fonts.gstatic.com', 'rel="preconnect"']) {
      expect(html, remote).not.toContain(remote);
    }
  });

  it('imports both families from the bundle', () => {
    const main = ui('src/main.tsx');
    for (const css of [
      '@fontsource-variable/jetbrains-mono/index.css',
      '@fontsource-variable/jetbrains-mono/wght-italic.css',
      '@fontsource/gugi/latin-400.css'
    ]) {
      expect(main, css).toContain(`import '${css}';`);
    }
  });

  it('names the bundled family first in the mono stack', () => {
    // The Fontsource variable build registers as 'JetBrains Mono Variable';
    // forgetting the rename would fall back to the system mono in silence.
    const css = ui('src/index.css');
    expect(css).toMatch(/--font-mono: 'JetBrains Mono Variable', 'JetBrains Mono',/);
  });
});
