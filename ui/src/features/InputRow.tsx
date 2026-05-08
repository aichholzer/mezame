import { SendIcon } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Textarea } from '@/components/ui/textarea';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { CwdChip } from '@/features/CwdChip';
import { ModeModelSelectors } from '@/features/ModeModelSelectors';
import { SlashAutocomplete, type SlashAutocompleteHandle } from '@/features/SlashAutocomplete';
import { cn } from '@/lib/utils';
import type { Session } from '@/types';

// Floating composer pinned to the bottom of the chat pane. The log
// keeps scrolling underneath it; a semi-transparent fill plus a
// backdrop blur lets the latest content show through faintly without
// ever hiding the composer itself.
//
// Layout:
//   [  textarea                                         [  send ] ]
//   [                                                             ]
//   [ [cwd chip]                       [ Agent ] [ Model picker ] ]
//
// Send sits top-right. The bottom row has the cwd chip on the left and
// the Agent / Model pickers on the right, so the composer is disabled
// while the agent is working; there is no in-UI cancel.

type Props = {
  session: Session | null;
  onSubmit: (text: string) => void;
};

const MIN_ROWS = 2;
const MAX_ROWS = 8;

export const InputRow = ({ session, onSubmit }: Props) => {
  const [value, setValue] = useState('');
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const slashRef = useRef<SlashAutocompleteHandle>(null);

  useEffect(() => {
    if (session && !session.busy) {
      textareaRef.current?.focus();
    }
  }, [session?.id, session?.busy]);

  // Auto-grow between MIN_ROWS and MAX_ROWS, scroll thereafter.
  useEffect(() => {
    const el = textareaRef.current;
    if (!el) {
      return;
    }
    const computed = getComputedStyle(el);
    const lineHeight = parseFloat(computed.lineHeight || '20');
    const paddingY = parseFloat(computed.paddingTop) + parseFloat(computed.paddingBottom);
    el.style.height = 'auto';
    const minPx = lineHeight * MIN_ROWS + paddingY;
    const maxPx = lineHeight * MAX_ROWS + paddingY;
    el.style.height = `${Math.min(Math.max(el.scrollHeight, minPx), maxPx)}px`;
  }, [value]);

  const busy = !!session?.busy;
  const disabled = !session || busy;
  const canSend = !disabled && value.trim().length > 0;

  const submit = (e?: React.FormEvent) => {
    e?.preventDefault();
    const text = value.trim();
    if (!text) {
      return;
    }
    onSubmit(text);
    setValue('');
  };

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (slashRef.current?.onKeyDown(e)) {
      return;
    }
    if (e.key === 'Enter' && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey) {
      e.preventDefault();
      submit();
    }
  };

  const handleCommit = (next: string) => {
    setValue(next);
    textareaRef.current?.focus();
  };

  return (
    <form
      onSubmit={submit}
      className={cn(
        // Absolute so the log pane behind it keeps using the full
        // viewport height. Insets leave a visible gutter of scrollback
        // around the card so you can see the chat peeking out.
        'pointer-events-none absolute inset-x-3 bottom-3 z-10'
      )}
    >
      <div
        className={cn(
          // The card itself: takes pointer events, floats, and blurs
          // whatever log content slides under it. Border tint matches
          // the send button's primary blue so the card reads as a
          // first-class control, not a stray panel.
          'pointer-events-auto relative rounded-xl border border-[color:var(--primary)]/60 bg-background/70 shadow-lg shadow-black/30 backdrop-blur-md'
        )}
      >
        <SlashAutocomplete
          ref={slashRef}
          value={value}
          commands={session?.commands ?? []}
          prompts={session?.prompts ?? []}
          onCommit={handleCommit}
        />

        <Textarea
          ref={textareaRef}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={handleKeyDown}
          disabled={!session}
          readOnly={busy}
          placeholder={busy ? 'The agent is working...' : 'Message... (Enter to send, Shift+Enter for newline)'}
          rows={MIN_ROWS}
          autoFocus
          className={cn(
            // Keep text clear of the overlay widgets on the right.
            // Top-right holds the send button; bottom row holds the
            // inline Agent / Model pickers. Extra right padding only
            // applies on the last line (bottom padding).
            'border-0 bg-transparent shadow-none pr-14 pl-3 pt-3 pb-12',
            'focus-visible:ring-0 focus-visible:ring-offset-0'
          )}
        />

        {/* Top-right: send. Disabled while the agent is working. */}
        <div className="absolute right-2 top-2">
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                type="submit"
                size="icon"
                className="size-9"
                disabled={!canSend}
                aria-label="Send message"
              >
                <SendIcon className="size-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="left">Send</TooltipContent>
          </Tooltip>
        </div>

        {/* Bottom row: cwd chip (left) and inline Agent / Model selectors (right). */}
        <div className="absolute inset-x-2 bottom-2 flex items-center justify-between gap-2">
          <CwdChip session={session} />
          <ModeModelSelectors session={session} layout="row" />
        </div>
      </div>
    </form>
  );
};
