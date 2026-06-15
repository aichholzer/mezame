// Tests for the per-session agent picker: the `fetchAgents` lib that
// reads `GET /agents`, and the NewSessionDialog that surfaces the
// picker only when more than one agent is configured and threads the
// choice into `onCreate`.

import { fireEvent, render, screen, waitFor } from '@/__test_utils';
import { fetchAgents } from '@/lib/agents';
import { NewSessionDialog } from '@/features/NewSessionDialog';

/** Stub fetch so GET /agents returns the given payload. */
function stubAgents(payload: unknown, ok = true) {
  vi.stubGlobal(
    'fetch',
    vi.fn((url: string) => {
      if (url === '/agents') {
        return Promise.resolve({ ok, json: () => Promise.resolve(payload) });
      }
      return Promise.resolve({ ok: false, json: () => Promise.resolve({}) });
    })
  );
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('fetchAgents', () => {
  it('parses the names and default from the server', async () => {
    stubAgents({ agents: ['kiro', 'claude'], default: 'kiro' });
    const info = await fetchAgents();
    expect(info.agents).toEqual(['kiro', 'claude']);
    expect(info.default).toBe('kiro');
  });

  it('degrades to an empty list when the request fails', async () => {
    stubAgents({}, false);
    const info = await fetchAgents();
    expect(info.agents).toEqual([]);
    expect(info.default).toBeNull();
  });

  it('ignores malformed payloads', async () => {
    stubAgents({ agents: 'not-an-array', default: 42 });
    const info = await fetchAgents();
    expect(info.agents).toEqual([]);
    expect(info.default).toBeNull();
  });
});

describe('NewSessionDialog agent picker', () => {
  it('hides the picker when only one agent is configured', async () => {
    stubAgents({ agents: ['kiro'], default: 'kiro' });
    render(<NewSessionDialog open onOpenChange={() => {}} onCreate={() => {}} />);
    // Give the open-effect's fetch a tick to resolve.
    await waitFor(() => expect(fetch).toHaveBeenCalledWith('/agents'));
    expect(screen.queryByLabelText('Agent')).toBeNull();
  });

  it('shows the picker seeded with the default and passes the choice to onCreate', async () => {
    stubAgents({ agents: ['kiro', 'claude'], default: 'kiro' });
    const onCreate = vi.fn();
    render(<NewSessionDialog open onOpenChange={() => {}} onCreate={onCreate} />);

    const select = (await screen.findByLabelText('Agent')) as HTMLSelectElement;
    expect(select.value).toBe('kiro');

    // Switch to the second agent and submit.
    fireEvent.change(select, { target: { value: 'claude' } });
    fireEvent.click(screen.getByRole('button', { name: 'Create' }));

    expect(onCreate).toHaveBeenCalledWith(null, null, 'claude');
  });

  it('passes a null agent when only one is configured', async () => {
    stubAgents({ agents: ['kiro'], default: 'kiro' });
    const onCreate = vi.fn();
    render(<NewSessionDialog open onOpenChange={() => {}} onCreate={onCreate} />);
    await waitFor(() => expect(fetch).toHaveBeenCalledWith('/agents'));

    fireEvent.click(screen.getByRole('button', { name: 'Create' }));
    expect(onCreate).toHaveBeenCalledWith(null, null, null);
  });
});
