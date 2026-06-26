// Reducer tests for `useMezame`. Drive `applyServerMessage` directly
// with synthetic `ServerMessage` payloads against a freshly-built
// `Session` and assert the resulting log + flags. No React, no real
// WebSocket, no fetch.

import {
  applyServerMessage,
  deriveLabel,
  shouldCloseAbsentSession,
  shouldSuspendIdle
} from '@/hooks/useMezame';
import type { LogEntry, ServerMessage, Session } from '@/types';

/** Build a session with the same defaults the production factory uses. */
function makeSession(overrides: Partial<Session> = {}): Session {
  return {
    id: 's1',
    label: '1',
    acpSessionId: null,
    liveSessionId: null,
    cwd: null,
    effectiveCwd: null,
    promptCapabilities: {},
    used: false,
    log: [],
    hydrated: false,
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
    suspended: false,
    lastActivityAt: Date.now(),
    inFlight: false,
    ...overrides
  };
}

function lastEntry(s: Session): LogEntry | undefined {
  return s.log.at(-1);
}

// ---------- ready ----------

describe('applyServerMessage / ready', () => {
  it('sets sessionId, cwd, prompt capabilities, and connected status', () => {
    const s = makeSession();
    const msg: ServerMessage = {
      type: 'ready',
      sessionId: 'abc',
      resumed: false,
      cwd: '/projects/x',
      promptCapabilities: { image: true }
    };
    applyServerMessage(s, msg);
    expect(s.acpSessionId).toBe('abc');
    expect(s.effectiveCwd).toBe('/projects/x');
    expect(s.promptCapabilities).toEqual({ image: true });
    expect(s.status).toBe('connected');
  });

  it('clears the existing log when resuming on first hydrate', () => {
    const s = makeSession({
      hydrated: false,
      log: [
        {
          kind: 'text',
          id: 'old',
          role: 'agent',
          text: 'stale',
          timestamp: 1
        }
      ]
    });
    applyServerMessage(s, {
      type: 'ready',
      sessionId: 'abc',
      resumed: true
    });
    expect(s.log).toEqual([]);
    expect(s.pinnedToBottom).toBe(true);
    expect(s.hydrated).toBe(true);
  });

  it('preserves the in-memory log on a reconnect (already hydrated)', () => {
    // Regression for the "browser reloads every so often" report: the
    // hub stamps resumed=true on every attach, so a transient
    // reconnect must NOT wipe the log and refetch history. Only the
    // first hydrate clears; subsequent resumed readies keep the log.
    const liveLog: LogEntry[] = [
      { kind: 'text', id: 'a', role: 'user', text: '> hi\n', timestamp: 1 },
      { kind: 'text', id: 'b', role: 'agent', text: 'hello', timestamp: 2 }
    ];
    const s = makeSession({ hydrated: true, log: [...liveLog] });
    applyServerMessage(s, {
      type: 'ready',
      sessionId: 'abc',
      resumed: true
    });
    expect(s.log).toEqual(liveLog);
    expect(s.hydrated).toBe(true);
  });

  it('clears busy / thinking / inFlight on resume so the composer unsticks', () => {
    // Simulates the post-idle-drop path: the socket dropped while a
    // turn was in flight, the close handler set busy=true, and now
    // the reconnect succeeds. The historical `prompt_done` is not
    // replayed, so the reducer has to clear the flags itself.
    const s = makeSession({
      busy: true,
      thinking: true,
      inFlight: true
    });
    applyServerMessage(s, {
      type: 'ready',
      sessionId: 'abc',
      resumed: true
    });
    expect(s.busy).toBe(false);
    expect(s.thinking).toBe(false);
    expect(s.inFlight).toBe(false);
  });

  it('does not touch busy / thinking on a fresh (non-resume) ready', () => {
    const s = makeSession({
      busy: false,
      thinking: false,
      inFlight: false
    });
    applyServerMessage(s, {
      type: 'ready',
      sessionId: 'abc',
      resumed: false
    });
    expect(s.busy).toBe(false);
    expect(s.thinking).toBe(false);
    expect(s.inFlight).toBe(false);
  });

  it('advances the durable acpSessionId on a clean resume', () => {
    const s = makeSession({ acpSessionId: 'real-id' });
    applyServerMessage(s, {
      type: 'ready',
      sessionId: 'real-id',
      resumed: true
    });
    expect(s.acpSessionId).toBe('real-id');
    expect(s.liveSessionId).toBe('real-id');
  });

  it('keeps acpSessionId pinned when a resume failed and fell back', () => {
    // Regression for the vanishing/overwritten-session bug: a failed
    // resume must NOT overwrite the durable pointer with the throwaway
    // fallback id. The live id tracks the fallback for this connection;
    // acpSessionId (persisted + used for the next ?session=) stays on
    // the original so a later restart can retry the real conversation.
    const s = makeSession({ acpSessionId: 'real-id', used: true });
    applyServerMessage(s, {
      type: 'ready',
      sessionId: 'fallback-empty-id',
      resumed: false,
      resumeFailedFor: 'real-id'
    });
    expect(s.acpSessionId).toBe('real-id');
    expect(s.liveSessionId).toBe('fallback-empty-id');
  });
});

