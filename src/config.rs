//! On-disk configuration and interactive setup.
//!
//! Config lives at `~/.mezame/config.json`. Schema changes are breaking for
//! existing users, so add fields with `#[serde(default)]` rather than
//! reshuffling. Transports live in a list (`TransportConfig`) internally
//! tagged on `kind`; see the README Configuration reference and todo #19.

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

pub(crate) const DEFAULT_PORT: u16 = 9510;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Config {
    pub(crate) transports: Vec<TransportConfig>,
    pub(crate) agent_cmd: String,
    #[serde(default)]
    pub(crate) agent_args: Vec<String>,
}

/// Transport entries are internally tagged by `kind`, so each variant can
/// carry its own config without a separate top-level section. Adding a new
/// transport is: add a variant here, add an arm in `main`, implement its
/// `run_*` entry point.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum TransportConfig {
    Cloudflared { bind: String },
    // Telegram { token: String } — commented out until `run_telegram`
    // ships. Leaving the variant here would require it to round-trip, and
    // we do not want to pretend it works.
}

pub(crate) fn config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".mezame/config.json"))
}

/// Path to the persistent browser state (currently-open tabs, history list,
/// active id, next numeric label). Server-side so any device hitting Mezame
/// sees the same list.
pub(crate) fn state_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".mezame/state.json"))
}

pub(crate) fn load_config() -> Result<Config> {
    let path = config_path()?;
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("Reading {}", path.display()))?;
    let cfg: Config = serde_json::from_str(&raw).context("Parsing config.json")?;
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
        agent_cmd,
        agent_args,
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
    KnownAgent {
        display: "Claude Agent CLI",
        bin: "claude",
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
