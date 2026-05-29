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

// Multi-session ACP store.
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

const doSync = async () => {
  syncTimer = null;
  const body: PersistedState = {
    sessions: sessions.map((s) => ({
      id: s.id,
      label: s.label,
      // Only persist the ACP session id once the session has been
      // used. Kiro writes the on-disk JSONL only on first prompt;
      // persisting a never-used id makes a future `session/load`
      // fail with "Session not found". Peer browsers therefore
      // only see a fresh tab from us after the first prompt; that
      // tradeoff is preferable to a noisy storm of load failures
      // every time someone reloads.
      acpSessionId: s.used ? s.acpSessionId : null,
      cwd: s.cwd
    })),
    closed,
    activeId,
    nextLabel
  };
  try {
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
///   log, busy state, etc.) but pick up label changes from the
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
    // Only restore sessions that have an `acpSessionId` recorded;
    // a fresh-but-unused tab on another browser has no on-disk
    // Kiro session yet, so attaching here would spawn a separate
    // agent. Once that browser sends a first prompt the id lands
    // on the server and the next tick brings it across.
    if (typeof entry.acpSessionId !== 'string' || entry.acpSessionId.length === 0) {
      continue;
    }
    restoreSession({
      id: entry.id,
      label: typeof entry.label === 'string' ? entry.label : '?',
      acpSessionId: entry.acpSessionId,
      cwd: typeof entry.cwd === 'string' ? entry.cwd : null
    });
    dirty = true;
  }

  // Close sessions present locally but missing on the server.
  // Iterate over a copy because we mutate `sessions` in the loop.
  const toClose: string[] = [];
  for (const s of sessions) {
    if (serverIds.has(s.id)) {
      continue;
    }
    // Skip sessions we created but that have not yet synced their
    // own ACP id back. Without this, a brand new tab on this
    // browser would be auto-closed by an SSE tick that fired
    // before our first PUT landed. The next reconcile after our
    // PUT will see ourselves on the server and leave us alone.
    if (!s.used) {
      continue;
    }
    toClose.push(s.id);
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
  // already a "best effort" archive (capped at HISTORY_MAX); using
  // the server snapshot means the most-recently-closed entries
  // stay consistent across browsers without ping-ponging.
  if (Array.isArray(saved.closed)) {
    const next = saved.closed.slice(0, HISTORY_MAX);
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
    // pending push from before the reconcile too, so we do not
    // overwrite the server with the now-stale local snapshot.
    if (syncTimer !== null) {
      clearTimeout(syncTimer);
      syncTimer = null;
    }
    suppressNextSync = true;
    notify();
  }
};

/** Local-only session removal used by reconcile. Mirrors the
 * non-persistence side effects of `closeSession` (cancel timer,
 * close socket, fall back to a fresh activeId, archive in `closed`)
 * but does NOT call `scheduleSync` because the caller is reacting
 * to a server snapshot the server already knows about. */
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
// On resume, mezame suppresses the ACP replay stream and the browser seeds
// its log from `/history?session=<id>`. The server reads Kiro's own JSONL
// event log and returns compact entries with real per-turn timestamps.

type HistoryEntry =
  | {
    /** `'user'` and `'agent'` map to text log entries with the
     * matching role; `'thought'` maps to a thought log entry that
     * the UI renders as a collapsible reasoning block. */
    role: 'user' | 'agent' | 'sys' | 'thought';
    text: string;
    /** Unix epoch millis. May be null for turns Kiro didn't stamp. */
    timestamp: number | null;
  }
  | {
    /** Structured tool call rehydrated from JSONL. Mirrors the
     * live `tool_call` wire shape so the client can push the same
     * structured log entry on reload as it does during a live
     * turn. `status` and `content` are filled in when the
     * matching `ToolResults` entry is parsed; `null` until then. */
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

const loadHistory = async (s: Session) => {
  if (!s.acpSessionId) {
    return;
  }
  let entries: HistoryEntry[] = [];
  try {
    const res = await fetch(`/history?session=${encodeURIComponent(s.acpSessionId)}`);
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
      // Agent markdown renders better if each turn ends in a newline so
      // the blank-line spacing pass does the right thing on next turn.
      text: e.role === 'user' ? `> ${e.text}\n` : `${e.text}\n`,
      timestamp: e.timestamp ?? Date.now()
    });
  }
  notify();
};

