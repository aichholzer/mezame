// Regression test for the "a settings change does not stick" bug.
// `/state` is a shared blob with two independent writers: the settings
// store (`lib/settings.ts`, owns `settings`) and the session sync in
// `useMezame` (owns sessions/closed/activeId/nextLabel).
//
// `doSync` used to PUT only the fields it owns, with no read first.
// Every session event clobbered the `settings` block the settings store
// had just written, because the server stores the body verbatim
// (last-writer-wins), and the user's preference reverted to its default.
// The fix makes `doSync` read-then-merge, mirroring `settings.ts`
// persist(): it must carry across fields it does not own. These tests
// drive `doSync` directly and assert the PUT body still holds a
// pre-existing `settings` object.
//
// The rest of the file covers the persisted-entry predicate: an entry
// that names no session is discarded, which is also what a `state.json`
// written by 0.13.x gets.

import {
  coerceClosedEntry,
  coercePersistedSession,
  doSync,
  hasSessionId,
  mergeSessionsForSync
} from '@/hooks/useMezame';

/** Session ids of the form the server mints, one per fixture name. Only
 * that form passes `hasSessionId`, so every fixture that should be carried
 * or restored needs one. */
const SID: Record<string, string> = {
  a: 'a'.repeat(32),
  b: 'b'.repeat(32),
  peer: '9600fd23'.repeat(4),
  gone: 'd'.repeat(32),
  fresh: 'f'.repeat(32)
};

type Captured = { url: string; body: unknown };

/** Stub fetch: GET /state returns the given server snapshot; PUT /state
 * captures the request body so we can assert what was written. */
function stubFetch(serverState: Record<string, unknown>): Captured[] {
  const calls: Captured[] = [];
  vi.stubGlobal(
    'fetch',
    vi.fn((url: string, init?: RequestInit) => {
      if (init?.method === 'PUT') {
        calls.push({ url, body: JSON.parse(String(init.body)) });
        return Promise.resolve({ ok: true, json: () => Promise.resolve({}) });
      }
      // GET: hand back the current server snapshot.
      return Promise.resolve({ ok: true, json: () => Promise.resolve(serverState) });
    })
  );
  return calls;
}

describe('doSync read-then-merge', () => {
  it('preserves an existing settings block when syncing sessions', async () => {
    const settings = {
      notifications: 'on',
      theme: 'dark',
      sendOnEnter: false
    };
    const calls = stubFetch({
      sessions: [],
      closed: [],
      activeId: null,
      nextLabel: 1,
      settings
    });

    await doSync();

    // A single PUT carrying the merged body.
    const puts = calls.filter((c) => c.url === '/state');
    expect(puts).toHaveLength(1);
    const body = puts[0].body as Record<string, unknown>;
    // The owned fields are present...
    expect(body).toHaveProperty('sessions');
    expect(body).toHaveProperty('nextLabel');
    // ...and the unowned settings block survived the write. The old
    // blind-overwrite behaviour dropped this and re-prompted the user.
    expect(body.settings).toEqual(settings);
  });

  it('falls back to writing only owned fields when the read fails', async () => {
    const calls: Captured[] = [];
    vi.stubGlobal(
      'fetch',
      vi.fn((url: string, init?: RequestInit) => {
        if (init?.method === 'PUT') {
          calls.push({ url, body: JSON.parse(String(init.body)) });
          return Promise.resolve({ ok: true, json: () => Promise.resolve({}) });
        }
        // GET fails: fetchState returns null, doSync writes owned only.
        return Promise.resolve({ ok: false, json: () => Promise.resolve({}) });
      })
    );

    await doSync();

    const puts = calls.filter((c) => c.url === '/state');
    expect(puts).toHaveLength(1);
    const body = puts[0].body as Record<string, unknown>;
    expect(body).toHaveProperty('sessions');
    expect(body).not.toHaveProperty('settings');
  });
});


// Regression for the "live session vanished from state.json on device
// switch" bug. `/state` is last-writer-wins and `doSync` owns the
// `sessions` array. A stale/backgrounded browser used to overwrite the
// shared list with its own partial view and drop a session another
// device still had open (the conversation survived on disk, but the
// pointer was lost and had to be re-added by hand). `mergeSessionsForSync`
// unions in server-only sessions so a stale writer can no longer clobber
// a peer's live session.

