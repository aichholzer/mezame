import { CheckIcon, MonitorIcon, MoonIcon, SunIcon } from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
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
import { setThemePreference, useThemePreference } from '@/hooks/useTheme';
import type { ThemePreference } from '@/lib/settings';

// Theme picker for the sidebar footer. An icon button (showing the
// active preference's glyph) opens a three-option menu: System, Light,
// Dark. Mirrors the History dropdown's trigger styling.

const OPTIONS: { value: ThemePreference; label: string; icon: LucideIcon }[] = [
  { value: 'system', label: 'System', icon: MonitorIcon },
  { value: 'light', label: 'Light', icon: SunIcon },
  { value: 'dark', label: 'Dark', icon: MoonIcon }
];

export const ThemeToggle = () => {
  const pref = useThemePreference();
  const active = OPTIONS.find((o) => o.value === pref) ?? OPTIONS[0];
  const TriggerIcon = active.icon;

  return (
    <DropdownMenu>
      <Tooltip>
        <TooltipTrigger asChild>
          <DropdownMenuTrigger asChild>
            <Button
              size="icon"
              variant="outline"
              className="size-9 rounded-lg text-[color:var(--primary)]"
              aria-label={`Theme: ${active.label}`}
            >
              <TriggerIcon className="size-4" />
            </Button>
          </DropdownMenuTrigger>
        </TooltipTrigger>
        <TooltipContent side="top">Theme: {active.label}</TooltipContent>
      </Tooltip>
      <DropdownMenuContent align="start" side="top">
        <DropdownMenuLabel>Appearance</DropdownMenuLabel>
        <DropdownMenuSeparator />
        {OPTIONS.map(({ value, label, icon: Icon }) => (
          <DropdownMenuItem
            key={value}
            onSelect={() => setThemePreference(value)}
            className="items-center"
          >
            <Icon className="size-4 text-muted-foreground" />
            <span className="flex-1">{label}</span>
            {value === pref && <CheckIcon className="size-4 text-[color:var(--primary)]" />}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
};
