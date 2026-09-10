import { useSyncExternalStore } from 'react';

// Vite injects the version string from `ui/package.json` at build time.
// See `vite.config.ts`.
declare const __MEZAME_VERSION__: string;

// Unique-per-build token (base-36 epoch ms). Compared against the
// server's `buildId` in the `ready` message to detect stale bundles.
declare const __MEZAME_BUILD_ID__: string;

import type {
  Attention,
  ClosedEntry,
  LogEntry,
  PermissionOption,
  PromptBlock,
  ServerMessage,
  Session,
  Status,
  ToolCallLocation,
  Usage
} from '@/types';
import { apiFetch, AuthError, authLost } from '@/lib/api';
import { getIdleSuspendMinutes } from '@/lib/settings';

// Multi-session store.
//
// Kept deliberately mutable behind `useSyncExternalStore`: every mutation
// bumps a version counter and notifies listeners; components reading state
// get a fresh snapshot. Per-field `useState` would force us to choose
// between lots of re-renders or juggling refs. This is simpler and the
// legacy JS already thinks this way.

const STATE_URL = '/state';
/** Where the active tab lives, per device. */
const ACTIVE_KEY = 'mezame.activeSession';

type Snapshot = {
  sessions: Session[];
  closed: ClosedEntry[];
  activeId: string | null;
  version: number;
};

type Listener = () => void;

let sessions: Session[] = [];
let closed: ClosedEntry[] = [];
let activeId: string | null = null;

let version = 0;
let snapshot: Snapshot = { sessions, closed, activeId, version };
const listeners = new Set<Listener>();

const notify = () => {
  version += 1;
  // Shallow-clone the arrays so React's identity check triggers the render.
  snapshot = { sessions: [...sessions], closed: [...closed], activeId, version };
  for (const l of listeners) {
    l();
  }
};

const subscribe = (l: Listener) => {
  listeners.add(l);
  return () => listeners.delete(l);
};

const getSnapshot = () => snapshot;

// ---------- session mutation helpers ----------
//
// These update the backing arrays in place and call `notify()` exactly
// once. Call sites deeper in the event flow (WS message handlers, etc.)
// don't need to call notify themselves.

const newId = () =>
  typeof crypto !== 'undefined' && 'randomUUID' in crypto ? crypto.randomUUID() : String(Math.random()).slice(2);

const newLogId = () =>
  typeof crypto !== 'undefined' && 'randomUUID' in crypto ? crypto.randomUUID() : `log-${Math.random()}`;

const currentSession = () => sessions.find((s) => s.id === activeId);

const findSession = (id: string) => sessions.find((s) => s.id === id);

const appendLog = (s: Session, entry: LogEntry) => {
  // Attempt to merge consecutive same-role text entries so the DOM stays
  // shallow during streaming. Permission cards never merge. Timestamp of
  // the merged entry stays the one from first chunk: a streaming response
  // is one logical "message" even if it spans many seconds.
  const last = s.log.at(-1);
  if (entry.kind === 'text' && last && last.kind === 'text' && last.role === entry.role) {
    last.text += entry.text;
  } else {
    s.log.push(entry);
  }
};

const ensureTrailingNewline = (s: Session) => {
  const last = s.log.at(-1);
  if (last && last.kind === 'text' && !last.text.endsWith('\n')) {
    last.text += '\n';
  }
};

const setStatus = (s: Session, status: Status) => {
  s.status = status;
};

const setBusy = (s: Session, busy: boolean) => {
  s.busy = busy;
};

const raiseAttention = (s: Session, level: NonNullable<Attention>) => {
  // Skip raising attention when the user is already looking at this
  // session: the Mezame tab is visible AND the session is the active
  // in-app tab. Any other combination (different in-app tab, or the
  // whole Mezame browser tab hidden) still raises attention so the
  // favicon badge and document title light up.
  const looking =
    s.id === activeId &&
    typeof document !== 'undefined' &&
    document.visibilityState === 'visible';
  if (looking) {
    return;
  }
  const rank: Record<NonNullable<Attention>, number> = { done: 1, permission: 2, error: 3 };
  if (!s.attention || rank[level] >= rank[s.attention]) {
    s.attention = level;
  }
};

/** Stamp the session's idle anchor to "now". Called whenever the user or
 * agent does something meaningful: a turn finishing, a prompt being sent,
 * the tab being activated, or a (re)connect completing. The idle scan
 * (`shouldSuspendIdle`) measures elapsed time from this stamp. */
const markActivity = (s: Session) => {
  s.lastActivityAt = Date.now();
};

// ---------- the server's list ----------
//
// The session list is the server's: `init` and every `state_changed`
// tick fetch `/state` and apply its `sessions` and `closed` whole. The
// one local addition is a tab minted on `/ws` whose `ready` has not
// arrived yet (`sessionId === null`): it is kept through every refetch
// and adopts the server's id on its first `ready`. Rename, close,
// restore and forget are each one request to the sessions endpoint with
// an optimistic local update; the tick-driven refetch confirms it, and
// a failure refetches at once, which undoes the optimistic change.
// When such a pending tab loses its socket, it goes and the list is
// fetched; if the list then holds nothing, one session is minted in
// its place, once per such loss.
//
// Two things about a document are not taken at face value. Documents
// are applied in the order their requests were sent, so a slow answer
// cannot undo a newer one that has landed already. And a row whose
// archive this browser has sent, or whose tab this browser closed
// before the row's id was known, gets no tab from a document whose
// request went out before that archive landed; the first one requested
// after it is believed whole, since it was read after the archive.

/** What `/state` answers, as far as this store reads it. */
type StateDoc = {
  sessions?: Array<{ id: string; title: string | null }>;
  closed?: Array<{ id: string; title: string | null; closedAt: number | null }>;
};

/** The label a session with no title shows. */
const UNTITLED = 'New session';

/** Bumped by every `reset`. A response that was in flight when the store
 * was emptied belongs to the account that is gone, or to a state the
 * next sign-in has replaced; the request that made it compares the
 * generation it started under with this one and drops a stale answer. */
let generation = 0;

/** The count of `/state` requests sent, by `init` and by every refetch,
 * and the number of the newest one whose answer has been applied. Two
 * requests on separate connections can complete in either order, and a
 * document read before a change would undo the one read after it, so
 * an answer that arrives after a newer one was applied is dropped. The
 * numbers also place a document relative to the archives this browser
 * sent. Both only grow; a reset needs no care here, the generation
 * check drops its stragglers. */
let refetchSent = 0;
let refetchApplied = 0;

/** Tabs closed before their first `ready`. Each is out of `sessions`
 * with `closeOnReady` set and its socket open, waiting for the id the
 * archive needs; the `ready` arm sends the archive and closes the
 * socket. Kept here so a document can count them and `reset` can close
 * them. */
const closingOnReady = new Set<Session>();

