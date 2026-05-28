import { HistoryIcon, PlusIcon, XIcon } from 'lucide-react';
import { useEffect, useState } from 'react';
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
import { useSidebarWidth } from '@/hooks/useSidebarWidth';
import { cn } from '@/lib/utils';
import type { Attention, ClosedEntry, Session } from '@/types';

// Fixed sidebar on desktop, slide-in drawer on mobile.
//
// Layout (top to bottom):
//   - Brand label (MEZAME wordmark, in the display font).
//   - Action row: History dropdown, New session button.
//   - Divider.
//   - Scrollable list of session rows, one tab per row, full width.
//
// On mobile the sidebar is hidden by default. `isOpen` is driven by the
// parent: the burger button in the chat pane flips it, and tapping any
// session row triggers `onRequestClose` so the drawer hides as the tab
// takes over the viewport.
//
// Active-state indicator is a 3 px accent bar on the row's left edge
// (replaces the chip-style ring used by the old horizontal bar). The
// status-driven fills (connected/connecting/error/busy-background) are
// unchanged.

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
  /** Drawer visibility on mobile. Ignored at desktop widths where the
   * sidebar is always rendered. */
  isOpen: boolean;
  /** Invoked when the drawer should close (tab activated, backdrop
   * tapped, close button pressed). No-op on desktop. */
  onRequestClose: () => void;
};

// Attention dot: fill carries the semantic (done/permission/error), a
// white outline plus drop shadow keeps it legible on top of any row
// background colour.
const attentionClass: Record<NonNullable<Attention>, string> = {
  done: 'bg-[color:var(--attn-done)]',
  permission: 'bg-[color:var(--attn-permission)]',
  error: 'bg-[color:var(--attn-error)]'
};

const attentionDotBase =
  'size-2 rounded-full ring-2 ring-background shadow-[0_0_0_1px_rgba(0,0,0,0.35)]';

// Per-status row backgrounds. Kept subtle (~18% of the accent colour)
// so many rows remain readable stacked; the active row still gets its
// left accent bar on top.
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
  connecting: { animation: 'mezame-pulse-orange 1.4s ease-in-out infinite' }
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