// ---------- append ----------

describe('applyServerMessage / append', () => {
  it('adds an agent text entry', () => {
    const s = makeSession();
    applyServerMessage(s, { type: 'append', role: 'agent', text: 'hello' });
    expect(s.log).toHaveLength(1);
    const entry = lastEntry(s);
    expect(entry?.kind).toBe('text');
    if (entry?.kind === 'text') {
      expect(entry.role).toBe('agent');
      expect(entry.text).toBe('hello');
    }
  });

  it('merges consecutive same-role text chunks', () => {
    const s = makeSession();
    applyServerMessage(s, { type: 'append', role: 'agent', text: 'hello ' });
    applyServerMessage(s, { type: 'append', role: 'agent', text: 'world' });
    expect(s.log).toHaveLength(1);
    const entry = lastEntry(s);
    if (entry?.kind === 'text') {
      expect(entry.text).toBe('hello world');
    }
  });

  it('does not merge across different roles', () => {
    const s = makeSession();
    applyServerMessage(s, { type: 'append', role: 'agent', text: 'reply' });
    applyServerMessage(s, { type: 'append', role: 'sys', text: '\n[note]\n' });
    expect(s.log).toHaveLength(2);
  });
});

// ---------- permission_request ----------

describe('applyServerMessage / permission_request', () => {
  it('appends a permission entry and raises attention', () => {
    const s = makeSession();
    // Active session check: with no document.visibilityState match,
    // `raiseAttention` will set the level. The session is not active
    // in the store (activeId is null at module level until activate
    // runs), so the guard never trips here.
    applyServerMessage(s, {
      type: 'permission_request',
      id: 7,
      title: 'Run shell command',
      options: [
        { optionId: 'allow', name: 'Allow' },
        { optionId: 'reject', name: 'Reject' }
      ]
    });
    expect(s.log).toHaveLength(1);
    const entry = lastEntry(s);
    expect(entry?.kind).toBe('permission');
    if (entry?.kind === 'permission') {
      expect(entry.requestId).toBe(7);
      expect(entry.title).toBe('Run shell command');
      expect(entry.options).toHaveLength(2);
      expect(entry.resolution).toBeUndefined();
      expect(entry.auto).toBeFalsy();
    }
    expect(s.attention).toBe('permission');
  });

  it('auto-resolves a permission_request when a remembered policy matches', () => {
    const remembered = { optionId: 'allow_once', name: 'Allow once' };
    const s = makeSession({
      rememberedPermissions: {
        'Run shell command': remembered
      }
    });
    applyServerMessage(s, {
      type: 'permission_request',
      id: 11,
      title: 'Run shell command',
      options: [
        { optionId: 'allow_once', name: 'Allow once' },
        { optionId: 'reject', name: 'Reject' }
      ]
    });
    const entry = lastEntry(s);
    expect(entry?.kind).toBe('permission');
    if (entry?.kind === 'permission') {
      expect(entry.resolution).toBe('Allow once');
      expect(entry.auto).toBe(true);
    }
    // Auto-resolved requests do not raise attention; the user is
    // not waiting on a click.
    expect(s.attention).toBeNull();
  });

  it('does not auto-resolve when only the title differs', () => {
    const s = makeSession({
      rememberedPermissions: {
        'Run shell command': { optionId: 'allow_once' }
      }
    });
    applyServerMessage(s, {
      type: 'permission_request',
      id: 12,
      title: 'Read file',
      options: [{ optionId: 'allow', name: 'Allow' }]
    });
    const entry = lastEntry(s);
    if (entry?.kind === 'permission') {
      expect(entry.resolution).toBeUndefined();
      expect(entry.auto).toBeFalsy();
    }
    expect(s.attention).toBe('permission');
  });
});