const persisted = (id: string, sessionId: string | null = SID[id]) => ({
  id,
  label: id,
  sessionId
});

const closedEntry = (sessionId: string) => ({
  id: `c-${sessionId}`,
  label: 'gone',
  sessionId,
  closedAt: 1
});

describe('mergeSessionsForSync', () => {
  it('carries forward a server session the local list is missing', () => {
    const local = [persisted('a')];
    const server = { sessions: [persisted('a'), persisted('peer', SID.peer)], closed: [] };
    const merged = mergeSessionsForSync(local, [], server);
    expect(merged.map((s) => s.id).sort()).toEqual(['a', 'peer']);
    expect(merged.find((s) => s.id === 'peer')?.sessionId).toBe(SID.peer);
  });

  it('does not duplicate sessions present on both sides', () => {
    const local = [persisted('a'), persisted('b')];
    const server = { sessions: [persisted('a'), persisted('b')], closed: [] };
    expect(mergeSessionsForSync(local, [], server).map((s) => s.id)).toEqual(['a', 'b']);
  });

  it('does not resurrect a session in our own closed history', () => {
    const local = [persisted('a')];
    const server = { sessions: [persisted('a'), persisted('gone')], closed: [] };
    const merged = mergeSessionsForSync(local, [closedEntry(SID.gone)], server);
    expect(merged.map((s) => s.id)).toEqual(['a']);
  });

  it('does not resurrect a session in the server closed history', () => {
    const local = [persisted('a')];
    const server = {
      sessions: [persisted('a'), persisted('gone')],
      closed: [closedEntry(SID.gone)]
    };
    expect(mergeSessionsForSync(local, [], server).map((s) => s.id)).toEqual(['a']);
  });

  it('skips a server session whose sessionId holds null', () => {
    // A tab elsewhere that has not applied its first `ready` yet. There
    // is nothing here to attach to; its next sync brings the minted id.
    const local = [persisted('a')];
    const server = { sessions: [persisted('a'), persisted('fresh', null)], closed: [] };
    expect(mergeSessionsForSync(local, [], server).map((s) => s.id)).toEqual(['a']);
  });

  it('drops a server session whose id the server would refuse', () => {
    // Requirement 16 criterion 20 as amended: a bad id is a missing one.
    const local = [persisted('a')];
    const server = {
      sessions: [persisted('a'), { id: 'bad', label: 'bad', sessionId: 'sid-bad' }],
      closed: []
    };
    expect(mergeSessionsForSync(local, [], server).map((s) => s.id)).toEqual(['a']);
  });

  it('skips a server session entry with no sessionId field at all', () => {
    // What a `state.json` written by 0.13.x looks like here: its ids sit
    // under a key this version does not read.
    const local = [persisted('a')];
    const legacy = { id: 'legacy', label: 'legacy' } as unknown as ReturnType<typeof persisted>;
    const server = { sessions: [persisted('a'), legacy], closed: [] };
    expect(mergeSessionsForSync(local, [], server).map((s) => s.id)).toEqual(['a']);
  });

  it('returns the local list unchanged when the server has no sessions', () => {
    const local = [persisted('a')];
    expect(mergeSessionsForSync(local, [], null)).toBe(local);
    expect(mergeSessionsForSync(local, [], {})).toBe(local);
  });
});

describe('doSync session carry-forward', () => {
  it('writes a server-only session back into the PUT body', async () => {
    // Module-level local sessions are empty in this suite. `peer`
    // reaches the PUT body only through the carry-forward merge.
    const peer = { id: 'peer-1', label: 'Memory Overhaul', sessionId: SID.peer };
    const calls = stubFetch({
      sessions: [peer],
      closed: [],
      activeId: 'peer-1',
      nextLabel: 5,
      settings: { theme: 'dark' }
    });

    await doSync();

    const puts = calls.filter((c) => c.url === '/state');
    expect(puts).toHaveLength(1);
    const body = puts[0].body as { sessions: Array<{ id: string }>; settings: unknown };
    expect(body.sessions.map((s) => s.id)).toContain('peer-1');
    // Existing read-then-merge behaviour still holds.
    expect(body.settings).toEqual({ theme: 'dark' });
  });
});

