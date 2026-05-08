import { CheckIcon, CopyIcon } from 'lucide-react';
import { useEffect, useState } from 'react';
import { cn } from '@/lib/utils';

type Props = {
  text: string;
  className?: string;
  title?: string;
};

// Small copy-to-clipboard button. Falls back to a hidden textarea +
// execCommand for insecure-context environments (local http over
// non-localhost), though okiro is loopback-only so navigator.clipboard
// should always work in practice.

export const CopyButton = ({ text, className, title = 'Copy' }: Props) => {
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!copied) {
      return;
    }
    const id = window.setTimeout(() => setCopied(false), 1500);
    return () => clearTimeout(id);
  }, [copied]);

  const copy = async () => {
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(text);
      } else {
        const ta = document.createElement('textarea');
        ta.value = text;
        ta.style.position = 'fixed';
        ta.style.opacity = '0';
        document.body.appendChild(ta);
        ta.select();
        document.execCommand('copy');
        ta.remove();
      }
      setCopied(true);
    } catch {
      // Clipboard blocked: do nothing, user can select and copy manually.
    }
  };

  return (
    <button
      type="button"
      onClick={copy}
      title={copied ? 'Copied' : title}
      aria-label={copied ? 'Copied' : title}
      className={cn(
        'inline-flex size-6 items-center justify-center rounded-sm border border-border bg-card/80 text-muted-foreground backdrop-blur-sm',
        'transition-colors hover:text-foreground hover:bg-accent',
        'cursor-pointer',
        className
      )}
    >
      {copied ? <CheckIcon className="size-3" /> : <CopyIcon className="size-3" />}
    </button>
  );
};
