//! On-disk configuration and interactive setup.
//!
//! Config lives at `~/.mezame/config.json`. Schema changes are breaking for
//! existing users, so add fields with `#[serde(default)]` rather than
//! reshuffling. Transports live in a list (`TransportConfig`) internally
//! tagged on `kind`; see the README Configuration reference and todo #19.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Input, Select};
use serde::{Deserialize, Serialize};

const MEZAME_ART: &str = r#"
 ███╗   ███╗███████╗███████╗ █████╗ ███╗   ███╗███████╗
 ████╗ ████║██╔════╝╚══███╔╝██╔══██╗████╗ ████║██╔════╝
 ██╔████╔██║█████╗    ███╔╝ ███████║██╔████╔██║█████╗  
 ██║╚██╔╝██║██╔══╝   ███╔╝  ██╔══██║██║╚██╔╝██║██╔══╝  
 ██║ ╚═╝ ██║███████╗███████╗██║  ██║██║ ╚═╝ ██║███████╗
 ╚═╝     ╚═╝╚══════╝╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝╚══════╝
 目覚め!
"#;

pub const DEFAULT_PORT: u16 = 9510;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub transports: Vec<TransportConfig>,
    /// Agents the user can choose between when opening a new session.
    /// Each is spawned on demand; the agent backing a session is fixed
    /// for that session's lifetime (a session is tied to one agent's
    /// own session store, so it cannot be re-bound to another agent on
    /// resume — see `?agent=` in the wire protocol). The first entry is
    /// the default for new sessions.
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    /// Legacy single-agent fields, kept so configs written before the
    /// multi-agent schema keep loading. `normalize` folds these into
    /// `agents` at load time; nothing reads them afterwards, and they
    /// are not re-serialised once `agents` is populated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_cmd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_args: Vec<String>,
}

/// One configurable ACP agent. `mezame init` writes a single entry;
/// users add more (e.g. the Claude Code ACP bridge) by hand-editing
/// `~/.mezame/config.json`. See the README configuration reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Stable identifier shown in the new-session picker and echoed back
    /// as the `?agent=` WS query param. Expected to be unique within
    /// `agents`; the first match wins if not.
    pub name: String,
    /// Binary to spawn, resolved against `$PATH` or given as an absolute
    /// path.
    pub command: String,
    /// Arguments passed on every spawn (e.g. `["acp"]` for Kiro CLI).
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the agent subprocess, merged onto
    /// Mezame's own environment. The Claude Code ACP bridge needs
    /// `ANTHROPIC_API_KEY` (or a logged-in Claude session); Kiro needs
    /// none. Kept out of `args` so secrets do not surface in process
    /// listings.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl Config {
    /// Fold legacy single-agent fields into `agents`. Idempotent: a
    /// config already using the `agents` list is left untouched aside
    /// from clearing the legacy fields. Called once after
    /// deserialisation so the rest of the code only ever deals with the
    /// list form.
    pub fn normalize(&mut self) {
        if self.agents.is_empty() {
            if let Some(cmd) = self.agent_cmd.take().filter(|c| !c.trim().is_empty()) {
                let name = legacy_agent_name(&cmd);
                self.agents.push(AgentConfig {
                    name,
                    command: cmd,
                    args: std::mem::take(&mut self.agent_args),
                    env: BTreeMap::new(),
                });
            }
        }
        // Drop any leftover legacy fields so they never round-trip.
        self.agent_cmd = None;
        self.agent_args.clear();
    }

    /// The agent a new session uses when the browser names none: the
    /// first configured agent. `None` only when `agents` is empty.
    pub fn default_agent(&self) -> Option<&AgentConfig> {
        self.agents.first()
    }

    /// Resolve the agent for a new session. `name` comes from the
    /// browser's `?agent=` param; `None` falls back to the default.
    /// Errors when a named agent is not configured, or when no agents
    /// are configured at all.
    pub fn resolve_agent(&self, name: Option<&str>) -> Result<&AgentConfig> {
        match name {
            Some(n) => self
                .agents
                .iter()
                .find(|a| a.name == n)
                .with_context(|| format!("No agent named `{n}` in config.json")),
            None => self
                .default_agent()
                .context("No agents configured. Re-run `mezame init`."),
        }
    }
}

