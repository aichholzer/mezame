// Reducer tests for `useMezame`. Drive `applyServerMessage` directly
// with synthetic `ServerMessage` payloads against a freshly-built
// `Session` and assert the resulting log + flags. No React, no real
// WebSocket, no fetch.

import {
  __testState,
  applyServerMessage,
  mezameActions,
  rehydrateAfterTurn,
  renderHistoryText,
  shouldSuspendIdle
} from '@/hooks/useMezame';
import { setUnauthorizedHandler } from '@/lib/api';
import type { LogEntry, ServerMessage, Session } from '@/types';

/** Build a session with the same defaults the production factory uses. */
function makeSession(overrides: Partial<Session> = {}): Session {
  return {
    id: 's1',
    label: '1',
    sessionId: null,
    effectiveCwd: null,
    promptCapabilities: {},
    log: [],
    hydrated: false,
    status: 'connecting',
    busy: false,
    thinking: false,
    attention: null,
    pinnedToBottom: true,
    models: [],
    currentModelId: null,
    ws: null,
    reconnectAttempt: 0,
    reconnectTimer: null,
    closing: false,
    suspended: false,
    lastActivityAt: Date.now(),
    inFlight: false,
    thoughtOpen: false,
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
      busy: false,
      cwd: '/projects/x',
      promptCapabilities: { image: true }
    };
    applyServerMessage(s, msg);
    expect(s.sessionId).toBe('abc');
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
      resumed: true,
      busy: false
    });
    expect(s.log).toEqual([]);
    expect(s.pinnedToBottom).toBe(true);
    expect(s.hydrated).toBe(true);
  });

  it('preserves the in-memory log on a reconnect (already hydrated)', () => {
    // Regression for the "browser reloads every so often" report: the
    // hub stamps resumed=true on every attach. A transient reconnect
    // must NOT wipe the log and refetch history. Only the first
    // hydrate clears; subsequent resumed readies keep the log.
    const liveLog: LogEntry[] = [
      { kind: 'text', id: 'a', role: 'user', text: '> hi\n', timestamp: 1 },
      { kind: 'text', id: 'b', role: 'agent', text: 'hello', timestamp: 2 }
    ];
    const s = makeSession({ hydrated: true, log: [...liveLog] });
    applyServerMessage(s, {
      type: 'ready',
      sessionId: 'abc',
      resumed: true,
      busy: false
    });
    expect(s.log).toEqual(liveLog);
    expect(s.hydrated).toBe(true);
  });

  it('clears busy / thinking / inFlight when the server reports no turn', () => {
    // The post-idle-drop path: the socket dropped while a turn was in
    // flight, the close handler set busy=true, and the reconnect lands
    // after the turn ended. `busy: false` is what unpins the composer.
    const s = makeSession({
      busy: true,
      thinking: true,
      inFlight: true
    });
    applyServerMessage(s, {
      type: 'ready',
      sessionId: 'abc',
      resumed: true,
      busy: false
    });
    expect(s.busy).toBe(false);
    expect(s.thinking).toBe(false);
    expect(s.inFlight).toBe(false);
  });

  it('locks the composer when the server reports a turn in flight', () => {
    // An attach landing mid-turn shows what an attach that saw the echo
    // shows. The hub guarantees this attach also receives that turn's
    // prompt_done, which is what unlocks it again.
    const s = makeSession();
    applyServerMessage(s, {
      type: 'ready',
      sessionId: 'abc',
      resumed: true,
      busy: true
    });
    expect(s.busy).toBe(true);
    expect(s.thinking).toBe(true);
    expect(s.inFlight).toBe(true);
  });

  it('records the session id on every ready', () => {
    // Mezame mints the id at upgrade time, so a tab holds a resumable
    // one from its first ready. A reconnect reports the same id and the
    // assignment is a no-op.
    const s = makeSession();
    applyServerMessage(s, {
      type: 'ready',
      sessionId: 'minted-id',
      resumed: true,
      busy: false
    });
    expect(s.sessionId).toBe('minted-id');
    applyServerMessage(s, {
      type: 'ready',
      sessionId: 'minted-id',
      resumed: true,
      busy: false
    });
    expect(s.sessionId).toBe('minted-id');
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
    // runs). The guard never trips here.
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
    }
    expect(s.attention).toBe('permission');
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
  it('attaches usage to the last agent entry, not an earlier one', () => {
    const s = makeSession();
    applyServerMessage(s, { type: 'append', role: 'agent', text: 'first' });
    applyServerMessage(s, { type: 'append', role: 'user', text: 'again' });
    applyServerMessage(s, { type: 'append', role: 'agent', text: 'second' });
    const usage = { input: 65, output: 4, cacheRead: 0, cacheWrite: 0 };
    applyServerMessage(s, { type: 'prompt_done', usage });
    const agents = s.log.filter(
      (e): e is Extract<LogEntry, { kind: 'text' }> => e.kind === 'text' && e.role === 'agent'
    );
    expect(agents).toHaveLength(2);
    expect(agents[0].usage).toBeUndefined();
    expect(agents[1].usage).toEqual(usage);
  });

  it('attaches nothing when prompt_done carries no usage', () => {
    const s = makeSession();
    applyServerMessage(s, { type: 'append', role: 'agent', text: 'echo' });
    applyServerMessage(s, { type: 'prompt_done' });
    const agent = s.log.find((e) => e.kind === 'text' && e.role === 'agent');
    expect(agent && agent.kind === 'text' ? agent.usage : 'missing').toBeUndefined();
  });

  it('leaves the previous answer alone when the turn produced no agent text', () => {
    // Turn 1 answers and gets its counts. Turn 2 is reasoning-only or
    // refused: its echo opens the turn, no agent text follows, and its
    // counts must land nowhere, not on turn 1's bubble.
    const s = makeSession();
    applyServerMessage(s, { type: 'append', role: 'user', text: '> one\n' });
    applyServerMessage(s, { type: 'append', role: 'agent', text: 'pong' });
    const first = { input: 10, output: 2, cacheRead: 0, cacheWrite: 0 };
    applyServerMessage(s, { type: 'prompt_done', usage: first });
    applyServerMessage(s, { type: 'append', role: 'user', text: '> two\n' });
    applyServerMessage(s, { type: 'thought', text: 'hmm' });
    applyServerMessage(s, { type: 'prompt_done', usage: { input: 99, output: 9, cacheRead: 9, cacheWrite: 9 } });
    const agents = s.log.filter(
      (e): e is Extract<LogEntry, { kind: 'text' }> => e.kind === 'text' && e.role === 'agent'
    );
    expect(agents).toHaveLength(1);
    expect(agents[0].usage).toEqual(first);
  });

  it('attaches nothing to entries rebuilt from history when the turn was joined mid-flight', () => {
    // A browser attaching while a turn runs sees no echo for it. The
    // `ready` marker fences the rebuilt log off from that turn's counts.
    const s = makeSession({
      log: [
        { kind: 'text', id: 'h1', role: 'user', text: '> old\n', timestamp: 1 },
        { kind: 'text', id: 'h2', role: 'agent', text: 'old answer', timestamp: 2 }
      ],
      hydrated: true
    });
    applyServerMessage(s, { type: 'ready', sessionId: 'abc', resumed: true, busy: true });
    applyServerMessage(s, { type: 'prompt_done', usage: { input: 1, output: 1, cacheRead: 1, cacheWrite: 1 } });
    const old = s.log.find((e) => e.kind === 'text' && e.role === 'agent');
    expect(old && old.kind === 'text' ? old.usage : 'missing').toBeUndefined();
  });

  it('ignores a usage object that is not four finite numbers', () => {
    const s = makeSession();
    applyServerMessage(s, { type: 'append', role: 'agent', text: 'pong' });
    const malformed = { input: '65', output: 4, cacheRead: 0 } as unknown as {
      input: number;
      output: number;
      cacheRead: number;
      cacheWrite: number;
    };
    applyServerMessage(s, { type: 'prompt_done', usage: malformed });
    const agent = s.log[0];
    expect(agent.kind === 'text' ? agent.usage : 'missing').toBeUndefined();
    expect(s.busy).toBe(false);
  });

  it('keeps the usage on a bubble whose turn survived a reconnect', () => {
    // This tab's own turn: echo, a partial bubble, then the socket drops
    // and the `ready` of the reconnect reports the turn still running.
    // The marker this tab already holds stays put, the later chunks merge
    // into the bubble it opened, and the counts land on it.
    const s = makeSession({ hydrated: true, sessionId: 'abc' });
    applyServerMessage(s, { type: 'append', role: 'user', text: '> q\n' });
    applyServerMessage(s, { type: 'append', role: 'agent', text: 'hel' });
    applyServerMessage(s, { type: 'ready', sessionId: 'abc', resumed: true, busy: true });
    expect(s.rehydrateOnTurnEnd).toBe(true);
    applyServerMessage(s, { type: 'append', role: 'agent', text: 'lo' });
    const usage = { input: 5, output: 2, cacheRead: 0, cacheWrite: 0 };
    applyServerMessage(s, { type: 'prompt_done', usage });
    const agents = s.log.filter(
      (e): e is Extract<LogEntry, { kind: 'text' }> => e.kind === 'text' && e.role === 'agent'
    );
    expect(agents).toHaveLength(1);
    expect(agents[0].text).toBe('hello\n');
    expect(agents[0].usage).toEqual(usage);
  });

  it('clears a stale marker when a reconnect finds no turn running', () => {
    const s = makeSession({ hydrated: true, sessionId: 'abc', turnStart: 3 });
    applyServerMessage(s, { type: 'ready', sessionId: 'abc', resumed: true, busy: false });
    expect(s.turnStart).toBeUndefined();
    expect(s.rehydrateOnTurnEnd).toBeFalsy();
  });

  it('skips a trailing sys entry and lands on the agent one', () => {
    const s = makeSession();
    applyServerMessage(s, { type: 'append', role: 'agent', text: 'answer' });
    applyServerMessage(s, { type: 'append', role: 'sys', text: 'note' });
    applyServerMessage(s, { type: 'prompt_done', usage: { input: 1, output: 2, cacheRead: 3, cacheWrite: 4 } });
    const agent = s.log[0];
    expect(agent.kind === 'text' && agent.usage?.cacheWrite).toBe(4);
  });

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
  it('hydrates models', () => {
    const s = makeSession();
    applyServerMessage(s, {
      type: 'session_info',
      info: {
        models: {
          currentModelId: 'claude-sonnet',
          availableModels: [{ modelId: 'claude-sonnet', name: 'Sonnet' }]
        }
      }
    });
    expect(s.models).toHaveLength(1);
    expect(s.currentModelId).toBe('claude-sonnet');
  });

  it('handles an info object with a null models key', () => {
    const s = makeSession({ currentModelId: 'stale' });
    applyServerMessage(s, { type: 'session_info', info: { models: null } });
    expect(s.models).toEqual([]);
    expect(s.currentModelId).toBeNull();
  });
});

