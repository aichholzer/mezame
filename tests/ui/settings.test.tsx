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
  getSendOnEnter,
  IDLE_SUSPEND_MAX_MINUTES,
  IDLE_SUSPEND_MIN_MINUTES,
  setIdleSuspendMinutes,
  setSendOnEnter,
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
