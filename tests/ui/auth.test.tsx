// The login gate and the auth state, end to end through the App shell:
// `/me` on load, the gate by state, the two failure texts, the flip on
// any 401, and one page load serving two accounts in a row with nothing
// carried from the first to the second.

import { render, screen, waitFor } from '@/__test_utils';
import userEvent from '@testing-library/user-event';
import { App } from '@/App';
import { apiFetch, AuthError } from '@/lib/api';
import { getAuthSnapshot, setAnonymous } from '@/lib/auth';
import { mezameActions } from '@/hooks/useMezame';

const PASSWORD = 'correct horse battery';

class FakeSocket {
  static instances: FakeSocket[] = [];
  url: string;
  readyState = 1;
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
}

class FakeEventSource {
  static instances: FakeEventSource[] = [];
  static CLOSED = 2;
  readyState = 0;
  closeCalls = 0;
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
    this.closeCalls += 1;
    this.readyState = 2;
  }
  emit(type: string): void {
    for (const fn of this.listeners.get(type) ?? []) {
      fn();
    }
  }
}

/** Whose cookie the stubbed server currently honours, or null. */
let signedIn: 'alice' | 'bob' | null = null;
/** Answer `/login` with 429 and this wait instead of checking the body. */
let limitedFor: number | null = null;
let requests: Array<{ url: string; method: string }> = [];

const STATE: Record<'alice' | 'bob', unknown> = {
  alice: { sessions: [{ id: 'a1', title: 'Alpha one' }], closed: [], settings: {} },
  bob: { sessions: [{ id: 'b1', title: 'Bob one' }], closed: [], settings: {} }
};

const jsonResponse = (body: unknown, status = 200, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json', ...headers }
  });

