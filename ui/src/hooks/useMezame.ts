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
  PersistedState,
  PromptBlock,
  ServerMessage,
  Session,
  Status,
  ToolCallLocation
} from '@/types';
import { getIdleSuspendMinutes } from '@/lib/settings';

// Multi-session store.
//
// Kept deliberately mutable behind `useSyncExternalStore`: every mutation
// bumps a version counter and notifies listeners; components reading state
// get a fresh snapshot. Per-field `useState` would force us to choose
// between lots of re-renders or juggling refs. This is simpler and the
// legacy JS already thinks this way.

const STATE_URL = '/state';
const HISTORY_MAX = 20;

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
let nextLabel = 1;

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

// ---------- persistence ----------

let syncTimer: number | null = null;

const scheduleSync = () => {
  if (suppressNextSync) {
    suppressNextSync = false;
    return;
  }
  if (syncTimer !== null) {
    clearTimeout(syncTimer);
  }
  syncTimer = window.setTimeout(doSync, 400);
};

/**
 * Build the `sessions` array to PUT to `/state`, merging this browser's
 * local view with any sessions recorded only on the server.
 *
 * `/state` is last-writer-wins per top-level key and `doSync` owns the
 * `sessions` array. A blind write of just the local list silently drops
 * a session another device opened but this browser has not yet learned
 * about: a backgrounded tab whose `/state/events` stream missed ticks,
 * a device just switched to, or an init race. That is how a live
 * session vanished from `state.json` while its conversation stayed on
 * disk.
 *
 * Absence from the local list is ambiguous, exactly as on the read path
 * (`shouldCloseAbsentSession`): it can mean "we never knew about it"
 * (carry it forward) or "we closed it" (let the close propagate). We
 * disambiguate with the same positive evidence: a deliberately closed
 * session is recorded in a `closed` list. So we carry forward every
 * server session we lack locally that is not recorded as closed on
 * EITHER side. The reconcile has the same keep-don't-drop bias.
 *
 * Pure and exported so the regression test can drive it without module
 * state or `fetch`.
 *
 * @internal
 */
/** True when a persisted session or closed entry carries a session id
 * this build can attach to.
 *
 * Applied at every point a persisted entry is read. It also handles a
 * `state.json` written by 0.13.x, whose ids sit under a key this version
 * does not read: those entries are discarded, the UI starts with one fresh
 * tab, and the next sync rewrites both lists under the new key.
 *
 * @internal
 */
export const hasSessionId = (entry: {
  sessionId?: string | null;
}): entry is { sessionId: string } =>
  typeof entry.sessionId === 'string' && entry.sessionId.length > 0;

export const mergeSessionsForSync = (
  localSessions: PersistedState['sessions'],
  localClosed: ClosedEntry[],
  existing: Partial<PersistedState> | null
): PersistedState['sessions'] => {
  const serverSessions = existing?.sessions;
  if (!Array.isArray(serverSessions)) {
    return localSessions;
  }
  const localIds = new Set(localSessions.map((s) => s.id));
  // A deliberately closed session must not be resurrected; honour both
  // our own `closed` history and the server's. A close that originated
  // on another device then also suppresses the carry-forward.
  const closedIds = new Set<string>();
  for (const c of localClosed) {
    if (c && hasSessionId(c)) {
      closedIds.add(c.sessionId);
    }
  }
  const serverClosed = existing?.closed;
  if (Array.isArray(serverClosed)) {
    for (const c of serverClosed) {
      if (c && hasSessionId(c)) {
        closedIds.add(c.sessionId);
      }
    }
  }
  const carried: PersistedState['sessions'] = [];
  for (const entry of serverSessions) {
    if (!entry || typeof entry.id !== 'string' || localIds.has(entry.id)) {
      continue;
    }
    // Only carry entries that name a session, matching the restore guard
    // in `reconcileFromServer`. A tab elsewhere that has not applied its
    // first `ready` yet has no id, and there is nothing here to attach to.
    if (!hasSessionId(entry)) {
      continue;
    }
    if (closedIds.has(entry.sessionId)) {
      continue;
    }
    carried.push({
      id: entry.id,
      label: typeof entry.label === 'string' ? entry.label : '?',
      sessionId: entry.sessionId
    });
  }
  return carried.length > 0 ? [...localSessions, ...carried] : localSessions;
};