describe('hasSessionId', () => {
  it('accepts exactly the form the server mints', () => {
    expect(hasSessionId({ sessionId: '0123456789abcdef'.repeat(2) })).toBe(true);
    expect(hasSessionId({ sessionId: SID.peer })).toBe(true);
  });

  it('rejects an absent, null or empty id', () => {
    expect(hasSessionId({})).toBe(false);
    expect(hasSessionId({ sessionId: null })).toBe(false);
    expect(hasSessionId({ sessionId: '' })).toBe(false);
  });

  it('rejects an id the server would refuse with a 400', () => {
    // Requirement 16 criterion 20 as amended. Such a tab used to pulse
    // "reconnecting" forever with no cause shown.
    for (const sessionId of ['sid-1', 'test', 'a'.repeat(31), 'a'.repeat(33), 'A'.repeat(32)]) {
      expect(hasSessionId({ sessionId }), sessionId).toBe(false);
    }
  });

  it('rejects a form the UI never persists', () => {
    // The server trims before it checks and would accept this; nothing
    // here ever writes a padded id, so the client form is the stricter one.
    expect(hasSessionId({ sessionId: ` ${SID.a} ` })).toBe(false);
  });
});

describe('coercePersistedSession', () => {
  it('coerces a label that is not a string to ?', () => {
    expect(coercePersistedSession({ id: 't', label: { x: 1 }, sessionId: SID.a })).toEqual({
      id: 't',
      label: '?',
      sessionId: SID.a
    });
  });

  it('drops an entry with no string id or no accepted session id', () => {
    expect(coercePersistedSession({ label: 'x', sessionId: SID.a })).toBeNull();
    expect(coercePersistedSession({ id: 't', label: 'x', sessionId: 'sid-1' })).toBeNull();
    expect(coercePersistedSession(null)).toBeNull();
    expect(coercePersistedSession('t')).toBeNull();
  });
});

describe('coerceClosedEntry', () => {
  it('coerces a non-numeric closedAt to 0 and does so deterministically', () => {
    const entry = { id: 'c', label: 'gone', sessionId: SID.gone, closedAt: 'yesterday' };
    const first = coerceClosedEntry(entry);
    expect(first).toEqual({ id: 'c', label: 'gone', sessionId: SID.gone, closedAt: 0 });
    expect(coerceClosedEntry(entry)).toEqual(first);
  });

  it('keeps a well-formed entry unchanged and fills a missing id from the session id', () => {
    const entry = { id: 'c', label: 'gone', sessionId: SID.gone, closedAt: 1 };
    expect(coerceClosedEntry(entry)).toEqual(entry);
    expect(coerceClosedEntry({ label: 7, sessionId: SID.gone, closedAt: 1 })).toEqual({
      id: SID.gone,
      label: '?',
      sessionId: SID.gone,
      closedAt: 1
    });
  });

  it('drops an entry whose session id the server would refuse', () => {
    expect(coerceClosedEntry({ id: 'c', label: 'gone', sessionId: 'sid-gone', closedAt: 1 })).toBeNull();
  });
});

// Runs last in the file on purpose: `init` is latched by `initStarted`
// and can be observed once per module load.
describe('init with no restorable persisted entry', () => {
  it('falls back to a fresh session', async () => {
    // jsdom implements WebSocket, so `newSession()` would open a real
    // socket and arm the reconnect loop. The stub is also how the
    // fallback is observed.
    const opened: string[] = [];
    class StubSocket {
      constructor(url: string) {
        opened.push(url);
      }
      close() {}
    }
    vi.stubGlobal('WebSocket', StubSocket as unknown as typeof WebSocket);
    // Every persisted entry fails the predicate: one legacy shape with no
    // id, one carrying null, one carrying an id the server would refuse.
    // None restores a tab, and the closed list is discarded with them.
    stubFetch({
      sessions: [
        { id: 'legacy', label: 'legacy' },
        { id: 'half', label: 'half', sessionId: null },
        { id: 'odd', label: 'odd', sessionId: 'not-hex' }
      ],
      closed: [{ id: 'c1', label: 'gone', closedAt: 1 }],
      activeId: 'legacy',
      nextLabel: 3
    });

    const { mezameActions } = await import('@/hooks/useMezame');
    await mezameActions.init();

    expect(opened).toHaveLength(1);
    expect(opened[0]).toContain('/ws');
    // A fresh session has no id to send back.
    expect(opened[0]).not.toContain('session=');
  });
});
