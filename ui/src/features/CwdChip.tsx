import { FolderIcon } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { mezameActions } from '@/hooks/useMezame';
import { cn } from '@/lib/utils';
import type { Session } from '@/types';

// Shows the session's working directory next to the mode/model pickers
// and lets the user spawn a sibling session pointed at a different
// path. Intentionally does NOT try to rebind the current session: ACP
// has no `session/set_cwd`, so changing cwd after `session/new` means
// starting a new session. We do that in a new tab so the existing log
// stays intact.
//
// Double-click (or Enter on focus) swaps the chip for an inline input
// preseeded with the current cwd. Commit spawns a new tab via
// mezameActions.newSession(cwd). Escape cancels.

type Props = {
  session: Session | null;
};

const SERVER_DEFAULT = 'server default';

// Middle-ellipsis for long paths so both the parent and leaf stay
// visible: `/Users/stefan/.../repos/mezame`.
const truncateMiddle = (value: string, max: number) => {
  if (value.length <= max) {
    return value;
  }
  const keep = Math.floor((max - 3) / 2);
  return `${value.slice(0, keep)}...${value.slice(-keep)}`;
};

const triggerClass = cn(
  'h-7 gap-1.5 rounded-md border border-border bg-card px-2 text-[11px] text-foreground',
  'hover:text-foreground hover:bg-accent',
  'data-[state=open]:bg-accent data-[state=open]:text-foreground'
);

export const CwdChip = ({ session }: Props) => {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (editing) {
      inputRef.current?.focus();
      inputRef.current?.select();
    }
  }, [editing]);

  if (!session) {
    return null;
  }

  // Prefer the server-reported cwd (actual path the agent session opened
  // at). Fall back to the user-supplied override, and finally to empty
  // while the session is still connecting.
  const cwd = session.effectiveCwd ?? session.cwd ?? '';
  const display = cwd.length > 0 ? truncateMiddle(cwd, 48) : SERVER_DEFAULT;

  const startEdit = () => {
    setDraft(cwd);
    setEditing(true);
  };

  const cancel = () => {
    setEditing(false);
    setDraft('');
  };

  const commit = () => {
    const next = draft.trim();
    if (next.length === 0 || next === cwd) {
      cancel();
      return;
    }
    // Spawn a sibling tab. The store places new tabs leftmost and
    // activates them automatically.
    mezameActions.newSession(next);
    cancel();
  };
  if (editing) {
    return (
      <Input
        ref={inputRef}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') {
            e.preventDefault();
            commit();
          } else if (e.key === 'Escape') {
            e.preventDefault();
            cancel();
          }
        }}
        onBlur={cancel}
        placeholder="/absolute/path"
        className="h-7 w-[22rem] max-w-[60vw] bg-card px-2 text-[11px]"
        spellCheck={false}
        autoCapitalize="off"
        autoCorrect="off"
      />
    );
  }

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          onDoubleClick={startEdit}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') {
              e.preventDefault();
              startEdit();
            }
          }}
          className={cn(triggerClass, 'max-w-[60vw]')}
          aria-label="Working directory (double-click to open a new session elsewhere)"
        >
          <FolderIcon className="size-3 shrink-0 opacity-70" />
          <span className="truncate">{display}</span>
        </Button>
      </TooltipTrigger>
      <TooltipContent side="top" className="max-w-[60ch]">
        <div className="font-mono text-[11px]">{cwd || SERVER_DEFAULT}</div>
        <div className="text-[10px] text-muted-foreground">
          Double-click to open a new session in a different directory
        </div>
      </TooltipContent>
    </Tooltip>
  );
};