export const SideBar = ({
  sessions,
  activeId,
  closed,
  onActivate,
  onClose,
  onRename,
  onNewTab,
  onRestore,
  onForget,
  isOpen,
  onRequestClose
}: Props) => {
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState('');
  // Resizable on desktop. The hook persists the width to localStorage
  // and clamps to a sensible band (192-480 px) so the sidebar can
  // never collapse onto the edge or eat the chat pane. On mobile the
  // sidebar is a fixed-width drawer; the runtime width is ignored
  // there via the `md:` width override below.
  const sidebar = useSidebarWidth();

  // Keep the active row visible when activation changes programmatically
  // (new session, restore from history, keyboard switch).
  useEffect(() => {
    if (!activeId) {
      return;
    }
    const row = document.querySelector(`[data-tab-id="${CSS.escape(activeId)}"]`);
    if (row instanceof HTMLElement) {
      row.scrollIntoView({ block: 'nearest' });
    }
  }, [activeId]);

  const commitRename = () => {
    if (renamingId && renameValue.trim()) {
      onRename(renamingId, renameValue.trim());
    }
    setRenamingId(null);
    setRenameValue('');
  };

  const handleActivate = (id: string) => {
    onActivate(id);
    // Drawer hides itself on mobile so the selected tab takes over the
    // viewport. No-op at desktop widths.
    onRequestClose();
  };

  return (
    <>
      {/* Backdrop: mobile-only, dims the chat while the drawer is
       * open so the modal affordance is unambiguous. Tapping it
       * closes the drawer. Hidden on desktop where the sidebar is
       * static. */}
      <div
        onClick={onRequestClose}
        className={cn(
          'fixed inset-0 z-30 bg-background/60 backdrop-blur-xs md:hidden',
          isOpen ? 'opacity-100' : 'pointer-events-none opacity-0',
          'transition-opacity duration-200'
        )}
        aria-hidden="true"
      />

      <aside
        className={cn(
          // Base layout: fixed on the left, full height, scrolls its
          // own session list. Desktop always renders at translate-x-0;
          // mobile slides in from the left. Width is driven by the
          // resize hook at runtime (see `style` below); the Tailwind
          // class only sets a `flex-none`-style hint so grid math
          // does not reflow weirdly when the inline width updates.
          'fixed inset-y-0 left-0 z-40 flex flex-col',
          'border-r border-border/40 bg-background/95 backdrop-blur-md',
          'md:static md:bg-background/70',
          // No transform transition while dragging; the live resize
          // already updates every frame and the transition would lag
          // it visibly.
          !sidebar.dragging && 'transition-transform duration-200 ease-out',
          isOpen ? 'translate-x-0' : '-translate-x-full md:translate-x-0'
        )}
        style={{
          width: `${sidebar.width}px`,
          paddingTop: 'var(--mz-safe-top)',
          paddingBottom: 'var(--mz-safe-bottom)',
          paddingLeft: 'var(--mz-safe-left)'
        }}
      >
        <div className="flex items-center justify-center px-3 pt-6 pb-5">
          <span
            className="text-[1.75rem] tracking-wide text-[color:var(--primary)] select-none"
            style={{ fontFamily: 'var(--font-display)' }}
          >
            MEZAME
          </span>
        </div>

        <div className="flex items-center gap-2 px-3 pb-2">
          <DropdownMenu>
            <Tooltip>
              <TooltipTrigger asChild>
                <DropdownMenuTrigger asChild>
                  <Button
                    size="icon"
                    variant="outline"
                    className="size-8 text-[color:var(--primary)]"
                    aria-label="History"
                  >
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
                <div className="px-2 py-1.5 text-xs text-muted-foreground">No recently closed sessions</div>
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
              <Button
                size="icon"
                variant="outline"
                className="size-8 text-[color:var(--primary)]"
                onClick={onNewTab}
                aria-label="New session"
              >
                <PlusIcon className="size-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent side="bottom">New session</TooltipContent>
          </Tooltip>

          {/* Mobile-only close button so the drawer can be dismissed
           * without tapping the backdrop. */}
          <Button
            size="icon"
            variant="ghost"
            className="size-8 ml-auto md:hidden"
            onClick={onRequestClose}
            aria-label="Close sidebar"
          >
            <XIcon className="size-4" />
          </Button>
        </div>

        <div className="mx-3 border-t border-[color:var(--primary)]/30" />

        <div className="flex min-h-0 flex-1 flex-col gap-1 overflow-y-auto scrollbar-thin px-3 py-2">
          {sessions.map((s) => {
            const isActive = s.id === activeId;
            const isRenaming = renamingId === s.id;
            const visual = tabVisualState(s, isActive);
            const visualClass = tabVisualClass[visual];
            // Attention dots signal "something finished in a tab you are
            // not looking at" (done) or "the agent is waiting on you"
            // (permission). Shown on Connected rows and on Busy-in-
            // background rows (so a pending permission prompt remains
            // visible on top of the green pulse). Suppressed on the
            // active row (the user is already looking) and when the row
            // carries its own strong colour (error / connecting).
            const showAttentionDot =
              !isActive &&
              s.attention !== null &&
              (visual === 'connected' || visual === 'busy-background');
            return (
              <Tooltip key={s.id}>
                <TooltipTrigger asChild>
                  <div
                    data-tab-id={s.id}
                    className={cn(
                      // Full-width row. h-9 (36 px) on desktop, h-11
                      // (44 px) on touch for thumb-friendly targets.
                      // Left padding reserves space for the active
                      // accent bar so content doesn't shift between
                      // states.
                      'group relative flex h-9 touch:h-11 w-full cursor-pointer items-center gap-2 rounded-sm border pl-3 pr-2 text-xs touch:text-[13px] select-none touch-manipulation',
                      visualClass
                    )}
                    style={tabVisualStyle[visual]}
                    onClick={() => !isRenaming && handleActivate(s.id)}
                    onDoubleClick={(ev) => {
                      ev.stopPropagation();
                      setRenamingId(s.id);
                      setRenameValue(s.label);
                    }}
                  >
                    {/* Active-row accent bar: sits inside the row's
                     * left padding so it never pushes content. */}
                    {isActive && (
                      <span
                        aria-hidden="true"
                        className="absolute inset-y-1 left-0.5 w-[3px] rounded-full bg-[color:var(--primary)]"
                      />
                    )}

                    {showAttentionDot && s.attention && (
                      <span className={cn(attentionDotBase, attentionClass[s.attention])} />
                    )}

                    {isRenaming ? (
                      <input
                        autoFocus
                        value={renameValue}
                        onChange={(e) => setRenameValue(e.target.value)}
                        onBlur={commitRename}
                        onClick={(e) => e.stopPropagation()}
                        onKeyDown={(e) => {
                          if (e.key === 'Enter') {
                            commitRename();
                          } else if (e.key === 'Escape') {
                            setRenamingId(null);
                            setRenameValue('');
                          }
                        }}
                        className="h-6 flex-1 rounded-sm bg-transparent px-1 text-base md:text-xs outline-hidden"
                      />
                    ) : (
                      <span className="min-w-0 flex-1 truncate">{s.label}</span>
                    )}

                    <button
                      type="button"
                      className="cursor-pointer rounded-sm p-0.5 touch:p-1.5 text-muted-foreground/60 hover:text-[color:var(--attn-error)]"
                      aria-label="Close session"
                      onClick={(ev) => {
                        ev.stopPropagation();
                        onClose(s.id);
                      }}
                    >
                      <XIcon className="size-3 touch:size-4" />
                    </button>
                  </div>
                </TooltipTrigger>
                <TooltipContent side="right">
                  <div>{s.cwd ? `${s.label} · ${s.cwd}` : s.label}</div>
                  <div className="text-muted-foreground mt-2">{tabTooltipStatus(visual)}</div>
                  <div className="text-muted-foreground">Double-click to rename.</div>
                </TooltipContent>
              </Tooltip>
            );
          })}
        </div>

        {/* Resize handle. A 6 px-wide strip pinned to the right edge
         * (overlapping the sidebar's own border by half so the cursor
         * lands on the visible boundary). Desktop-only: on mobile the
         * sidebar is a drawer and resize would not make sense.
         * Pointer events are wired to the hook, which owns the drag
         * lifecycle including document-level move/up listeners. */}
        <div
          role="separator"
          aria-orientation="vertical"
          aria-label="Resize sidebar"
          onPointerDown={sidebar.beginDrag}
          className={cn(
            'absolute inset-y-0 right-0 hidden w-1.5 -mr-0.5 cursor-col-resize md:block',
            // A subtle hover tint reveals the handle without putting a
            // permanent line over the existing right border. While
            // dragging we light it up to make the action feel direct.
            'transition-colors duration-150',
            sidebar.dragging
              ? 'bg-[color:var(--primary)]/40'
              : 'hover:bg-[color:var(--primary)]/20'
          )}
        />
      </aside>
    </>
  );
};