export const doSync = async () => {
  syncTimer = null;
  const owned: PersistedState = {
    sessions: sessions.map((s) => ({
      id: s.id,
      label: s.label,
      // Persisted with no gate. Mezame mints the id at upgrade time, so a
      // tab holds a resumable one from its first `ready` and reaches peer
      // browsers on its first connect.
      sessionId: s.sessionId
    })),
    closed,
    activeId,
    nextLabel
  };
  try {
    // Read-then-merge: `/state` is a shared blob with more than one
    // writer. We own the session fields above; the settings store
    // (`lib/settings.ts`) owns `settings`. A blind PUT of only our
    // fields would clobber `settings` on every session event (open,
    // rename, close, tab switch, cross-browser reconcile), silently
    // resetting the user's preferences. Carry across whatever we do not
    // own. `settings.ts` persist() preserves our fields the same way.
    //
    // The `sessions` array is ours, but a blind write of just the local
    // list has its own hazard: it drops a session another device opened
    // that we have not synced yet (the residual of the cross-device
    // clobber the reconcile guards against on the read path).
    // `mergeSessionsForSync` unions in those server-only sessions first.
    const existing = await fetchState();
    const body = {
      ...(existing ?? {}),
      ...owned,
      sessions: mergeSessionsForSync(owned.sessions, closed, existing)
    };
    await fetch(STATE_URL, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body)
    });
  } catch {
    // Unreachable server: state stays local. WS failures imply mezame is
    // down; nothing works anyway.
  }
};

const fetchState = async (): Promise<Partial<PersistedState> | null> => {
  try {
    const res = await fetch(STATE_URL);
    if (!res.ok) {
      return null;
    }
    return (await res.json()) as Partial<PersistedState>;
  } catch {
    return null;
  }
};

// ---------- cross-browser session sync ----------
//
// `state.json` is the cross-device store for the session list. Each
// browser PUTs to `/state` after a local change (new session, rename,
// close, switch active). The server is the source of truth: when a
// tick lands on `/state/events` the browser refetches and reconciles
// its local list against the server snapshot.
//
// Reconciliation is a two-way merge:
//
// - Sessions present locally but not on the server were closed
//   somewhere else; we close them here too. Without this, a close
//   on browser A could never propagate to browser B because B's
//   own next PUT would overwrite the server view back to "all
//   four sessions" and A would see them reappear.
// - Sessions present on the server but not locally were opened
//   somewhere else; we restore them.
// - Sessions present on both keep their local instance (its WS,
//   log, busy state, etc.) but take label changes from the
//   server.
//
// The active session is preserved when possible: if the server's
// activeId is different but our active is still in the merged list,
// we keep our active. If our active was removed by the merge, we
// fall back to the server's activeId, then to whatever is left.
//
// To avoid a "ping-pong" where this browser's reconcile triggers
// another PUT that triggers another tick, the reconciled state is
// applied without scheduling a sync. The server already has the
// snapshot we just merged from; there is nothing to push back.

let stateEventSource: EventSource | null = null;
let suppressNextSync = false;

// Fallback one-shot latch for the stale-bundle reload when
// sessionStorage is unavailable (private mode, storage disabled).
// Prevents the reload-on-every-reconnect loop in that environment.
let reloadLatched = false;