// ---------- mcp_oauth_request ----------

describe('applyServerMessage / mcp_oauth_request', () => {
  it('appends an mcp_oauth entry and raises attention', () => {
    const s = makeSession();
    applyServerMessage(s, {
      type: 'mcp_oauth_request',
      id: 'r1',
      serverName: 'github',
      url: 'https://example.com/auth'
    });
    expect(s.log).toHaveLength(1);
    const entry = lastEntry(s);
    expect(entry?.kind).toBe('mcp_oauth');
    if (entry?.kind === 'mcp_oauth') {
      expect(entry.serverName).toBe('github');
      expect(entry.url).toBe('https://example.com/auth');
      expect(entry.opened).toBe(false);
    }
    expect(s.attention).toBe('permission');
  });

  it('dedupes by request id', () => {
    const s = makeSession();
    const msg: ServerMessage = {
      type: 'mcp_oauth_request',
      id: 'same-id',
      serverName: 'github',
      url: 'https://example.com/auth'
    };
    applyServerMessage(s, msg);
    applyServerMessage(s, msg);
    expect(s.log).toHaveLength(1);
  });

  it('falls back to serverName + url when id is null', () => {
    const s = makeSession();
    applyServerMessage(s, {
      type: 'mcp_oauth_request',
      id: null,
      serverName: 'github',
      url: 'https://example.com/auth'
    });
    applyServerMessage(s, {
      type: 'mcp_oauth_request',
      id: null,
      serverName: 'github',
      url: 'https://example.com/auth'
    });
    expect(s.log).toHaveLength(1);
  });
});

// ---------- tool_call ----------

describe('applyServerMessage / tool_call', () => {
  it('pushes a new entry on first emission', () => {
    const s = makeSession();
    applyServerMessage(s, {
      type: 'tool_call',
      toolCallId: 'tc-1',
      title: 'Read file',
      status: 'in_progress',
      kind: 'file_read',
      rawInput: { path: '/x' }
    });
    expect(s.log).toHaveLength(1);
    const entry = lastEntry(s);
    if (entry?.kind === 'tool_call') {
      expect(entry.toolCallId).toBe('tc-1');
      expect(entry.title).toBe('Read file');
      expect(entry.status).toBe('in_progress');
      expect(entry.toolKind).toBe('file_read');
    }
  });

  it('mutates the existing entry in place on update by toolCallId', () => {
    const s = makeSession();
    applyServerMessage(s, {
      type: 'tool_call',
      toolCallId: 'tc-1',
      title: 'Read file',
      status: 'in_progress'
    });
    applyServerMessage(s, {
      type: 'tool_call',
      toolCallId: 'tc-1',
      status: 'completed',
      content: [{ kind: 'text', data: 'ok' }]
    });
    expect(s.log).toHaveLength(1);
    const entry = lastEntry(s);
    if (entry?.kind === 'tool_call') {
      expect(entry.status).toBe('completed');
      expect(entry.title).toBe('Read file'); // preserved
      expect(entry.content).toEqual([{ kind: 'text', data: 'ok' }]);
    }
  });
});

// ---------- prompt_done ----------

describe('applyServerMessage / prompt_done', () => {
  it('clears thinking, clears busy, clears inFlight, raises attention to done', () => {
    const s = makeSession({ thinking: true, busy: true, inFlight: true });
    applyServerMessage(s, { type: 'prompt_done' });
    expect(s.thinking).toBe(false);
    expect(s.busy).toBe(false);
    expect(s.inFlight).toBe(false);
    expect(s.attention).toBe('done');
  });
});

// ---------- error ----------