/** Ids whose archive this browser has sent, each with the value of
 * `refetchSent` when its 204 arrived, or null while the request is out.
 * A document read before the archive landed still lists such a row as
 * active and would give it a tab again, so the row is skipped while the
 * request is out and by every document whose request went out before
 * the 204 arrived. The first document requested after it was read after
 * the archive committed and is believed whole: it lists the row only if
 * a restore elsewhere undid the archive, and the entry goes either way.
 * Absence from a list is no confirmation on its own, since a document
 * read before the mint lists nothing either. A failed request drops the
 * entry (its refetch puts the tab back), as does a restore from the
 * closed list. */
const archiving = new Map<string, number | null>();

/** The generation under which a tab with no id yet lost its socket and
 * left the list, or null when no session is wanted. A reconnect without
 * an id would mint a second row, so the tab is removed and the list
 * fetched instead; when the document that follows lists nothing, the
 * mint never reached the server and the user has no tab, so one is
 * minted in its place. Cleared by that mint, by any applied document
 * (the user has tabs, or the mint just happened), by a `ready` (the
 * user has a live session) and by `reset`. Holding the generation
 * rather than a boolean means a document applied after a reset cannot
 * act on a want from before it. */
let sessionWanted: number | null = null;

/** The tab minted in a dropped tab's place, while its own `ready` has
 * not arrived. Its socket dropping the same way sets no want: a proxy
 * that refuses every upgrade gets the original mint and this one, then
 * a quiet empty list. Cleared by its `ready`, its drop and `reset`. */
let recoveryMint: Session | null = null;

// Fallback one-shot latch for the stale-bundle reload when
// sessionStorage is unavailable (private mode, storage disabled).
// Prevents the reload-on-every-reconnect loop in that environment.
let reloadLatched = false;

const persistActiveId = () => {
  try {
    if (activeId === null) {
      localStorage.removeItem(ACTIVE_KEY);
    } else {
      localStorage.setItem(ACTIVE_KEY, activeId);
    }
  } catch {
    // Storage unavailable: the active tab is per page load then.
  }
};

const readActiveId = (): string | null => {
  try {
    return localStorage.getItem(ACTIVE_KEY);
  } catch {
    return null;
  }
};

/** Drop a session's socket and timers, marking it closing so the late
 * `onclose` schedules nothing. Local bookkeeping only; the row is the
 * caller's business. */
const dropSocket = (s: Session) => {
  s.closing = true;
  if (s.reconnectTimer !== null) {
    clearTimeout(s.reconnectTimer);
    s.reconnectTimer = null;
  }
  try {
    s.ws?.close();
  } catch {
    // Already disconnected: fine.
  }
  s.ws = null;
};

/** Remove one tab locally: what a 4404 close does, the session having
 * been closed under it elsewhere. */
const removeSessionLocal = (id: string) => {
  const i = sessions.findIndex((x) => x.id === id);
  if (i < 0) {
    return;
  }
  dropSocket(sessions[i]);
  sessions.splice(i, 1);
  if (activeId === id) {
    activeId = sessions.length > 0 ? sessions[Math.max(0, i - 1)].id : null;
    persistActiveId();
  }
  notify();
};

/** Apply a `/state` document whole. A session on both sides keeps its
 * local instance (socket, log and flags) and takes the server's title
 * once there is one; a local tab whose id is absent from the list is
 * removed; a tab that has not received its first `ready`
 * (`sessionId === null`) is kept through every refetch; a server
 * session this browser has not seen gets a tab and connects, unless its
 * archive is not yet known to precede this document's request, or it is
 * taken for the row of a tab closed before its id was known. `number`
 * is the request's place in the count of `/state` requests. The server
 * lists active sessions oldest first and the sidebar shows newest
 * first, so the order is reversed here. */
const applyStateDoc = (doc: StateDoc, number: number) => {
  const rows = Array.isArray(doc.sessions) ? doc.sessions : [];
  const next: Session[] = sessions.filter((s) => s.sessionId === null);
  // While a tab closed before its `ready` waits for its id, the newest
  // row no tab holds is taken to be its mint, one row per waiting tab,
  // and gets no tab of its own. The archive that follows the id settles
  // which row it was, and the tick after that lists the rest.
  let unclaimed = closingOnReady.size;
  for (const row of [...rows].reverse()) {
    if (!row || typeof row.id !== 'string') {
      continue;
    }
    const local = sessions.find((s) => s.id === row.id);
    const archive = archiving.get(row.id);
    if (local) {
      // The server's title wins once it has one; a named tab whose
      // title write has not landed yet keeps its name.
      if (typeof row.title === 'string' && row.title.length > 0) {
        local.label = row.title;
      }
      next.push(local);
    } else if (archive !== undefined && (archive === null || number <= archive)) {
      // The document may have been read before the archive landed.
    } else if (unclaimed > 0) {
      unclaimed -= 1;
    } else {
      const s = makeSession(row.id, row.title ?? UNTITLED, row.id);
      next.push(s);
      connect(s);
    }
  }
  for (const s of sessions) {
    if (s.sessionId !== null && !next.includes(s)) {
      dropSocket(s);
    }
  }
  for (const [id, archive] of [...archiving]) {
    if (archive !== null && number > archive) {
      archiving.delete(id); // requested after the 204: the list above is the truth about the row
    }
  }
  sessions = next;
  closed = (Array.isArray(doc.closed) ? doc.closed : [])
    .filter((row) => row !== null && typeof row === 'object' && typeof row.id === 'string')
    .map((row) => ({
      id: row.id,
      label: typeof row.title === 'string' && row.title.length > 0 ? row.title : UNTITLED,
      closedAt: typeof row.closedAt === 'number' && Number.isFinite(row.closedAt) ? row.closedAt : 0
    }));
  // The active pointer follows the list: when it names no tab the list
  // holds, it takes the first tab or, with none, nothing. This covers a
  // tab removed under it, and a list that was empty when the pointer
  // was cleared, as after the only pending tab's socket dropped and its
  // row came back with an id.
  if (!sessions.some((s) => s.id === activeId)) {
    const first = sessions.length > 0 ? sessions[0].id : null;
    if (first !== activeId) {
      activeId = first;
      persistActiveId();
    }
  }
  // A tab with no id yet lost its socket since the last document was
  // applied, and this one is the first since: when it lists nothing,
  // the mint never reached the server (it restarted during the
  // handshake, or a proxy refused the upgrade) and the user is left
  // with no tab and a disabled composer, so one session is minted in
  // the dropped tab's place. Either way the want is answered here; a
  // document applied under a later generation belongs to another
  // sign-in and answers nothing.
  const wanted = sessionWanted === generation;
  sessionWanted = null;
  if (wanted && sessions.length === 0) {
    recoveryMint = newSession();
  }
  notify();
};

/** Fetch `/state` and apply it whole. Quiet on failure: a 401 has
 * already flipped the auth state, and an unreachable server leaves the
 * local view standing until the next tick. An answer that lands after
 * a later refetch's answer was applied is older state and is dropped. */
