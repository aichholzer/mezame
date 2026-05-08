import { useEffect, useLayoutEffect, useRef } from 'react';
import { CopyButton } from '@/components/CopyButton';
import { Markdown } from '@/features/Markdown';
import { okiroActions } from '@/hooks/useOkiro';
import { useTick } from '@/hooks/useTick';
import { formatAbsolute, timeAgo } from '@/lib/time';
import { cn } from '@/lib/utils';
import type { LogEntry, PermissionOption, Session } from '@/types';

type Props = {
  session: Session;
  isActive: boolean;
};

const TimestampLabel = ({ ts, now }: { ts: number; now: number }) => (
  <span className="text-[11px] text-muted-foreground select-none" title={formatAbsolute(ts)}>
    {timeAgo(ts, now)}
  </span>
);

const PermissionCard = ({
  session,
  entry,
  options
}: {
  session: Session;
  entry: Extract<LogEntry, { kind: 'permission' }>;
  options: PermissionOption[];
}) => {
  const resolved = !!entry.resolution;
  const optionTone = (opt: PermissionOption) => {
    const kind = (opt.kind || opt.optionId || '').toString();
    if (kind.startsWith('allow')) {
      return 'border-[color:var(--attn-done)]/40 text-[color:var(--attn-done)] hover:bg-[color:var(--attn-done)]/10';
    }
    if (kind.startsWith('reject')) {
      return 'border-[color:var(--attn-error)]/40 text-[color:var(--attn-error)] hover:bg-[color:var(--attn-error)]/10';
    }
    return 'border-border text-foreground hover:bg-accent';
  };

  return (
    <div
      className={cn(
        'my-3 rounded-sm border border-l-[3px] border-l-[color:var(--attn-permission)] bg-card px-3 py-2',
        resolved && 'border-l-muted-foreground opacity-60'
      )}
    >
      <div className="mb-2 text-xs text-[color:var(--attn-permission)]">
        permission requested: {entry.title}
      </div>
      {resolved ? (
        <div className="text-xs text-muted-foreground">→ {entry.resolution}</div>
      ) : (
        <div className="flex flex-wrap gap-1.5">
          {options.map((opt) => (
            <button
              key={opt.optionId}
              type="button"
              onClick={() => okiroActions.resolvePermission(session.id, entry.id, opt)}
              className={cn(
                'cursor-pointer rounded-sm border bg-card px-2.5 py-1 text-xs transition-colors',
                optionTone(opt)
              )}
            >
              {opt.name || opt.optionId || 'option'}
            </button>
          ))}
        </div>
      )}
    </div>
  );
};

/** Strip the legacy terminal-prompt glyph and trailing whitespace that
 * the store prepends/appends. Neither belongs in a chat bubble. */
const cleanUserText = (text: string): string => text.replace(/^> /, '').trimEnd();

const TextEntry = ({ entry, now }: { entry: Extract<LogEntry, { kind: 'text' }>; now: number }) => {
  if (entry.role === 'sys') {
    // Pure-whitespace glue (trailing newlines used to enforce terminal
    // spacing) is dropped entirely in the bubble layout.
    const trimmed = entry.text.trim();
    if (!trimmed) {
      return null;
    }
    const isError = trimmed.startsWith('[error');
    return (
      <div className="my-2 flex justify-center">
        <div
          className={cn(
            'max-w-[90%] rounded-md border px-2.5 py-1 text-[11px] italic text-center',
            isError
              ? 'border-[color:var(--attn-error)]/40 bg-[color:var(--attn-error)]/10 text-[color:var(--attn-error)] not-italic'
              : 'border-border/60 bg-card/40 text-muted-foreground'
          )}
        >
          {trimmed}
        </div>
      </div>
    );
  }

  if (entry.role === 'agent') {
    const copyText = entry.text.trim();
    return (
      <div className="my-3">
        <Markdown text={entry.text} />
        <div className="mt-2 flex items-center gap-2.5">
          <CopyButton text={copyText} title="Copy message" className="size-7" />
          <TimestampLabel ts={entry.timestamp} now={now} />
        </div>
      </div>
    );
  }

  // User
  const cleaned = cleanUserText(entry.text);
  return (
    <div className="mt-10 mb-6 flex justify-end">
      <div className="max-w-[90%] rounded-2xl rounded-br-sm border border-[color:var(--user-bubble)]/40 bg-[color:var(--user-bubble)]/15 px-4 py-3 sm:max-w-[78%]">
        <div className="whitespace-pre-wrap break-words text-foreground">{cleaned}</div>
        <div className="mt-2 flex items-center gap-2.5">
          <TimestampLabel ts={entry.timestamp} now={now} />
          <CopyButton text={cleaned} title="Copy message" className="size-7" />
        </div>
      </div>
    </div>
  );
};

export const LogPane = ({ session, isActive }: Props) => {
  const scrollRef = useRef<HTMLDivElement>(null);
  const lastScrollHeight = useRef(0);
  const tick = useTick();
  // Relative-time label refresh needs an actual `now` snapshot. tick just
  // forces this component (and children) to re-render.
  const now = Date.now();
  void tick;

  // Auto-scroll when new content arrives if the user is pinned to the
  // bottom. useLayoutEffect so the scroll happens in the same frame as
  // the DOM update, avoiding visible jumps.
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) {
      return;
    }
    const grew = el.scrollHeight > lastScrollHeight.current;
    lastScrollHeight.current = el.scrollHeight;
    if (grew && session.pinnedToBottom) {
      el.scrollTop = el.scrollHeight;
    }
  });

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) {
      return;
    }
    const onScroll = () => {
      const pinned = el.scrollHeight - el.scrollTop - el.clientHeight < 20;
      okiroActions.setPinnedToBottom(session.id, pinned);
    };
    el.addEventListener('scroll', onScroll);
    return () => el.removeEventListener('scroll', onScroll);
  }, [session.id]);

  // Hide the thinking indicator once the agent has started streaming a
  // reply: the growing bubble is its own progress signal. Only show it
  // between submit and the first agent chunk (or for tool-call-only
  // turns where no agent text arrives at all).
  const lastEntry = session.log.at(-1);
  const agentStreaming =
    lastEntry && lastEntry.kind === 'text' && lastEntry.role === 'agent';
  const showThinking = session.thinking && !agentStreaming;

  return (
    <div
      ref={scrollRef}
      className={cn(
        // Bottom padding leaves room for the floating composer so the
        // final message isn't permanently hidden. ~13rem covers the
        // composer at its MIN_ROWS height plus gutters; taller composers
        // visually overlap a bit of scrollback, which is intended.
        'flex-1 overflow-y-auto px-3 pt-3 pb-[13rem] break-words scrollbar-thin',
        !isActive && 'hidden'
      )}
    >
      {session.log.map((entry) => {
        if (entry.kind === 'text') {
          return <TextEntry key={entry.id} entry={entry} now={now} />;
        }
        return (
          <PermissionCard key={entry.id} session={session} entry={entry} options={entry.options} />
        );
      })}

      {showThinking && (
        <div className="my-3 inline-flex items-center gap-2 rounded-md border border-[color:var(--primary)]/30 bg-[color:var(--primary)]/10 px-2.5 py-1.5 text-xs text-muted-foreground">
          <span
            role="status"
            aria-label="thinking"
            className="inline-block size-2.5 animate-spin rounded-full border-2 border-[color:var(--primary)] border-t-transparent"
          />
          thinking
        </div>
      )}
    </div>
  );
};