const reconcileFromServer = async () => {
  const saved = await fetchState();
  if (!saved?.sessions || !Array.isArray(saved.sessions)) {
    return;
  }
  const serverIds = new Set<string>();
  for (const entry of saved.sessions) {
    if (entry && typeof entry.id === 'string') {
      serverIds.add(entry.id);
    }
  }
  let dirty = false;

  // Restore sessions present on the server but not locally.
  for (const entry of saved.sessions) {
    if (!entry || typeof entry.id !== 'string') {
      continue;
    }
    if (sessions.some((s) => s.id === entry.id)) {
      continue;
    }
    // Only restore entries that name a session. A tab elsewhere that has
    // not applied its first `ready` yet has no id; once it does, the id
    // lands on the server and the next tick brings it across.
    if (!hasSessionId(entry)) {
      continue;
    }
    restoreSession({
      id: entry.id,
      label: typeof entry.label === 'string' ? entry.label : '?',
      sessionId: entry.sessionId
    });
    dirty = true;
  }

  // Close sessions present locally but missing on the server, but
  // ONLY when we have positive evidence the disappearance was a
  // deliberate close. Each browser PUTs its own full session list
  // last-writer-wins. "Absent from the latest server snapshot" is
  // therefore ambiguous: it can mean "closed on another device"
  // OR "another device just overwrote the list with a partial view
  // that happened to omit this session" (an init race, a restore
  // that didn't carry every tab, etc.). Treating the ambiguous case
  // as a close is what made live sessions vanish from state.json
  // with no trace.
  //
  // A deliberate close records the session in the server's `closed`
  // history (see `closeSession`). A clobber does not. So we only close
  // locally when the session's id shows up in the server's `closed`
  // list; otherwise we keep it, and our next sync re-adds it to the
  // server snapshot.
  //
  // Tradeoff: if a deliberate close is later evicted from the capped
  // `closed` history (HISTORY_MAX) before this browser reconciles, we
  // will keep a tab that was actually closed elsewhere. The stale tab
  // is benign and self-corrects on the next close.
  const serverClosedIds = new Set<string>();
  if (Array.isArray(saved.closed)) {
    for (const entry of saved.closed) {
      if (entry && hasSessionId(entry)) {
        serverClosedIds.add(entry.sessionId);
      }
    }
  }
  // Iterate over a copy because we mutate `sessions` in the loop.
  const toClose: string[] = [];
  for (const s of sessions) {
    if (serverIds.has(s.id)) {
      continue;
    }
    if (shouldCloseAbsentSession(s, serverClosedIds)) {
      toClose.push(s.id);
    }
  }
  for (const id of toClose) {
    closeSessionLocal(id);
    dirty = true;
  }

  // Pick up label changes for sessions present on both sides.
  for (const entry of saved.sessions) {
    if (!entry || typeof entry.id !== 'string') {
      continue;
    }
    const local = sessions.find((s) => s.id === entry.id);
    if (!local) {
      continue;
    }
    const newLabel = typeof entry.label === 'string' ? entry.label : null;
    if (newLabel && newLabel !== local.label) {
      local.label = newLabel;
      dirty = true;
    }
  }

  // Bump nextLabel so a future `New session` button on this browser
  // does not collide with a numeric label coined elsewhere.
  if (typeof saved.nextLabel === 'number' && saved.nextLabel > nextLabel) {
    nextLabel = saved.nextLabel;
  }

  // Closed-history list: server view wins. The dropdown is
  // already a "best effort" archive (capped at HISTORY_MAX). On the
  // server snapshot the most-recently-closed entries stay consistent
  // across browsers with no ping-pong.
  if (Array.isArray(saved.closed)) {
    const next = saved.closed.filter((entry) => entry && hasSessionId(entry)).slice(0, HISTORY_MAX);
    if (JSON.stringify(next) !== JSON.stringify(closed)) {
      closed = next;
      dirty = true;
    }
  }

  // If our active was removed, fall back to the server's active or
  // the first remaining session.
  if (activeId && !sessions.some((s) => s.id === activeId)) {
    if (saved.activeId && sessions.some((s) => s.id === saved.activeId)) {
      activeId = saved.activeId;
    } else if (sessions.length > 0) {
      activeId = sessions[0].id;
    } else {
      activeId = null;
    }
    dirty = true;
  }

  if (dirty) {
    // Suppress the next scheduleSync: we have just applied the
    // server's view, there is nothing to push back. Cancel any
    // pending push from before the reconcile too. It would overwrite
    // the server with the now-stale local snapshot.
    if (syncTimer !== null) {
      clearTimeout(syncTimer);
      syncTimer = null;
    }
    suppressNextSync = true;
    notify();
  }
};

/** Decide whether a local session that is absent from the server's
 * latest `sessions` snapshot should be closed locally during
 * reconcile.
 *
 * Absence is ambiguous under the last-writer-wins `state.json` model:
 * it can mean "closed deliberately on another device" or "another
 * device clobbered the list with a partial view that omitted this
 * session". We only treat it as a close when the server's `closed`
 * history corroborates it; otherwise we keep the session and let the
 * next sync restore it. Pure and exported so the regression test for
 * the vanishing-session bug can drive it without `fetch`.
 *
 * @internal
 */
export const shouldCloseAbsentSession = (
  session: Pick<Session, 'sessionId'>,
  serverClosedIds: Set<string>
): boolean => {
  // Never auto-close a tab that has no id yet: it may simply predate our
  // first PUT.
  if (!session.sessionId) {
    return false;
  }
  // Require positive evidence of a deliberate close.
  return serverClosedIds.has(session.sessionId);
};