/// Derive a picker name for a migrated legacy agent from its command,
/// e.g. `/usr/bin/kiro-cli` -> `kiro-cli`. Falls back to `default` when
/// the command has no usable file-name component.
fn legacy_agent_name(cmd: &str) -> String {
    std::path::Path::new(cmd)
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

/// Transport entries are internally tagged by `kind`, so each variant can
/// carry its own config without a separate top-level section. Adding a new
/// transport is: add a variant here, add an arm in `main`, implement its
/// `run_*` entry point.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TransportConfig {
    Cloudflared { bind: String },
    // Telegram { token: String } — commented out until `run_telegram`
    // ships. Leaving the variant here would require it to round-trip, and
    // we do not want to pretend it works.
}

pub fn config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".mezame/config.json"))
}

/// Path to the persistent browser state (currently-open tabs, history list,
/// active id, next numeric label). Server-side so any device hitting Mezame
/// sees the same list.
pub fn state_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".mezame/state.json"))
}

/// Read the durable "auto-allow all tool permissions" preference from
/// the UI state store (`state.json`). The flag is written by the
/// browser Settings pane through the existing `PUT /state` endpoint and
/// read here on demand whenever the agent raises
/// `session/request_permission`. Reads are rare (human-paced) so the
/// per-request file read is negligible and always reflects the latest
/// toggle without a restart. Any failure — missing file, malformed
/// JSON, absent field — resolves to the safe default `false`, so a
/// permission request falls back to prompting the human.
pub async fn read_auto_allow_permissions() -> bool {
    let Ok(path) = state_path() else {
        return false;
    };
    let Ok(raw) = tokio::fs::read_to_string(&path).await else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    auto_allow_from_state(&value)
}

/// Pure extraction of the auto-allow flag from a parsed `state.json`
/// value. Split from the IO in `read_auto_allow_permissions` so the
/// lookup can be unit-tested without touching the filesystem. Defaults
/// to `false` when `settings.autoAllowPermissions` is missing or not a
/// boolean.
pub fn auto_allow_from_state(state: &serde_json::Value) -> bool {
    state
        .get("settings")
        .and_then(|s| s.get("autoAllowPermissions"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

pub fn load_config() -> Result<Config> {
    let path = config_path()?;
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("Reading {}", path.display()))?;
    let mut cfg: Config = serde_json::from_str(&raw).context("Parsing config.json")?;
    cfg.normalize();
    Ok(cfg)
}

pub(crate) fn init_config() -> Result<Config> {
    // Transport prompt commented out while Cloudflared is the only
    // implemented option. When Telegram ships, rewrite this to build up the
    // `transports` list interactively (ask for Cloudflared, offer to add
    // another, loop) rather than resurrecting the single-choice block
    // below verbatim.
    //
    // let transport_idx = Select::with_theme(&ColorfulTheme::default())
    //     .with_prompt("Which transport?")
    //     .items(&["Cloudflared  (serve a terminal-like web UI; front with your tunnel)",
    //              "Telegram     (long-poll a Telegram bot)  [not yet implemented]"])
    //     .default(0)
    //     .interact()?;

    let theme = ColorfulTheme::default();

    let loopback = format!("127.0.0.1:{DEFAULT_PORT}");
    let all = format!("0.0.0.0:{DEFAULT_PORT}");

    let bind_options = [
        format!("{loopback}  (loopback only, default)"),
        format!("{all}    (all IPv4 interfaces, reachable from LAN)"),
        "Custom          (type an address:port)".to_string(),
    ];

    println!("{}", MEZAME_ART);
    let bind_idx = Select::with_theme(&theme)
        .with_prompt("Bind address")
        .items(&bind_options)
        .default(0)
        .interact()?;
    let bind = match bind_idx {
        0 => loopback,
        1 => all,
        _ => {
            let s: String = Input::with_theme(&theme)
                .with_prompt("Bind address")
                .validate_with(|input: &String| -> Result<(), &str> {
                    if input.trim().is_empty() {
                        Err("Bind address is required")
                    } else {
                        Ok(())
                    }
                })
                .interact_text()?;
            s.trim().to_string()
        }
    };

    let agent_cmd: String;
    let default_args: Vec<String>;
    match pick_agent(&theme)? {
        Some(picked) => {
            agent_cmd = picked.path.to_string_lossy().into_owned();
            default_args = picked.default_args;
        }
        None => {
            let typed: String = Input::with_theme(&theme)
                .with_prompt("ACP agent command (e.g. kiro-cli, claude, gemini, codex)")
                .validate_with(|input: &String| -> Result<(), &str> {
                    if input.trim().is_empty() {
                        Err("Agent command is required")
                    } else {
                        Ok(())
                    }
                })
                .interact_text()?;
            agent_cmd = typed.trim().to_string();
            if agent_cmd.is_empty() {
                bail!("Agent command is required");
            }
            default_args = Vec::new();
        }
    }

    let default_args_str = default_args.join(" ");
    let args_raw: String = Input::with_theme(&theme)
        .with_prompt("Agent args (space-separated, e.g. `acp` for Kiro CLI)")
        .allow_empty(true)
        .default(default_args_str.clone())
        .show_default(!default_args_str.is_empty())
        .interact_text()?;
    let agent_args: Vec<String> = args_raw.split_whitespace().map(str::to_string).collect();

    let cfg = Config {
        transports: vec![TransportConfig::Cloudflared { bind }],
        // `mezame init` writes a single agent; multi-agent configs are
        // assembled by hand-editing config.json (see the README
        // configuration reference). The name is derived from the
        // command so the new-session picker has a stable label.
        agents: vec![AgentConfig {
            name: legacy_agent_name(&agent_cmd),
            command: agent_cmd,
            args: agent_args,
            env: BTreeMap::new(),
        }],
        agent_cmd: None,
        agent_args: Vec::new(),
    };

    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&cfg)?)?;
    println!("Wrote {}", path.display());
    println!();
    Ok(cfg)
}