const refetchState = async (): Promise<void> => {
  const started = generation;
  refetchSent += 1;
  const number = refetchSent;
  let doc: StateDoc;
  try {
    const res = await apiFetch(STATE_URL);
    if (!res.ok) {
      return;
    }
    doc = (await res.json()) as StateDoc;
  } catch {
    return;
  }
  if (started !== generation) {
    return; // a reset ran while this was in flight: not our state any more
  }
  if (number < refetchApplied) {
    return; // a refetch sent after this one has been applied already
  }
  refetchApplied = number;
  applyStateDoc(doc, number);
};

/** Archive one row: the request goes out and the id is kept out of the
 * list until a document requested after the 204 has been applied, or
 * the request fails. */
const archiveRow = (sessionId: string) => {
  archiving.set(sessionId, null);
  void sessionRequest(sessionPath(sessionId), 'PATCH', { archived: true }).then((ok) => {
    if (!ok) {
      archiving.delete(sessionId); // the failure's refetch puts the tab back
    } else if (archiving.has(sessionId)) {
      // Documents requested from here on were read after the archive
      // committed. A restore that ran meanwhile took the id out, and a
      // later 204 for the same id moves the mark forward.
      archiving.set(sessionId, refetchSent);
    }
  });
};

/** One request to the sessions endpoint. The 204 is confirmed by the
 * tick-driven refetch; any other status, and any rejection short of a
 * 401, refetches `/state` at once, and the server's view undoes the
 * optimistic change the caller made. Resolves true on the 2xx alone. */
const sessionRequest = async (path: string, method: string, body?: unknown): Promise<boolean> => {
  try {
    const res = await apiFetch(path, {
      method,
      ...(body === undefined
        ? {}
        : {
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body)
          })
    });
    if (res.ok) {
      return true;
    }
  } catch (e) {
    if (e instanceof AuthError) {
      return false; // the reset has emptied everything already
    }
  }
  void refetchState();
  return false;
};

const sessionPath = (sessionId: string): string =>
  `/sessions/${encodeURIComponent(sessionId)}`;

// ---------- the state event stream ----------

const STREAM_RETRY_MIN_MS = 5_000;
const STREAM_RETRY_MAX_MS = 60_000;

let stateEventSource: EventSource | null = null;
/** The reopen scheduled after a fatal close, and the delay it will use. */
let streamRetryTimer: number | null = null;
let streamRetryDelay = STREAM_RETRY_MIN_MS;

const startStateEventStream = () => {
  if (typeof EventSource === 'undefined' || stateEventSource !== null) {
    return;
  }
  const es = new EventSource('/state/events');
  stateEventSource = es;
  es.addEventListener('state_changed', () => {
    void refetchState();
  });
  // EventSource auto-reconnects on transport errors with browser
  // defaults. On a fresh connect we proactively refetch so a browser
  // that missed ticks while offline catches up.
  es.addEventListener('open', () => {
    streamRetryDelay = STREAM_RETRY_MIN_MS;
    void refetchState();
  });
  // A non-200 answer (a proxy's 502 while the server restarts, a cookie
  // that has gone stale) closes the source for good, with no retry from
  // the browser. This stream is the one channel that brings another
  // device's changes here, so it is reopened after a pause that doubles
  // while the failures continue; `reset` cancels the pause.
  es.addEventListener('error', () => {
    if (es.readyState !== EventSource.CLOSED) {
      return;
    }
    stateEventSource = null;
    if (streamRetryTimer !== null) {
      return;
    }
    streamRetryTimer = window.setTimeout(() => {
      streamRetryTimer = null;
      startStateEventStream();
    }, streamRetryDelay);
    streamRetryDelay = Math.min(STREAM_RETRY_MAX_MS, streamRetryDelay * 2);
  });
};

// ---------- history rehydration ----------
//
// The hub's broadcast has no replay, so a tab seeds its log from
// `/history?session=<id>` on its first `ready`. The server answers from
// the session's transcript, with a per-entry timestamp.

type HistoryEntry =
  | {
    /** `'user'` and `'agent'` map to text log entries with the
     * matching role; `'thought'` maps to a thought log entry that
     * the UI renders as a collapsible reasoning block. */
    role: 'user' | 'agent' | 'sys' | 'thought';
    text: string;
    /** Unix epoch millis. */
    timestamp: number | null;
    /** The turn's counts, on an agent entry whose row holds them. */
    usage?: unknown;
  }
  | {
    /** A tool call from the transcript. The same object as the live
     * `tool_call` event, with `role` in place of `type` and a
     * `timestamp` added, so the client pushes the same structured log
     * entry on reload as it does during a live turn. */
    role: 'tool_call';
    toolCallId: string;
    title: string;
    status: string | null;
    kind: string | null;
    rawInput: unknown;
    content: unknown;
    locations: unknown;
    timestamp: number | null;
  };

/** How a text entry from the transcript is rendered into the log.
 *
 * A `user` entry is stored without the `> ` prefix and without a trailing
 * newline, and both are added here. Agent markdown renders better if each
 * turn ends in a newline, so the blank-line spacing pass does the right
 * thing on the next turn.
 *
 * Exported because this formula is the third clause of the echo agreement
 * property: the same string the hub broadcast as the live echo has to come
 * back out of the transcript, byte for byte.
 *
 * @internal
 */
export const renderHistoryText = (entry: {
  role: 'user' | 'agent' | 'sys' | 'thought';
  text: string;
}): string => (entry.role === 'user' ? `> ${entry.text}\n` : `${entry.text}\n`);

const loadHistory = async (s: Session, usage?: Usage) => {
  const sessionId = s.sessionId;
  if (!sessionId) {
    return;
  }
  let entries: HistoryEntry[] = [];
  try {
    const res = await apiFetch(`/history?session=${encodeURIComponent(sessionId)}`);
    if (!res.ok) {
      return;
    }
    const body = (await res.json()) as { entries?: HistoryEntry[] };
    entries = body.entries ?? [];
  } catch {
    return;
  }
  // Rebuild the log fresh from history. Existing contents (if any) are
  // discarded: `/history` is the authoritative view of past turns.
  s.log = [];
  for (const e of entries) {
    if (e.role === 'thought') {
      s.log.push({
        kind: 'thought',
        id: newLogId(),
        text: e.text,
        timestamp: e.timestamp ?? Date.now()
      });
      continue;
    }
    if (e.role === 'tool_call') {
      const locations = Array.isArray(e.locations) ? (e.locations as ToolCallLocation[]) : [];
      s.log.push({
        kind: 'tool_call',
        id: newLogId(),
        toolCallId: e.toolCallId,
        title: e.title,
        status: e.status,
        toolKind: e.kind,
        rawInput: e.rawInput,
        content: e.content,
        locations,
        timestamp: e.timestamp ?? Date.now()
      });
      continue;
    }
    const entry: Extract<LogEntry, { kind: 'text' }> = {
      kind: 'text',
      id: newLogId(),
      role: e.role,
      text: renderHistoryText(e),
      timestamp: e.timestamp ?? Date.now()
    };
    // An agent entry carries the turn's counts when the row holds them,
    // so the footer under an answer survives a reload.
    if (e.role === 'agent' && isUsage(e.usage)) {
      entry.usage = e.usage;
    }
    s.log.push(entry);
  }
  // History holds finished turns only. A turn in flight at attach time
  // starts after them, so its counts cannot land on a rebuilt entry.
  if (s.inFlight) {
    s.turnStart = s.log.length;
  }
  // A rebuild that follows a turn's end carries that turn's counts: they
  // belong to the last answer, which the rebuild has just made whole.
  if (usage) {
    const at = lastAgentTextIndex(s.log);
    const entry = at >= 0 ? s.log[at] : undefined;
    if (entry && entry.kind === 'text') {
      entry.usage = usage;
    }
  }
  notify();
};

