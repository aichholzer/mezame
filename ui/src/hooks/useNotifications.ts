import { useEffect, useRef, useSyncExternalStore } from 'react';
import { useMezame } from '@/hooks/useMezame';
import {
  getSettingsSnapshot,
  setNotificationPreference,
  subscribeToSettings,
  type NotificationPreference
} from '@/lib/settings';

// Browser push notifications for background sessions.
//
// When a session that is NOT the active in-app tab raises attention,
// or the entire Mezame browser tab is hidden, fire `new Notification`
// so the user sees something happen even when they aren't looking.
// The favicon badge and attention-dot logic still run; this is an
// additional channel, not a replacement.
//
// First-use flow: the preference starts at `unset`. The first time an
// event would fire, we bump the preference to `pending`; the
// `NotificationsPrompt` banner watches for that and surfaces an
// in-app prompt asking the user to opt in. Click "Enable" calls
// `Notification.requestPermission()` and flips the preference to
// `on`.

type AttentionLevel = 'done' | 'permission' | 'error';

const TITLE_BY_LEVEL: Record<AttentionLevel, string> = {
  done: 'Turn complete',
  permission: 'Permission requested',
  error: 'Error'
};

/**
 * Whether the runtime supports `Notification` and we're in a secure
 * context (`isSecureContext` is true on https:// and 127.0.0.1 /
 * localhost). Returns false in jsdom and on insecure remote origins.
 */
export const notificationsAvailable = (): boolean => {
  if (typeof window === 'undefined') {
    return false;
  }
  if (!('Notification' in window)) {
    return false;
  }
  return window.isSecureContext === true;
};

const fireNotification = (sessionLabel: string, level: AttentionLevel): void => {
  if (!notificationsAvailable()) {
    return;
  }
  if (Notification.permission !== 'granted') {
    return;
  }
  try {
    const n = new Notification(`Mezame: ${TITLE_BY_LEVEL[level]}`, {
      body: `Session ${sessionLabel}`,
      icon: '/favicon.png',
      // `tag` causes the OS to replace any prior notification with the
      // same key, so rapid-fire status changes don't stack into a wall
      // of toasts.
      tag: `mezame-${sessionLabel}-${level}`
    });
    n.onclick = () => {
      window.focus();
      n.close();
    };
  } catch {
    // Quota errors, runtime restrictions, etc. Drop silently.
  }
};

/** Subscribe a component to the notification preference. */
export const useNotificationPreference = (): NotificationPreference =>
  useSyncExternalStore(
    subscribeToSettings,
    () => getSettingsSnapshot().notifications,
    () => getSettingsSnapshot().notifications
  );

/** Mount once at the app root. */
export const useNotifications = (): void => {
  const { sessions, activeId } = useMezame();
  const preference = useNotificationPreference();
  const previous = useRef<Map<string, AttentionLevel | null>>(new Map());

  useEffect(() => {
    const seen: Set<string> = new Set();
    for (const s of sessions) {
      seen.add(s.id);
      const before = previous.current.get(s.id) ?? null;
      const now = s.attention;

      const transitioned = now !== null && now !== before;
      const isBackground =
        s.id !== activeId ||
        (typeof document !== 'undefined' && document.visibilityState === 'hidden');

      if (transitioned && isBackground) {
        if (preference === 'on') {
          fireNotification(s.label, now);
        } else if (preference === 'unset' && notificationsAvailable()) {
          // Surface the prompt banner. We deliberately do not fire the
          // notification yet; the user may decline.
          setNotificationPreference('pending');
        }
      }
      previous.current.set(s.id, now);
    }
    for (const id of [...previous.current.keys()]) {
      if (!seen.has(id)) {
        previous.current.delete(id);
      }
    }
  }, [sessions, activeId, preference]);
};