const installFetch = () => {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? 'GET';
      requests.push({ url, method });
      if (url === '/me') {
        return signedIn === null
          ? new Response('login required\n', { status: 401 })
          : jsonResponse({ id: `id-${signedIn}`, name: signedIn, role: 'user' });
      }
      if (url === '/login') {
        if (limitedFor !== null) {
          return new Response('too many login attempts\n', {
            status: 429,
            headers: { 'Retry-After': String(limitedFor) }
          });
        }
        const body = JSON.parse(String(init?.body)) as { username: string; password: string };
        if ((body.username === 'alice' || body.username === 'bob') && body.password === PASSWORD) {
          signedIn = body.username as 'alice' | 'bob';
          return jsonResponse({ id: `id-${signedIn}`, name: signedIn, role: 'user' });
        }
        return new Response('wrong username or password\n', { status: 401 });
      }
      if (url === '/logout') {
        signedIn = null;
        return new Response(null, { status: 204 });
      }
      if (signedIn === null) {
        return new Response('login required\n', { status: 401 });
      }
      if (url.startsWith('/state')) {
        return jsonResponse(STATE[signedIn]);
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

const stubBrowser = () => {
  vi.stubGlobal('WebSocket', FakeSocket as unknown as typeof WebSocket);
  vi.stubGlobal('EventSource', FakeEventSource as unknown as typeof EventSource);
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
  Element.prototype.scrollIntoView = () => {};
};

const signIn = async (name: string) => {
  const user = userEvent.setup();
  await user.type(await screen.findByLabelText('Username'), name);
  await user.type(screen.getByLabelText('Password'), PASSWORD);
  await user.click(screen.getByRole('button', { name: 'Sign in' }));
};

beforeEach(() => {
  signedIn = null;
  limitedFor = null;
  requests = [];
  FakeSocket.instances = [];
  FakeEventSource.instances = [];
  mezameActions.reset();
  setAnonymous();
  installFetch();
  stubBrowser();
  try {
    localStorage.clear();
  } catch {
    // fine
  }
});

afterEach(() => {
  mezameActions.reset();
  setAnonymous();
  vi.unstubAllGlobals();
});

describe('the gate', () => {
  it('asks /me on load and renders the login form while nobody is signed in', async () => {
    render(<App />);
    expect(await screen.findByRole('button', { name: 'Sign in' })).toBeInTheDocument();
    expect(requests.filter((r) => r.url === '/me')).toHaveLength(1);
    expect(screen.queryByText('MEZAME')).not.toBeNull(); // the gate's own wordmark
    expect(screen.queryByLabelText('New session')).toBeNull();
  });

  it('renders the app once /me says who holds the cookie', async () => {
    signedIn = 'alice';
    render(<App />);
    expect(await screen.findByText('Alpha one')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Sign in' })).toBeNull();
    expect(getAuthSnapshot()).toEqual({
      status: 'user',
      user: { id: 'id-alice', name: 'alice', role: 'user' }
    });
  });

  it('shows the fixed text on a 401 and the wait on a 429', async () => {
    render(<App />);
    await screen.findByRole('button', { name: 'Sign in' });
    const user = userEvent.setup();
    await user.type(screen.getByLabelText('Username'), 'alice');
    await user.type(screen.getByLabelText('Password'), 'not the password');
    await user.click(screen.getByRole('button', { name: 'Sign in' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Wrong username or password.');

    limitedFor = 42;
    await user.click(screen.getByRole('button', { name: 'Sign in' }));
    await waitFor(() =>
      expect(screen.getByRole('alert')).toHaveTextContent('Too many attempts. Try again in 42 seconds.')
    );
    expect(getAuthSnapshot().status).toBe('anonymous');
  });

  it('a login success enters the app and seeds the list', async () => {
    render(<App />);
    await screen.findByRole('button', { name: 'Sign in' });
    await signIn('alice');
    expect(await screen.findByText('Alpha one')).toBeInTheDocument();
    expect(FakeSocket.instances.some((w) => w.url.endsWith('session=a1'))).toBe(true);
  });
});

describe('losing the session', () => {
  it('any 401 flips the state to anonymous and rejects with AuthError', async () => {
    signedIn = 'alice';
    render(<App />);
    await screen.findByText('Alpha one');
    signedIn = null; // the cookie died server-side
    await expect(apiFetch('/state')).rejects.toBeInstanceOf(AuthError);
    expect(getAuthSnapshot().status).toBe('anonymous');
    expect(await screen.findByRole('button', { name: 'Sign in' })).toBeInTheDocument();
  });

  it('a 4401 on every socket signs out with no reconnect, and a login reconnects every tab', async () => {
    signedIn = 'alice';
    render(<App />);
    await screen.findByText('Alpha one');
    const sockets = FakeSocket.instances.filter((w) => w.url.includes('session='));
    expect(sockets.length).toBeGreaterThan(0);
    const opened = FakeSocket.instances.length;
    for (const w of sockets) {
      w.onclose?.({ code: 4401 });
    }
    expect(await screen.findByRole('button', { name: 'Sign in' })).toBeInTheDocument();
    // No reconnect was scheduled: nothing opened a new socket.
    expect(FakeSocket.instances.length).toBe(opened);
    const streams = FakeEventSource.instances.length;

    signedIn = null;
    await signIn('alice');
    expect(await screen.findByText('Alpha one')).toBeInTheDocument();
    expect(FakeSocket.instances.length).toBeGreaterThan(opened);
    expect(FakeEventSource.instances.length).toBe(streams + 1);
  });

  it('logout as A then login as B shows only B_s list, with A_s sockets closed and the stream reopened once', async () => {
    signedIn = 'alice';
    render(<App />);
    await screen.findByText('Alpha one');
    const aliceSockets = FakeSocket.instances.filter((w) => w.url.includes('session=a1'));
    expect(aliceSockets).toHaveLength(1);
    expect(FakeEventSource.instances).toHaveLength(1);

    const user = userEvent.setup();
    await user.click(screen.getByRole('button', { name: 'Log out' }));
    expect(await screen.findByRole('button', { name: 'Sign in' })).toBeInTheDocument();
    expect(aliceSockets[0].closeCalls).toBeGreaterThan(0);
    expect(FakeEventSource.instances[0].closeCalls).toBeGreaterThan(0);

    await signIn('bob');
    expect(await screen.findByText('Bob one')).toBeInTheDocument();
    expect(screen.queryByText('Alpha one')).toBeNull();
    expect(FakeSocket.instances.some((w) => w.url.endsWith('session=b1'))).toBe(true);
    expect(FakeEventSource.instances).toHaveLength(2);
  });
});
