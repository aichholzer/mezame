// The one fetch every request of the client goes through.
//
// `apiFetch` sends the session cookie (`credentials: 'same-origin'`; the
// browser stamps `Sec-Fetch-Site` itself) and turns a 401 into two
// things: the registered handler runs, which is how the auth store
// learns the cookie is gone from any request at all, and the caller's
// promise rejects with `AuthError` so no handler ever reads a refusal
// as data. A socket close with code 4401 reports through `authLost`,
// the same path without a Response.

/** A request the server answered 401: the caller is signed out. */
export class AuthError extends Error {
  constructor() {
    super('login required');
    this.name = 'AuthError';
  }
}

type UnauthorizedHandler = () => void;

let onUnauthorized: UnauthorizedHandler | null = null;

/** Register what a 401 (or a 4401 close) does. The auth store sets this
 * to its own sign-out; registration rather than an import keeps the
 * store modules free of a cycle. */
export const setUnauthorizedHandler = (handler: UnauthorizedHandler): void => {
  onUnauthorized = handler;
};

/** Report an authentication loss seen outside a fetch: a socket closed
 * with code 4401. */
export const authLost = (): void => {
  onUnauthorized?.();
};

/** `fetch` with the session cookie, rejecting with [`AuthError`] on a
 * 401 after the handler has run. Every other status is the caller's to
 * read. */
export const apiFetch = async (
  input: RequestInfo | URL,
  init?: RequestInit
): Promise<Response> => {
  const res = await fetch(input, { credentials: 'same-origin', ...init });
  if (res.status === 401) {
    onUnauthorized?.();
    throw new AuthError();
  }
  return res;
};
