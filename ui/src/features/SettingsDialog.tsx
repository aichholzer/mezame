import { SettingsIcon } from 'lucide-react';
import { useSyncExternalStore } from 'react';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger
} from '@/components/ui/dialog';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { cn } from '@/lib/utils';
import {
  getSettingsSnapshot,
  IDLE_SUSPEND_MAX_MINUTES,
  IDLE_SUSPEND_MIN_MINUTES,
  setIdleSuspendMinutes,
  setSendOnEnter,
  subscribeToSettings
} from '@/lib/settings';

// Settings pane. A cog button in the sidebar footer (beside the theme
// picker) opens a dialog that hosts app-wide preferences. The pane is
// built as a list of rows. Every preference here is saved to `state.json`
// through PUT /state, so it follows the user across devices.

/** Reactive read of the send-on-Enter preference from the settings store. */
const useSendOnEnter = (): boolean =>
  useSyncExternalStore(
    subscribeToSettings,
    () => getSettingsSnapshot().sendOnEnter,
    () => getSettingsSnapshot().sendOnEnter
  );

/** Reactive read of the idle-suspend threshold (minutes). */
const useIdleSuspendMinutes = (): number =>
  useSyncExternalStore(
    subscribeToSettings,
    () => getSettingsSnapshot().idleSuspendMinutes,
    () => getSettingsSnapshot().idleSuspendMinutes
  );

/** Accessible on/off switch. No checkbox primitive ships in the UI kit.
 * This is a minimal `role="switch"` button styled to match the existing
 * controls. */
const Switch = ({
  checked,
  onChange,
  label
}: {
  checked: boolean;
  onChange: (next: boolean) => void;
  label: string;
}) => (
  <button
    type="button"
    role="switch"
    aria-checked={checked}
    aria-label={label}
    onClick={() => onChange(!checked)}
    className={cn(
      'relative inline-flex h-5 w-9 shrink-0 cursor-pointer items-center rounded-full transition-colors',
      'focus-visible:outline-hidden focus-visible:ring-2 focus-visible:ring-ring',
      checked ? 'bg-primary' : 'bg-[color:var(--outline-variant)]'
    )}
  >
    <span
      className={cn(
        'inline-block size-4 rounded-full bg-card shadow transition-transform',
        checked ? 'translate-x-4' : 'translate-x-0.5'
      )}
    />
  </button>
);

export const SettingsDialog = () => {
  const sendOnEnter = useSendOnEnter();
  const idleMinutes = useIdleSuspendMinutes();

  return (
    <Dialog>
      <Tooltip>
        <TooltipTrigger asChild>
          <DialogTrigger asChild>
            <Button
              size="icon"
              variant="outline"
              className="size-9 rounded-lg text-[color:var(--primary)]"
              aria-label="Settings"
            >
              <SettingsIcon className="size-4" />
            </Button>
          </DialogTrigger>
        </TooltipTrigger>
        <TooltipContent side="top">Settings</TooltipContent>
      </Tooltip>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Settings</DialogTitle>
          <DialogDescription>Preferences are saved across all your sessions and devices.</DialogDescription>
        </DialogHeader>
        <div className="flex flex-col gap-4 pt-1">
          <div className="flex items-start justify-between gap-4">
            <div className="flex flex-col gap-0.5">
              <span className="text-sm text-foreground">Send message shortcut</span>
              <span className="text-xs text-muted-foreground">
                Choose if enter sends message or creates new line
              </span>
            </div>
            <Switch
              checked={sendOnEnter}
              onChange={setSendOnEnter}
              label="Send message shortcut"
            />
          </div>
          <div className="flex flex-col gap-2">
            <div className="flex flex-col gap-0.5">
              <span className="text-sm text-foreground">Suspend idle sessions after</span>
              <span className="text-xs text-muted-foreground">
                Disconnect a background session once it has been idle this long, freeing its
                agent and MCP servers. It reconnects automatically when you return to it.
              </span>
            </div>
            <div className="flex items-center gap-3">
              <input
                type="range"
                min={IDLE_SUSPEND_MIN_MINUTES}
                max={IDLE_SUSPEND_MAX_MINUTES}
                step={1}
                value={idleMinutes}
                onChange={(e) => setIdleSuspendMinutes(Number(e.target.value))}
                aria-label="Suspend idle sessions after (minutes)"
                className="h-1.5 flex-1 cursor-pointer accent-[color:var(--primary)]"
              />
              <div className="flex items-center gap-1">
                <Input
                  type="number"
                  min={IDLE_SUSPEND_MIN_MINUTES}
                  max={IDLE_SUSPEND_MAX_MINUTES}
                  value={idleMinutes}
                  onChange={(e) => setIdleSuspendMinutes(Number(e.target.value))}
                  aria-label="Suspend idle sessions after (minutes, exact)"
                  className="h-8 w-16 text-center"
                />
                <span className="text-xs text-muted-foreground">min</span>
              </div>
            </div>
            <div className="flex justify-between text-[11px] text-muted-foreground">
              <span>{IDLE_SUSPEND_MIN_MINUTES} min</span>
              <span>{IDLE_SUSPEND_MAX_MINUTES} min</span>
            </div>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
};
