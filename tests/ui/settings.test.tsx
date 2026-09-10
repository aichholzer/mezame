// Tests for the Settings pane and the store behind it:
//   - each settings field (its default, get/set, and subscriber
//     notification only on an actual change)
//   - the SettingsDialog (the cog opens the pane; each control reflects
//     and mutates the stored preference)

import { fireEvent, render, screen, userEvent } from '@/__test_utils';
import { SettingsDialog } from '@/features/SettingsDialog';
import {
  __resetSettingsForTests,
  getIdleSuspendMinutes,
  getNotificationPreference,
  getSendOnEnter,
  getThemePreference,
  IDLE_SUSPEND_MAX_MINUTES,
  IDLE_SUSPEND_MIN_MINUTES,
  initSettings,
  readThemeFromStorage,
  setIdleSuspendMinutes,
  setSendOnEnter,
  setThemePreference,
  subscribeToSettings
} from '@/lib/settings';

beforeEach(() => {
  __resetSettingsForTests();
  // The store persists via fetch(PUT /state); stub it so the debounced
  // write in tests is a no-op.
  vi.stubGlobal(
    'fetch',
    vi.fn(() => Promise.resolve({ ok: true, json: () => Promise.resolve({}) }))
  );
});

// ---------- settings store ----------

describe('send-on-Enter settings store', () => {
  it('defaults to on', () => {
    expect(getSendOnEnter()).toBe(true);
  });

  it('persists the choice', () => {
    setSendOnEnter(false);
    expect(getSendOnEnter()).toBe(false);
  });

  it('notifies subscribers only on actual change', () => {
    const listener = vi.fn();
    const unsub = subscribeToSettings(listener);
    setSendOnEnter(false);
    expect(listener).toHaveBeenCalledTimes(1);
    setSendOnEnter(false); // no change
    expect(listener).toHaveBeenCalledTimes(1);
    unsub();
  });
});

// ---------- SettingsDialog ----------

describe('SettingsDialog', () => {
  it('opens the pane from the cog button', async () => {
    const user = userEvent.setup();
    render(<SettingsDialog />);
    expect(screen.queryByText('Send message shortcut')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: /settings/i }));
    expect(await screen.findByText('Send message shortcut')).toBeInTheDocument();
  });

  it('the send-message-shortcut switch reflects and mutates the preference', async () => {
    const user = userEvent.setup();
    render(<SettingsDialog />);
    await user.click(screen.getByRole('button', { name: /settings/i }));
    const sw = await screen.findByRole('switch', { name: /send message shortcut/i });
    // Default is sendOnEnter = true -> switch on.
    expect(sw).toHaveAttribute('aria-checked', 'true');
    await user.click(sw);
    expect(getSendOnEnter()).toBe(false);
    expect(sw).toHaveAttribute('aria-checked', 'false');
  });
});


// ---------- idle-suspend setting ----------

describe('idle-suspend settings store', () => {
  it('defaults to 15 minutes', () => {
    expect(getIdleSuspendMinutes()).toBe(15);
  });

  it('clamps below the minimum', () => {
    setIdleSuspendMinutes(0);
    expect(getIdleSuspendMinutes()).toBe(IDLE_SUSPEND_MIN_MINUTES);
  });

  it('clamps above the maximum', () => {
    setIdleSuspendMinutes(999);
    expect(getIdleSuspendMinutes()).toBe(IDLE_SUSPEND_MAX_MINUTES);
  });

  it('rounds fractional minutes to whole minutes', () => {
    setIdleSuspendMinutes(12.4);
    expect(getIdleSuspendMinutes()).toBe(12);
  });

  it('notifies subscribers only on actual change', () => {
    const listener = vi.fn();
    const unsub = subscribeToSettings(listener);
    setIdleSuspendMinutes(20);
    expect(listener).toHaveBeenCalledTimes(1);
    setIdleSuspendMinutes(20); // no change
    expect(listener).toHaveBeenCalledTimes(1);
    unsub();
  });
});

