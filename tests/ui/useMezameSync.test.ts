// Regression test for the "auto-allow permissions setting does not
// stick" bug. `/state` is a shared blob with two independent writers:
// the settings store (`lib/settings.ts`, owns `settings`) and the
// session sync in `useMezame` (owns sessions/closed/activeId/nextLabel).
//
// `doSync` used to PUT only the fields it owns, with no read first.
// Every session event clobbered the `settings` block the settings store
// had just written. The server stores the body verbatim
// (last-writer-wins). `autoAllowPermissions` reverted to its default
// and the user was re-prompted. The fix makes `doSync` read-then-merge,
// mirroring `settings.ts` persist(): it must carry across fields it does
// not own. This test drives `doSync` directly and asserts the PUT body
// still contains a pre-existing `settings` object.

import { doSync, mergeSessionsForSync } from '@/hooks/useMezame';

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
      autoAllowPermissions: true,
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

const persisted = (id: string, acpSessionId: string | null = `acp-${id}`) => ({
  id,
  label: id,
  acpSessionId,
  cwd: null
});

const closedEntry = (acpSessionId: string) => ({
  id: `c-${acpSessionId}`,
  label: 'gone',
  acpSessionId,
  cwd: null,
  closedAt: 1
});

describe('mergeSessionsForSync', () => {
  it('carries forward a server session the local list is missing', () => {
    const local = [persisted('a')];
    const server = { sessions: [persisted('a'), persisted('peer', '9600fd23')], closed: [] };
    const merged = mergeSessionsForSync(local, [], server);
    expect(merged.map((s) => s.id).sort()).toEqual(['a', 'peer']);
    expect(merged.find((s) => s.id === 'peer')?.acpSessionId).toBe('9600fd23');
  });

  it('does not duplicate sessions present on both sides', () => {
    const local = [persisted('a'), persisted('b')];
    const server = { sessions: [persisted('a'), persisted('b')], closed: [] };
    expect(mergeSessionsForSync(local, [], server).map((s) => s.id)).toEqual(['a', 'b']);
  });

  it('does not resurrect a session in our own closed history', () => {
    const local = [persisted('a')];
    const server = { sessions: [persisted('a'), persisted('gone')], closed: [] };
    const merged = mergeSessionsForSync(local, [closedEntry('acp-gone')], server);
    expect(merged.map((s) => s.id)).toEqual(['a']);
  });

  it('does not resurrect a session in the server closed history', () => {
    const local = [persisted('a')];
    const server = {
      sessions: [persisted('a'), persisted('gone')],
      closed: [closedEntry('acp-gone')]
    };
    expect(mergeSessionsForSync(local, [], server).map((s) => s.id)).toEqual(['a']);
  });

  it('skips server sessions with no acp id (unused tab elsewhere)', () => {
    const local = [persisted('a')];
    const server = { sessions: [persisted('a'), persisted('fresh', null)], closed: [] };
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
    const peer = { id: 'peer-1', label: 'Memory Overhaul', acpSessionId: '9600fd23', cwd: null };
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