// ---------- history render ----------
//
// The formula the echo agreement property models on the Rust side. A
// `user` entry is stored bare and both the prefix and the newline are
// added here, so the string the hub broadcast as the live echo comes back
// out of the transcript byte for byte.

describe('renderHistoryText', () => {
  it('prefixes a user entry once and terminates it once', () => {
    expect(renderHistoryText({ role: 'user', text: 'hello' })).toBe('> hello\n');
    expect(renderHistoryText({ role: 'user', text: '' })).toBe('> \n');
    expect(renderHistoryText({ role: 'user', text: 'a\nb' })).toBe('> a\nb\n');
    expect(renderHistoryText({ role: 'user', text: ' pad ' })).toBe('>  pad \n');
  });

  it('terminates every other role without a prefix', () => {
    expect(renderHistoryText({ role: 'agent', text: 'hello' })).toBe('hello\n');
    expect(renderHistoryText({ role: 'sys', text: 'notice' })).toBe('notice\n');
    expect(renderHistoryText({ role: 'thought', text: 'hmm' })).toBe('hmm\n');
  });
});

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
      sessionId: 'sid-1',
      status: 'connected',
      busy: false,
      inFlight: false,
      suspended: false,
      closing: false,
      lastActivityAt: 10_000_000 - 120_000,
      ...over
    });

  it('suspends an idle, connected background session', () => {
    expect(shouldSuspendIdle(idle(), ctx())).toBe(true);
  });

  it('does not suspend before the threshold elapses', () => {
    expect(shouldSuspendIdle(idle({ lastActivityAt: 10_000_000 - 30_000 }), ctx())).toBe(false);
  });

  it('never suspends a session with a turn in flight', () => {
    expect(shouldSuspendIdle(idle({ inFlight: true }), ctx())).toBe(false);
    expect(shouldSuspendIdle(idle({ busy: true }), ctx())).toBe(false);
  });

  it('never suspends an unresumable session', () => {
    expect(shouldSuspendIdle(idle({ sessionId: null }), ctx())).toBe(false);
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
    applyServerMessage(s, { type: 'ready', sessionId: 'x', resumed: false, busy: false });
    expect(s.lastActivityAt).toBeGreaterThan(1);
  });
});

