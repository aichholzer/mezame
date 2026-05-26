// Wire-protocol types between the browser and mezame. These mirror the
// shapes produced by handle_agent_message in src/ws.rs.

export type Role = 'user' | 'agent' | 'sys';

export type Attention = 'done' | 'permission' | 'error' | null;

export type ServerMessage =
  | {
    type: 'ready';
    sessionId: string;
    resumed: boolean;
    cwd?: string;
    promptCapabilities?: PromptCapabilities;
    buildId?: string;
  }
  | { type: 'session_info'; info: SessionInfo }
  | { type: 'commands'; commands: SlashCommand[]; prompts: SlashPrompt[] }
  | { type: 'append'; role: Role; text: string }
  | { type: 'tool_call'; toolCallId: string; title?: string | null; status?: string | null; kind?: string | null; rawInput?: unknown; content?: unknown; locations?: unknown }
  | { type: 'permission_request'; id: number | string; title: string; options: PermissionOption[] }
  | { type: 'mcp_oauth_request'; id: number | string | null; serverName: string; url: string }
  | { type: 'prompt_done' }
  | { type: 'error'; message: string };

export type ClientMessage =
  | { type: 'prompt'; text?: string; blocks?: PromptBlock[] }
  | { type: 'permission_response'; id: number | string; optionId: string }
  | { type: 'set_mode'; modeId: string }
  | { type: 'set_model'; modelId: string }
  | { type: 'cancel' };

/** Prompt capabilities advertised by the agent at initialize time.
 * Missing fields default to false; the agent advertises what it will
 * accept as prompt content. */
export type PromptCapabilities = {
  image?: boolean;
  audio?: boolean;
  embeddedContext?: boolean;
};

/** Subset of ACP ContentBlock types we build on the client side. The
 * server accepts any ACP-shaped block and forwards it without
 * validation, so this union can grow without a server change. */
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
  modes?: {
    currentModeId?: string;
    availableModes?: ModeEntry[];
  } | null;
  models?: {
    currentModelId?: string;
    availableModels?: ModelEntry[];
  } | null;
};

export type ModeEntry = {
  id: string;
  name?: string;
  description?: string;
};

export type ModelEntry = {
  modelId: string;
  name?: string;
  description?: string;
};

export type SlashCommand = {
  name: string;
  description?: string;
  meta?: {
    hint?: string;
    subcommands?: string[];
    subcommandHints?: Record<string, string>;
    inputType?: string;
    local?: boolean;
    optionsMethod?: string;
  };
};

export type SlashPrompt = {
  name: string;
  description?: string;
  serverName?: string;
  arguments?: Array<{ name: string; description?: string; required?: boolean }>;
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

/** Known status values from ACP. Anything else is displayed verbatim. */
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
    kind: 'permission';
    id: string;
    requestId: number | string;
    title: string;
    options: PermissionOption[];
    timestamp: number;
    /** Set once the user picks an option. Presence disables buttons. */
    resolution?: string;
    /** True when the resolution was auto-applied from a remembered
     * policy on the session, not from a click. Drives an "(auto)"
     * indicator in the resolved card. */
    auto?: boolean;
  }
  | {
    kind: 'mcp_oauth';
    id: string;
    /** Server-provided id used to dedupe re-emitted requests. May be
     * null when the agent did not include one; in that case we dedupe
     * by `serverName` + `url` instead. */
    requestId: number | string | null;
    serverName: string;
    url: string;
    timestamp: number;
    /** Flipped when the user clicks Open. The card stays in the log so
     * the URL remains accessible if the user closes the new tab. */
    opened: boolean;
  }
  | {
    kind: 'tool_call';
    id: string;
    /** ACP tool-call id; keyed for in-place updates. */
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
  /** ACP session id returned by `session/new` or `session/load`. */
  acpSessionId: string | null;
  /** Optional working directory override passed via `?cwd=`. */
  cwd: string | null;
  /** Actual cwd the agent session was opened with, reported by the
   * server on `ready`. Equals `cwd` when the user supplied an override,
   * otherwise the server's own process cwd. Display-only. */
  effectiveCwd: string | null;
  /** Prompt capabilities advertised by the agent at initialize time.
   * Drives which attachment affordances the composer exposes. */
  promptCapabilities: PromptCapabilities;
  /** True once the user has sent at least one prompt. Kiro only persists
   * a session to disk on first turn; unused sessions cannot be resumed. */
  used: boolean;

  /** UI state. None of these survive a reload. */
  log: LogEntry[];
  status: Status;
  busy: boolean;
  thinking: boolean;
  attention: Attention;
  pinnedToBottom: boolean;
  /** Agent modes / models reported by `session_info`. */
  modes: ModeEntry[];
  currentModeId: string | null;
  models: ModelEntry[];
  currentModelId: string | null;
  /** Kiro slash-command catalogue for autocomplete. Fills from the
   * `_kiro.dev/commands/available` notification. */
  commands: SlashCommand[];
  prompts: SlashPrompt[];

  /** Per-session "remember for this session" policies, keyed by the
   * permission-request title. When the user ticks the box on a
   * permission card, the chosen option is stored here; subsequent
   * permission_requests with the same title auto-resolve with the
   * remembered option and fire the WS reply without UI. Cleared by
   * the user via the resolved-card button or implicitly on tab
   * close. Never persisted: this is session-local only.
   */
  rememberedPermissions: Record<string, PermissionOption>;

  /** Transient wiring. Not visible to render code. */
  ws: WebSocket | null;
  reconnectAttempt: number;
  reconnectTimer: number | null;
  closing: boolean;
  /** Whether a `session/prompt` request is currently in flight. Used
   * by the WS close handler to decide whether to re-flag the session
   * as `busy` while reconnecting. Set when the user sends a prompt,
   * cleared on `prompt_done` or `error`. Idle sessions therefore do
   * not get pinned to "Agent is working..." across an idle drop. */
  inFlight: boolean;
};

export type ClosedEntry = {
  id: string;
  label: string;
  acpSessionId: string;
  cwd: string | null;
  closedAt: number;
};

export type PersistedState = {
  sessions: Array<Pick<Session, 'id' | 'label' | 'acpSessionId' | 'cwd'>>;
  closed: ClosedEntry[];
  activeId: string | null;
  nextLabel: number;
};
