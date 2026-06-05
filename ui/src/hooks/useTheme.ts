import { useEffect } from 'react';
import { useSyncExternalStore } from 'react';
import {
  getSettingsSnapshot,
  getThemePreference,
  readThemeFromStorage,
  setThemePreference,
  subscribeToSettings,
  type ThemePreference
} from '@/lib/settings';

// Theme application layer.
//
// The CSS in index.css carries two palettes: the `:root` light tokens
// and a `.dark` override block. This module decides whether `.dark`
// belongs on <html> and keeps it in sync with both the user's
// preference and — when the preference is `system` — the OS's
// `prefers-color-scheme`.
//
// `system` is the default and the reason this feature exists: an OS
// configured to switch to dark on a schedule will flip Mezame with it,
// hands-free, via the matchMedia listener below.

const DARK_QUERY = '(prefers-color-scheme: dark)';

const prefersDark = (): boolean => {
  if (typeof window === 'undefined' || !window.matchMedia) {
    return false;
  }
  return window.matchMedia(DARK_QUERY).matches;
};

/** Resolve a preference to a concrete light/dark decision. */
export const resolveDark = (pref: ThemePreference): boolean =>
  pref === 'dark' || (pref === 'system' && prefersDark());

const applyDark = (dark: boolean): void => {
  if (typeof document === 'undefined') {
    return;
  }
  document.documentElement.classList.toggle('dark', dark);
};

/** Apply the persisted theme synchronously, before React mounts, to
 * avoid a flash of the light theme. Reads the localStorage mirror
 * (not /state, which is async). Call once from main.tsx. */
export const bootTheme = (): void => {
  applyDark(resolveDark(readThemeFromStorage()));
};

/** Read-only subscription to the current theme preference. */
export const useThemePreference = (): ThemePreference =>
  useSyncExternalStore(
    subscribeToSettings,
    () => getSettingsSnapshot().theme,
    () => getSettingsSnapshot().theme
  );

/** Mount once at the app root. Applies `.dark` whenever the preference
 * changes, and — while on `system` — re-applies when the OS flips. */
export const useApplyTheme = (): void => {
  const pref = useThemePreference();

  useEffect(() => {
    applyDark(resolveDark(pref));

    // Only track the OS when following it. Explicit light/dark ignore
    // `prefers-color-scheme` entirely.
    if (pref !== 'system' || typeof window === 'undefined' || !window.matchMedia) {
      return;
    }
    const mql = window.matchMedia(DARK_QUERY);
    const onChange = () => applyDark(resolveDark('system'));
    mql.addEventListener('change', onChange);
    return () => mql.removeEventListener('change', onChange);
  }, [pref]);
};

export { getThemePreference, setThemePreference };