// ---------- rehydrate after a partly watched turn ----------

describe('rehydrateAfterTurn', () => {
  it('rebuilds the log from history and puts the usage on the answer', async () => {
    const s = makeSession({ sessionId: 'abc', hydrated: true });
    // What a mid-turn attach holds: the tail of the answer alone.
    applyServerMessage(s, { type: 'append', role: 'agent', text: ' half' });
    s.rehydrateOnTurnEnd = true;
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => ({
        ok: true,
        json: async () => ({
          entries: [
            { role: 'user', text: 'q', timestamp: 1 },
            { role: 'thought', text: 'hmm', timestamp: 2 },
            { role: 'agent', text: 'first half', timestamp: 3 }
          ]
        })
      }))
    );
    const usage = { input: 65, output: 4, cacheRead: 0, cacheWrite: 0 };
    await rehydrateAfterTurn(s, usage);
    expect(s.rehydrateOnTurnEnd).toBe(false);
    expect(s.log.map((e) => (e.kind === 'text' ? `${e.role}:${e.text}` : e.kind))).toEqual([
      'user:> q\n',
      'thought',
      'agent:first half\n'
    ]);
    const answer = s.log[2];
    expect(answer.kind === 'text' ? answer.usage : undefined).toEqual(usage);
    vi.unstubAllGlobals();
  });

  it('attaches the usage a history entry carries, so a reload keeps the footer', async () => {
    // Requirement 11 criterion 7: `/history` agent entries carry the
    // counts now; the rebuilt log keeps them with no live turn involved.
    const s = makeSession({ sessionId: 'abc', hydrated: true });
    const usage = { input: 65, output: 4, cacheRead: 1370, cacheWrite: 0 };
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => ({
        ok: true,
        status: 200,
        json: async () => ({
          entries: [
            { role: 'user', text: 'q', timestamp: 1, usage },
            { role: 'agent', text: 'first', timestamp: 2, usage },
            { role: 'agent', text: 'second', timestamp: 3 }
          ]
        })
      }))
    );
    await rehydrateAfterTurn(s);
    const [q, first, second] = s.log;
    expect(q.kind === 'text' ? q.usage : 'set').toBeUndefined();
    expect(first.kind === 'text' ? first.usage : undefined).toEqual(usage);
    expect(second.kind === 'text' ? second.usage : 'set').toBeUndefined();
    vi.unstubAllGlobals();
  });
});

