import { HistoryIcon, PlusIcon, XIcon } from 'lucide-react';
import { useState } from 'react';
import { Button } from '@/components/ui/button';
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger
} from '@/components/ui/dropdown-menu';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { cn } from '@/lib/utils';
import type { Attention, ClosedEntry, Session } from '@/types';

// Vite injects the version string from `ui/package.json` at build time.
// See `vite.config.ts`.
declare const __OKIRO_VERSION__: string;

type Props = {
  sessions: Session[];
  activeId: string | null;
  closed: ClosedEntry[];
  onActivate: (id: string) => void;
  onClose: (id: string) => void;
  onRename: (id: string, label: string) => void;
  onNewTab: () => void;
  onRestore: (acpSessionId: string) => void;
  onForget: (acpSessionId: string) => void;
};

// Attention dot: fill carries the semantic (done/permission/error), a
// white outline plus drop shadow keeps it legible on top of any tab
// background colour (including the matching "Connected" green).
const attentionClass: Record<NonNullable<Attention>, string> = {
  done: 'bg-[color:var(--attn-done)]',
  permission: 'bg-[color:var(--attn-permission)]',
  error: 'bg-[color:var(--attn-error)]'
};

const attentionDotBase =
  'size-2 mr-1 rounded-full ring-2 ring-background shadow-[0_0_0_1px_rgba(0,0,0,0.35)]';

// Per-status tab backgrounds. Kept subtle (~18% of the accent colour)
// so many tabs remain readable side-by-side; the active tab still gets
// its usual accent highlight on top.
//
// The extra `busy-background` state pulses green for a tab that is
// still running a turn while the user has moved to another tab, so
// background work is never silently hidden. Precedence (top wins):
//   error > connecting/reconnecting > busy-in-background > connected.
type TabVisualState = 'connecting' | 'connected' | 'error' | 'busy-background';

const tabVisualState = (s: Session, isActive: boolean): TabVisualState => {
  if (s.status === 'error') {
    return 'error';
  }
  if (s.status === 'connecting' || s.status === 'reconnecting') {
    return 'connecting';
  }
  if (s.busy && !isActive) {
    return 'busy-background';
  }
  return 'connected';
};

const tabVisualClass: Record<TabVisualState, string> = {
  connecting: 'border-[color:var(--attn-permission)]/60 text-foreground',
  connected:
    'bg-[color:var(--attn-done)]/18 border-[color:var(--attn-done)]/45 text-foreground hover:bg-[color:var(--attn-done)]/28',
  error:
    'bg-[color:var(--attn-error)]/20 border-[color:var(--attn-error)]/55 text-foreground hover:bg-[color:var(--attn-error)]/30',
  // No Tailwind bg/border utilities here: the `tab-busy-border` class
  // owns them via a layered background (inner fill + conic gradient
  // around the border). See index.css.
  'busy-background': 'tab-busy-border text-foreground'
};

const tabVisualStyle: Partial<Record<TabVisualState, React.CSSProperties>> = {
  connecting: { animation: 'okiro-pulse-orange 1.4s ease-in-out infinite' }
};

const tabTooltipStatus = (state: TabVisualState): string => {
  switch (state) {
    case 'connecting':
      return 'Connecting...';
    case 'connected':
      return 'Connected';
    case 'error':
      return 'Disconnected';
    case 'busy-background':
      return 'Working...';
  }
};

const timeAgo = (ts: number): string => {
  const diff = Math.max(0, Date.now() - ts);
  const s = Math.floor(diff / 1000);
  if (s < 60) {
    return 'just now';
  }
  const m = Math.floor(s / 60);
  if (m < 60) {
    return `${m} min ago`;
  }
  const h = Math.floor(m / 60);
  if (h < 24) {
    return `${h} h ago`;
  }
  const d = Math.floor(h / 24);
  return `${d} d ago`;
};

