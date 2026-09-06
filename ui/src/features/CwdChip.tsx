import { FolderIcon } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { cn } from '@/lib/utils';
import type { Session } from '@/types';

// Shows the directory Mezame is running in, as the `ready` event reported
// it. Display only: the server's own process directory is its one source,
// and no client can choose another.

type Props = {
  session: Session | null;
};

const SERVER_DEFAULT = 'server default';

// Middle-ellipsis for long paths. The parent and the leaf both stay
// visible: `/Users/YOURUSER/.../repos/mezame`.
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
  if (!session) {
    return null;
  }

  // Empty until the first `ready` lands.
  const cwd = session.effectiveCwd ?? '';
  const display = cwd.length > 0 ? truncateMiddle(cwd, 48) : SERVER_DEFAULT;

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className={cn(triggerClass, 'max-w-[60vw]')}
          aria-label="Working directory Mezame is running in"
        >
          <FolderIcon className="size-3 shrink-0 text-[color:var(--primary)]" />
          <span className="truncate">{display}</span>
        </Button>
      </TooltipTrigger>
      <TooltipContent side="top" className="max-w-[60ch]">
        <div className="font-mono text-[11px]">{cwd || SERVER_DEFAULT}</div>
        <div className="text-[10px] text-muted-foreground">
          The directory Mezame is running in
        </div>
      </TooltipContent>
    </Tooltip>
  );
};
