import { CheckIcon, ChevronDownIcon } from 'lucide-react';
import type { ReactNode } from 'react';
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
import { okiroActions } from '@/hooks/useOkiro';
import { cn } from '@/lib/utils';
import type { Session } from '@/types';

// Tab-level selectors for the agent "mode" (really Kiro agent variants
// like kiro_default / kiro_planner / kiro_guide) and the underlying
// model. Both are session-scoped and disabled while the session is busy
// — Kiro doesn't guarantee mid-turn mode/model switches behave sensibly.

type Props = {
  session: Session | null;
  /** `row` packs both pickers side-by-side (legacy). `stack` places
   * them vertically so they fit next to a tall textarea without
   * stealing width. */
  layout?: 'row' | 'stack';
};

const triggerClass = cn(
  'h-7 gap-1.5 rounded-md border border-border bg-card px-2 text-[11px] text-foreground',
  'hover:text-foreground hover:bg-accent',
  'data-[state=open]:bg-accent data-[state=open]:text-foreground'
);

const itemClass = 'gap-2 py-1.5 text-xs';

const Picker = <T extends { id: string; label: string; description?: string }>({
  label,
  options,
  currentId,
  onPick,
  disabled,
  emptyLabel,
  triggerLabel
}: {
  label: string;
  options: T[];
  currentId: string | null;
  onPick: (id: string) => void;
  disabled: boolean;
  emptyLabel: string;
  triggerLabel: ReactNode;
}) => (
  <DropdownMenu>
    <Tooltip>
      <TooltipTrigger asChild>
        <DropdownMenuTrigger asChild disabled={disabled || options.length === 0}>
          <Button variant="ghost" size="sm" className={triggerClass}>
            <span className="text-muted-foreground">{label}:</span>
            <span className="max-w-[10rem] truncate">
              {options.length === 0 ? emptyLabel : triggerLabel}
            </span>
            <ChevronDownIcon className="size-3 shrink-0 opacity-60" />
          </Button>
        </DropdownMenuTrigger>
      </TooltipTrigger>
      <TooltipContent side="top">{label}</TooltipContent>
    </Tooltip>
    <DropdownMenuContent align="start" className="max-h-[60vh] overflow-y-auto">
      <DropdownMenuLabel>{label}</DropdownMenuLabel>
      <DropdownMenuSeparator />
      {options.length === 0 ? (
        <div className="px-2 py-1.5 text-xs text-muted-foreground">{emptyLabel}</div>
      ) : (
        options.map((opt) => {
          const active = opt.id === currentId;
          return (
            <DropdownMenuItem key={opt.id} className={itemClass} onSelect={() => onPick(opt.id)}>
              <CheckIcon
                className={cn('mt-0.5 size-3 shrink-0', active ? 'opacity-100' : 'opacity-0')}
              />
              <div className="min-w-0 flex-1">
                <div className="font-medium text-foreground">{opt.label}</div>
                {opt.description && (
                  <div className="text-[11px] text-muted-foreground">{opt.description}</div>
                )}
              </div>
            </DropdownMenuItem>
          );
        })
      )}
    </DropdownMenuContent>
  </DropdownMenu>
);

export const ModeModelSelectors = ({ session, layout = 'row' }: Props) => {
  if (!session) {
    return null;
  }
  // Hide the group entirely when the agent didn't advertise any modes
  // or models. Non-Kiro agents often don't, and the empty "none"
  // dropdowns add noise.
  if (session.modes.length === 0 && session.models.length === 0) {
    return null;
  }
  const busy = session.busy;

  const modeOptions = session.modes.map((m) => ({
    id: m.id,
    label: m.name || m.id,
    description: m.description
  }));
  const modelOptions = session.models.map((m) => ({
    id: m.modelId,
    label: m.name || m.modelId,
    description: m.description
  }));

  const currentMode = session.modes.find((m) => m.id === session.currentModeId);
  const currentModel = session.models.find((m) => m.modelId === session.currentModelId);

  const containerClass =
    layout === 'stack' ? 'flex flex-col gap-1.5 w-40' : 'flex items-center gap-1.5 flex-wrap justify-end';

  return (
    <div className={containerClass}>
      <Picker
        label="Agent"
        options={modeOptions}
        currentId={session.currentModeId}
        onPick={okiroActions.setMode}
        disabled={busy}
        emptyLabel="none"
        triggerLabel={currentMode?.name || currentMode?.id || '—'}
      />
      <Picker
        label="Model"
        options={modelOptions}
        currentId={session.currentModelId}
        onPick={okiroActions.setModel}
        disabled={busy}
        emptyLabel="none"
        triggerLabel={currentModel?.name || currentModel?.modelId || '—'}
      />
    </div>
  );
};
