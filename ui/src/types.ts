// The wire contract between a browser and Mezame. Eight server events,
// four client commands. See docs/wire-protocol.md.

export type Role = 'user' | 'agent' | 'sys';

export type Attention = 'done' | 'permission' | 'error' | null;

export type ServerMessage =
  | {
    type: 'ready';
    sessionId: string;
    /** True on every attach: an attach is always a join to a session
     * that already exists. The client reads it as "clear any stale
     * local log and seed yourself from /history". */
    resumed: boolean;
    /** True when a turn is in flight on this session at attach time.
     * The composer opens read-only and unlocks on that turn's
     * `prompt_done`. */
    busy: boolean;
    cwd?: string;
    promptCapabilities?: PromptCapabilities;
    buildId?: string;
  }
  | { type: 'session_info'; info: SessionInfo }
  | { type: 'append'; role: Role; text: string }
  | { type: 'thought'; text: string }
  | { type: 'tool_call'; toolCallId: string; title?: string | null; status?: string | null; kind?: string | null; rawInput?: unknown; content?: unknown; locations?: unknown }
  | { type: 'permission_request'; id: number | string; title: string; options: PermissionOption[] }
  | { type: 'prompt_done' }
  | { type: 'error'; message: string };

export type ClientMessage =
  | { type: 'prompt'; blocks: PromptBlock[] }
  | { type: 'permission_response'; id: number | string; optionId: string }
  | { type: 'set_model'; modelId: string }
  | { type: 'cancel' };

/** What Mezame accepts as prompt content. Missing fields default to
 * false. */
export type PromptCapabilities = {
  image?: boolean;
  audio?: boolean;
  embeddedContext?: boolean;
};

/** The block vocabulary a prompt is built from. Mezame forwards every
 * member of the array unchecked, so adding a member here needs no server
 * change. */
export type PromptBlock =
  | { type: 'text'; text: string }
  | { type: 'image'; mimeType: string; data: string }
  | {
    type: 'resource';
    resource:
    | { uri: string; mimeType?: string; text: string }
    | { uri: string; mimeType?: string; blob: string };
  };

export type SessionInfo = {
  models?: {
    currentModelId?: string;
    availableModels?: ModelEntry[];
  } | null;
};

export type ModelEntry = {
  modelId: string;
  name?: string;
  description?: string;
};

export type PermissionOption = {
  optionId: string;
  name?: string;
  kind?: string;
};

export type ToolCallLocation = {
  path?: string;
  line?: number;
};

/** The four status values the wire contract admits. Anything else is
 * displayed verbatim. */
export type ToolCallStatus = 'pending' | 'in_progress' | 'completed' | 'failed' | (string & {});

/** A log entry in a tab. `text` segments are rendered as pre-wrap spans,
 * permissions render an inline card with buttons, and tool calls render
 * a collapsible summary row with arguments, content, and locations.
 * The log is flat and append-only; updates (permission resolution,
 * tool-call progress) mutate the item in place.
 */
export type LogEntry =
  | { kind: 'text'; id: string; role: Role; text: string; timestamp: number }
  | {
    kind: 'thought';
    id: string;
    /** Aggregated reasoning text. Chunks are merged into the latest
     * `thought` entry until the turn ends (`prompt_done` / `error`),
     * after which the next thought chunk starts a fresh entry. */
    text: string;
    timestamp: number;
  }
  | {
    kind: 'permission';
    id: string;
    requestId: number | string;
    title: string;
    options: PermissionOption[];
    timestamp: number;
    /** Set once the user picks an option. Presence disables buttons. */
    resolution?: string;
  }
  | {
    kind: 'tool_call';
    id: string;
    /** The wire's tool-call id; keyed for in-place updates. */
    toolCallId: string;
    title: string;
    status: ToolCallStatus | null;
    toolKind: string | null;
    rawInput: unknown;
    content: unknown;
    locations: ToolCallLocation[];
    timestamp: number;
  };

export type Status = 'connecting' | 'connected' | 'reconnecting' | 'error';

export type Session = {
  /** Client-local id; stable across reloads because it's persisted. */
  id: string;
  /** Display label shown in the tab bar. */
  label: string;
  /** The session id Mezame minted, reported on the first `ready`. Null
   * until then. Persisted, and sent back as `?session=` on every
   * reconnect, so a reload or a second device reaches the same
   * conversation. */
  sessionId: string | null;
  /** The working directory Mezame runs in, as the `ready` event reported
   * it. The server's own process directory is its only source.
   * Display-only. */
  effectiveCwd: string | null;
  /** What Mezame accepts as prompt content. Drives which attachment
   * affordances the composer exposes. */
  promptCapabilities: PromptCapabilities;

  /** UI state. None of these survive a reload. */
  log: LogEntry[];
  /** True once this tab has seeded its log for the current page
   * session, either from a fresh connect or a `/history` hydrate.
   * Gates the destructive wipe-and-reseed in the `ready` handler so a
   * transient WebSocket reconnect (macOS idle/sleep drops, network
   * blips) does NOT clear the in-memory log and refetch history. Only
   * the first `ready` of a tab hydrates; reconnects keep the log they
   * already have. Resets only on a real page load (it is not
   * persisted). */
  hydrated: boolean;
  status: Status;
  busy: boolean;
  thinking: boolean;
  attention: Attention;
  pinnedToBottom: boolean;
  /** Epoch ms of the last activity on this session: a turn finishing
   * (`prompt_done`/`error`), a prompt being sent, the tab being activated,
   * or a (re)connect completing. Anchors the idle timer that suspends a
   * session after the user-configured quiet period (see
   * `shouldSuspendIdle`). Not persisted; seeded on construct. */
  lastActivityAt: number;
  /** Models reported by `session_info`. */
  models: ModelEntry[];
  currentModelId: string | null;

  /** Transient wiring. Not visible to render code. */
  ws: WebSocket | null;
  reconnectAttempt: number;
  reconnectTimer: number | null;
  closing: boolean;
  /** True while the session is intentionally suspended to release its
   * server-side resources after an idle period. Distinct from `closing`:
   * a suspended session stays in the sidebar (rendered grey) and its
   * socket is dropped WITHOUT auto-reconnect. The server's grace timer
   * then reclaims the session. It reconnects on the next interaction.
   * Not persisted. */
  suspended: boolean;
  /** Whether a turn is currently in flight. Used by the WS close handler
   * to decide whether to re-flag the session as `busy` while
   * reconnecting. Set when the user sends a prompt, cleared on
   * `prompt_done` or `error`. Idle sessions therefore do not get pinned
   * to "working" across an idle drop. */
  inFlight: boolean;
  /** True while reasoning tokens are streaming for the current turn.
   * Subsequent `thought` chunks merge into the trailing thought log
   * entry. Cleared on `prompt_done` / `error` so the next turn opens a
   * fresh thought block. */
  thoughtOpen: boolean;
};

export type ClosedEntry = {
  id: string;
  label: string;
  sessionId: string;
  closedAt: number;
};

export type PersistedState = {
  sessions: Array<Pick<Session, 'id' | 'label' | 'sessionId'>>;
  closed: ClosedEntry[];
  activeId: string | null;
  nextLabel: number;
};