// ---------- the server's list, through the store singletons ----------
//
// These cases drive `mezameActions` against stubbed sockets, event
// stream and fetch: what `init` builds, what a tick's refetch applies,
// what each action sends, and what a failure undoes.

type RouteAnswer = { status: number; body?: unknown } | undefined;

class FakeSocket {
  static instances: FakeSocket[] = [];
  url: string;
  readyState = 1; // OPEN
  closeCalls = 0;
  onopen: (() => void) | null = null;
  onclose: ((e: { code: number }) => void) | null = null;
  onerror: (() => void) | null = null;
  onmessage: ((e: { data: string }) => void) | null = null;
  constructor(url: string) {
    this.url = url;
    FakeSocket.instances.push(this);
  }
  close(): void {
    this.closeCalls += 1;
  }
  send(): void {}
  /** Deliver a `ready` frame as the server would. */
  ready(sessionId: string): void {
    this.onmessage?.({
      data: JSON.stringify({ type: 'ready', sessionId, resumed: false, busy: false })
    });
  }
}

class FakeEventSource {
  static instances: FakeEventSource[] = [];
  static CLOSED = 2;
  readyState = 0;
  private listeners = new Map<string, Array<() => void>>();
  constructor(_url: string) {
    FakeEventSource.instances.push(this);
  }
  addEventListener(type: string, fn: () => void): void {
    const list = this.listeners.get(type) ?? [];
    list.push(fn);
    this.listeners.set(type, list);
  }
  close(): void {
    this.readyState = 2;
  }
  emit(type: string): void {
    for (const fn of this.listeners.get(type) ?? []) {
      fn();
    }
  }
}

/** Every request the stubbed fetch saw. */
let requests: Array<{ url: string; method: string; body?: unknown }> = [];
/** What `/state` answers now. */
let stateDoc: unknown = { sessions: [], closed: [], settings: {} };
/** Per-case overrides, matched on method + prefix. */
let routes: Array<{ method: string; prefix: string; answer: RouteAnswer }> = [];

const jsonResponse = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' }
  });

const installFetch = () => {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? 'GET';
      requests.push({
        url,
        method,
        body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined
      });
      const route = routes.find((r) => r.method === method && url.startsWith(r.prefix));
      if (route?.answer) {
        return jsonResponse(route.answer.body ?? null, route.answer.status);
      }
      if (url.startsWith('/state')) {
        return jsonResponse(stateDoc);
      }
      if (url.startsWith('/history')) {
        return jsonResponse({ entries: [] });
      }
      if (url.startsWith('/sessions/')) {
        return new Response(null, { status: 204 });
      }
      return jsonResponse({});
    })
  );
};

const flush = async () => {
  await new Promise((resolve) => setTimeout(resolve, 0));
  await new Promise((resolve) => setTimeout(resolve, 0));
};

const tick = async () => {
  FakeEventSource.instances.at(-1)?.emit('state_changed');
  await flush();
};

const ids = () => __testState().sessions.map((s) => s.id);
const labels = () => __testState().sessions.map((s) => s.label);
const socketFor = (sessionId: string) =>
  FakeSocket.instances.filter((w) => w.url.endsWith(`session=${sessionId}`));

