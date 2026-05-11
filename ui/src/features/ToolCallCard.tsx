import { ChevronDownIcon, ChevronRightIcon, FileIcon, SettingsIcon } from 'lucide-react';
import { useState } from 'react';
import { CopyButton } from '@/components/CopyButton';
import { Markdown } from '@/features/Markdown';
import { cn } from '@/lib/utils';
import type { LogEntry, ToolCallLocation } from '@/types';

// Renders an ACP tool_call as a collapsible summary row. The row itself
// shows title, status pill, and kind; expanded, it reveals arguments
// (as JSON), content (as markdown if present), and the file locations
// the tool touched. Multiple tool_call_update notifications with the
// same toolCallId mutate the underlying store entry in place, so this
// component just re-renders.

type Entry = Extract<LogEntry, { kind: 'tool_call' }>;

type Props = {
  entry: Entry;
};

const statusTone = (status: string | null): string => {
  switch (status) {
    case 'in_progress':
    case 'pending':
      return 'border-[color:var(--primary)]/40 text-[color:var(--primary)]';
    case 'completed':
      return 'border-[color:var(--attn-done)]/40 text-[color:var(--attn-done)]';
    case 'failed':
      return 'border-[color:var(--attn-error)]/40 text-[color:var(--attn-error)]';
    default:
      return 'border-border text-muted-foreground';
  }
};

// Status labels: protocol values are snake_case (`in_progress`); humans
// read "in progress" more naturally.
const statusLabel = (status: string | null): string => {
  if (!status) {
    return 'running';
  }
  return status.replace(/_/g, ' ');
};

const stringifyContent = (content: unknown): string | null => {
  if (content === null || content === undefined) {
    return null;
  }
  if (typeof content === 'string') {
    return content;
  }
  // ACP tool-call content is typically an array of ContentBlocks. We
  // render text blocks verbatim; anything else falls through to a
  // JSON dump so nothing is silently lost.
  if (Array.isArray(content)) {
    const textBlocks: string[] = [];
    let nonText = false;
    for (const block of content) {
      if (block && typeof block === 'object') {
        const b = block as Record<string, unknown>;
        if (b.type === 'content' && b.content && typeof b.content === 'object') {
          const inner = b.content as Record<string, unknown>;
          if (inner.type === 'text' && typeof inner.text === 'string') {
            textBlocks.push(inner.text);
            continue;
          }
        }
        if (b.type === 'text' && typeof b.text === 'string') {
          textBlocks.push(b.text);
          continue;
        }
      }
      nonText = true;
    }
    if (textBlocks.length > 0 && !nonText) {
      return textBlocks.join('\n');
    }
  }
  try {
    return JSON.stringify(content, null, 2);
  } catch {
    return String(content);
  }
};

const stringifyInput = (rawInput: unknown): string | null => {
  if (rawInput === null || rawInput === undefined) {
    return null;
  }
  if (typeof rawInput === 'string') {
    return rawInput;
  }
  try {
    return JSON.stringify(rawInput, null, 2);
  } catch {
    return String(rawInput);
  }
};

const Location = ({ location }: { location: ToolCallLocation }) => (
  <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
    <FileIcon className="size-3 shrink-0 opacity-70" />
    <span className="truncate">
      {location.path ?? 'unknown'}
      {typeof location.line === 'number' && `:${location.line}`}
    </span>
  </div>
);

export const ToolCallCard = ({ entry }: Props) => {
  const [open, setOpen] = useState(false);

  const tone = statusTone(entry.status);
  const input = stringifyInput(entry.rawInput);
  const content = stringifyContent(entry.content);
  const hasDetails =
    input !== null || content !== null || (entry.locations && entry.locations.length > 0);

  return (
    <div
      className={cn(
        'my-2 rounded-sm border border-l-[3px] bg-card/40 text-sm',
        tone
      )}
    >
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        disabled={!hasDetails}
        className={cn(
          'flex w-full items-center gap-2 px-2.5 py-1.5 text-left',
          hasDetails ? 'cursor-pointer hover:bg-accent/30' : 'cursor-default'
        )}
        aria-expanded={open}
      >
        {hasDetails ? (
          open ? (
            <ChevronDownIcon className="size-3.5 shrink-0 opacity-70" />
          ) : (
            <ChevronRightIcon className="size-3.5 shrink-0 opacity-70" />
          )
        ) : (
          <SettingsIcon className="size-3.5 shrink-0 opacity-70" />
        )}
        <span className="flex-1 truncate text-foreground">{entry.title}</span>
        <span
          className={cn(
            'rounded-sm border bg-background/50 px-1.5 py-0.5 text-[10px] uppercase tracking-wide',
            tone
          )}
        >
          {statusLabel(entry.status)}
        </span>
      </button>

      {open && hasDetails && (
        <div className="min-w-0 space-y-3 border-t border-border/50 px-2.5 py-2.5">
          {input !== null && (
            <div>
              <div className="mb-1 flex items-center gap-2 text-[11px] text-muted-foreground">
                <span>Arguments</span>
                <CopyButton text={input} title="Copy arguments" className="size-6" />
              </div>
              <pre className="overflow-x-auto rounded-sm bg-muted/40 p-2 text-[11px] leading-snug text-foreground">
                {input}
              </pre>
            </div>
          )}

          {content !== null && (
            <div>
              <div className="mb-1 flex items-center gap-2 text-[11px] text-muted-foreground">
                <span>Output</span>
                <CopyButton text={content} title="Copy output" className="size-6" />
              </div>
              {/* Render text output as markdown when it looks like prose
                  (agent-authored summaries often are). JSON dumps go
                  through Markdown too, where they render as inline code
                  without fenced highlighting; acceptable. */}
              <Markdown text={content} />
            </div>
          )}

          {entry.locations && entry.locations.length > 0 && (
            <div>
              <div className="mb-1 text-[11px] text-muted-foreground">Locations</div>
              <div className="space-y-0.5">
                {entry.locations.map((loc, i) => (
                  <Location key={i} location={loc} />
                ))}
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
};