/** After a turn this tab watched only partly, rebuild the log from the
 * transcript, which is whole once `prompt_done` has been sent. Exported
 * for the suite; `handleMessage` is the one production caller. */
export const rehydrateAfterTurn = (s: Session, usage?: Usage): Promise<void> => {
  s.rehydrateOnTurnEnd = false;
  return loadHistory(s, usage);
};

// ---------- WebSocket lifecycle ----------

const makeSession = (id: string, label: string, sessionId: string | null): Session => ({
  id,
  label,
  sessionId,
  // Filled by the `ready` arm from the directory the server reports.
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
  thoughtOpen: false
});

const connect = (s: Session) => {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const params = new URLSearchParams();
  if (s.sessionId) {
    params.set('session', s.sessionId);
  }
  const query = params.toString();
  const url = query ? `${proto}//${location.host}/ws?${query}` : `${proto}//${location.host}/ws`;

  const ws = new WebSocket(url);
  s.ws = ws;
  setStatus(s, 'connecting');
  notify();

  ws.onopen = () => {
    s.reconnectAttempt = 0;
    setStatus(s, 'connecting'); // Server still needs to emit `ready`.
    notify();
  };

  ws.onclose = (event) => {
    // Stale-socket guard: if we have already moved on to a newer socket
    // (a reconnect, or a suspend -> resume cycle), this close belongs to a
    // dead one and must not drive reconnection.
    if (s.ws !== ws) {
      return;
    }
    if (s.closing) {
      return;
    }
    // The server's own close codes. 4401: the cookie is gone; the auth
    // state flips and its reset closes every socket, so nothing here
    // reconnects. 4404: the session was closed under this tab; it goes,
    // and the refetch brings whatever else changed.
    if (event.code === 4401) {
      authLost();
      return;
    }
    if (event.code === 4404) {
      removeSessionLocal(s.id);
      void refetchState();
      return;
    }
    // A tab whose `ready` has not arrived holds no id to reconnect by,
    // and a connect without one mints a second session. The server may
    // have created the row before the socket dropped, so the tab goes
    // and `/state` is fetched: if the mint landed, the row comes back
    // once, as a tab with its id. A tab closed before its `ready` loses
    // its wait the same way; its row, if there is one, shows as a tab.
    if (s.sessionId === null) {
      // A tab the user still held leaves a session wanted, which the
      // document that follows answers. The tab minted to answer an
      // earlier such want does not ask again, and a tab the user closed
      // already asks for nothing.
      if (s === recoveryMint) {
        recoveryMint = null;
      } else if (!s.closeOnReady) {
        sessionWanted = generation;
      }
      s.closeOnReady = false;
      closingOnReady.delete(s);
      removeSessionLocal(s.id);
      void refetchState();
      return;
    }
    // Intentional idle-suspend: stay grey, do not reconnect. The server's
    // grace timer reclaims the session; we reattach on the next
    // interaction.
    if (s.suspended) {
      return;
    }
    setStatus(s, 'reconnecting');
    // Only treat the disconnect as "still busy" when a turn was
    // actually in flight when the socket dropped. Idle sessions
    // would otherwise be pinned to busy until the next prompt_done,
    // which is never coming if there is no outstanding request.
    if (s.inFlight) {
      setBusy(s, true);
    }
    const delay = Math.min(30000, 500 * Math.pow(2, s.reconnectAttempt));
    s.reconnectAttempt += 1;
    s.reconnectTimer = window.setTimeout(() => connect(s), delay);
    notify();
  };

  ws.onerror = () => {
    // onclose fires right after; let it drive the retry.
  };

  ws.onmessage = (e) => handleMessage(s, e);
};

/** Soft-suspend a session for idleness: drop its socket WITHOUT archiving
 * or auto-reconnecting. The server's grace timer then reclaims the
 * session. The tab stays in the sidebar (grey). Mutates in place; the
 * caller owns `notify()`. No-op when already suspended or closing. */
const suspendSessionNoNotify = (s: Session) => {
  if (s.suspended || s.closing) {
    return;
  }
  s.suspended = true;
  if (s.reconnectTimer !== null) {
    clearTimeout(s.reconnectTimer);
    s.reconnectTimer = null;
  }
  s.reconnectAttempt = 0;
  try {
    s.ws?.close();
  } catch {
    // Already gone: fine.
  }
  // Null the handle so the now-dead socket's late onclose is recognised as
  // stale (see the `s.ws !== ws` guard) and never schedules a retry.
  s.ws = null;
};

/** Resume a suspended session: clear the flag and reconnect, which
 * reattaches via `?session=`. The in-memory log is kept and the tab stays
 * `hydrated`. No-op when not suspended. */
const resumeSession = (s: Session) => {
  if (!s.suspended) {
    return;
  }
  s.suspended = false;
  s.reconnectAttempt = 0;
  markActivity(s);
  connect(s);
};

/** Pure predicate: should this session be suspended for idleness right
 * now? Split out from the scan so the branch logic is unit-testable
 * without timers, sockets, or the module singletons.
 *
 * All must hold: not already suspended/closing; resumable (it names a
 * session); not mid-turn (`busy`/`inFlight`); on a healthy live socket
 * (`connected`); idle past the threshold. The active tab is exempt UNLESS
 * the browser tab itself is hidden; a visible active tab is "in use"
 * even without turns.
 *
 * @internal
 */
export const shouldSuspendIdle = (
  session: Pick<
    Session,
    | 'suspended'
    | 'closing'
    | 'sessionId'
    | 'busy'
    | 'inFlight'
    | 'status'
    | 'lastActivityAt'
  >,
  ctx: { isActive: boolean; visible: boolean; now: number; thresholdMs: number }
): boolean => {
  if (session.suspended || session.closing) {
    return false;
  }
  if (!session.sessionId) {
    return false;
  }
  if (session.busy || session.inFlight) {
    return false;
  }
  if (session.status !== 'connected') {
    return false;
  }
  if (ctx.isActive && ctx.visible) {
    return false;
  }
  return ctx.now - session.lastActivityAt >= ctx.thresholdMs;
};

let idleScanTimer: number | null = null;
const IDLE_SCAN_INTERVAL_MS = 15_000;

/** Scan every session and suspend those idle past the user-configured
 * threshold. Driven by an interval started in `init`. */
const maybeSuspendIdle = () => {
  const thresholdMs = getIdleSuspendMinutes() * 60_000;
  const now = Date.now();
  const visible =
    typeof document === 'undefined' || document.visibilityState === 'visible';
  let dirty = false;
  for (const s of sessions) {
    const ctx = { isActive: s.id === activeId, visible, now, thresholdMs };
    if (shouldSuspendIdle(s, ctx)) {
      suspendSessionNoNotify(s);
      dirty = true;
    }
  }
  if (dirty) {
    notify();
  }
};

