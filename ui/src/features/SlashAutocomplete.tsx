import { forwardRef, useEffect, useImperativeHandle, useState } from 'react';
import { cn } from '@/lib/utils';
import type { SlashCommand, SlashPrompt } from '@/types';

// Popover that appears above the input when the first character is `/`.
// Keyboard model:
//   - ArrowUp/Down: move highlight (wraps).
//   - Enter: commit the highlighted entry into the input (text includes
//     trailing space so the user can keep typing args). Does NOT submit.
//   - Tab: same as Enter.
//   - Escape: close the popover without committing.
//
// Commit replaces the whole input value with the command name + space.
// That's the simplest correct behaviour for `/cmd`; if the user already
// had `/cmd arg`, the commit swaps out just the command token.

type Entry =
  | { kind: 'command'; command: SlashCommand }
  | { kind: 'prompt'; prompt: SlashPrompt };

export type SlashAutocompleteHandle = {
  /** Handle a key that the host input saw. Returns true if the key was
   * consumed (and the host should not further process it). */
  onKeyDown: (e: React.KeyboardEvent) => boolean;
  isOpen: boolean;
};

type Props = {
  value: string;
  commands: SlashCommand[];
  prompts: SlashPrompt[];
  onCommit: (next: string) => void;
};

const filterEntries = (value: string, commands: SlashCommand[], prompts: SlashPrompt[]): Entry[] => {
  if (!value.startsWith('/')) {
    return [];
  }
  // Close the autocomplete as soon as the user has typed past the
  // command name (any whitespace after the `/token`). Otherwise
  // committing `/help ` would keep matching `/help` and swallow the
  // next Enter.
  if (/\s/.test(value)) {
    return [];
  }
  const token = value.slice(1).toLowerCase();
  const cmd: Entry[] = commands
    .filter((c) => c.name.slice(1).toLowerCase().startsWith(token))
    .map((c) => ({ kind: 'command', command: c }));
  const prm: Entry[] = prompts
    .filter((p) => p.name.toLowerCase().startsWith(token))
    .map((p) => ({ kind: 'prompt', prompt: p }));
  return [...cmd, ...prm];
};

const commitValue = (original: string, entry: Entry): string => {
  // Replace the first token; preserve any args the user already typed.
  const rest = original.replace(/^\S*/, '').trimStart();
  const inserted = entry.kind === 'command' ? entry.command.name : `/${entry.prompt.name}`;
  return rest.length > 0 ? `${inserted} ${rest}` : `${inserted} `;
};

export const SlashAutocomplete = forwardRef<SlashAutocompleteHandle, Props>(
  ({ value, commands, prompts, onCommit }, ref) => {
    const entries = filterEntries(value, commands, prompts);
    const isOpen = value.startsWith('/') && entries.length > 0;
    const [index, setIndex] = useState(0);

    // Reset the highlighted index when the filter set changes, so it never
    // points past the end of the list.
    useEffect(() => {
      if (index >= entries.length) {
        setIndex(0);
      }
    }, [entries.length, index]);

    useImperativeHandle(ref, () => ({
      isOpen,
      onKeyDown: (e) => {
        if (!isOpen) {
          return false;
        }
        if (e.key === 'ArrowDown') {
          e.preventDefault();
          setIndex((i) => (i + 1) % entries.length);
          return true;
        }
        if (e.key === 'ArrowUp') {
          e.preventDefault();
          setIndex((i) => (i - 1 + entries.length) % entries.length);
          return true;
        }
        if (e.key === 'Enter' || e.key === 'Tab') {
          e.preventDefault();
          const entry = entries[index];
          if (entry) {
            onCommit(commitValue(value, entry));
          }
          return true;
        }
        if (e.key === 'Escape') {
          e.preventDefault();
          return true;
        }
        return false;
      }
    }));

    if (!isOpen) {
      return null;
    }

    return (
      <div
        role="listbox"
        className={cn(
          // The clamp keeps the popover from extending off-screen when
          // the virtual keyboard is open: at most 16rem, or the space
          // between the top of the viewport and a ~200 px reservation
          // for the composer and its bottom gutter. 200 is empirical;
          // tune during on-device testing if the popover touches the
          // composer on a narrow phone.
          'absolute bottom-full left-2 right-2 z-20 mb-1.5 overflow-y-auto rounded-md border border-border bg-popover shadow-md',
          'max-h-[min(16rem,calc(100dvh-var(--mz-kb-inset)-200px))]',
          'scrollbar-thin'
        )}
      >
        {entries.map((entry, i) => {
          const active = i === index;
          const key = entry.kind === 'command' ? entry.command.name : `prompt:${entry.prompt.name}`;
          const label = entry.kind === 'command' ? entry.command.name : `/${entry.prompt.name}`;
          const description = entry.kind === 'command' ? entry.command.description : entry.prompt.description;
          const hint = entry.kind === 'command' ? entry.command.meta?.hint : undefined;
          return (
            <button
              key={key}
              role="option"
              aria-selected={active}
              type="button"
              onMouseEnter={() => setIndex(i)}
              onClick={() => onCommit(commitValue(value, entry))}
              className={cn(
                'flex w-full items-start gap-2 px-2.5 py-1.5 text-left text-xs',
                active ? 'bg-accent text-accent-foreground' : 'text-foreground hover:bg-accent/60'
              )}
            >
              <span className="font-mono text-[color:var(--primary)]">{label}</span>
              <span className="min-w-0 flex-1 truncate text-muted-foreground">
                {description}
                {hint ? <span className="ml-1 text-muted-foreground/70">· {hint}</span> : null}
                {entry.kind === 'prompt' && entry.prompt.serverName ? (
                  <span className="ml-1 text-muted-foreground/70">· {entry.prompt.serverName}</span>
                ) : null}
              </span>
            </button>
          );
        })}
      </div>
    );
  }
);

SlashAutocomplete.displayName = 'SlashAutocomplete';