describe('applyServerMessage / error', () => {
  it('appends a sys error line and raises error attention', () => {
    const s = makeSession({ thinking: true, busy: true, inFlight: true });
    applyServerMessage(s, { type: 'error', message: 'boom' });
    const entry = lastEntry(s);
    if (entry?.kind === 'text') {
      expect(entry.role).toBe('sys');
      expect(entry.text).toContain('boom');
    }
    expect(s.thinking).toBe(false);
    expect(s.busy).toBe(false);
    expect(s.inFlight).toBe(false);
    expect(s.attention).toBe('error');
  });
});

// ---------- session_info ----------

describe('applyServerMessage / session_info', () => {
  it('hydrates modes and models', () => {
    const s = makeSession();
    applyServerMessage(s, {
      type: 'session_info',
      info: {
        modes: {
          currentModeId: 'kiro_default',
          availableModes: [{ id: 'kiro_default', name: 'Default' }]
        },
        models: {
          currentModelId: 'claude-sonnet',
          availableModels: [{ modelId: 'claude-sonnet', name: 'Sonnet' }]
        }
      }
    });
    expect(s.modes).toHaveLength(1);
    expect(s.currentModeId).toBe('kiro_default');
    expect(s.models).toHaveLength(1);
    expect(s.currentModelId).toBe('claude-sonnet');
  });

  it('handles partial info (only modes)', () => {
    const s = makeSession();
    applyServerMessage(s, {
      type: 'session_info',
      info: { modes: { currentModeId: 'x', availableModes: [] }, models: null }
    });
    expect(s.currentModeId).toBe('x');
    expect(s.currentModelId).toBeNull();
  });
});

// ---------- commands ----------

describe('applyServerMessage / commands', () => {
  it('stores commands and prompts (last-wins on second emission)', () => {
    const s = makeSession();
    applyServerMessage(s, {
      type: 'commands',
      commands: [{ name: '/clear' }],
      prompts: [{ name: 'summarise' }]
    });
    expect(s.commands).toHaveLength(1);
    expect(s.prompts).toHaveLength(1);

    // Re-emission with fresh data: replace, do not merge.
    applyServerMessage(s, {
      type: 'commands',
      commands: [{ name: '/clear' }, { name: '/help' }],
      prompts: []
    });
    expect(s.commands).toHaveLength(2);
    expect(s.prompts).toHaveLength(0);
  });
});

// ---------- reconcile: vanishing-session guard ----------
//
// Regression for the bug where a live session silently disappeared
// from state.json. Reconcile must only close a local session that is
// absent from the server's `sessions` snapshot when the server's
// `closed` history corroborates a deliberate close. An unverified
// omission (another browser clobbered the list with a partial view)
// must NOT close the session.

describe('shouldCloseAbsentSession', () => {
  const closedIds = (...ids: string[]) => new Set(ids);

  it('closes a used session whose acp id is in the server closed history', () => {
    const s = { used: true, acpSessionId: 'acp-1' };
    expect(shouldCloseAbsentSession(s, closedIds('acp-1'))).toBe(true);
  });

  it('keeps a used session absent from the snapshot but NOT in closed history', () => {
    // The clobber case: another browser PUT a partial list that
    // omitted this session. No closed-history corroboration, so we
    // must keep it rather than erase it.
    const s = { used: true, acpSessionId: 'acp-1' };
    expect(shouldCloseAbsentSession(s, closedIds('acp-other'))).toBe(false);
    expect(shouldCloseAbsentSession(s, closedIds())).toBe(false);
  });

  it('keeps a used session that has no acp id yet', () => {
    const s = { used: true, acpSessionId: null };
    expect(shouldCloseAbsentSession(s, closedIds('acp-1'))).toBe(false);
  });

  it('keeps an unused (never-prompted) session regardless of closed history', () => {
    const s = { used: false, acpSessionId: 'acp-1' };
    expect(shouldCloseAbsentSession(s, closedIds('acp-1'))).toBe(false);
  });
});

// ---------- deriveLabel ----------
//
// Pure heuristic that turns a session's first prompt into a short tab
// label. No network, no model. Returns null when the prompt is not a
// useful label so the caller keeps the numeric placeholder.

