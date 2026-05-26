// Smoke tests for the Markdown renderer. Lives across react-markdown,
// remark-gfm, rehype-highlight, remark-math, rehype-katex, and a small
// custom code-block wrapper that adds a copy button. The point is to
// catch a version bump that quietly changes the rendered output, not
// to assert the exact HTML.

import { render, screen } from '@/__test_utils';
import { Markdown } from '@/features/Markdown';

describe('Markdown', () => {
  it('renders plain text inside a paragraph', () => {
    const { container } = render(<Markdown text="hello world" />);
    const p = container.querySelector('p');
    expect(p).not.toBeNull();
    expect(p?.textContent).toBe('hello world');
  });

  it('renders ATX headings', () => {
    render(<Markdown text={'# Title\n\n## Subhead'} />);
    expect(screen.getByRole('heading', { level: 1 })).toHaveTextContent('Title');
    expect(screen.getByRole('heading', { level: 2 })).toHaveTextContent('Subhead');
  });

  it('renders inline code in a <code> element', () => {
    const { container } = render(<Markdown text="use `npm test` to run" />);
    const code = container.querySelector('code');
    expect(code).not.toBeNull();
    expect(code?.textContent).toBe('npm test');
  });

  it('renders a fenced code block with a language class on <code>', () => {
    const fence = '```rust\nfn main() {}\n```';
    const { container } = render(<Markdown text={fence} />);
    const code = container.querySelector('pre code');
    expect(code).not.toBeNull();
    expect(code?.className).toMatch(/language-rust/);
  });

  it('shows the language pill for fenced code with a language', () => {
    const fence = '```rust\nfn main() {}\n```';
    render(<Markdown text={fence} />);
    // The pill is a small label; assert via the visible "rust" text.
    expect(screen.getByText('rust')).toBeInTheDocument();
  });

  it('renders a fenced code block without a language with no language class', () => {
    const fence = '```\nplain\n```';
    const { container } = render(<Markdown text={fence} />);
    const code = container.querySelector('pre code');
    expect(code).not.toBeNull();
    expect(code?.className ?? '').not.toMatch(/language-/);
  });

  it('renders a copy button next to fenced code blocks', () => {
    const fence = '```js\nconsole.log(1);\n```';
    render(<Markdown text={fence} />);
    // CopyButton uses `aria-label="Copy"` by default.
    expect(screen.getByRole('button', { name: /copy/i })).toBeInTheDocument();
  });

  it('renders a GFM pipe table inside a wrapper', () => {
    const table = [
      '| a | b |',
      '|---|---|',
      '| 1 | 2 |'
    ].join('\n');
    render(<Markdown text={table} />);
    const t = screen.getByRole('table');
    expect(t).toBeInTheDocument();
    // Header cells.
    const ths = t.querySelectorAll('th');
    expect(ths.length).toBe(2);
    expect(ths[0].textContent).toBe('a');
    // Body cell.
    const tds = t.querySelectorAll('tbody td');
    expect(tds.length).toBe(2);
    expect(tds[0].textContent).toBe('1');
  });

  it('renders external links with target=_blank and rel set', () => {
    render(<Markdown text="see [docs](https://example.com)" />);
    const a = screen.getByRole('link', { name: /docs/i });
    expect(a).toHaveAttribute('href', 'https://example.com');
    expect(a).toHaveAttribute('target', '_blank');
    expect(a.getAttribute('rel') ?? '').toMatch(/noreferrer/);
  });
});
