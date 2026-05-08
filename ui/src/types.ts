// Wire-protocol types between the browser and okiro. These mirror the
// shapes produced by handle_agent_message in src/main.rs.

export type Role = 'user' | 'agent' | 'sys';

export type Attention = 'done' | 'permission' | 'error' | null;

export type ServerMessage =
  | { type: 'ready'; sessionId: string; resumed: boolean; cwd?: string }
  | { type: 'session_info'; info: SessionInfo }
  | { type: 'commands'; commands: SlashCommand[]; prompts: SlashPrompt[] }
  | { type: 'append'; role: Role; text: string }
  | { type: 'permission_request'; id: number | string; title: string; options: PermissionOption[] }
  | { type: 'prompt_done' }
  | { type: 'error'; message: string };

export type ClientMessage =
  | { type: 'prompt'; text: string }
  | { type: 'permission_response'; id: number | string; optionId: string }
  | { type: 'set_mode'; modeId: string }
  | { type: 'set_model'; modelId: string }
  | { type: 'cancel' };

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

/** A log entry in a tab. `text` segments are rendered as pre-wrap spans,
 * permissions render an inline card with buttons. The log is flat and
 * append-only; updates (permission resolution) mutate the item in place.
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

  /** Transient wiring. Not visible to render code. */
  ws: WebSocket | null;
  reconnectAttempt: number;
  reconnectTimer: number | null;
  closing: boolean;
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