const startIdleScan = () => {
  if (idleScanTimer !== null || typeof window === 'undefined') {
    return;
  }
  idleScanTimer = window.setInterval(maybeSuspendIdle, IDLE_SCAN_INTERVAL_MS);
};

/** When the browser tab becomes visible again, resume the ACTIVE session
 * if it was suspended while hidden. Background suspended tabs stay
 * suspended until the user clicks them. Focus never revives every session
 * at once. */
const resumeActiveOnVisible = () => {
  if (typeof document === 'undefined' || document.visibilityState !== 'visible') {
    return;
  }
  const s = activeId ? findSession(activeId) : undefined;
  if (s && s.suspended) {
    resumeSession(s);
  }
};

const handleMessage = (s: Session, event: MessageEvent<string>) => {
  let msg: ServerMessage;
  try {
    msg = JSON.parse(event.data) as ServerMessage;
  } catch {
    return;
  }

  // Captured before the reducer flips them. The `/history` seed below can
  // then tell a first load from a reconnect, and the sync latch can tell a
  // first id from a reconnect that reports the same one.
  const wasHydrated = s.hydrated;
  const hadSessionId = s.sessionId !== null;

  // Side effects that live outside the pure reducer: a stale build id
  // triggers a full page reload, and `ready { resumed: true }` kicks off
  // the `/history` rehydration fetch. Both are kept here so
  // `applyServerMessage` stays free of `window`/`fetch` and stays
  // trivially testable.
  if (msg.type === 'ready' && msg.buildId && msg.buildId !== __MEZAME_BUILD_ID__) {
    // Reload at most once per served build id. `ready` fires on every
    // WS (re)connect, and macOS / idle sockets reconnect often. An
    // unconditional reload here turns a single bundle/binary mismatch
    // (e.g. a tunnel caching a stale asset) into a reload-on-every-
    // reconnect loop: reload, load the same mismatching bundle, get
    // `ready` again, reload again. The latch breaks that loop: if a
    // reload does not resolve the mismatch, we surface it once and
    // stop fighting the user. A genuinely new deploy carries a new
    // server buildId. That is a fresh latch key, and a real upgrade
    // still triggers exactly one reload.
    let alreadyTried = false;
    try {
      const key = `mezame.reloadedFor.${msg.buildId}`;
      alreadyTried = sessionStorage.getItem(key) === '1';
      if (!alreadyTried) {
        sessionStorage.setItem(key, '1');
      }
    } catch {
      // sessionStorage unavailable (private mode, disabled): fall
      // back to a module-level latch so we still never loop.
      alreadyTried = reloadLatched;
      reloadLatched = true;
    }
    if (!alreadyTried) {
      window.location.reload();
      return;
    }
    // Mismatch persisted across a reload. Stop reloading; let the
    // session continue on the bundle we have.
    // eslint-disable-next-line no-console
    console.warn(
      `Mezame UI build ${__MEZAME_BUILD_ID__} does not match server ${msg.buildId}; ` +
        'a reload did not resolve it (stale cache?). Continuing without further reloads.'
    );
  }

  applyServerMessage(s, msg);

  // A turn this tab joined part-way through has ended; the log holds
  // whatever was broadcast after the attach and nothing before it. The
  // transcript is whole now, so the log is rebuilt from it, with the
  // turn's counts on the answer.
  if (msg.type === 'prompt_done' && s.rehydrateOnTurnEnd) {
    void rehydrateAfterTurn(s, isUsage(msg.usage) ? msg.usage : undefined);
  }

  if (msg.type === 'ready') {
    // The tab minted in a dropped tab's place has its id now, so it can
    // no longer drop the way the marker guards against.
    if (s === recoveryMint) {
      recoveryMint = null;
    }
    // A tab named in the new-session dialog titles its row once the
    // server's id is known. The label already holds the name, so the
    // next tick's refetch keeps it whether or not the write has landed.
    if (!hadSessionId && s.sessionId !== null && s.pendingTitle !== undefined) {
      const title = s.pendingTitle;
      s.pendingTitle = undefined;
      void sessionRequest(sessionPath(s.sessionId), 'PATCH', { title });
    }
    // The user closed this tab before its first `ready`, and the
    // archive waited for the id that has just arrived: it goes out now
    // and the socket closes. The tab left the list when it was closed;
    // the reducer has just dropped any tab a document had added for the
    // id, so the active pointer is checked. No history is loaded.
    if (s.closeOnReady && s.sessionId !== null) {
      archiveClosedOnReady(s, s.sessionId);
      notify();
      return;
    }
    // The user has a live session: whichever tab's drop left one
    // wanted, the next document need not mint.
    sessionWanted = null;
    // Seed from /history only on the tab's first hydrate. `wasHydrated`
    // is captured before `applyServerMessage` flips the flag. A
    // transient reconnect (which arrives as `resumed: true` from the
    // hub) then does not refetch history and rebuild the log underneath
    // the user. The in-memory log from the live session is kept as-is.
    if (msg.resumed && !wasHydrated) {
      void loadHistory(s);
    }
  }

  notify();
};

/** Finish the close of a tab that was closed before its first `ready`,
 * now that the id is known: the row joins the closed list, its archive
 * is sent, and the socket that waited for the id is dropped. */
const archiveClosedOnReady = (s: Session, sessionId: string) => {
  s.closeOnReady = false;
  closingOnReady.delete(s);
  dropSocket(s);
  closed.unshift({ id: sessionId, label: s.label, closedAt: Date.now() });
  archiveRow(sessionId);
  if (activeId !== null && !sessions.some((t) => t.id === activeId)) {
    activeId = sessions.length > 0 ? sessions[0].id : null;
    persistActiveId();
  }
};

/**
 * Pure reducer that mutates `s` in response to a parsed `ServerMessage`.
 * No `window`, no `fetch`, no timers; the call site (`handleMessage`)
 * owns those. Exported so the test suite can drive it directly without
 * a real WebSocket.
 *
 * @internal
 */
/** Index of the last `text` entry with role `agent` at or after `from`, or
 * -1. Shared by the `prompt_done` reducer arm and the log pane's
 * streaming gate so the two agree on which bubble is the trailing one. */
export const lastAgentTextIndex = (log: LogEntry[], from = 0): number => {
  for (let i = log.length - 1; i >= Math.max(from, 0); i -= 1) {
    const e = log[i];
    if (e.kind === 'text' && e.role === 'agent') {
      return i;
    }
  }
  return -1;
};

/** The wire's `usage` is trusted only in its declared shape: four finite
 * numbers. A stale bundle against a newer binary, or a proxy rewrite,
 * must not reach the formatters with a string or a missing field. */