// ---------- WebSocket lifecycle ----------

const makeSession = (
  id: string,
  label: string,
  acpSessionId: string | null,
  cwd: string | null
): Session => ({
  id,
  label,
  acpSessionId,
  cwd,
  effectiveCwd: cwd,
  promptCapabilities: {},
  used: acpSessionId !== null,
  log: [],
  status: 'connecting',
  busy: false,
  thinking: false,
  attention: null,
  pinnedToBottom: true,
  modes: [],
  currentModeId: null,
  models: [],
  currentModelId: null,
  commands: [],
  prompts: [],
  rememberedPermissions: {},
  ws: null,
  reconnectAttempt: 0,
  reconnectTimer: null,
  closing: false,
  inFlight: false,
  thoughtOpen: false
});

const connect = (s: Session) => {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const params = new URLSearchParams();
  if (s.acpSessionId) {
    params.set('session', s.acpSessionId);
  }
  if (s.cwd) {
    params.set('cwd', s.cwd);
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
    if (s.closing) {
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

const handleMessage = (s: Session, event: MessageEvent<string>) => {
  let msg: ServerMessage;
  try {
    msg = JSON.parse(event.data) as ServerMessage;
  } catch {
    return;
  }

  // Side effects that live outside the pure reducer: a stale build id
  // triggers a full page reload, and `ready { resumed: true }` kicks off
  // the `/history` rehydration fetch. Both are kept here so
  // `applyServerMessage` stays free of `window`/`fetch` to keep it
  // trivially testable.
  if (msg.type === 'ready' && msg.buildId && msg.buildId !== __MEZAME_BUILD_ID__) {
    window.location.reload();
    return;
  }

  applyServerMessage(s, msg);

  if (msg.type === 'ready') {
    if (msg.resumed) {
      void loadHistory(s);
    }
    scheduleSync();
  }

  // After-the-fact side effect for the auto-resolve path: when
  // `applyServerMessage` saw a permission_request with a remembered
  // policy, it pushed a pre-resolved entry. We still owe the agent
  // a WS reply here. Fire-and-forget; if the WS is gone the user
  // will see a transport error from the next operation.
  if (msg.type === 'permission_request') {
    const last = s.log[s.log.length - 1];
    if (last && last.kind === 'permission' && last.auto && last.requestId === msg.id) {
      const remembered = s.rememberedPermissions[msg.title];
      if (remembered) {
        s.ws?.send(
          JSON.stringify({
            type: 'permission_response',
            id: last.requestId,
            optionId: remembered.optionId
          })
        );
      }
    }
  }

  // Tool calls that finish without streamed content: the agent
  // flipped status to `completed` / `failed` over the live wire
  // but the result text only landed on disk in the JSONL. Pull
  // it back via `/tool-result` so the user can expand the card
  // and read the output without reloading the page. Web search
  // is the canonical example: Kiro emits the status update but
  // not the search result body.
  if (msg.type === 'tool_call') {
    const finalStatuses = new Set(['completed', 'failed', 'cancelled']);
    const status = typeof msg.status === 'string' ? msg.status : null;
    if (status && finalStatuses.has(status)) {
      const entry = s.log.find(
        (e) => e.kind === 'tool_call' && e.toolCallId === msg.toolCallId
      );
      if (entry && entry.kind === 'tool_call' && entry.content === null) {
        void backfillToolResult(s, entry.toolCallId);
      }
    }
  }

  notify();
};

// ---------- tool result backfill ----------
//
// Kiro writes the JSONL asynchronously: the live status flip can
// land before the on-disk `ToolResults` entry. Poll a few times
// with brief backoff before giving up. Five tries spaced 250ms
// apart covers the typical write delay; if the result still is
// not there after that, the user can reload to pick it up via
// the regular history rehydration path.

const backfillToolResult = async (s: Session, toolCallId: string): Promise<void> => {
  if (!s.acpSessionId) {
    return;
  }
  const sessionId = s.acpSessionId;
  for (let attempt = 0; attempt < 5; attempt += 1) {
    if (attempt > 0) {
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
    let payload: { status?: string | null; content?: unknown } | null = null;
    try {
      const url = `/tool-result?session=${encodeURIComponent(sessionId)}&id=${encodeURIComponent(toolCallId)}`;
      const res = await fetch(url);
      if (res.status === 404) {
        continue;
      }
      if (!res.ok) {
        return;
      }
      payload = (await res.json()) as { status?: string | null; content?: unknown };
    } catch {
      return;
    }
    if (!payload || payload.content === null || payload.content === undefined) {
      continue;
    }
    // Find the entry again: the log may have grown but `id` matching
    // by toolCallId is still the contract. If the user closed the
    // session between the dispatch and the result landing, drop it.
    const entry = s.log.find(
      (e) => e.kind === 'tool_call' && e.toolCallId === toolCallId
    );
    if (!entry || entry.kind !== 'tool_call') {
      return;
    }
    entry.content = payload.content;
    if (typeof payload.status === 'string' && payload.status.length > 0) {
      entry.status = payload.status;
    }
    notify();
    return;
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
export const applyServerMessage = (s: Session, msg: ServerMessage): void => {
  switch (msg.type) {
    case 'ready':
      // A resume replays history via session/update — clear stale log
      // so the replay (or the /history seed) lands in a fresh pane.
      if (msg.resumed) {
        s.log = [];
        s.pinnedToBottom = true;
      }
      s.acpSessionId = msg.sessionId;
      s.effectiveCwd = msg.cwd ?? s.effectiveCwd ?? s.cwd;
      s.promptCapabilities = msg.promptCapabilities ?? {};
      // After a resume, clear any in-flight markers we set when the
      // socket dropped. The agent will not replay the old
      // `prompt_done` (the server suppresses the live replay during
      // the resume window), so without this the composer would stay
      // pinned to busy until the next turn naturally completed.
      // Safe even on a fresh connect: a session that has not sent a
      // prompt has both flags clear already.
      if (msg.resumed) {
        s.thinking = false;
        s.inFlight = false;
        setBusy(s, false);
      }
      setStatus(s, 'connected');
      break;
    case 'append':
      // User-role chunks during replay: make sure each one starts on its
      // own line even if the previous chunk ended mid-text.
      if (msg.role === 'user') {
        ensureTrailingNewline(s);
        // The hub broadcasts a single `append { role: 'user' }` echo
        // when any browser sends a prompt, so this is also the
        // signal to peer browsers that a turn just started. Mark
        // the session busy here so every attached browser shows
        // the spinner and locks its composer for the duration of
        // the turn; `prompt_done` clears all three flags. The
        // sender already set these in `sendPrompt`, so the
        // assignment is a no-op for them. We skip on history
        // replays (those land via `loadHistory`, not the live
        // broadcast, so this branch is only hit on real turns).
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
      // collapsible block, not a torrent of one-token rows.
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
      // If the user previously ticked "remember for this session" for
      // a permission with this exact title, resolve the new request
      // immediately with the stored option. The matching `permission_response`
      // WS frame is sent by `handleMessage` after the reducer runs;
      // the reducer itself stays free of side effects.
      const remembered = s.rememberedPermissions[msg.title];
      if (remembered) {
        s.log.push({
          kind: 'permission',
          id: newLogId(),
          requestId: msg.id,
          title: msg.title,
          options: msg.options,
          timestamp: Date.now(),
          resolution: remembered.name || remembered.optionId || 'option',
          auto: true
        });
        // Deliberately no `raiseAttention`: the user already opted in,
        // so a remembered allow-or-reject should not draw the eye.
        break;
      }
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
    case 'mcp_oauth_request': {
      // De-dupe re-emissions: Kiro keeps sending while the agent waits.
      // Match by `requestId` when present, otherwise by serverName+url.
      const existing = s.log.find(
        (e) =>
          e.kind === 'mcp_oauth' &&
          ((msg.id !== null && e.requestId === msg.id) ||
            (e.serverName === msg.serverName && e.url === msg.url))
      );
      if (existing) {
        break;
      }
      raiseAttention(s, 'permission');
      s.log.push({
        kind: 'mcp_oauth',
        id: newLogId(),
        requestId: msg.id,
        serverName: msg.serverName,
        url: msg.url,
        timestamp: Date.now(),
        opened: false
      });
      break;
    }
    case 'tool_call': {
      // Merge with an existing tool-call entry if we have seen this id
      // before (ACP `tool_call_update`); otherwise push a new row.
      const existing = s.log.find(
        (e) => e.kind === 'tool_call' && e.toolCallId === msg.toolCallId
      );
      const nextTitle = typeof msg.title === 'string' && msg.title.length > 0 ? msg.title : null;
      const nextStatus = typeof msg.status === 'string' && msg.status.length > 0 ? msg.status : null;
      const nextKind = typeof msg.kind === 'string' && msg.kind.length > 0 ? msg.kind : null;
      const nextLocations = Array.isArray(msg.locations) ? (msg.locations as ToolCallLocation[]) : null;
      if (existing && existing.kind === 'tool_call') {
        // ACP updates carry only the fields that changed. Fall back to
        // the prior value when a field is absent or null.
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
      break;
    case 'session_info':
      s.modes = msg.info.modes?.availableModes ?? [];
      s.currentModeId = msg.info.modes?.currentModeId ?? null;
      s.models = msg.info.models?.availableModels ?? [];
      s.currentModelId = msg.info.models?.currentModelId ?? null;
      break;
    case 'commands':
      s.commands = msg.commands;
      s.prompts = msg.prompts;
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
 * session that is currently sitting in `reconnecting` to retry now
 * instead of waiting out the exponential back-off. macOS' WebSocket
 * tends to die quietly across long idle periods or display sleep,
 * so without this the user sees stale UI for up to 30 seconds. */
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
}

const newSession = (cwd: string | null = null, name: string | null = null) => {
  const id = newId();
  const label = name && name.length > 0 ? name : String(nextLabel++);
  const s = makeSession(id, label, null, cwd);
  // New sessions appear leftmost, right after the fixed `+` button, so
  // the freshly-created tab is closest to the control that spawned it.
  sessions.unshift(s);
  connect(s);
  activate(id);
};

const restoreSession = (saved: { id: string; label: string; acpSessionId: string | null; cwd: string | null }) => {
  const s = makeSession(saved.id, saved.label, saved.acpSessionId, saved.cwd);
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
  // Only archive sessions that reached a sessionId AND were actually used;
  // unused sessions aren't on disk so restoring them would fail.
  if (s.used && s.acpSessionId) {
    closed.unshift({
      id: s.id,
      label: s.label,
      acpSessionId: s.acpSessionId,
      cwd: s.cwd,
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

const restoreFromHistory = (acpSessionId: string) => {
  const i = closed.findIndex((e) => e.acpSessionId === acpSessionId);
  if (i < 0) {
    return;
  }
  const entry = closed.splice(i, 1)[0];
  const s = makeSession(entry.id, entry.label, entry.acpSessionId, entry.cwd);
  // Restoring is user-initiated; place the tab leftmost alongside
  // freshly-created ones for consistency.
  sessions.unshift(s);
  connect(s);
  activate(s.id);
  scheduleSync();
};

const forgetHistory = (acpSessionId: string) => {
  const i = closed.findIndex((e) => e.acpSessionId === acpSessionId);
  if (i < 0) {
    return;
  }
  closed.splice(i, 1);
  notify();
  scheduleSync();
};

const sendPrompt = (text: string, attachments: PromptBlock[] = []) => {
  const s = currentSession();
  if (!s || !s.ws || s.ws.readyState !== WebSocket.OPEN) {
    return;
  }
  ensureTrailingNewline(s);

  // The user prompt is no longer rendered locally on send; the hub
  // echoes it back as an `append { role: user }` broadcast frame so
  // every attached browser (sender included) sees the same text in
  // its timeline. Local-render-only would hide our prompt from peer
  // browsers and produce inconsistent timelines after multi-attach.
  // The round-trip is microseconds in practice (broadcast in-process,
  // WS sink is local), so the sender sees no perceptible delay.
  //
  // Attachments are still part of the wire payload but the echo
  // shows only the text portion; agents that surface uploaded files
  // do so via tool calls in their own time.

  // Build the ACP-shaped prompt. Text always comes first when present.
  // Attachments preserve the order the user added them.
  const blocks: PromptBlock[] = [];
  if (text.length > 0) {
    blocks.push({ type: 'text', text });
  }
  for (const a of attachments) {
    blocks.push(a);
  }
  s.ws.send(JSON.stringify({ type: 'prompt', blocks }));

  s.thinking = true;
  s.inFlight = true;
  setBusy(s, true);
  if (!s.used) {
    s.used = true;
    scheduleSync();
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
  option: PermissionOption,
  remember: boolean = false
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
  if (remember) {
    entry.remembered = true;
    s.rememberedPermissions = {
      ...s.rememberedPermissions,
      [entry.title]: option
    };
  }
  // User answered the prompt: drop any lingering permission attention
  // so the favicon/title badge de-escalates immediately rather than
  // waiting for a turn end or tab switch.
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

/** Drop a single remembered permission policy by title. The next
 * matching `permission_request` will prompt the user again. Does
 * not change anything already in the log. */
const forgetRememberedPermission = (sessionId: string, title: string) => {
  const s = findSession(sessionId);
  if (!s) {
    return;
  }
  if (!(title in s.rememberedPermissions)) {
    return;
  }
  const next = { ...s.rememberedPermissions };
  delete next[title];
  s.rememberedPermissions = next;
  notify();
};

/** Drop every remembered permission policy on the given session.
 * Surfaced as a small button on resolved cards that landed via auto;
 * the next matching `permission_request` will then prompt the user
 * again. Does not change anything already in the log. */
const clearRememberedPermissions = (sessionId: string) => {
  const s = findSession(sessionId);
  if (!s) {
    return;
  }
  if (Object.keys(s.rememberedPermissions).length === 0) {
    return;
  }
  s.rememberedPermissions = {};
  notify();
};

const setMode = (modeId: string) => {
  const s = currentSession();
  if (!s || !s.ws || s.ws.readyState !== WebSocket.OPEN) {
    return;
  }
  s.ws.send(JSON.stringify({ type: 'set_mode', modeId }));
  // Optimistic: update local state straight away. If the server rejects,
  // we'll get an `error` message and the log will surface it.
  s.currentModeId = modeId;
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

const markOauthOpened = (sessionId: string, logEntryId: string) => {
  const s = findSession(sessionId);
  if (!s) {
    return;
  }
  const entry = s.log.find((e) => e.id === logEntryId);
  if (!entry || entry.kind !== 'mcp_oauth') {
    return;
  }
  entry.opened = true;
  // The card stays in the log (the URL may be needed again), but the
  // attention dot can drop: the user has acknowledged the request.
  if (s.attention === 'permission') {
    s.attention = null;
  }
  notify();
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
    closed = saved.closed.slice(0, HISTORY_MAX);
  }
  if (saved?.sessions && Array.isArray(saved.sessions) && saved.sessions.length > 0) {
    nextLabel = saved.nextLabel ?? saved.sessions.length + 1;
    for (const entry of saved.sessions) {
      restoreSession(entry);
    }
    const restoreActive =
      saved.activeId && sessions.some((s) => s.id === saved.activeId) ? saved.activeId : sessions[0].id;
    activate(restoreActive);
  } else {
    newSession();
  }
  // Subscribe to cross-browser change notifications so a session
  // started elsewhere shows up here without a manual reload.
  startStateEventStream();
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
  forgetRememberedPermission,
  clearRememberedPermissions,
  setPinnedToBottom,
  setMode,
  setModel,
  markOauthOpened
};