describe('the session list is the server_s', () => {
  beforeEach(() => {
    mezameActions.reset();
    FakeSocket.instances = [];
    FakeEventSource.instances = [];
    requests = [];
    routes = [];
    stateDoc = { sessions: [], closed: [], settings: {} };
    installFetch();
    vi.stubGlobal('WebSocket', FakeSocket as unknown as typeof WebSocket);
    vi.stubGlobal('EventSource', FakeEventSource as unknown as typeof EventSource);
    try {
      localStorage.clear();
    } catch {
      // fine
    }
  });

  afterEach(() => {
    mezameActions.reset();
    vi.unstubAllGlobals();
  });

  it('init builds the tabs from /state, newest first, and connects each', async () => {
    stateDoc = {
      sessions: [
        { id: 'aaa1', title: 'Alpha' },
        { id: 'bbb2', title: null }
      ],
      closed: [{ id: 'ccc3', title: 'Gone', closedAt: 5 }]
    };
    await mezameActions.init();
    expect(ids()).toEqual(['bbb2', 'aaa1']);
    expect(labels()).toEqual(['New session', 'Alpha']);
    expect(__testState().closed).toEqual([{ id: 'ccc3', label: 'Gone', closedAt: 5 }]);
    expect(socketFor('aaa1')).toHaveLength(1);
    expect(socketFor('bbb2')).toHaveLength(1);
    expect(__testState().activeId).toBe('bbb2');
  });

  it('a tick applies the list whole: a removed session disappears, a new one appears', async () => {
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }, { id: 'bbb2', title: 'Beta' }], closed: [] };
    await mezameActions.init();
    const beta = socketFor('bbb2')[0];
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }, { id: 'ddd4', title: 'Delta' }], closed: [] };
    await tick();
    expect(ids()).toEqual(['ddd4', 'aaa1']);
    expect(beta.closeCalls).toBeGreaterThan(0);
    expect(socketFor('ddd4')).toHaveLength(1);
  });

  it('a refetch during a minted tab_s connect window keeps the pending tab', async () => {
    await mezameActions.init(); // empty list mints one pending tab
    expect(__testState().sessions).toHaveLength(1);
    expect(__testState().sessions[0].sessionId).toBeNull();
    const pendingId = ids()[0];
    await tick(); // the server list is still empty
    expect(ids()).toEqual([pendingId]);
  });

  it('a ready after such a refetch leaves one tab whose id is the server_s', async () => {
    await mezameActions.init();
    mezameActions.newSession('Named');
    const pending = FakeSocket.instances.at(-1);
    expect(pending?.url.endsWith('/ws')).toBe(true);
    // The mint's row and its tick land before the socket's `ready`.
    stateDoc = { sessions: [{ id: 'mint1', title: null }], closed: [] };
    await tick();
    expect(ids()).toContain('mint1');
    const duplicates = __testState().sessions.filter((s) => s.id === 'mint1');
    expect(duplicates).toHaveLength(1);
    const tabs = __testState().sessions.length;

    pending?.ready('mint1');
    await flush();
    const holders = __testState().sessions.filter((s) => s.id === 'mint1');
    expect(holders).toHaveLength(1);
    expect(holders[0].sessionId).toBe('mint1');
    expect(holders[0].label).toBe('Named');
    expect(__testState().sessions.length).toBe(tabs - 1);
    // The named tab titles its row on the first ready...
    const patch = requests.find((r) => r.method === 'PATCH' && r.url === '/sessions/mint1');
    expect(patch?.body).toEqual({ title: 'Named' });
    // ...and the next tick's refetch keeps the name while the title is
    // still null on the server.
    await tick();
    const after = __testState().sessions.find((s) => s.id === 'mint1');
    expect(after?.label).toBe('Named');
  });

  it('each action sends its one request', async () => {
    stateDoc = {
      sessions: [{ id: 'aaa1', title: 'Alpha' }],
      closed: [{ id: 'ccc3', title: 'Gone', closedAt: 5 }]
    };
    await mezameActions.init();
    requests = [];

    mezameActions.renameSession('aaa1', '  Renamed  ');
    await flush();
    expect(requests).toContainEqual({
      url: '/sessions/aaa1',
      method: 'PATCH',
      body: { title: 'Renamed' }
    });

    mezameActions.restoreFromHistory('ccc3');
    await flush();
    expect(requests).toContainEqual({
      url: '/sessions/ccc3',
      method: 'PATCH',
      body: { archived: false }
    });
    expect(socketFor('ccc3')).toHaveLength(1);
    expect(__testState().closed).toEqual([]);

    mezameActions.closeSession('ccc3');
    await flush();
    expect(requests).toContainEqual({
      url: '/sessions/ccc3',
      method: 'PATCH',
      body: { archived: true }
    });
    expect(__testState().closed).toEqual([
      expect.objectContaining({ id: 'ccc3', label: 'Gone' })
    ]);

    mezameActions.forgetHistory('ccc3');
    await flush();
    expect(requests).toContainEqual({ url: '/sessions/ccc3', method: 'DELETE', body: undefined });
    expect(__testState().closed).toEqual([]);
  });

  it('a 400 refetches and the optimistic rename is undone', async () => {
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }], closed: [] };
    await mezameActions.init();
    routes = [{ method: 'PATCH', prefix: '/sessions/aaa1', answer: { status: 400 } }];
    mezameActions.renameSession('aaa1', 'Nope');
    expect(labels()).toEqual(['Nope']);
    await flush();
    expect(labels()).toEqual(['Alpha']);
  });

  it('a 500 refetches and the closed tab is re-added', async () => {
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }], closed: [] };
    await mezameActions.init();
    routes = [{ method: 'PATCH', prefix: '/sessions/aaa1', answer: { status: 500 } }];
    mezameActions.closeSession('aaa1');
    expect(ids()).not.toContain('aaa1');
    await flush();
    expect(ids()).toContain('aaa1');
    expect(__testState().closed).toEqual([]);
  });

  it('a failed restore removes the tab again and opens no socket', async () => {
    stateDoc = {
      sessions: [],
      closed: [{ id: 'ccc3', title: 'Gone', closedAt: 5 }]
    };
    await mezameActions.init();
    routes = [{ method: 'PATCH', prefix: '/sessions/ccc3', answer: { status: 500 } }];
    mezameActions.restoreFromHistory('ccc3');
    expect(ids()).toContain('ccc3');
    await flush();
    expect(ids()).not.toContain('ccc3');
    expect(__testState().closed).toEqual([
      expect.objectContaining({ id: 'ccc3', label: 'Gone' })
    ]);
    expect(socketFor('ccc3')).toHaveLength(0);
  });

  it('a 4401 close reports the loss and schedules no reconnect', async () => {
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }], closed: [] };
    await mezameActions.init();
    const lost = vi.fn();
    setUnauthorizedHandler(lost);
    const before = FakeSocket.instances.length;
    socketFor('aaa1')[0].onclose?.({ code: 4401 });
    await flush();
    expect(lost).toHaveBeenCalledTimes(1);
    expect(FakeSocket.instances.length).toBe(before);
    expect(__testState().sessions[0].reconnectTimer).toBeNull();
    setUnauthorizedHandler(() => {});
  });

  it('a /state answer in flight when the store was reset is not applied afterwards', async () => {
    // The response belongs to the account that signed out (or to a state
    // the next sign-in replaced): it must not repopulate the lists.
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }], closed: [] };
    await mezameActions.init();
    let release: (() => void) | null = null;
    const held = new Promise<void>((resolve) => {
      release = resolve;
    });
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        if (String(input).startsWith('/state')) {
          await held;
          return jsonResponse({ sessions: [{ id: 'old1', title: 'Stale' }], closed: [] });
        }
        return jsonResponse({});
      })
    );
    FakeEventSource.instances.at(-1)?.emit('state_changed'); // refetch goes out
    mezameActions.reset();
    expect(ids()).toEqual([]);
    release!();
    await flush();
    expect(ids(), 'the stale answer was dropped').toEqual([]);
    expect(__testState().closed).toEqual([]);
    expect(FakeSocket.instances.every((w) => !w.url.includes('old1'))).toBe(true);
  });

  it('an init whose fetch was out when the store was reset builds nothing', async () => {
    let release: (() => void) | null = null;
    const held = new Promise<void>((resolve) => {
      release = resolve;
    });
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        if (String(input).startsWith('/state')) {
          await held;
          return jsonResponse({ sessions: [{ id: 'aaa1', title: 'Alpha' }], closed: [] });
        }
        return jsonResponse({});
      })
    );
    const pending = mezameActions.init();
    mezameActions.reset();
    release!();
    await pending;
    await flush();
    expect(ids()).toEqual([]);
    expect(FakeEventSource.instances).toHaveLength(0);
  });

  it('a fatally closed event stream is reopened after a pause, and reset cancels the pause', async () => {
    vi.useFakeTimers();
    try {
      await mezameActions.init();
      expect(FakeEventSource.instances).toHaveLength(1);
      const first = FakeEventSource.instances[0];
      first.readyState = FakeEventSource.CLOSED;
      first.emit('error');
      expect(FakeEventSource.instances, 'not reopened at once').toHaveLength(1);
      await vi.advanceTimersByTimeAsync(5_000);
      expect(FakeEventSource.instances, 'reopened after the pause').toHaveLength(2);

      // A second failure doubles the pause; a reset in the pause cancels it.
      const second = FakeEventSource.instances[1];
      second.readyState = FakeEventSource.CLOSED;
      second.emit('error');
      await vi.advanceTimersByTimeAsync(5_000);
      expect(FakeEventSource.instances, 'the pause doubled').toHaveLength(2);
      mezameActions.reset();
      await vi.advanceTimersByTimeAsync(120_000);
      expect(FakeEventSource.instances, 'nothing reopens after a reset').toHaveLength(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it('a 4404 close removes the session and refetches', async () => {
    stateDoc = {
      sessions: [{ id: 'aaa1', title: 'Alpha' }, { id: 'bbb2', title: 'Beta' }],
      closed: []
    };
    await mezameActions.init();
    requests = [];
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }], closed: [] };
    socketFor('bbb2')[0].onclose?.({ code: 4404 });
    await flush();
    expect(ids()).toEqual(['aaa1']);
    expect(requests.some((r) => r.method === 'GET' && r.url === '/state')).toBe(true);
  });

  it('a_socket_that_drops_before_its_first_ready_is_not_reopened_without_an_id_and_the_list_is_fetched', async () => {
    // A reconnect with no `session` parameter would mint a second row.
    // The tab goes and `/state` is fetched instead; the row the mint
    // created comes back once, as a tab with its id.
    vi.useFakeTimers();
    try {
      await mezameActions.init(); // the empty list mints one pending tab
      const pending = FakeSocket.instances.at(-1)!;
      expect(pending.url.endsWith('/ws')).toBe(true);
      requests = [];
      const opened = FakeSocket.instances.length;
      // The mint landed before the socket dropped: the server holds the row.
      stateDoc = { sessions: [{ id: 'mint1', title: null }], closed: [] };
      pending.onclose?.({ code: 1006 });
      await vi.advanceTimersByTimeAsync(31_000); // past the longest back-off
      const mints = FakeSocket.instances.slice(opened).filter((w) => w.url.endsWith('/ws'));
      expect(mints, 'no second socket without a session parameter').toHaveLength(0);
      expect(requests.some((r) => r.method === 'GET' && r.url === '/state')).toBe(true);
      expect(ids()).toEqual(['mint1']);
      expect(socketFor('mint1')).toHaveLength(1);
      // The dropped tab was the active one; the row that stands for it
      // takes its place, so the composer is not left disabled.
      expect(__testState().activeId, 'the active pointer follows the row').toBe('mint1');
    } finally {
      vi.useRealTimers();
    }
  });

  it('a_saved_active_tab_is_restored_over_the_list_s_first', async () => {
    localStorage.setItem('mezame.activeSession', 'aaa1');
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }, { id: 'bbb2', title: 'Beta' }], closed: [] };
    await mezameActions.init();
    expect(ids()).toEqual(['bbb2', 'aaa1']);
    expect(__testState().activeId).toBe('aaa1');
    expect(localStorage.getItem('mezame.activeSession')).toBe('aaa1');
  });

  it('a_tab_closed_before_its_first_ready_archives_its_row_once_the_id_arrives_and_no_document_puts_it_back', async () => {
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }], closed: [] };
    await mezameActions.init();
    mezameActions.newSession();
    const pending = FakeSocket.instances.at(-1)!;
    expect(pending.url.endsWith('/ws')).toBe(true);
    const placeholder = ids()[0];
    // The archive's answer is held back, so the documents requested
    // while it is out can be told from the one requested after it landed.
    const base = fetch;
    let release: (() => void) | null = null;
    const held = new Promise<void>((resolve) => {
      release = resolve;
    });
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const res = await base(input, init);
        if (init?.method === 'PATCH') {
          await held;
        }
        return res;
      })
    );
    requests = [];
    mezameActions.closeSession(placeholder);
    expect(ids()).toEqual(['aaa1']);
    expect(pending.closeCalls, 'the socket waits for the id').toBe(0);
    expect(requests.filter((r) => r.method === 'PATCH')).toHaveLength(0);
    // The mint's row and its tick land before the socket's ready.
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }, { id: 'mint1', title: null }], closed: [] };
    await tick();
    expect(ids(), 'the closing tab_s row gets no tab of its own').toEqual(['aaa1']);
    pending.ready('mint1');
    await flush();
    expect(requests).toContainEqual({
      url: '/sessions/mint1',
      method: 'PATCH',
      body: { archived: true }
    });
    expect(pending.closeCalls).toBeGreaterThan(0);
    expect(__testState().closed).toEqual([expect.objectContaining({ id: 'mint1' })]);
    // While the archive is out, a document read before the mint lists
    // no such row, and one read before the archive lists it active.
    // Neither gives it a tab: the first is no confirmation, the second
    // is older than the archive.
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }], closed: [] };
    await tick();
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }, { id: 'mint1', title: null }], closed: [] };
    await tick();
    expect(ids(), 'a document requested before the archive landed does not put the row back').toEqual(['aaa1']);
    expect(socketFor('mint1')).toHaveLength(0);
    release!();
    await flush();
    // The archive's own tick confirms it.
    stateDoc = {
      sessions: [{ id: 'aaa1', title: 'Alpha' }],
      closed: [{ id: 'mint1', title: null, closedAt: 9 }]
    };
    await tick();
    expect(ids()).toEqual(['aaa1']);
    expect(__testState().closed).toEqual([{ id: 'mint1', label: 'New session', closedAt: 9 }]);
  });

  it('a_row_archived_here_and_restored_elsewhere_gets_its_tab_back_from_the_first_document_requested_after_the_archive_landed', async () => {
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }, { id: 'bbb2', title: 'Beta' }], closed: [] };
    await mezameActions.init();
    mezameActions.closeSession('bbb2');
    await flush(); // the archive's 204 has landed; its tick never reaches this browser
    expect(ids()).toEqual(['aaa1']);
    expect(__testState().closed).toEqual([expect.objectContaining({ id: 'bbb2' })]);
    // Another device restores the row. The next document requested here
    // lists it active again and was read after the archive committed,
    // so it is believed: the row gets its tab back and a socket.
    await tick();
    expect(ids(), 'the restored row gets its tab back').toEqual(['bbb2', 'aaa1']);
    expect(socketFor('bbb2').filter((w) => w.closeCalls === 0)).toHaveLength(1);
    expect(__testState().closed).toEqual([]);
  });

  it('a_refetch_answered_after_a_newer_one_was_applied_is_dropped', async () => {
    stateDoc = { sessions: [{ id: 'aaa1', title: 'Alpha' }], closed: [] };
    await mezameActions.init();
    // Two refetches go out; the first was read before a rename landed,
    // the second after it. Their answers arrive in the reverse order.
    const docs: unknown[] = [
      { sessions: [{ id: 'aaa1', title: 'Alpha' }], closed: [] },
      { sessions: [{ id: 'aaa1', title: 'Renamed' }], closed: [] }
    ];
    const holds: Array<() => void> = [];
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        if (String(input).startsWith('/state')) {
          const doc = docs.shift();
          await new Promise<void>((resolve) => holds.push(resolve));
          return jsonResponse(doc);
        }
        return jsonResponse({});
      })
    );
    const es = FakeEventSource.instances.at(-1)!;
    es.emit('state_changed');
    es.emit('state_changed');
    expect(holds).toHaveLength(2);
    holds[1]();
    await flush();
    expect(labels()).toEqual(['Renamed']);
    holds[0]();
    await flush();
    expect(labels(), 'the older answer is dropped').toEqual(['Renamed']);
  });

  it('a_restore_whose_tab_a_refetch_removed_while_the_request_was_out_opens_no_socket', async () => {
    stateDoc = {
      sessions: [{ id: 'aaa1', title: 'Alpha' }],
      closed: [{ id: 'ccc3', title: 'Gone', closedAt: 5 }]
    };
    await mezameActions.init();
    const base = fetch;
    let release: (() => void) | null = null;
    const held = new Promise<void>((resolve) => {
      release = resolve;
    });
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        if (init?.method === 'PATCH') {
          await held;
          return new Response(null, { status: 204 });
        }
        return base(input, init);
      })
    );
    mezameActions.restoreFromHistory('ccc3');
    expect(ids()).toEqual(['ccc3', 'aaa1']);
    // A document read before the restore landed still lists the row as
    // closed, and its application removes the optimistic tab.
    await tick();
    expect(ids()).toEqual(['aaa1']);
    requests = [];
    release!();
    await flush();
    expect(socketFor('ccc3'), 'no socket for a tab that is in no list').toHaveLength(0);
    expect(requests.some((r) => r.method === 'GET' && r.url === '/state')).toBe(true);
  });

  it('a_first_visit_tab_that_drops_before_its_first_ready_with_no_row_behind_it_is_minted_again', async () => {
    // The mint never reached the server (it restarted during the
    // handshake, or a proxy refused the upgrade): the refetch lists
    // nothing, and the user would be left with no tab and the composer
    // disabled until `+`. One session is minted in the dropped tab's
    // place, and it is the active one.
    vi.useFakeTimers();
    try {
      await mezameActions.init(); // the empty list mints one pending tab
      const pending = FakeSocket.instances.at(-1)!;
      expect(pending.url.endsWith('/ws')).toBe(true);
      const opened = FakeSocket.instances.length;
      requests = [];
      pending.onclose?.({ code: 1006 });
      await vi.advanceTimersByTimeAsync(31_000); // past the longest back-off
      expect(requests.some((r) => r.method === 'GET' && r.url === '/state')).toBe(true);
      const after = FakeSocket.instances.slice(opened);
      expect(after.map((w) => w.url.endsWith('/ws')), 'one new socket, without a session parameter').toEqual([true]);
      expect(__testState().sessions.map((s) => s.sessionId), 'one pending tab').toEqual([null]);
      expect(__testState().activeId, 'the minted tab is the active one').toBe(ids()[0]);
    } finally {
      vi.useRealTimers();
    }
  });

  it('a_recovery_mint_that_drops_before_its_first_ready_is_not_minted_again', async () => {
    // A proxy that refuses every upgrade: the original mint and the one
    // made in its place both drop. No third socket is opened, and the
    // list is left empty rather than minting without end.
    vi.useFakeTimers();
    try {
      await mezameActions.init();
      FakeSocket.instances.at(-1)!.onclose?.({ code: 1006 });
      await vi.advanceTimersByTimeAsync(31_000);
      const recovery = FakeSocket.instances.at(-1)!;
      expect(FakeSocket.instances).toHaveLength(2);
      expect(recovery.url.endsWith('/ws')).toBe(true);
      requests = [];
      recovery.onclose?.({ code: 1006 });
      await vi.advanceTimersByTimeAsync(31_000);
      expect(requests.some((r) => r.method === 'GET' && r.url === '/state'), 'the list is still fetched').toBe(true);
      expect(FakeSocket.instances, 'no third socket').toHaveLength(2);
      expect(ids()).toEqual([]);
      expect(__testState().activeId).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it('a_ready_answers_the_want_a_pre_ready_drop_left_and_a_later_drop_raises_it_again', async () => {
    // The want lives from a pending tab's drop to the next applied
    // document or `ready`, whichever comes first, and a later drop
    // raises it anew: the bound is per drop, not per page load.
    vi.useFakeTimers();
    try {
      await mezameActions.init();
      FakeSocket.instances.at(-1)!.onclose?.({ code: 1006 });
      await vi.advanceTimersByTimeAsync(31_000);
      const recovery = FakeSocket.instances.at(-1)!;
      expect(FakeSocket.instances).toHaveLength(2);
      // A second tab is opened while the recovery mint is still pending,
      // and its socket drops before its `ready`. The drop's refetch is
      // held so the recovery mint's `ready` lands first.
      mezameActions.newSession();
      const second = FakeSocket.instances.at(-1)!;
      expect(FakeSocket.instances).toHaveLength(3);
      const base = fetch;
      let release: (() => void) | null = null;
      const held = new Promise<void>((resolve) => {
        release = resolve;
      });
      vi.stubGlobal(
        'fetch',
        vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
          if (String(input).startsWith('/state')) {
            await held;
          }
          return base(input, init);
        })
      );
      second.onclose?.({ code: 1006 });
      await vi.advanceTimersByTimeAsync(0);
      expect(ids()).toHaveLength(1);
      recovery.ready('rec1');
      expect(ids()).toEqual(['rec1']);
      // The document lists nothing: the row went elsewhere, and its tab
      // goes with it. The `ready` answered the want, so nothing is minted
      // and the list is empty.
      release!();
      await vi.advanceTimersByTimeAsync(31_000);
      expect(FakeSocket.instances, 'no mint: a ready arrived since the drop').toHaveLength(3);
      expect(ids()).toEqual([]);
      // A third tab's drop is a new want, and it is answered by a mint.
      mezameActions.newSession();
      expect(FakeSocket.instances).toHaveLength(4);
      FakeSocket.instances.at(-1)!.onclose?.({ code: 1006 });
      await vi.advanceTimersByTimeAsync(31_000);
      expect(FakeSocket.instances, 'a later drop mints again').toHaveLength(5);
      expect(FakeSocket.instances.at(-1)!.url.endsWith('/ws')).toBe(true);
      expect(__testState().sessions.map((s) => s.sessionId)).toEqual([null]);
    } finally {
      vi.useRealTimers();
    }
  });
});
