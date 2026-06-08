// Tests for the Settings pane and the auto-allow-permissions setting:
//   - the settings store field (default off, get/set, subscriber
//     notification only on actual change)
//   - the SettingsDialog (cog opens the pane; the switch reflects and
//     mutates the stored preference)

import { render, screen, userEvent } from '@/__test_utils';
import { SettingsDialog } from '@/features/SettingsDialog';
import {
  __resetSettingsForTests,
  getAutoAllowPermissions,
  getSendOnEnter,
  setAutoAllowPermissions,
  setSendOnEnter,
  subscribeToSettings
} from '@/lib/settings';

beforeEach(() => {
  __resetSettingsForTests();
  // The store persists via fetch(PUT /state); stub it so the debounced
  // write in tests is a no-op rather than a real network call.
  vi.stubGlobal(
    'fetch',
    vi.fn(() => Promise.resolve({ ok: true, json: () => Promise.resolve({}) }))
  );
});

// ---------- settings store ----------

describe('auto-allow settings store', () => {
  it('defaults to off', () => {
    expect(getAutoAllowPermissions()).toBe(false);
  });

  it('persists the choice', () => {
    setAutoAllowPermissions(true);
    expect(getAutoAllowPermissions()).toBe(true);
  });

  it('notifies subscribers only on actual change', () => {
    const listener = vi.fn();
    const unsub = subscribeToSettings(listener);
    setAutoAllowPermissions(true);
    expect(listener).toHaveBeenCalledTimes(1);
    setAutoAllowPermissions(true); // no change
    expect(listener).toHaveBeenCalledTimes(1);
    unsub();
  });
});

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
    expect(screen.queryByText('Auto-allow all permissions')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: /settings/i }));
    expect(await screen.findByText('Auto-allow all permissions')).toBeInTheDocument();
  });

  it('the switch reflects the stored preference', async () => {
    const user = userEvent.setup();
    setAutoAllowPermissions(true);
    render(<SettingsDialog />);
    await user.click(screen.getByRole('button', { name: /settings/i }));
    const sw = await screen.findByRole('switch', { name: /auto-allow all permissions/i });
    expect(sw).toHaveAttribute('aria-checked', 'true');
  });

  it('toggling the switch updates the preference', async () => {
    const user = userEvent.setup();
    render(<SettingsDialog />);
    await user.click(screen.getByRole('button', { name: /settings/i }));
    const sw = await screen.findByRole('switch', { name: /auto-allow all permissions/i });
    expect(sw).toHaveAttribute('aria-checked', 'false');
    await user.click(sw);
    expect(getAutoAllowPermissions()).toBe(true);
    expect(sw).toHaveAttribute('aria-checked', 'true');
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
