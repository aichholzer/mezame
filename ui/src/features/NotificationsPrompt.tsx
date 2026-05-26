import { BellIcon, XIcon } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  notificationsAvailable,
  useNotificationPreference
} from '@/hooks/useNotifications';
import { setNotificationPreference } from '@/lib/settings';

// Renders only when the preference is `pending`: that flag is bumped
// by `useNotifications` the first time an event would have fired but
// we have not asked the user yet. Click "Enable" calls the browser
// permission prompt and persists the choice; "Not now" sets the
// preference to `off` so we never bother again until the user opts
// in from settings.

export const NotificationsPrompt = () => {
  const preference = useNotificationPreference();
  if (preference !== 'pending' || !notificationsAvailable()) {
    return null;
  }

  const enable = async () => {
    try {
      const result = await Notification.requestPermission();
      setNotificationPreference(result === 'granted' ? 'on' : 'off');
    } catch {
      setNotificationPreference('off');
    }
  };

  const dismiss = () => setNotificationPreference('off');

  return (
    <div
      role="status"
      aria-live="polite"
      className={
        'fixed left-1/2 top-3 z-50 flex -translate-x-1/2 items-center gap-3 ' +
        'rounded-md border border-border bg-card px-3 py-2 text-sm shadow-lg'
      }
    >
      <BellIcon className="size-4 text-[color:var(--primary)]" />
      <span>
        Get a desktop notification when a background session needs attention?
      </span>
      <Button size="sm" onClick={enable}>
        Enable
      </Button>
      <Button size="sm" variant="ghost" onClick={dismiss} aria-label="Dismiss">
        <XIcon className="size-4" />
      </Button>
    </div>
  );
};