/** Local-only session removal used by reconcile. Mirrors the
 * non-persistence side effects of `closeSession` (cancel timer,
 * close socket, fall back to a fresh activeId, archive in `closed`)
 * but does NOT call `scheduleSync` because the caller is reacting
 * to a server snapshot the server already holds. */
const closeSessionLocal = (id: string) => {
  const i = sessions.findIndex((x) => x.id === id);
  if (i < 0) {
    return;
  }
  const s = sessions[i];
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
  // We do NOT push to `closed` here: the other browser already
  // recorded the close in its own history list and we are about
  // to receive that history via the server snapshot.
  sessions.splice(i, 1);
  if (activeId === id) {
    activeId = sessions.length > 0 ? sessions[Math.max(0, i - 1)].id : null;
  }
};

const startStateEventStream = () => {
  if (typeof EventSource === 'undefined' || stateEventSource !== null) {
    return;
  }
  const es = new EventSource('/state/events');
  stateEventSource = es;
  es.addEventListener('state_changed', () => {
    void reconcileFromServer();
  });
  // EventSource auto-reconnects on transport errors with browser
  // defaults. On a fresh connect we proactively reconcile so a
  // browser that missed ticks while offline catches up.
  es.addEventListener('open', () => {
    void reconcileFromServer();
  });
  es.addEventListener('error', () => {
    if (es.readyState === EventSource.CLOSED) {
      stateEventSource = null;
    }
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

const loadHistory = async (s: Session) => {
  const sessionId = s.sessionId;
  if (!sessionId) {
    return;
  }
  let entries: HistoryEntry[] = [];
  try {
    const res = await fetch(`/history?session=${encodeURIComponent(sessionId)}`);
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
    s.log.push({
      kind: 'text',
      id: newLogId(),
      role: e.role,
      text: renderHistoryText(e),
      timestamp: e.timestamp ?? Date.now()
    });
  }
  notify();
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

  ws.onclose = () => {
    // Stale-socket guard: if we have already moved on to a newer socket
    // (a reconnect, or a suspend -> resume cycle), this close belongs to a
    // dead one and must not drive reconnection.
    if (s.ws !== ws) {
      return;
    }
    if (s.closing) {
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

  if (msg.type === 'ready') {
    // Seed from /history only on the tab's first hydrate. `wasHydrated`
    // is captured before `applyServerMessage` flips the flag. A
    // transient reconnect (which arrives as `resumed: true` from the
    // hub) then does not refetch history and rebuild the log underneath
    // the user. The in-memory log from the live session is kept as-is.
    if (msg.resumed && !wasHydrated) {
      void loadHistory(s);
    }
    // A minted id has to reach the state endpoint even right after a
    // peer's reconcile set the latch. Without this the id stays
    // unpersisted until the next rename or close, and no peer browser
    // learns about the tab.
    if (!hadSessionId && s.sessionId !== null) {
      suppressNextSync = false;
    }
    scheduleSync();
  }

  notify();
};

/**
 * Pure reducer that mutates `s` in response to a parsed `ServerMessage`.
 * No `window`, no `fetch`, no timers; the call site (`handleMessage`)
 * owns those. Exported so the test suite can drive it directly without
 * a real WebSocket.
 *
 * @internal
 */
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
  scheduleSync();
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

const newSession = (name: string | null = null) => {
  const id = newId();
  const label = name && name.length > 0 ? name : String(nextLabel++);
  const s = makeSession(id, label, null);
  // New sessions appear leftmost, right after the fixed `+` button.
  sessions.unshift(s);
  connect(s);
  activate(id);
};

const restoreSession = (saved: { id: string; label: string; sessionId: string | null }) => {
  const s = makeSession(saved.id, saved.label, saved.sessionId);
  // Init-time restore: preserve the order captured in persisted state by
  // appending. The UI's leftmost-insertion rule only applies to user-
  // initiated new sessions.
  sessions.push(s);
  connect(s);
};

const renameSession = (id: string, label: string) => {
  const s = findSession(id);
  if (!s || !label.trim()) {
    return;
  }
  s.label = label.trim();
  notify();
  scheduleSync();
};

const closeSession = (id: string) => {
  const i = sessions.findIndex((x) => x.id === id);
  if (i < 0) {
    return;
  }
  const s = sessions[i];
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
  // Only archive a session that names one: there is nothing to reattach
  // to otherwise.
  if (s.sessionId) {
    closed.unshift({
      id: s.id,
      label: s.label,
      sessionId: s.sessionId,
      closedAt: Date.now()
    });
    if (closed.length > HISTORY_MAX) {
      closed.length = HISTORY_MAX;
    }
  }
  sessions.splice(i, 1);
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
  scheduleSync();
};

const restoreFromHistory = (sessionId: string) => {
  const i = closed.findIndex((e) => e.sessionId === sessionId);
  if (i < 0) {
    return;
  }
  const entry = closed.splice(i, 1)[0];
  const s = makeSession(entry.id, entry.label, entry.sessionId);
  // Restoring is user-initiated; place the tab leftmost alongside
  // freshly-created ones.
  sessions.unshift(s);
  connect(s);
  activate(s.id);
  scheduleSync();
};

const forgetHistory = (sessionId: string) => {
  const i = closed.findIndex((e) => e.sessionId === sessionId);
  if (i < 0) {
    return;
  }
  closed.splice(i, 1);
  notify();
  scheduleSync();
};

// Derive a short label from the user's first prompt. Pure heuristic: no
// network and no model. Numeric tab labels stop being anonymous after the
// first turn.
//
// Returns null when the prompt isn't useful as a label (empty, slash
// command, attachments-only). The caller leaves the original label in
// that case.
export const deriveLabel = (text: string): string | null => {
  const cleaned = text
    .replace(/```[\s\S]*?```/g, ' ')
    .replace(/https?:\/\/\S+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();

  if (!cleaned || cleaned.startsWith('/')) {
    return null;
  }

  const locale = typeof navigator !== 'undefined' ? navigator.language : 'en';

  // Sentence boundary: Intl.Segmenter handles CJK punctuation that a
  // plain /[.!?\n]/ regex would miss.
  let firstSentence = cleaned;
  const sentSeg = new Intl.Segmenter(locale, { granularity: 'sentence' });
  for (const seg of sentSeg.segment(cleaned)) {
    firstSentence = seg.segment.trim();
    break;
  }

  if (firstSentence.length < 2) {
    return null;
  }

  // Soft cap so a long single sentence doesn't become the label.
  // Word segmentation matters for scripts without spaces (CJK). We
  // slice the original string up to the end of the last word we keep.
  // Spacing and punctuation between words are preserved verbatim.
  const MAX_WORDS = 10;
  const wordSeg = new Intl.Segmenter(locale, { granularity: 'word' });
  let lastEnd = 0;
  let wordCount = 0;
  for (const piece of wordSeg.segment(firstSentence)) {
    if (piece.isWordLike) {
      lastEnd = piece.index + piece.segment.length;
      wordCount += 1;
      if (wordCount >= MAX_WORDS) {
        break;
      }
    }
  }
  if (wordCount === 0) {
    return null;
  }
  return firstSentence.slice(0, lastEnd).trim();
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
  // Auto-label from the prompt while the tab still carries its bare
  // numeric placeholder (e.g. "3"). A manual name set through the new
  // session dialog or a rename is non-numeric and survives. A prompt
  // `deriveLabel` cannot make a label out of leaves the placeholder in
  // place, so a later prompt gets another go.
  if (/^\d+$/.test(s.label)) {
    const derived = deriveLabel(text);
    if (derived) {
      s.label = derived;
      scheduleSync();
    }
  }
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

// ---------- init ----------

let initStarted = false;

const init = async () => {
  if (initStarted) {
    return;
  }
  initStarted = true;
  const saved = await fetchState();
  if (saved?.closed && Array.isArray(saved.closed)) {
    closed = saved.closed.filter((entry) => entry && hasSessionId(entry)).slice(0, HISTORY_MAX);
  }
  // Every persisted entry has to name a session; one that does not is
  // discarded with no error and no tab is restored for it. Checking the
  // filtered list rather than the raw one closes the hole a `sessions[0]`
  // read leaves when the filter removed every entry.
  const restorable = Array.isArray(saved?.sessions)
    ? saved.sessions.filter(
      (entry) => entry && typeof entry.id === 'string' && hasSessionId(entry)
    )
    : [];
  if (restorable.length > 0) {
    nextLabel = saved?.nextLabel ?? restorable.length + 1;
    for (const entry of restorable) {
      restoreSession(entry);
    }
    const restoreActive =
      saved?.activeId && sessions.some((s) => s.id === saved.activeId)
        ? saved.activeId
        : sessions[0].id;
    activate(restoreActive);
  } else {
    newSession();
  }
  // Subscribe to cross-browser change notifications so a session
  // started elsewhere shows up here without a manual reload.
  startStateEventStream();
  startIdleScan();
};

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
