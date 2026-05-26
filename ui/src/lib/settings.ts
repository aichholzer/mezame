// Settings store. Persisted to `state.json` via the existing PUT /state
// endpoint, alongside the session list. Read on init, written on each
// change.
//
// Currently only carries the notification preference; will grow as
// other features (sounds, theme, custom CSS) settle.

export type NotificationPreference = 'unset' | 'pending' | 'on' | 'off';

type Settings = {
  notifications: NotificationPreference;
};

const DEFAULTS: Settings = {
  notifications: 'unset'
};

const STATE_URL = '/state';

let current: Settings = { ...DEFAULTS };
let initStarted = false;

const listeners = new Set<() => void>();

const notify = () => {
  for (const l of listeners) {
    l();
  }
};

/** Subscribe to settings changes. Returns an unsubscribe function. */
export const subscribeToSettings = (l: () => void): (() => void) => {
  listeners.add(l);
  return () => listeners.delete(l);
};

/** Used by `useSyncExternalStore` and friends. */
export const getSettingsSnapshot = (): Settings => current;

export const getNotificationPreference = (): NotificationPreference =>
  current.notifications;

export const setNotificationPreference = (next: NotificationPreference): void => {
  if (current.notifications === next) {
    return;
  }
  current = { ...current, notifications: next };
  notify();
  void persist();
};

/** Hydrate from /state on app boot. Idempotent. */
export const initSettings = async (): Promise<void> => {
  if (initStarted) {
    return;
  }
  initStarted = true;
  try {
    const res = await fetch(STATE_URL);
    if (!res.ok) {
      return;
    }
    const body = (await res.json()) as { settings?: Partial<Settings> };
    if (body.settings && typeof body.settings === 'object') {
      const pref = body.settings.notifications;
      if (
        pref === 'unset' ||
        pref === 'pending' ||
        pref === 'on' ||
        pref === 'off'
      ) {
        current = { ...current, notifications: pref };
        notify();
      }
    }
  } catch {
    // Network failure: fall back to defaults. The app keeps working;
    // settings just stay at their defaults until /state is reachable.
  }
};

let persistTimer: number | null = null;

/** Debounced PUT /state. Reads the existing state, merges in the
 * settings, writes the result. Mirrors how `useMezame.scheduleSync`
 * persists session state but lives separately because settings change
 * cadence and shape are different. */
const persist = async (): Promise<void> => {
  if (persistTimer !== null) {
    clearTimeout(persistTimer);
  }
  persistTimer = window.setTimeout(async () => {
    persistTimer = null;
    try {
      // Read-then-write: server is the source of truth for fields we
      // do not own (sessions, closed, activeId, nextLabel).
      const existing: Record<string, unknown> = {};
      try {
        const res = await fetch(STATE_URL);
        if (res.ok) {
          Object.assign(existing, (await res.json()) as Record<string, unknown>);
        }
      } catch {
        // Best effort: write only what we own if the read failed.
      }
      const body = { ...existing, settings: { ...current } };
      await fetch(STATE_URL, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body)
      });
    } catch {
      // Persistence is best-effort; UI keeps working with the in-memory
      // snapshot.
    }
  }, 250);
};

/** Reset internal state for tests. Not exported via the package's
 * public API; tests reach for it via the typed import. */
export const __resetSettingsForTests = (): void => {
  current = { ...DEFAULTS };
  initStarted = false;
  if (persistTimer !== null) {
    clearTimeout(persistTimer);
    persistTimer = null;
  }
  listeners.clear();
};
