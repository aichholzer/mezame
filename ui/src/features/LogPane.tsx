import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { CopyButton } from '@/components/CopyButton';
import { Markdown } from '@/features/Markdown';
import { McpOauthCard } from '@/features/McpOauthCard';
import { ToolCallCard } from '@/features/ToolCallCard';
import { mezameActions } from '@/hooks/useMezame';
import { useKeyboardInsetValue } from '@/hooks/useKeyboardInset';
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
  // "Remember for this session" tickbox state. Resets per card because
  // each is its own React component instance; that's the right scope.
  const [remember, setRemember] = useState(false);
  // Whether this card is connected to a remembered-policy slot, i.e.
  // it either set the policy (`remembered`) or was auto-resolved by
  // it (`auto`). Subsequent auto cards keep the badge until the user
  // disables the policy on any matching card.
  const isRememberedCard = entry.remembered || entry.auto;
  // Active iff the policy is still live (not yet disabled). Looking
  // it up against the live session state means a click on Disable on
  // any card with the same title clears the badge on every other
  // card too, which matches the user's mental model: there is one
  // policy per title, not one per card.
  const policyActive = entry.title in session.rememberedPermissions;
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
        <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
          <span>
            {`\u2192 ${entry.resolution}`}
            {entry.auto && (
              <span
                className="ml-1 rounded-sm bg-muted px-1 py-0.5 text-[10px] uppercase text-muted-foreground"
                title="Resolved automatically by a remembered policy"
              >
                auto
              </span>
            )}
          </span>
          {isRememberedCard && policyActive && (
            <>
              <span
                className="rounded-sm bg-muted px-1.5 py-0.5 text-[11px] text-muted-foreground"
                title="Future requests with this title auto-resolve until you disable the policy"
              >
                Remembered for this session
              </span>
              <button
                type="button"
                onClick={() =>
                  mezameActions.forgetRememberedPermission(session.id, entry.title)
                }
                className="cursor-pointer rounded-sm border border-border px-1.5 py-0.5 text-[11px] hover:bg-accent"
                title="Stop auto-resolving future requests with this title"
              >
                Disable
              </button>
            </>
          )}
        </div>
      ) : (
        <div className="flex flex-col gap-2">
          <div className="flex flex-col gap-2 md:flex-row md:flex-wrap md:gap-1.5">
            {options.map((opt) => (
              <button
                key={opt.optionId}
                type="button"
                onClick={() => mezameActions.resolvePermission(session.id, entry.id, opt, remember)}
                className={cn(
                  // Stacked on mobile with 44 px minimum height so each
                  // option has a clearly separate hit area; inline on
                  // desktop with the denser sizing.
                  'cursor-pointer rounded-sm border bg-card text-sm md:text-xs transition-colors',
                  'min-h-11 md:min-h-0 px-4 md:px-2.5 py-2.5 md:py-1',
                  optionTone(opt)
                )}
              >
                {opt.name || opt.optionId || 'option'}
              </button>
            ))}
          </div>
          <label className="inline-flex cursor-pointer items-center gap-1.5 text-[11px] text-muted-foreground">
            <input
              type="checkbox"
              checked={remember}
              onChange={(e) => setRemember(e.target.checked)}
              className="size-3 cursor-pointer"
            />
            Remember my choice and apply automatically next time
          </label>
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
    const isError = trimmed.startsWith('[Error');
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
      <div className="max-w-[90%] rounded rounded-br-[2px] border border-[color:var(--user-bubble)]/40 bg-[color:var(--user-bubble)]/15 px-4 py-3 sm:max-w-[78%]">
        <div className="whitespace-pre-wrap break-words text-foreground">{cleaned}</div>
        <div className="mt-2 flex items-center gap-2.5">
          <CopyButton text={cleaned} title="Copy message" className="size-7" />
          <TimestampLabel ts={entry.timestamp} now={now} />
        </div>
      </div>
    </div>
  );
};

export const LogPane = ({ session, isActive }: Props) => {
  const scrollRef = useRef<HTMLDivElement>(null);
  const lastScrollHeight = useRef(0);
  const tick = useTick();
  const kbInset = useKeyboardInsetValue();
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

  // Re-pin the scroll to the bottom when the virtual keyboard opens
  // or closes. Without this the content that was flush with the
  // composer would end up either stranded above the now-lifted
  // composer (keyboard open) or leaving a gap below the new resting
  // position (keyboard closed). Only runs when the session was
  // already pinned; if the user had scrolled up to read older
  // messages, we leave their scroll position alone.
  useEffect(() => {
    const el = scrollRef.current;
    if (!el || !session.pinnedToBottom) {
      return;
    }
    el.scrollTop = el.scrollHeight;
  }, [kbInset, session.pinnedToBottom]);

  useEffect(() => {
    const el = scrollRef.current;
    if (!el) {
      return;
    }
    const onScroll = () => {
      const pinned = el.scrollHeight - el.scrollTop - el.clientHeight < 20;
      mezameActions.setPinnedToBottom(session.id, pinned);
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
        // final message isn't permanently hidden. The 13rem base
        // covers the composer at its MIN_ROWS height plus gutters;
        // `--mz-kb-inset` lifts the reserved area above the virtual
        // keyboard when it is open, and `--mz-safe-bottom` clears
        // the iOS home indicator. Both vars default to 0 on desktop.
        'flex-1 overflow-y-auto px-3 pt-3 break-words scrollbar-thin',
        !isActive && 'hidden'
      )}
      style={{
        paddingBottom:
          'calc(13rem + var(--mz-kb-inset) + var(--mz-safe-bottom))'
      }}
    >
      {session.log.map((entry) => {
        if (entry.kind === 'text') {
          return <TextEntry key={entry.id} entry={entry} now={now} />;
        }
        if (entry.kind === 'tool_call') {
          return <ToolCallCard key={entry.id} entry={entry} />;
        }
        if (entry.kind === 'mcp_oauth') {
          return <McpOauthCard key={entry.id} session={session} entry={entry} />;
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
