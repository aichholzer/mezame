// Configured-agents lookup. Mirrors `GET /agents` in the Rust core
// (`src/http.rs`): the list of agent names the user defined in
// `~/.mezame/config.json` plus which one is the default. Used to
// populate the new-session agent picker. The server owns the agent
// definitions (command, args, env); the browser only ever sees names.

export type AgentsInfo = {
  /** Configured agent names, in config order. The first is the default. */
  agents: string[];
  /** The default agent name (first entry), or null when none configured. */
  default: string | null;
};

const AGENTS_URL = '/agents';

const EMPTY: AgentsInfo = { agents: [], default: null };

/** Fetch the configured agents. Returns an empty list on any failure so
 * callers can degrade gracefully (the picker simply hides itself when
 * there is nothing meaningful to choose between). */
export const fetchAgents = async (): Promise<AgentsInfo> => {
  try {
    const res = await fetch(AGENTS_URL);
    if (!res.ok) {
      return EMPTY;
    }
    const body = (await res.json()) as Partial<AgentsInfo>;
    const agents = Array.isArray(body.agents)
      ? body.agents.filter((a): a is string => typeof a === 'string')
      : [];
    const def = typeof body.default === 'string' ? body.default : null;
    return { agents, default: def };
  } catch {
    return EMPTY;
  }
};
