// Reducer tests for `useMezame`. Drive `applyServerMessage` directly
// with synthetic `ServerMessage` payloads against a freshly-built
// `Session` and assert the resulting log + flags. No React, no real
// WebSocket, no fetch.

import { applyServerMessage } from '@/hooks/useMezame';
import type { LogEntry, ServerMessage, Session } from '@/types';

/** Build a session with the same defaults the production factory uses. */
function makeSession(overrides: Partial<Session> = {}): Session {
  return {
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

  it('clears the existing log when resuming', () => {
    const s = makeSession({
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
