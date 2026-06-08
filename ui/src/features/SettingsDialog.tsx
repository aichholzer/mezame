import { SettingsIcon } from 'lucide-react';
import { useSyncExternalStore } from 'react';
import { Button } from '@/components/ui/button';
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
  setAutoAllowPermissions,
  subscribeToSettings
} from '@/lib/settings';

// Settings pane. A cog button in the sidebar footer (beside the theme
// picker) opens a dialog that hosts app-wide preferences. The pane is
// built as a list of rows so future toggles slot in without reworking
// the layout.
//
// The first row is "auto-allow all permissions": when on, Mezame
// answers every agent permission request with an allow option instead
// of showing an inline approval card. The decision is enforced
// server-side (the hub reads the persisted flag from state.json on
// each `session/request_permission`), so it applies to every attached
// device and to unattended sessions alike. Off by default; the copy
// spells out the risk because the toggle removes the human approval
// step for all tool calls, including file writes and shell commands.

/** Reactive read of the auto-allow preference from the settings store. */
const useAutoAllowPermissions = (): boolean =>
  useSyncExternalStore(
    subscribeToSettings,
    () => getSettingsSnapshot().autoAllowPermissions,
    () => getSettingsSnapshot().autoAllowPermissions
  );

/** Accessible on/off switch. No checkbox primitive ships in the UI kit,
 * so this is a minimal `role="switch"` button styled to match the
 * existing controls. */
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
  const autoAllow = useAutoAllowPermissions();

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
              <span className="text-sm text-foreground">Auto-allow all permissions</span>
              <span className="text-xs text-muted-foreground">
                Approve every tool request automatically, without a prompt. This includes file
                edits and shell commands. Leave off unless you trust the agent in this workspace.
              </span>
            </div>
            <Switch
              checked={autoAllow}
              onChange={setAutoAllowPermissions}
              label="Auto-allow all permissions"
            />
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
};
