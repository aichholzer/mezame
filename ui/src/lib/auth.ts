// Who is signed in, behind `useSyncExternalStore`.
//
// The state starts `unknown` (the shell renders blank), `checkMe` asks
// `/me` on load, and every later 401, 4401 close or logout lands in
// `setAnonymous`, which runs the reset of both stores before it flips:
// sockets closed, timers cleared, lists emptied, settings back to their
// defaults, and both `init` guards re-armed, so the next sign-in starts
// from nothing and runs them again.

import { useSyncExternalStore } from 'react';
import { apiFetch, AuthError, setUnauthorizedHandler } from '@/lib/api';
import { mezameActions } from '@/hooks/useMezame';
import { resetSettings } from '@/lib/settings';

/** What `/me` and a successful `/login` answer. */
export type Me = { id: string; name: string; role: string };

export type AuthStatus = 'unknown' | 'anonymous' | 'user';

export type AuthState = { status: AuthStatus; user: Me | null };

let state: AuthState = { status: 'unknown', user: null };

const listeners = new Set<() => void>();

const notify = () => {
  for (const l of listeners) {
    l();
  }
};

export const subscribeAuth = (l: () => void): (() => void) => {
  listeners.add(l);
  return () => listeners.delete(l);
};

export const getAuthSnapshot = (): AuthState => state;

export const useAuth = (): AuthState =>
  useSyncExternalStore(subscribeAuth, getAuthSnapshot, getAuthSnapshot);

/** Leave the signed-in state: reset both stores, then flip. Idempotent;
 * the resets run even when the state is already `anonymous`, so a 401
 * racing a logout leaves nothing behind either way. */
export const setAnonymous = (): void => {
  mezameActions.reset();
  resetSettings();
  if (state.status !== 'anonymous' || state.user !== null) {
    state = { status: 'anonymous', user: null };
    notify();
  }
};

setUnauthorizedHandler(setAnonymous);

const setUser = (user: Me): void => {
  state = { status: 'user', user };
  notify();
};

/** Ask `/me` who holds the cookie. Run once on load; the answer moves
 * the state off `unknown` either way. */
export const checkMe = async (): Promise<void> => {
  try {
    const res = await apiFetch('/me');
    if (res.ok) {
      setUser((await res.json()) as Me);
      return;
    }
  } catch (e) {
    if (e instanceof AuthError) {
      return; // the handler flipped the state already
    }
  }
  // An unreadable answer or an unreachable server: show the gate rather
  // than a blank shell forever; a successful login proves the rest.
  setAnonymous();
};

export type LoginResult =
  | { kind: 'ok' }
  | { kind: 'invalid' }
  | { kind: 'limited'; seconds: number };

/** `POST /login`. `ok` flips the state to `user`, on which the app's
 * effect runs `init` and `initSettings` again. */
export const login = async (username: string, password: string): Promise<LoginResult> => {
  let res: Response;
  try {
    res = await apiFetch('/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username, password })
    });
  } catch (e) {
    if (e instanceof AuthError) {
      return { kind: 'invalid' };
    }
    throw e;
  }
  if (res.status === 429) {
    const seconds = Number(res.headers.get('retry-after') ?? '60');
    return { kind: 'limited', seconds: Number.isFinite(seconds) ? seconds : 60 };
  }
  if (!res.ok) {
    return { kind: 'invalid' };
  }
  setUser((await res.json()) as Me);
  return { kind: 'ok' };
};

/** `POST /logout`: this device's cookie is cleared, the active-tab
 * memory goes with it, and the state flips through the same reset a 401
 * takes. */
export const logout = async (): Promise<void> => {
  try {
    await apiFetch('/logout', { method: 'POST' });
  } catch {
    // The cookie may already be dead; sign out locally regardless.
  }
  try {
    localStorage.removeItem('mezame.activeSession');
  } catch {
    // Storage unavailable: nothing to clear.
  }
  setAnonymous();
};
