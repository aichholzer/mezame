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
  Role,
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
      // Don't persist an acpSessionId until the session has been used;
      // Kiro only writes to disk on first prompt, so resuming a never-
      // used session fails noisily.
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

// ---------- history rehydration ----------
//
// On resume, mezame suppresses the ACP replay stream and the browser seeds
// its log from `/history?session=<id>`. The server reads Kiro's own JSONL
// event log and returns compact entries with real per-turn timestamps.

type HistoryEntry = {
  role: Role;
  text: string;
  /** Unix epoch millis. May be null for turns Kiro didn't stamp. */
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
  ws: null,
  reconnectAttempt: 0,
  reconnectTimer: null,
  closing: false
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
    setBusy(s, true);
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
  switch (msg.type) {
    case 'ready':
      // Build-id gate: if the server reports a different build id than
      // the one baked into this UI bundle, the binary was rebuilt and
      // the browser is serving a stale bundle. Force a full reload so
      // the user always sees the latest UI without manual intervention.
      if (msg.buildId && msg.buildId !== __MEZAME_BUILD_ID__) {
        window.location.reload();
        return;
      }
      // A resume replays history via session/update — clear stale log so
      // the replay lands in a fresh pane.
      if (msg.resumed) {
        s.log = [];
        s.pinnedToBottom = true;
        s.acpSessionId = msg.sessionId;
        s.effectiveCwd = msg.cwd ?? s.effectiveCwd ?? s.cwd;
        s.promptCapabilities = msg.promptCapabilities ?? {};
        setStatus(s, 'connected');
        // Seed from /history for real per-turn timestamps. The server
        // suppresses the ACP replay stream during the resume window, so
        // this is the single source of truth.
        void loadHistory(s);
      } else {
        s.acpSessionId = msg.sessionId;
        s.effectiveCwd = msg.cwd ?? s.effectiveCwd ?? s.cwd;
        s.promptCapabilities = msg.promptCapabilities ?? {};
        setStatus(s, 'connected');
      }
      scheduleSync();
      break;
    case 'append':
      // User-role chunks during replay: make sure each one starts on its
      // own line even if the previous chunk ended mid-text.
      if (msg.role === 'user') {
        ensureTrailingNewline(s);
      }
      appendLog(s, { kind: 'text', id: newLogId(), role: msg.role, text: msg.text, timestamp: Date.now() });
      break;
    case 'permission_request':
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
      ensureTrailingNewline(s);
      // Force a blank line between turns regardless of what the agent's
      // last chunk ended with.
      appendLog(s, { kind: 'text', id: newLogId(), role: 'sys', text: '\n', timestamp: Date.now() });
      setBusy(s, false);
      raiseAttention(s, 'done');
      break;
    case 'error':
      appendLog(s, { kind: 'text', id: newLogId(), role: 'sys', text: `\n[Error: ${msg.message}]\n`, timestamp: Date.now() });
      s.thinking = false;
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
  notify();
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

if (typeof document !== 'undefined') {
  document.addEventListener('visibilitychange', clearActiveAttentionOnVisible);
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

  // Local echo in the log: text on its own line, attachments as a
  // compact "[attached: N item(s)]" suffix. The agent will see the
  // full blocks; we avoid spamming the chat with base64.
  const echo = attachments.length > 0
    ? `> ${text}\n  [attached: ${attachments.length} item${attachments.length === 1 ? '' : 's'}]\n`
    : `> ${text}\n`;
  appendLog(s, { kind: 'text', id: newLogId(), role: 'user', text: echo, timestamp: Date.now() });

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

const resolvePermission = (sessionId: string, logEntryId: string, option: PermissionOption) => {
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
  setMode,
  setModel,
  markOauthOpened
};