describe('deriveLabel', () => {
  it('returns null for empty or whitespace-only text', () => {
    expect(deriveLabel('')).toBeNull();
    expect(deriveLabel('   ')).toBeNull();
  });

  it('returns null for slash commands', () => {
    expect(deriveLabel('/clear')).toBeNull();
  });

  it('returns null when the first sentence is shorter than two chars', () => {
    expect(deriveLabel('a')).toBeNull();
    expect(deriveLabel('...')).toBeNull();
  });

  it('uses a short prompt verbatim', () => {
    expect(deriveLabel('Fix the login bug')).toBe('Fix the login bug');
    expect(deriveLabel('Hi')).toBe('Hi');
  });

  it('collapses runs of whitespace', () => {
    expect(deriveLabel('   Add   dark   mode   toggle   ')).toBe('Add dark mode toggle');
  });

  it('keeps only the first sentence', () => {
    expect(deriveLabel('Refactor the parser. Then add tests.')).toBe('Refactor the parser');
  });

  it('caps the label at ten words', () => {
    expect(
      deriveLabel('one two three four five six seven eight nine ten eleven twelve')
    ).toBe('one two three four five six seven eight nine ten');
  });

  it('strips fenced code blocks before deriving', () => {
    expect(deriveLabel('Look at ```const x = 1``` please')).toBe('Look at please');
  });

  it('strips URLs before deriving', () => {
    expect(deriveLabel('Check https://example.com/foo now')).toBe('Check now');
  });
});


// ---------- idle suspend ----------

describe('shouldSuspendIdle', () => {
  const ctx = (over: Partial<{ isActive: boolean; visible: boolean; now: number; thresholdMs: number }> = {}) => ({
    isActive: false,
    visible: true,
    now: 10_000_000,
    thresholdMs: 60_000,
    ...over
  });
  // A background session idle for 2 minutes against a 1-minute threshold.
  const idle = (over: Partial<Session> = {}) =>
    makeSession({
      used: true,
      acpSessionId: 'acp-1',
      status: 'connected',
      busy: false,
      inFlight: false,
      suspended: false,
      closing: false,
      lastActivityAt: 10_000_000 - 120_000,
      ...over
    });

  it('suspends an idle, used, connected background session', () => {
    expect(shouldSuspendIdle(idle(), ctx())).toBe(true);
  });

  it('does not suspend before the threshold elapses', () => {
    expect(shouldSuspendIdle(idle({ lastActivityAt: 10_000_000 - 30_000 }), ctx())).toBe(false);
  });

  it('never suspends a session with a turn in flight', () => {
    expect(shouldSuspendIdle(idle({ inFlight: true }), ctx())).toBe(false);
    expect(shouldSuspendIdle(idle({ busy: true }), ctx())).toBe(false);
  });

  it('never suspends an unused or unresumable session', () => {
    expect(shouldSuspendIdle(idle({ used: false }), ctx())).toBe(false);
    expect(shouldSuspendIdle(idle({ acpSessionId: null }), ctx())).toBe(false);
  });

  it('never suspends a session that is not on a healthy socket', () => {
    expect(shouldSuspendIdle(idle({ status: 'reconnecting' }), ctx())).toBe(false);
  });

  it('never re-suspends an already-suspended or closing session', () => {
    expect(shouldSuspendIdle(idle({ suspended: true }), ctx())).toBe(false);
    expect(shouldSuspendIdle(idle({ closing: true }), ctx())).toBe(false);
  });

  it('exempts the active tab while the browser tab is visible', () => {
    expect(shouldSuspendIdle(idle(), ctx({ isActive: true, visible: true }))).toBe(false);
  });

  it('suspends the active tab when the browser tab is hidden', () => {
    expect(shouldSuspendIdle(idle(), ctx({ isActive: true, visible: false }))).toBe(true);
  });
});

describe('applyServerMessage idle anchor', () => {
  it('stamps lastActivityAt on prompt_done', () => {
    const s = makeSession({ lastActivityAt: 1 });
    applyServerMessage(s, { type: 'prompt_done' });
    expect(s.lastActivityAt).toBeGreaterThan(1);
  });

  it('stamps lastActivityAt on ready', () => {
    const s = makeSession({ lastActivityAt: 1 });
    applyServerMessage(s, { type: 'ready', sessionId: 'x', resumed: false });
    expect(s.lastActivityAt).toBeGreaterThan(1);
  });
});
