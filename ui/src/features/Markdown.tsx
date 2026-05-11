import type { ComponentPropsWithoutRef } from 'react';
import { memo } from 'react';
import ReactMarkdown, { type Components } from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import rehypeKatex from 'rehype-katex';
import remarkGfm from 'remark-gfm';
import remarkMath from 'remark-math';
import { CopyButton } from '@/components/CopyButton';
import { cn } from '@/lib/utils';

// Markdown renderer for agent output.
//
// - GitHub-flavoured markdown via remark-gfm (tables, strikethrough,
//   task lists, autolinks).
// - Code highlighting via rehype-highlight (uses highlight.js). Theme CSS
//   imported once at the app level.
// - Custom overrides to keep the terminal aesthetic and add a copy button
//   to every fenced code block.
//
// Memoised by input text so React does not re-run the parser when
// unrelated state changes. During streaming the `text` prop grows, which
// busts the memo on every chunk, which is what we want.

const InlineCode = ({ className, children, ...props }: ComponentPropsWithoutRef<'code'>) => (
  <code
    className={cn('rounded-sm bg-muted px-1 py-0.5 text-[0.9em] text-foreground', className)}
    {...props}
  >
    {children}
  </code>
);

const FencedCode = ({ className, children, ...props }: ComponentPropsWithoutRef<'code'>) => {
  const langMatch = /language-(\w+)/.exec(className ?? '');
  const lang = langMatch?.[1];
  // children are the source lines as strings / rehype-generated elements;
  // flatten to string for copy.
  const raw =
    typeof children === 'string'
      ? children
      : Array.isArray(children)
        ? children.map((c) => (typeof c === 'string' ? c : '')).join('')
        : String(children ?? '');
  const text = raw.replace(/\n$/, '');

  return (
    <span className="group relative block">
      {lang && (
        <span className="absolute top-1.5 left-2 z-10 rounded-sm bg-card/70 px-1.5 py-0.5 text-[10px] text-muted-foreground">
          {lang}
        </span>
      )}
      <CopyButton
        text={text}
        className="absolute top-1.5 right-1.5 z-10 opacity-0 transition-opacity group-hover:opacity-100"
      />
      <code className={cn('block', className)} {...props}>
        {children}
      </code>
    </span>
  );
};

const components: Components = {
  code(props) {
    const className = props.className ?? '';
    if (className.startsWith('language-')) {
      return <FencedCode {...props} />;
    }
    return <InlineCode {...props} />;
  },
  pre(props) {
    const { className, ...rest } = props;
    return (
      <pre
        className={cn(
          'my-2 overflow-x-auto rounded-md border border-border bg-[#0d1117] p-3 text-xs leading-relaxed scrollbar-thin',
          className
        )}
        {...rest}
      />
    );
  },
  table({ className, ...rest }) {
    // Wrap tables so wide content scrolls horizontally within the
    // wrapper instead of expanding the page. Without this, a table
    // with long cells would push the body viewport wider than the
    // window on narrow screens.
    return (
      <div className="my-2 w-full overflow-x-auto scrollbar-thin">
        <table className={cn('border-collapse', className)} {...rest} />
      </div>
    );
  },
  a({ className, children, href, ...rest }) {
    return (
      <a
        href={href}
        target="_blank"
        rel="noreferrer noopener"
        className={cn('text-[color:var(--primary)] underline-offset-2 hover:underline', className)}
        {...rest}
      >
        {children}
      </a>
    );
  }
};

type Props = {
  text: string;
};

export const Markdown = memo(({ text }: Props) => (
  <div
    className={cn(
      'leading-relaxed text-foreground',
      '[&_p]:my-1.5',
      '[&_ul]:my-2 [&_ul]:list-disc [&_ul]:pl-5',
      '[&_ol]:my-2 [&_ol]:list-decimal [&_ol]:pl-5',
      '[&_li]:my-0.5',
      '[&_h1]:mt-5 [&_h1]:mb-2 [&_h1]:text-xl [&_h1]:font-semibold [&_h1]:border-b [&_h1]:border-border [&_h1]:pb-1',
      '[&_h2]:mt-4 [&_h2]:mb-2 [&_h2]:text-lg [&_h2]:font-semibold [&_h2]:border-b [&_h2]:border-border/60 [&_h2]:pb-0.5',
      '[&_h3]:mt-3 [&_h3]:mb-1.5 [&_h3]:text-base [&_h3]:font-semibold',
      '[&_h4]:mt-3 [&_h4]:mb-1 [&_h4]:text-sm [&_h4]:font-semibold [&_h4]:text-muted-foreground',
      '[&_h1:first-child]:mt-0 [&_h2:first-child]:mt-0 [&_h3:first-child]:mt-0 [&_h4:first-child]:mt-0',
      '[&_blockquote]:my-2 [&_blockquote]:border-l-2 [&_blockquote]:border-border [&_blockquote]:pl-3 [&_blockquote]:text-muted-foreground',
      '[&_table]:my-2 [&_table]:border-collapse',
      '[&_th]:border [&_th]:border-border [&_th]:px-2 [&_th]:py-1 [&_th]:text-left',
      '[&_td]:border [&_td]:border-border [&_td]:px-2 [&_td]:py-1',
      '[&_hr]:my-3 [&_hr]:border-border',
      // KaTeX: display math as its own block, with the same colour as body
      // (KaTeX's default inline span colour would otherwise go unstyled
      // and inherit unpredictably).
      '[&_.katex-display]:my-3 [&_.katex-display]:overflow-x-auto [&_.katex-display]:overflow-y-hidden'
    )}
  >
    <ReactMarkdown
      remarkPlugins={[remarkGfm, remarkMath]}
      rehypePlugins={[[rehypeHighlight, { detect: true, ignoreMissing: true }], rehypeKatex]}
      components={components}
    >
      {text}
    </ReactMarkdown>
  </div>
));

Markdown.displayName = 'Markdown';
