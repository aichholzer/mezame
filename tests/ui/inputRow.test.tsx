// Tests for the composer's submit keybinding, which the Settings pane's
// "Send message shortcut" toggle flips between two modes:
//   - sendOnEnter (default): bare Enter submits, Shift+Enter newlines.
//   - modifier mode: Cmd/Ctrl+Enter submits, bare Enter newlines.
//
// We drive the textarea's keydown directly and assert whether `onSubmit`
// fired, rather than reaching through the whole hub.

import { fireEvent, render, screen, waitFor } from '@/__test_utils';
import { InputRow } from '@/features/InputRow';
import { __resetSettingsForTests, setSendOnEnter } from '@/lib/settings';
import type { Session } from '@/types';

// jsdom has no matchMedia; InputRow pulls it in via useIsMobile. A
// permissive stub (no listeners needed for these synchronous tests).
beforeEach(() => {
  __resetSettingsForTests();
  Object.defineProperty(window, 'matchMedia', {
    configurable: true,
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false
    })
  });
});

/** Minimal ready (non-busy) session so the composer is enabled. */
function makeSession(overrides: Partial<Session> = {}): Session {
  return {
    id: 's1',
    label: '1',
    acpSessionId: 'acp-1',
    liveSessionId: 'acp-1',
    cwd: null,
    effectiveCwd: null,
    promptCapabilities: {},
    used: false,
    log: [],
    hydrated: true,
    status: 'connected',
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
    inFlight: false,
    thoughtOpen: false,
    ...overrides
  };
}

const typeInto = (text: string) => {
  const ta = screen.getByRole('textbox');
  fireEvent.change(ta, { target: { value: text } });
  return ta;
};

describe('InputRow submit keybinding', () => {
  describe('send-on-Enter (default)', () => {
    it('plain Enter submits', async () => {
      const onSubmit = vi.fn();
      render(<InputRow session={makeSession()} onSubmit={onSubmit} />);
      const ta = typeInto('hello');
      fireEvent.keyDown(ta, { key: 'Enter' });
      await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
      expect(onSubmit).toHaveBeenCalledWith('hello', []);
    });

    it('Shift+Enter does not submit (newline)', () => {
      const onSubmit = vi.fn();
      render(<InputRow session={makeSession()} onSubmit={onSubmit} />);
      const ta = typeInto('hello');
      fireEvent.keyDown(ta, { key: 'Enter', shiftKey: true });
      expect(onSubmit).not.toHaveBeenCalled();
    });

    it('Cmd/Ctrl+Enter does not submit', () => {
      const onSubmit = vi.fn();
      render(<InputRow session={makeSession()} onSubmit={onSubmit} />);
      const ta = typeInto('hello');
      fireEvent.keyDown(ta, { key: 'Enter', metaKey: true });
      fireEvent.keyDown(ta, { key: 'Enter', ctrlKey: true });
      expect(onSubmit).not.toHaveBeenCalled();
    });
  });

  describe('modifier mode (sendOnEnter = false)', () => {
    it('plain Enter does not submit (newline)', () => {
      setSendOnEnter(false);
      const onSubmit = vi.fn();
      render(<InputRow session={makeSession()} onSubmit={onSubmit} />);
      const ta = typeInto('hello');
      fireEvent.keyDown(ta, { key: 'Enter' });
      expect(onSubmit).not.toHaveBeenCalled();
    });

    it('Cmd+Enter submits', async () => {
      setSendOnEnter(false);
      const onSubmit = vi.fn();
      render(<InputRow session={makeSession()} onSubmit={onSubmit} />);
      const ta = typeInto('hello');
      fireEvent.keyDown(ta, { key: 'Enter', metaKey: true });
      await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
      expect(onSubmit).toHaveBeenCalledWith('hello', []);
    });

    it('Ctrl+Enter submits', async () => {
      setSendOnEnter(false);
      const onSubmit = vi.fn();
      render(<InputRow session={makeSession()} onSubmit={onSubmit} />);
      const ta = typeInto('hello');
      fireEvent.keyDown(ta, { key: 'Enter', ctrlKey: true });
      await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    });

    it('Shift+Cmd+Enter does not submit', () => {
      setSendOnEnter(false);
      const onSubmit = vi.fn();
      render(<InputRow session={makeSession()} onSubmit={onSubmit} />);
      const ta = typeInto('hello');
      fireEvent.keyDown(ta, { key: 'Enter', metaKey: true, shiftKey: true });
      expect(onSubmit).not.toHaveBeenCalled();
    });
  });
});
