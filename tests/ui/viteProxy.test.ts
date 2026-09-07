// The dev loop's proxy map. Vite turns a string entry into
// `{ target, changeOrigin: true }`, and a rewritten `Host` fails the
// server's Origin check, which compares `Origin` against `Host` with the
// port; so every entry is an object with no `changeOrigin`. The map also
// used to name a `/legacy` route that no longer exists and left out
// `/history`, which Vite then answered with `index.html`.

import config from '../../ui/vite.config';

describe('the Vite dev proxy', () => {
  const proxy =
    (config as { server?: { proxy?: Record<string, unknown> } }).server?.proxy ?? {};

  it('forwards the three routes the page talks to', () => {
    expect(Object.keys(proxy).sort()).toEqual(['/history', '/state', '/ws']);
  });

  it('never rewrites Host, which the Origin check compares against', () => {
    for (const [route, entry] of Object.entries(proxy)) {
      expect(typeof entry, route).toBe('object');
      expect(entry, route).not.toBeNull();
      expect((entry as { changeOrigin?: boolean }).changeOrigin, route).toBeFalsy();
    }
    expect((proxy['/ws'] as { ws?: boolean }).ws).toBe(true);
  });
});
