// `init` restores what `state.json` holds, coerced: a label that is not a
// string shows as `?` and a closedAt that is not a number as 0, so a file
// another writer put a bad shape into no longer blanks the page on every
// load. Observed through the hook itself rather than through `init` not
// throwing, which it never did: the throw was React's, at render.
//
// `init` is latched per module load, so this file holds the one case and
// imports the module fresh.

import { render, screen } from '@/__test_utils';

const HEX = '0123456789abcdef'.repeat(2);
const CLOSED = 'f'.repeat(32);

class StubSocket {
  constructor(_url: string) {}
  close(): void {}
  send(): void {}
}

class StubEventSource {
  constructor(_url: string) {}
  addEventListener(): void {}
  close(): void {}
}

describe('init coerces what it restores', () => {
  it('shows ? for an object label and 0 for a non-numeric closedAt', async () => {
    vi.stubGlobal('WebSocket', StubSocket as unknown as typeof WebSocket);
    vi.stubGlobal('EventSource', StubEventSource as unknown as typeof EventSource);
    vi.stubGlobal(
      'fetch',
      vi.fn(async (url: string, init?: RequestInit) => {
        if (init?.method === 'PUT') {
          return new Response(null, { status: 204 });
        }
        if (String(url).startsWith('/state')) {
          return new Response(
            JSON.stringify({
              sessions: [{ id: 't', label: { bad: true }, sessionId: HEX }],
              closed: [{ id: 'c', label: 7, sessionId: CLOSED, closedAt: 'yesterday' }],
              activeId: 't',
              nextLabel: 'many'
            }),
            { status: 200, headers: { 'Content-Type': 'application/json' } }
          );
        }
        return new Response('{"entries":[]}', { status: 200 });
      })
    );

    const { mezameActions, useMezame } = await import('@/hooks/useMezame');
    await mezameActions.init();

    const Probe = () => {
      const { sessions, closed } = useMezame();
      return (
        <pre data-testid="probe">
          {JSON.stringify({ labels: sessions.map((s) => s.label), closed })}
        </pre>
      );
    };
    render(<Probe />);
    const probe = JSON.parse(screen.getByTestId('probe').textContent ?? '{}') as {
      labels: string[];
      closed: unknown[];
    };
    expect(probe.labels).toEqual(['?']);
    expect(probe.closed).toEqual([{ id: 'c', label: '?', sessionId: CLOSED, closedAt: 0 }]);
  });
});
