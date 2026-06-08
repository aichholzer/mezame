// Regression test for the "auto-allow permissions setting does not
// stick" bug. `/state` is a shared blob with two independent writers:
// the settings store (`lib/settings.ts`, owns `settings`) and the
// session sync in `useMezame` (owns sessions/closed/activeId/nextLabel).
//
// `doSync` used to PUT only the fields it owns, with no read first, so
// every session event clobbered the `settings` block the settings store
// had just written. The server stores the body verbatim
// (last-writer-wins), so `autoAllowPermissions` reverted to its default
// and the user was re-prompted. The fix makes `doSync` read-then-merge,
// mirroring `settings.ts` persist(): it must carry across fields it does
// not own. This test drives `doSync` directly and asserts the PUT body
// still contains a pre-existing `settings` object.

import { doSync } from '@/hooks/useMezame';

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
    // blind-overwrite behaviour dropped this, re-prompting the user.
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