export const TabBar = ({
  sessions,
  activeId,
  closed,
  onActivate,
  onClose,
  onRename,
  onNewTab,
  onRestore,
  onForget
}: Props) => {
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState('');

  const commitRename = () => {
    if (renamingId && renameValue.trim()) {
      onRename(renamingId, renameValue.trim());
    }
    setRenamingId(null);
    setRenameValue('');
  };

  return (
    <header className="border-b bg-background">
      <div className="flex w-full items-center gap-2 px-2 py-2.5">
        <DropdownMenu>
        <Tooltip>
          <TooltipTrigger asChild>
            <DropdownMenuTrigger asChild>
              <Button size="icon" variant="outline" className="size-8" aria-label="History">
                <HistoryIcon className="size-4" />
              </Button>
            </DropdownMenuTrigger>
          </TooltipTrigger>
          <TooltipContent side="bottom">Recently closed</TooltipContent>
        </Tooltip>
        <DropdownMenuContent align="start">
          <DropdownMenuLabel>Recently closed</DropdownMenuLabel>
          <DropdownMenuSeparator />
          {closed.length === 0 ? (
            <div className="px-2 py-1.5 text-xs text-muted-foreground">no recently closed sessions</div>
          ) : (
            closed.map((entry) => (
              <DropdownMenuItem
                key={entry.acpSessionId}
                onSelect={() => onRestore(entry.acpSessionId)}
                className="flex-col items-stretch gap-0.5"
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="text-sm text-foreground">{entry.label}</span>
                  <button
                    type="button"
                    className="rounded-sm px-1 text-muted-foreground/60 hover:text-[color:var(--attn-error)]"
                    onClick={(ev) => {
                      ev.stopPropagation();
                      ev.preventDefault();
                      onForget(entry.acpSessionId);
                    }}
                    aria-label="Forget"
                  >
                    <XIcon className="size-3" />
                  </button>
                </div>
                <div className="truncate text-[11px] text-muted-foreground">
                  {entry.cwd ? `${entry.cwd} · ` : ''}
                  {timeAgo(entry.closedAt)}
                </div>
              </DropdownMenuItem>
            ))
          )}
        </DropdownMenuContent>
      </DropdownMenu>

      <Tooltip>
        <TooltipTrigger asChild>
          <Button size="icon" variant="outline" className="size-8" onClick={onNewTab} aria-label="New session">
            <PlusIcon className="size-4" />
          </Button>
        </TooltipTrigger>
        <TooltipContent side="bottom">New session</TooltipContent>
      </Tooltip>

      <div className="flex min-w-0 flex-1 gap-1 overflow-x-auto scrollbar-thin">
        {sessions.map((s) => {
          const isActive = s.id === activeId;
          const isRenaming = renamingId === s.id;
          const visual = tabVisualState(s, isActive);
          const visualClass = tabVisualClass[visual];
          // Attention dots signal "something finished in a tab you are
          // not looking at" (done) or "the agent is waiting on you"
          // (permission). Shown on Connected tabs and on Busy-in-
          // background tabs (so a pending permission prompt remains
          // visible on top of the green pulse). Suppressed on the
          // active tab (the user is already looking) and when the tab
          // carries its own strong colour (error / connecting).
          const showAttentionDot =
            !isActive &&
            s.attention !== null &&
            (visual === 'connected' || visual === 'busy-background');
          return (
            <Tooltip key={s.id}>
              <TooltipTrigger asChild>
                <div
                  className={cn(
                    'group inline-flex h-8 cursor-pointer items-center gap-1.5 rounded-sm border px-2.5 text-xs select-none',
                    visualClass,
                    isActive && 'ring-1 ring-ring/50'
                  )}
                  style={tabVisualStyle[visual]}
                  onClick={() => !isRenaming && onActivate(s.id)}
                  onDoubleClick={(ev) => {
                    ev.stopPropagation();
                    setRenamingId(s.id);
                    setRenameValue(s.label);
                  }}
                >
                  {showAttentionDot && s.attention && (
                    <span className={cn(attentionDotBase, attentionClass[s.attention])} />
                  )}

                  {isRenaming ? (
                    <input
                      autoFocus
                      value={renameValue}
                      onChange={(e) => setRenameValue(e.target.value)}
                      onBlur={commitRename}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') {
                          commitRename();
                        } else if (e.key === 'Escape') {
                          setRenamingId(null);
                          setRenameValue('');
                        }
                      }}
                      className="h-6 w-28 rounded-sm bg-background px-1 text-xs outline-hidden focus:ring-1 focus:ring-ring"
                    />
                  ) : (
                    <span>{s.label}</span>
                  )}

                  <button
                    type="button"
                    className="cursor-pointer rounded-sm px-0.5 text-muted-foreground/60 hover:text-[color:var(--attn-error)]"
                    aria-label="Close session"
                    onClick={(ev) => {
                      ev.stopPropagation();
                      onClose(s.id);
                    }}
                  >
                    <XIcon className="size-3" />
                  </button>
                </div>
              </TooltipTrigger>
              <TooltipContent side="bottom">
                <div>{s.cwd ? `${s.label} · ${s.cwd}` : s.label}</div>
                <div className="text-muted-foreground mt-2">{tabTooltipStatus(visual)}</div>
                <div className="text-muted-foreground">Double-click to rename.</div>
              </TooltipContent>
            </Tooltip>
          );
        })}
      </div>

      <div className="flex items-center gap-1.5 select-none">
        <span className="text-sm font-bold tracking-wider text-foreground">Okiro!</span>
        <span className="text-[10px] text-muted-foreground/70 font-mono">{__OKIRO_VERSION__}</span>
      </div>
      </div>
    </header>
  );
};
