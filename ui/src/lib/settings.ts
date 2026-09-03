// Settings store. Persisted to `state.json` via the existing PUT /state
// endpoint, alongside the session list. Read on init, written on each
// change.
//
// Holds the notification preference, the theme, the auto-allow flag,
// the send-on-Enter chord and the idle-suspend threshold. More will land
// here as other features (sounds, custom CSS) settle.

export type NotificationPreference = 'unset' | 'pending' | 'on' | 'off';

/** Theme preference. `system` follows the OS via `prefers-color-scheme`
 * and tracks live changes: an OS day/night schedule flips the app
 * automatically. `light`/`dark` are explicit overrides. */
export type ThemePreference = 'system' | 'light' | 'dark';

type Settings = {
  notifications: NotificationPreference;
  theme: ThemePreference;
  autoAllowPermissions: boolean;
  sendOnEnter: boolean;
  idleSuspendMinutes: number;
};

const DEFAULTS: Settings = {
  notifications: 'unset',
  theme: 'system',
  // Auto-allow all tool permissions. Off by default: a new install
  // always prompts for each tool call until the user opts in via the
  // Settings pane. Read server-side per permission request (see
  // `read_auto_allow_permissions` in the Rust core). Flipping it here
  // takes effect on the next request without a reconnect.
  autoAllowPermissions: false,
  // Send-on-Enter. True (default) keeps the classic chat behaviour:
  // a bare Enter submits and Shift+Enter inserts a newline. False
  // flips it: Enter inserts a newline and the platform modifier
  // (Cmd on macOS, Ctrl elsewhere) plus Enter submits. Purely a UI
  // affordance: unlike autoAllowPermissions the Rust core never reads
  // it. It rides the generic /state blob with no server change.
  sendOnEnter: true,
  // Minutes a backgrounded (or hidden active) session may sit idle after
  // its last turn before Mezame suspends it and frees its agent + MCP
  // fleet. Read by the idle scan in useMezame; rides the /state settings
  // blob. The Rust core never reads it (no server change).
  idleSuspendMinutes: 15
};

// Theme is mirrored to localStorage (in addition to the /state round
// trip the rest of the settings use) so it can be read synchronously
// before first paint; see `bootTheme` in hooks/useTheme.ts. Without
// this the app would paint light then flip to dark once /state
// resolves, a jarring flash for a night-mode feature.
/** Idle-suspend bounds, in minutes. The Settings slider clamps to this
 * inclusive range; the store clamps again defensively on every set so a
 * hand-edited state.json or an out-of-range number box cannot push the
 * threshold outside it. */
export const IDLE_SUSPEND_MIN_MINUTES = 1;
export const IDLE_SUSPEND_MAX_MINUTES = 60;

const clampIdleMinutes = (n: number): number => {
  if (!Number.isFinite(n)) {
    return DEFAULTS.idleSuspendMinutes;
  }
  return Math.min(
    IDLE_SUSPEND_MAX_MINUTES,
    Math.max(IDLE_SUSPEND_MIN_MINUTES, Math.round(n))
  );
};

const THEME_KEY = 'mezame.theme';

const isThemePreference = (v: unknown): v is ThemePreference =>
  v === 'system' || v === 'light' || v === 'dark';

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

export const getThemePreference = (): ThemePreference => current.theme;

export const setThemePreference = (next: ThemePreference): void => {
  if (current.theme === next) {
    return;
  }
  current = { ...current, theme: next };
  writeThemeToStorage(next);
  notify();
  void persist();
};

export const getAutoAllowPermissions = (): boolean =>
  current.autoAllowPermissions;

export const setAutoAllowPermissions = (next: boolean): void => {
  if (current.autoAllowPermissions === next) {
    return;
  }
  current = { ...current, autoAllowPermissions: next };
  notify();
  void persist();
};

export const getSendOnEnter = (): boolean => current.sendOnEnter;

export const setSendOnEnter = (next: boolean): void => {
  if (current.sendOnEnter === next) {
    return;
  }
  current = { ...current, sendOnEnter: next };
  notify();
  void persist();
};

export const getIdleSuspendMinutes = (): number => current.idleSuspendMinutes;

export const setIdleSuspendMinutes = (next: number): void => {
  const clamped = clampIdleMinutes(next);
  if (current.idleSuspendMinutes === clamped) {
    return;
  }
  current = { ...current, idleSuspendMinutes: clamped };
  notify();
  void persist();
};

/** Synchronous read of the persisted theme for pre-paint boot. Falls
 * back to the default when storage is empty or unavailable. */
export const readThemeFromStorage = (): ThemePreference => {
  try {
    const v = window.localStorage.getItem(THEME_KEY);
    return isThemePreference(v) ? v : DEFAULTS.theme;
  } catch {
    return DEFAULTS.theme;
  }
};

const writeThemeToStorage = (next: ThemePreference): void => {
  try {
    window.localStorage.setItem(THEME_KEY, next);
  } catch {
    // Private mode / storage disabled: the /state round trip still
    // persists; only the pre-paint fast path is lost.
  }
};

/** Hydrate from /state on app boot. Idempotent. */
export const initSettings = async (): Promise<void> => {
  if (initStarted) {
    return;
  }
  initStarted = true;
  // Seed theme from the synchronous localStorage mirror so the
  // in-memory snapshot agrees with what `bootTheme` already painted,
  // before the (slower, authoritative) /state read below.
  current = { ...current, theme: readThemeFromStorage() };
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
      const theme = body.settings.theme;
      if (isThemePreference(theme) && theme !== current.theme) {
        current = { ...current, theme };
        writeThemeToStorage(theme);
        notify();
      }
      const autoAllow = body.settings.autoAllowPermissions;
      if (typeof autoAllow === 'boolean' && autoAllow !== current.autoAllowPermissions) {
        current = { ...current, autoAllowPermissions: autoAllow };
        notify();
      }
      const sendOnEnter = body.settings.sendOnEnter;
      if (typeof sendOnEnter === 'boolean' && sendOnEnter !== current.sendOnEnter) {
        current = { ...current, sendOnEnter };
        notify();
      }
      const idleMins = body.settings.idleSuspendMinutes;
      if (typeof idleMins === 'number' && Number.isFinite(idleMins)) {
        const clamped = clampIdleMinutes(idleMins);
        if (clamped !== current.idleSuspendMinutes) {
          current = { ...current, idleSuspendMinutes: clamped };
          notify();
        }
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
  try {
    window.localStorage.removeItem(THEME_KEY);
  } catch {
    // ignore
  }
};
