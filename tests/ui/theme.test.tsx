// Tests for the night-mode / theme layer:
//   - the settings store (theme get/set/subscribe + localStorage mirror)
//   - resolveDark (system -> matchMedia, explicit overrides)
//   - useApplyTheme (applies `.dark`, reacts to live OS changes on system)
//   - the ThemeToggle dropdown (selection persists, check on active)

import { act, render, screen, userEvent } from '@/__test_utils';
import { ThemeToggle } from '@/features/ThemeToggle';
import {
  resolveDark,
  useApplyTheme
} from '@/hooks/useTheme';
import {
  __resetSettingsForTests,
  getThemePreference,
  readThemeFromStorage,
  setThemePreference,
  subscribeToSettings
} from '@/lib/settings';

// matchMedia is not implemented in jsdom. Provide a controllable mock:
// `setOsDark` flips the resolved value and fires registered listeners,
// emulating an OS day/night schedule change.
let osDark = false;
const mediaListeners = new Set<() => void>();

const setOsDark = (dark: boolean) => {
  osDark = dark;
  for (const l of mediaListeners) {
    l();
  }
};

beforeEach(() => {
  __resetSettingsForTests();
  document.documentElement.classList.remove('dark');
  osDark = false;
  mediaListeners.clear();

  Object.defineProperty(window, 'matchMedia', {
    configurable: true,
    writable: true,
    value: (query: string) => ({
      matches: query.includes('dark') ? osDark : false,
      media: query,
      addEventListener: (_: string, cb: () => void) => mediaListeners.add(cb),
      removeEventListener: (_: string, cb: () => void) => mediaListeners.delete(cb),
      // Legacy API some code paths still call; unused here.
      addListener: (cb: () => void) => mediaListeners.add(cb),
      removeListener: (cb: () => void) => mediaListeners.delete(cb),
      dispatchEvent: () => false,
      onchange: null
    })
  });
});

// ---------- settings store ----------

describe('theme settings store', () => {
  it('defaults to system', () => {
    expect(getThemePreference()).toBe('system');
  });

  it('persists the choice to localStorage', () => {
    setThemePreference('dark');
    expect(getThemePreference()).toBe('dark');
    expect(readThemeFromStorage()).toBe('dark');
  });

  it('notifies subscribers only on actual change', () => {
    const listener = vi.fn();
    const unsub = subscribeToSettings(listener);
    setThemePreference('light');
    expect(listener).toHaveBeenCalledTimes(1);
    setThemePreference('light'); // no change
    expect(listener).toHaveBeenCalledTimes(1);
    unsub();
  });
});

// ---------- resolveDark ----------

describe('resolveDark', () => {
  it('honours explicit light/dark regardless of OS', () => {
    setOsDark(true);
    expect(resolveDark('light')).toBe(false);
    expect(resolveDark('dark')).toBe(true);
  });

  it('follows the OS when system', () => {
    setOsDark(false);
    expect(resolveDark('system')).toBe(false);
    setOsDark(true);
    expect(resolveDark('system')).toBe(true);
  });
});

// ---------- useApplyTheme ----------

const Host = () => {
  useApplyTheme();
  return null;
};

const hasDark = () => document.documentElement.classList.contains('dark');

describe('useApplyTheme', () => {
  it('applies dark when the explicit preference is dark', () => {
    setThemePreference('dark');
    render(<Host />);
    expect(hasDark()).toBe(true);
  });

  it('does not apply dark for explicit light even if the OS is dark', () => {
    setOsDark(true);
    setThemePreference('light');
    render(<Host />);
    expect(hasDark()).toBe(false);
  });

  it('reacts to a live OS change while on system', () => {
    // Default preference is system; OS starts light.
    render(<Host />);
    expect(hasDark()).toBe(false);
    act(() => setOsDark(true));
    expect(hasDark()).toBe(true);
    act(() => setOsDark(false));
    expect(hasDark()).toBe(false);
  });

  it('ignores OS changes once an explicit preference is set', () => {
    setThemePreference('light');
    render(<Host />);
    act(() => setOsDark(true));
    expect(hasDark()).toBe(false);
  });
});

// ---------- ThemeToggle ----------

describe('ThemeToggle', () => {
  it('selecting Dark persists the preference', async () => {
    const user = userEvent.setup();
    render(<ThemeToggle />);
    await user.click(screen.getByRole('button', { name: /theme: system/i }));
    await user.click(await screen.findByText('Dark'));
    expect(getThemePreference()).toBe('dark');
  });

  it('the trigger label reflects the active preference', () => {
    setThemePreference('light');
    render(<ThemeToggle />);
    expect(
      screen.getByRole('button', { name: /theme: light/i })
    ).toBeInTheDocument();
  });
});
