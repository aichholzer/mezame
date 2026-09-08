// The usage footer under the last agent bubble (requirement 11.3, 11.4):
// present once `prompt_done` attached counts, absent without them, and
// hidden while the turn still streams.

import { render, screen } from '@/__test_utils';
import { LogPane } from '@/features/LogPane';
import type { LogEntry, Session, Usage } from '@/types';

// jsdom has no matchMedia; the pane reaches it through its hooks.
beforeEach(() => {
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

const USAGE: Usage = { input: 65, output: 4, cacheRead: 12_345, cacheWrite: 0 };

function agentEntry(text: string, usage?: Usage): LogEntry {
  return { kind: 'text', id: `e-${text}`, role: 'agent', text, timestamp: 1, usage };
}

function makeSession(overrides: Partial<Session> = {}): Session {
  return {
    id: 's1',
    label: '1',
    sessionId: 'sid-1',
    effectiveCwd: null,
    promptCapabilities: {},
    log: [],
    hydrated: true,
    status: 'connected',
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

describe('LogPane usage footer', () => {
  it('shows the short form under the answer and the exact counts in the title', () => {
    render(<LogPane session={makeSession({ log: [agentEntry('pong', USAGE)] })} isActive />);
    const footer = screen.getByTestId('usage-footer');
    expect(footer).toHaveTextContent('65 in · 4 out · 12.3k cached');
    expect(footer).toHaveAttribute('title', '65 input · 4 output · 12,345 cache read · 0 cache write');
  });

  it('shows nothing when the entry carries no usage', () => {
    render(<LogPane session={makeSession({ log: [agentEntry('pong')] })} isActive />);
    expect(screen.queryByTestId('usage-footer')).toBeNull();
    expect(screen.queryByText(/cached/)).toBeNull();
  });

  it('keeps a finished answer\'s counts while the next turn streams, and hides only the trailing bubble\'s meta', () => {
    // Two answers: the first finished with counts, the second in flux.
    // The counts stay put (only `prompt_done` sets them, so they are
    // never in flux), and the copy control hides on the trailing bubble
    // alone.
    render(
      <LogPane
        session={makeSession({
          thinking: true,
          log: [agentEntry('pong', USAGE), agentEntry('pon')]
        })}
        isActive
      />
    );
    expect(screen.getAllByTestId('usage-footer')).toHaveLength(1);
    // The first bubble keeps both copy controls (rail and mobile); the
    // trailing one has none yet.
    expect(screen.getAllByTitle('Copy message')).toHaveLength(2);
  });

  it('keeps the previous answer\'s copy controls while the new turn thinks', () => {
    // Between Enter and the first agent chunk the trailing agent bubble is
    // the finished previous answer. The turn marker sits past it, so the
    // gate leaves it alone: both copy controls stay.
    const log: LogEntry[] = [
      agentEntry('done', USAGE),
      { kind: 'text', id: 'u2', role: 'user', text: '> next\n', timestamp: 2 }
    ];
    render(<LogPane session={makeSession({ thinking: true, turnStart: 2, log })} isActive />);
    expect(screen.getAllByTitle('Copy message')).toHaveLength(2);
    expect(screen.getByTestId('usage-footer')).toBeInTheDocument();
  });

  it('shows no meta row at all for a trailing bubble in flux', () => {
    render(
      <LogPane session={makeSession({ thinking: true, log: [agentEntry('pon')] })} isActive />
    );
    expect(screen.queryByTestId('usage-footer')).toBeNull();
    expect(screen.queryByTitle('Copy message')).toBeNull();
  });
});