const isUsage = (u: unknown): u is Usage => {
  if (typeof u !== 'object' || u === null) {
    return false;
  }
  const r = u as Record<string, unknown>;
  return ['input', 'output', 'cacheRead', 'cacheWrite'].every(
    (k) => typeof r[k] === 'number' && Number.isFinite(r[k] as number)
  );
};

export const applyServerMessage = (s: Session, msg: ServerMessage): void => {
  switch (msg.type) {
    case 'ready':
      // Seed the pane from history ONLY on this tab's first `ready`.
      // The hub stamps `resumed: true` on every attach (an attach is
      // always a join to a live hub). Keying the wipe on `resumed`
      // alone cleared the log and refetched `/history` on every
      // transient reconnect, which looked exactly like the browser
      // reloading mid-chat and could drop an in-flight reply. A
      // reconnect of an already-hydrated tab keeps its in-memory log;
      // only a genuine first load (fresh page, reopened tab) hydrates.
      if (msg.resumed && !s.hydrated) {
        s.log = [];
        s.pinnedToBottom = true;
      }
      s.hydrated = true;
      // The session Mezame bound this connection to. Recorded
      // unconditionally: on a reconnect it is the id we asked for and the
      // assignment is a no-op, and on a tab's first connect it adopts the
      // id the server just minted.
      s.sessionId = msg.sessionId;
      // From the first `ready` on, the tab's own id is the server's: a
      // minted tab adopts it here, the active pointer follows, and any
      // duplicate tab a refetch already added for the id is dropped in
      // this one's favour, so exactly one tab holds the session.
      if (s.id !== msg.sessionId) {
        const wasActive = activeId === s.id;
        const duplicate = sessions.findIndex((t) => t !== s && t.id === msg.sessionId);
        if (duplicate >= 0) {
          dropSocket(sessions[duplicate]);
          sessions.splice(duplicate, 1);
        }
        s.id = msg.sessionId;
        if (wasActive) {
          activeId = msg.sessionId;
          persistActiveId();
        }
      }
      s.effectiveCwd = msg.cwd ?? s.effectiveCwd;
      s.promptCapabilities = msg.promptCapabilities ?? {};
      // `busy` says whether a turn is in flight on this session right
      // now. All three flags follow it, so an attach that lands mid-turn
      // shows what an attach that saw the echo shows, and an attach that
      // lands after a turn is not left pinned to busy by markers set when
      // the socket dropped. The hub guarantees that an attach reading
      // `busy` as true also receives that turn's `prompt_done`.
      s.thinking = msg.busy === true;
      s.inFlight = msg.busy === true;
      setBusy(s, msg.busy === true);
      // An attach into a running turn saw none of its echo; whatever the
      // log holds now belongs to earlier turns, unless this tab's own
      // turn is what is running (a reconnect mid-turn), in which case the
      // marker it already holds stays and the chunks that follow merge
      // into the bubble it opened. `loadHistory` moves the marker again
      // once it has rebuilt the log. Either way the frames broadcast
      // while this tab was away are gone, so the turn's end triggers a
      // rebuild from `/history`. A `ready` with no turn running clears a
      // marker a dropped connection may have left behind.
      if (msg.busy === true) {
        s.turnStart ??= s.log.length;
        s.rehydrateOnTurnEnd = true;
      } else {
        s.turnStart = undefined;
      }
      setStatus(s, 'connected');
      markActivity(s);
      break;
    case 'append':
      // User-role chunks during replay: make sure each one starts on its
      // own line even if the previous chunk ended mid-text.
      if (msg.role === 'user') {
        ensureTrailingNewline(s);
        // The hub broadcasts a single `append { role: 'user' }` echo
        // when any browser sends a prompt. That echo also tells peer
        // browsers a turn just started. Mark the session busy here so
        // every attached browser shows the spinner and locks its
        // composer for the duration of the turn; `prompt_done` clears
        // all three flags. The sender already set these in
        // `sendPrompt` and the assignment is a no-op for them. History
        // replays land via `loadHistory`; this branch is only hit on
        // real turns.
        s.thinking = true;
        s.inFlight = true;
        setBusy(s, true);
      }
      appendLog(s, {
        kind: 'text',
        id: newLogId(),
        role: msg.role,
        text: msg.text,
        timestamp: Date.now()
      });
      if (msg.role === 'user') {
        // The turn's own entries follow the echo.
        s.turnStart = s.log.length;
      }
      break;
    case 'thought': {
      // Reasoning tokens stream as many small chunks. Merge into a
      // single `thought` log entry per turn so the UI renders one
      // collapsible block.
      const last = s.log.at(-1);
      if (s.thoughtOpen && last && last.kind === 'thought') {
        last.text += msg.text;
      } else {
        s.log.push({
          kind: 'thought',
          id: newLogId(),
          text: msg.text,
          timestamp: Date.now()
        });
        s.thoughtOpen = true;
      }
      break;
    }
    case 'permission_request': {
      // Every card is put to the user. No `permission_response` frame is
      // ever sent that no user action produced.
      raiseAttention(s, 'permission');
      s.log.push({
        kind: 'permission',
        id: newLogId(),
        requestId: msg.id,
        title: msg.title,
        options: msg.options,
        timestamp: Date.now()
      });
      break;
    }
    case 'tool_call': {
      // Merge with an existing tool-call entry when this id has been seen
      // before; otherwise push a new row.
      const existing = s.log.find(
        (e) => e.kind === 'tool_call' && e.toolCallId === msg.toolCallId
      );
      const nextTitle = typeof msg.title === 'string' && msg.title.length > 0 ? msg.title : null;
      const nextStatus = typeof msg.status === 'string' && msg.status.length > 0 ? msg.status : null;
      const nextKind = typeof msg.kind === 'string' && msg.kind.length > 0 ? msg.kind : null;
      const nextLocations = Array.isArray(msg.locations) ? (msg.locations as ToolCallLocation[]) : null;
      if (existing && existing.kind === 'tool_call') {
        // An update carries only the fields that changed, and a null
        // field means "no change". Fall back to the prior value.
        if (nextTitle !== null) {
          existing.title = nextTitle;
        }
        if (nextStatus !== null) {
          existing.status = nextStatus;
        }
        if (nextKind !== null) {
          existing.toolKind = nextKind;
        }
        if (msg.rawInput !== undefined && msg.rawInput !== null) {
          existing.rawInput = msg.rawInput;
        }
        if (msg.content !== undefined && msg.content !== null) {
          existing.content = msg.content;
        }
        if (nextLocations !== null) {
          existing.locations = nextLocations;
        }
      } else {
        s.log.push({
          kind: 'tool_call',
          id: newLogId(),
          toolCallId: msg.toolCallId,
          title: nextTitle ?? 'tool',
          status: nextStatus,
          toolKind: nextKind,
          rawInput: msg.rawInput ?? null,
          content: msg.content ?? null,
          locations: nextLocations ?? [],
          timestamp: Date.now()
        });
      }
      break;
    }
    case 'prompt_done':
      // The turn's token counts belong to the answer they paid for: the
      // last agent text entry of this turn, and no earlier one. A turn
      // that produced no text (reasoning only, or a refusal) attaches
      // nothing. `/history` carries none, so a reload shows none
      // (`loadHistory` never sets it).
      if (isUsage(msg.usage)) {
        const at = lastAgentTextIndex(s.log, s.turnStart ?? 0);
        const entry = at >= 0 ? s.log[at] : undefined;
        if (entry && entry.kind === 'text') {
          entry.usage = msg.usage;
        }
      }
      s.turnStart = undefined;
      s.thinking = false;
      s.inFlight = false;
      s.thoughtOpen = false;
      ensureTrailingNewline(s);
      // Force a blank line between turns regardless of what the agent's
      // last chunk ended with.
      appendLog(s, {
        kind: 'text',
        id: newLogId(),
        role: 'sys',
        text: '\n',
        timestamp: Date.now()
      });
      setBusy(s, false);
      raiseAttention(s, 'done');
      markActivity(s);
      break;
    case 'error':
      appendLog(s, {
        kind: 'text',
        id: newLogId(),
        role: 'sys',
        text: `\n[Error: ${msg.message}]\n`,
        timestamp: Date.now()
      });
      s.turnStart = undefined;
      s.thinking = false;
      s.inFlight = false;
      s.thoughtOpen = false;
      setBusy(s, false);
      raiseAttention(s, 'error');
      markActivity(s);
      break;
    case 'session_info':
      s.models = msg.info.models?.availableModels ?? [];
      s.currentModelId = msg.info.models?.currentModelId ?? null;
      break;
  }
};

