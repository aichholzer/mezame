// Tests for the browser-notifications layer:
//   - the settings store (preference get/set/subscribe)
//   - the `useNotifications` hook (transition detection + first-use
//     prompt activation + actual fire when on)
//   - the `NotificationsPrompt` banner (only shown when pending)

import { act, render, screen, userEvent } from '@/__test_utils';
import { useEffect } from 'react';
import { NotificationsPrompt } from '@/features/NotificationsPrompt';
import { useNotifications } from '@/hooks/useNotifications';
import {
  __resetSettingsForTests,
  getNotificationPreference,
  setNotificationPreference,
  subscribeToSettings
} from '@/lib/settings';
import type { Session } from '@/types';

// -- Mock the session store. The notifications hook only needs the
// shape `{ sessions, activeId }`, not the full module.
let mockSessions: Session[] = [];
let mockActiveId: string | null = null;

vi.mock('@/hooks/useMezame', () => ({
  useMezame: () => ({
    sessions: mockSessions,
    activeId: mockActiveId,
    closed: [],
    activeSession: null
  })
}));

// -- Mock Notification globally. Jsdom doesn't provide it; we count
// calls and assert payloads.
const NotificationMock = vi.fn().mockImplementation(function (
  this: { onclick: (() => void) | null; close: () => void },
  _title: string,
  _options?: NotificationOptions
) {
  this.onclick = null;
  this.close = vi.fn();
});

beforeEach(() => {
  __resetSettingsForTests();
  mockSessions = [];
  mockActiveId = null;
  NotificationMock.mockClear();

  // Re-install per test so vi.spyOn doesn't accumulate.
  Object.defineProperty(window, 'Notification', {
    configurable: true,
    writable: true,
    value: Object.assign(NotificationMock, {
      permission: 'granted' as NotificationPermission,
      requestPermission: vi
        .fn<() => Promise<NotificationPermission>>()
        .mockResolvedValue('granted')
    })
  });
  Object.defineProperty(window, 'isSecureContext', {
    configurable: true,
    value: true
  });
});

const baseSession = (overrides: Partial<Session> = {}): Session => ({
  id: 's1',
  label: '1',
  acpSessionId: null,
  cwd: null,
  effectiveCwd: null,
  promptCapabilities: {},
  used: false,
  log: [],
  status: 'connecting',
  busy: false,
  thinking: false,
  attention: null,
  pinnedToBottom: true,
  modes: [],
  currentModeId: null,
  models: [],
  currentModelId: null,
  commands: [],
  prompts: [],
  rememberedPermissions: {},
  ws: null,
  reconnectAttempt: 0,
  reconnectTimer: null,
  closing: false,
  ...overrides
});

/** Tiny test host that mounts the hook so its useEffect runs. */
const HookHost = () => {
  useNotifications();
  return null;
};

const Reset = () => {
  // Used to force the host to re-run with a new mockSessions value.
  useEffect(() => {}, []);
  return null;
};

// ---------- settings ----------

describe('settings store', () => {
  it('starts at unset', () => {
    expect(getNotificationPreference()).toBe('unset');
  });

  it('notifies subscribers on change', () => {
    const listener = vi.fn();
    const unsub = subscribeToSettings(listener);
    setNotificationPreference('on');
    expect(listener).toHaveBeenCalledTimes(1);
    setNotificationPreference('on'); // No change: no notify.
    expect(listener).toHaveBeenCalledTimes(1);
    unsub();
    setNotificationPreference('off');
    expect(listener).toHaveBeenCalledTimes(1);
  });
});

// ---------- useNotifications transition detection ----------

describe('useNotifications', () => {
  it('does not fire when preference is off', () => {
    setNotificationPreference('off');
    mockSessions = [baseSession({ id: 'a', attention: 'done' })];
    mockActiveId = 'b'; // background relative to active
    render(<HookHost />);
    expect(NotificationMock).not.toHaveBeenCalled();
  });

  it('fires for a background session when preference is on', () => {
    setNotificationPreference('on');
    mockSessions = [
      baseSession({ id: 'a', label: 'one', attention: 'done' })
    ];
    mockActiveId = 'b';
    render(<HookHost />);
    expect(NotificationMock).toHaveBeenCalledTimes(1);
    const args = NotificationMock.mock.calls[0];
    expect(args[0]).toContain('Turn complete');
    const opts = args[1] as NotificationOptions;
    expect(opts.body).toContain('one');
    expect(opts.tag).toContain('one');
  });

  it('does not fire for the active session when the tab is visible', () => {
    setNotificationPreference('on');
    Object.defineProperty(document, 'visibilityState', {
      configurable: true,
      value: 'visible'
    });
    mockSessions = [
      baseSession({ id: 'active', attention: 'done' })
    ];
    mockActiveId = 'active';
    render(<HookHost />);
    expect(NotificationMock).not.toHaveBeenCalled();
  });

  it('fires for the active session when the tab is hidden', () => {
    setNotificationPreference('on');
    Object.defineProperty(document, 'visibilityState', {
      configurable: true,
      value: 'hidden'
    });
    mockSessions = [
      baseSession({ id: 'active', label: 'A', attention: 'permission' })
    ];
    mockActiveId = 'active';
    render(<HookHost />);
    expect(NotificationMock).toHaveBeenCalledTimes(1);
    expect(NotificationMock.mock.calls[0][0]).toContain('Permission');
  });

  it('promotes preference from unset to pending on first event', () => {
    expect(getNotificationPreference()).toBe('unset');
    mockSessions = [baseSession({ id: 'a', attention: 'done' })];
    mockActiveId = 'b';
    render(<HookHost />);
    expect(getNotificationPreference()).toBe('pending');
    expect(NotificationMock).not.toHaveBeenCalled();
  });
});

// ---------- NotificationsPrompt ----------

describe('NotificationsPrompt', () => {
  it('renders nothing when preference is not pending', () => {
    setNotificationPreference('off');
    const { container } = render(<NotificationsPrompt />);
    expect(container).toBeEmptyDOMElement();
  });

  it('renders a banner when preference is pending', () => {
    setNotificationPreference('pending');
    render(<NotificationsPrompt />);
    expect(
      screen.getByText(/notification.*background session/i)
    ).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /enable/i })).toBeInTheDocument();
  });

  it('Enable calls requestPermission and persists on grant', async () => {
    const user = userEvent.setup();
    setNotificationPreference('pending');
    render(<NotificationsPrompt />);

    await user.click(screen.getByRole('button', { name: /enable/i }));
    // Wait a tick for the async permission resolve.
    await act(async () => {
      await Promise.resolve();
    });
    expect(getNotificationPreference()).toBe('on');
  });

  it('Dismiss flips preference to off', async () => {
    const user = userEvent.setup();
    setNotificationPreference('pending');
    render(<NotificationsPrompt />);
    await user.click(screen.getByRole('button', { name: /dismiss/i }));
    expect(getNotificationPreference()).toBe('off');
  });
});
