// Smoke test: proves Vitest discovers, compiles, and runs tests under
// `tests/ui/`. Delete or replace once real tests exist.

describe('vitest scaffolding', () => {
  it('runs at all', () => {
    expect(1 + 1).toBe(2);
  });

  it('has the @testing-library/jest-dom matchers wired up', () => {
    const el = document.createElement('div');
    el.textContent = 'hello';
    expect(el).toHaveTextContent('hello');
  });

  it('resolves the @ alias from ui/src/', async () => {
    // Pull in a known pure module to confirm the alias from
    // vite.config.ts (and inherited by vitest.config.ts) resolves.
    const { cn } = await import('@/lib/utils');
    expect(typeof cn).toBe('function');
  });
});