// ---------- public actions ----------

const activate = (id: string) => {
  activeId = id;
  persistActiveId();
  const s = findSession(id);
  if (s && s.attention) {
    s.attention = null;
  }
  // Activating a session counts as interaction: stamp the idle anchor, and
  // if the tab was suspended for idleness, resume it now (reconnect ->
  // resume the agent session, rehydrating from history if needed).
  if (s) {
    markActivity(s);
    if (s.suspended) {
      resumeSession(s);
    }
  }
  notify();
};

/** Clears attention on the active session when the Mezame browser tab
 * becomes visible again. Covers the case where an event raised
 * attention on the already-active in-app tab while the browser tab
 * was hidden. */
const clearActiveAttentionOnVisible = () => {
  if (typeof document === 'undefined' || document.visibilityState !== 'visible') {
    return;
  }
  const s = activeId ? findSession(activeId) : undefined;
  if (s && s.attention !== null) {
    s.attention = null;
    notify();
  }
};

/** When the browser tab becomes visible after being idle, kick any
 * session that is currently sitting in `reconnecting` to retry now,
 * without waiting out the exponential back-off. macOS' WebSocket
 * tends to die quietly across long idle periods or display sleep.
 * Without this the user sees stale UI for up to 30 seconds. */
const kickReconnectsOnVisible = () => {
  if (typeof document === 'undefined' || document.visibilityState !== 'visible') {
    return;
  }
  let dirty = false;
  for (const s of sessions) {
    if (s.status !== 'reconnecting' || s.closing) {
      continue;
    }
    if (s.reconnectTimer !== null) {
      clearTimeout(s.reconnectTimer);
      s.reconnectTimer = null;
    }
    // Reset back-off so the first attempt after a deliberate kick is
    // immediate and any subsequent failures start fresh.
    s.reconnectAttempt = 0;
    connect(s);
    dirty = true;
  }
  if (dirty) {
    notify();
  }
};

if (typeof document !== 'undefined') {
  document.addEventListener('visibilitychange', clearActiveAttentionOnVisible);
  document.addEventListener('visibilitychange', kickReconnectsOnVisible);
  document.addEventListener('visibilitychange', resumeActiveOnVisible);
}

/** Mint a session: a tab with no id, connected without a `session`
 * parameter, leftmost and active. Returns the tab, so a caller that
 * must recognise it later (the recovery mint) can hold on to it. */
const newSession = (name: string | null = null): Session => {
  const id = newId();
  const named = name !== null && name.trim().length > 0;
  const s = makeSession(id, named ? name.trim() : UNTITLED, null);
  if (named) {
    // Kept locally now, written to the row on the first `ready`, once
    // the server's id is known.
    s.pendingTitle = name.trim();
  }
  // New sessions appear leftmost, right after the fixed `+` button.
  sessions.unshift(s);
  connect(s);
  activate(id);
  return s;
};

const renameSession = (id: string, label: string) => {
  const s = findSession(id);
  const title = label.trim();
  if (!s || !title) {
    return;
  }
  s.label = title;
  notify();
  if (s.sessionId !== null) {
    void sessionRequest(sessionPath(s.sessionId), 'PATCH', { title });
  } else {
    // No row yet: the rename becomes the title the first `ready` writes.
    s.pendingTitle = title;
  }
};

const closeSession = (id: string) => {
  const i = sessions.findIndex((x) => x.id === id);
  if (i < 0) {
    return;
  }
  const s = sessions[i];
  sessions.splice(i, 1);
  if (s.sessionId !== null) {
    dropSocket(s);
    // Optimistic: the row moves to the closed list now; the tick-driven
    // refetch confirms it, and a failure refetches at once, which puts
    // the tab back.
    closed.unshift({ id: s.sessionId, label: s.label, closedAt: Date.now() });
    archiveRow(s.sessionId);
  } else {
    // No id yet, though the server may hold the row already: the socket
    // stays open, out of the list, until `ready` brings the id the
    // archive needs. The `ready` arm sends it and closes the socket.
    s.closeOnReady = true;
    closingOnReady.add(s);
  }
  if (sessions.length === 0) {
    // Never leave the UI empty.
    notify();
    newSession();
    return;
  }
  if (activeId === id) {
    activate(sessions[Math.max(0, i - 1)].id);
  } else {
    notify();
  }
};

const restoreFromHistory = (sessionId: string) => {
  const i = closed.findIndex((e) => e.id === sessionId);
  if (i < 0) {
    return;
  }
  const entry = closed.splice(i, 1)[0];
  const s = makeSession(entry.id, entry.label, entry.id);
  // The row's archive, if it was this browser's and is still unconfirmed,
  // is undone by this restore; documents may list the row active again.
  archiving.delete(sessionId);
  // Restoring is user-initiated; place the tab leftmost alongside
  // freshly-created ones. The socket opens only once the restore has
  // landed: an upgrade before it would find an archived row and 404.
  sessions.unshift(s);
  activate(s.id);
  const started = generation;
  void (async () => {
    if (await sessionRequest(sessionPath(sessionId), 'PATCH', { archived: false })) {
      if (started !== generation) {
        return; // a reset ran while the request was out
      }
      // A document read before the restore landed, and applied while the
      // request was out, still listed the row as closed and removed the
      // tab. A socket for the tab would then belong to no list, so the
      // list is fetched again instead and the row gets its tab from it.
      if (!sessions.includes(s)) {
        void refetchState();
        return;
      }
      connect(s);
      notify();
    }
    // A failure already refetched: the tab is removed again, the closed
    // entry is back, and no socket was opened.
  })();
};