describe('SettingsDialog idle-suspend control', () => {
  it('reflects the stored threshold and mutates it via the slider', async () => {
    const user = userEvent.setup();
    render(<SettingsDialog />);
    await user.click(screen.getByRole('button', { name: /settings/i }));
    const slider = await screen.findByRole('slider', {
      name: /suspend idle sessions after \(minutes\)/i
    });
    expect((slider as HTMLInputElement).value).toBe('15');
    fireEvent.change(slider, { target: { value: '30' } });
    expect(getIdleSuspendMinutes()).toBe(30);
  });

  it('exposes the configured min and max bounds', async () => {
    const user = userEvent.setup();
    render(<SettingsDialog />);
    await user.click(screen.getByRole('button', { name: /settings/i }));
    const slider = (await screen.findByRole('slider', {
      name: /suspend idle sessions after \(minutes\)/i
    })) as HTMLInputElement;
    expect(slider.min).toBe(String(IDLE_SUSPEND_MIN_MINUTES));
    expect(slider.max).toBe(String(IDLE_SUSPEND_MAX_MINUTES));
  });
});

// ---------- persistence shape ----------

describe('settings persistence', () => {
  it('PUTs /state with the settings object alone', async () => {
    vi.useFakeTimers();
    setSendOnEnter(false);
    setIdleSuspendMinutes(30);
    await vi.advanceTimersByTimeAsync(300);
    vi.useRealTimers();
    const fetchMock = fetch as unknown as ReturnType<typeof vi.fn>;
    const put = fetchMock.mock.calls.find(
      ([, init]) => (init as RequestInit | undefined)?.method === 'PUT'
    );
    expect(put).toBeDefined();
    const [url, init] = put as [string, RequestInit];
    expect(url).toBe('/state');
    const body = JSON.parse(String(init.body)) as Record<string, unknown>;
    expect(Object.keys(body)).toEqual(['settings']);
    expect(body.settings).toMatchObject({ sendOnEnter: false, idleSuspendMinutes: 30 });
  });
});

// ---------- the read half: /state's settings land in the store ----------

describe('settings init', () => {
  it('every_field_of_the_state_answer_lands_in_the_store', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(() =>
        Promise.resolve({
          ok: true,
          json: () =>
            Promise.resolve({
              sessions: [],
              closed: [],
              settings: { theme: 'dark', sendOnEnter: false, idleSuspendMinutes: 30, notifications: 'on' }
            })
        })
      )
    );
    await initSettings();
    expect(getThemePreference()).toBe('dark');
    expect(readThemeFromStorage(), 'the theme mirror follows').toBe('dark');
    expect(getSendOnEnter()).toBe(false);
    expect(getIdleSuspendMinutes()).toBe(30);
    expect(getNotificationPreference()).toBe('on');
  });

  it('a_theme_set_while_init_is_in_flight_stays_the_user_s_and_the_write_carries_it', async () => {
    vi.useFakeTimers();
    try {
      let release: (() => void) | null = null;
      const held = new Promise<void>((resolve) => {
        release = resolve;
      });
      const fetchMock = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
        if (init?.method === 'PUT') {
          return { ok: true, json: () => Promise.resolve({}) };
        }
        await held;
        return {
          ok: true,
          json: () => Promise.resolve({ settings: { theme: 'light', sendOnEnter: false } })
        };
      });
      vi.stubGlobal('fetch', fetchMock);
      const pending = initSettings();
      setThemePreference('dark');
      await vi.advanceTimersByTimeAsync(300); // the debounced write fires
      release!();
      await pending;
      expect(getThemePreference(), 'the user_s choice stands').toBe('dark');
      expect(readThemeFromStorage()).toBe('dark');
      expect(getSendOnEnter(), 'the fields the user left alone are the server_s').toBe(false);
      const put = fetchMock.mock.calls.find(([, init]) => init?.method === 'PUT');
      expect(put).toBeDefined();
      const body = JSON.parse(String((put![1] as RequestInit).body)) as { settings: { theme: string } };
      expect(body.settings.theme).toBe('dark');
    } finally {
      vi.useRealTimers();
    }
  });
});