/// Known ACP agent CLI we probe for on `$PATH`. Entries here show up as a
/// selectable menu in `mezame init` when the binary is present. Extending
/// the list is a two-line change.
struct KnownAgent {
    /// Human-readable label shown in the init menu.
    display: &'static str,
    /// Binary name resolved against `$PATH`.
    bin: &'static str,
    /// Args we pre-fill when the user picks this agent. Only set it when
    /// we are sure the subcommand is correct (Kiro CLI uses `acp`; the
    /// others currently hedge, so leave empty).
    default_args: &'static [&'static str],
}

const KNOWN_AGENTS: &[KnownAgent] = &[
    KnownAgent {
        display: "Kiro CLI",
        bin: "kiro-cli",
        default_args: &["acp"],
    },
    // Claude Code does not speak ACP itself; it is driven through the
    // Claude Code ACP bridge (npm `@agentclientprotocol/claude-agent-acp`,
    // bin `claude-agent-acp`). Install it globally to surface it here, or
    // run it via `npx` by typing the command at the "Other" prompt.
    KnownAgent {
        display: "Claude Code (ACP bridge)",
        bin: "claude-agent-acp",
        default_args: &[],
    },
    KnownAgent {
        display: "Gemini CLI",
        bin: "gemini",
        default_args: &[],
    },
    KnownAgent {
        display: "Codex",
        bin: "codex",
        default_args: &[],
    },
];

/// Resolved agent picked from the menu. The path carries the full
/// `$PATH`-resolved location so the saved config is not re-resolving the
/// binary at run time (handy when the user has multiple installs).
struct PickedAgent {
    path: PathBuf,
    default_args: Vec<String>,
}

/// Offer known agents found on `$PATH` as a `Select`. Returns `Ok(None)`
/// when the user chose "Other" or when none were discovered; the caller
/// falls back to a free-form `Input`.
fn pick_agent(theme: &ColorfulTheme) -> Result<Option<PickedAgent>> {
    let mut found: Vec<(&KnownAgent, PathBuf)> = Vec::new();
    for agent in KNOWN_AGENTS {
        if let Some(path) = which(agent.bin) {
            found.push((agent, path));
        }
    }

    if found.is_empty() {
        return Ok(None);
    }

    let mut items: Vec<String> = found
        .iter()
        .map(|(a, path)| format!("{} ({})", a.display, path.display()))
        .collect();
    items.push("Other (type a command)".to_string());

    let idx = Select::with_theme(theme)
        .with_prompt("ACP agent")
        .items(&items)
        .default(0)
        .interact()?;

    if idx == found.len() {
        return Ok(None);
    }

    let (agent, path) = &found[idx];
    Ok(Some(PickedAgent {
        path: path.clone(),
        default_args: agent.default_args.iter().map(|s| s.to_string()).collect(),
    }))
}

/// Tiny `$PATH` lookup; mirrors the helper in `build.rs`. Avoids pulling
/// in a `which` dep just for this one call.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