const forgetHistory = (sessionId: string) => {
  const i = closed.findIndex((e) => e.id === sessionId);
  if (i < 0) {
    return;
  }
  closed.splice(i, 1);
  notify();
  void sessionRequest(sessionPath(sessionId), 'DELETE');
};

const sendPrompt = (text: string, attachments: PromptBlock[] = []) => {
  const s = currentSession();
  if (!s || !s.ws || s.ws.readyState !== WebSocket.OPEN) {
    return;
  }
  // Refuse to open a second turn while one is in flight. The hub drops a
  // second prompt in silence, so sending one would look like nothing
  // happened. The composer is already `readOnly` while busy, and guarding
  // here too closes the races that bypass the textarea: a multi-attach
  // peer having started the turn, an Enter fired in the gap before busy
  // propagated, or a stuck readOnly.
  if (s.busy || s.inFlight) {
    return;
  }
  ensureTrailingNewline(s);

  // The user prompt is no longer rendered locally on send; the hub
  // echoes it back as an `append { role: user }` broadcast frame so
  // every attached browser (sender included) sees the same text in
  // its timeline. Local-render-only would hide our prompt from peer
  // browsers and produce inconsistent timelines after multi-attach.
  // The round-trip is microseconds in practice (broadcast in-process,
  // WS sink is local). The sender sees no perceptible delay.
  //
  // Attachments are still part of the wire payload but the echo
  // shows only the text portion; agents that surface uploaded files
  // do so via tool calls in their own time.

  // Text always comes first when present. Attachments preserve the order
  // the user added them.
  const blocks: PromptBlock[] = [];
  if (text.length > 0) {
    blocks.push({ type: 'text', text });
  }
  for (const a of attachments) {
    blocks.push(a);
  }
  s.ws.send(JSON.stringify({ type: 'prompt', blocks }));

  markActivity(s);
  s.thinking = true;
  s.inFlight = true;
  setBusy(s, true);
  notify();
};

const sendCancel = () => {
  const s = currentSession();
  if (!s || !s.ws || s.ws.readyState !== WebSocket.OPEN) {
    return;
  }
  s.ws.send(JSON.stringify({ type: 'cancel' }));
  appendLog(s, { kind: 'text', id: newLogId(), role: 'sys', text: '\n[Cancel requested]\n', timestamp: Date.now() });
  notify();
};

const resolvePermission = (
  sessionId: string,
  logEntryId: string,
  option: PermissionOption
) => {
  const s = findSession(sessionId);
  if (!s) {
    return;
  }
  const entry = s.log.find((e) => e.id === logEntryId);
  if (!entry || entry.kind !== 'permission' || entry.resolution) {
    return;
  }
  entry.resolution = option.name || option.optionId || 'option';
  // User answered the prompt: drop any lingering permission attention
  // so the favicon/title badge de-escalates immediately, with no wait
  // for a turn end or tab switch.
  if (s.attention === 'permission') {
    s.attention = null;
  }
  s.ws?.send(
    JSON.stringify({
      type: 'permission_response',
      id: entry.requestId,
      optionId: option.optionId
    })
  );
  notify();
};

const setModel = (modelId: string) => {
  const s = currentSession();
  if (!s || !s.ws || s.ws.readyState !== WebSocket.OPEN) {
    return;
  }
  s.ws.send(JSON.stringify({ type: 'set_model', modelId }));
  s.currentModelId = modelId;
  notify();
};

const setPinnedToBottom = (sessionId: string, pinned: boolean) => {
  const s = findSession(sessionId);
  if (!s) {
    return;
  }
  if (s.pinnedToBottom !== pinned) {
    s.pinnedToBottom = pinned;
    // No notify: scroll state doesn't affect rendering.
  }
};

// ---------- init and reset ----------

// Guards a concurrent run alone, never a second one: completion and
// `reset` both re-arm it, so each entry into the signed-in state runs
// `init` again.
let initInFlight: Promise<void> | null = null;

const doInit = async (): Promise<void> => {
  const started = generation;
  // Counted with the refetches, so the document's place among the
  // archives this browser sent is known.
  refetchSent += 1;
  const number = refetchSent;
  let doc: StateDoc | null = null;
  try {
    const res = await apiFetch(STATE_URL);
    if (res.ok) {
      doc = (await res.json()) as StateDoc;
    }
  } catch (e) {
    if (e instanceof AuthError) {
      return; // the reset has run; the login gate is up
    }
    // Unreachable server: start empty; the event stream's first open
    // refetches once it comes back.
  }
  if (started !== generation) {
    return; // a reset ran while the fetch was out: this init is void
  }
  // Read before the document is applied: applying it points the active
  // tab at the list's first row and saves that id, which would replace
  // the one the last page load saved.
  const saved = readActiveId();
  if (doc !== null) {
    refetchApplied = Math.max(refetchApplied, number);
    applyStateDoc(doc, number);
  }
  if (sessions.length === 0) {
    // Never leave the UI empty: a first visit mints a session.
    newSession();
  }
  if (saved !== null && sessions.some((s) => s.id === saved)) {
    activate(saved);
  } else if (activeId === null && sessions.length > 0) {
    activate(sessions[0].id);
  }
  // Cross-device change notifications: a session opened elsewhere shows
  // up here without a manual reload.
  startStateEventStream();
  startIdleScan();
};

const init = (): Promise<void> => {
  initInFlight ??= doInit().finally(() => {
    initInFlight = null;
  });
  return initInFlight;
};

/** Undo everything `init` and the session flow built: every socket
 * closed with `closing` set, every timer cleared, the event stream
 * closed, both lists emptied, the active pointer nulled and the `init`
 * guard re-armed. Runs whenever the auth state leaves the signed-in
 * user: a 401, a 4401 close, or the logout button. */
const reset = () => {
  for (const s of sessions) {
    dropSocket(s);
  }
  for (const s of closingOnReady) {
    s.closeOnReady = false;
    dropSocket(s);
  }
  closingOnReady.clear();
  archiving.clear();
  sessionWanted = null;
  recoveryMint = null;
  if (idleScanTimer !== null) {
    clearInterval(idleScanTimer);
    idleScanTimer = null;
  }
  stateEventSource?.close();
  stateEventSource = null;
  if (streamRetryTimer !== null) {
    clearTimeout(streamRetryTimer);
    streamRetryTimer = null;
  }
  streamRetryDelay = STREAM_RETRY_MIN_MS;
  sessions = [];
  closed = [];
  activeId = null;
  initInFlight = null;
  generation += 1;
  notify();
};

/** @internal Test-only view of the module's lists. */
export const __testState = () => ({ sessions, closed, activeId });

// ---------- public hook ----------

export const useMezame = () => {
  const state = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
  return {
    sessions: state.sessions,
    closed: state.closed,
    activeId: state.activeId,
    activeSession: state.sessions.find((s) => s.id === state.activeId) ?? null
  };
};

export const mezameActions = {
  init,
  reset,
  activate,
  newSession,
  renameSession,
  closeSession,
  restoreFromHistory,
  forgetHistory,
  sendPrompt,
  sendCancel,
  resolvePermission,
  setPinnedToBottom,
  setModel
};
