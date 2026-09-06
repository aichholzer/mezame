//! On-disk configuration and interactive setup.
//!
//! Config lives at `~/.mezame/config.json` and holds server settings
//! only. A schema change breaks existing users: add fields with
//! `#[serde(default)]` and leave the existing ones where they are.
//! Transports live in a list (`TransportConfig`) internally tagged on
//! `kind`; see the architecture document's configuration reference.

use std::path::PathBuf;

use anyhow::{Context, Result};
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

/// Server settings, as they sit at `~/.mezame/config.json`.
///
/// One field. No `deny_unknown_fields`: a file written by 0.13.x carries
/// keys this version knows nothing about, and they are ignored, the file
/// is left on disk untouched, and the parsed bind address is served. No
/// re-run of `mezame init` is needed to move onto this line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub transports: Vec<TransportConfig>,
}

/// Transport entries are internally tagged by `kind`. Each variant holds
/// its own config with no separate top-level section. A new transport
/// takes three steps: add a variant here, add an arm in `main`, implement
/// its `run_*` entry point.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TransportConfig {
    Cloudflared { bind: String },
    // Telegram { token: String }: commented out until `run_telegram`
    // ships. An enabled variant would have to round-trip through the
    // config, advertising a transport that does nothing.
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

pub fn load_config() -> Result<Config> {
    let path = config_path()?;
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("Reading {}", path.display()))?;
    let cfg: Config = serde_json::from_str(&raw).context("Parsing config.json")?;
    Ok(cfg)
}

pub(crate) fn init_config() -> Result<Config> {
    // Transport prompt commented out while Cloudflared is the only
    // implemented option. When Telegram ships, rewrite this to build the
    // `transports` list interactively: ask for Cloudflared, offer to add
    // another, loop. The single-choice block below is a record of what was
    // there.
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
                .validate_with(|input: &String| validate_bind_entry(input))
                .interact_text()?;
            s.trim().to_string()
        }
    };

    let cfg = Config {
        transports: vec![TransportConfig::Cloudflared { bind }],
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

/// The check the free-form bind entry is held to.
///
/// A pure function so it has a test. `init_config`'s interactive body
/// cannot be driven from one: `dialoguer` refuses a non-terminal, and the
/// re-prompt it performs on a rejected entry is verified by hand.
pub fn validate_bind_entry(input: &str) -> Result<(), &'static str> {
    if input.trim().is_empty() {
        Err("Bind address is required")
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::validate_bind_entry;

    #[test]
    fn an_empty_or_whitespace_entry_is_rejected() {
        for refused in ["", " ", "   ", "\t", "\n", " \t \n "] {
            assert!(
                validate_bind_entry(refused).is_err(),
                "should reject {refused:?}"
            );
        }
    }

    #[test]
    fn any_non_empty_entry_is_accepted() {
        // The entry is not parsed here. A bind address that does not
        // resolve fails at `TcpListener::bind` with the operating
        // system's own message, which says more than a guess made here
        // would.
        for accepted in [
            "127.0.0.1:9510",
            "0.0.0.0:9510",
            "[::1]:9510",
            "localhost:9510",
            " 127.0.0.1:9510 ",
            "nonsense",
        ] {
            assert!(
                validate_bind_entry(accepted).is_ok(),
                "should accept {accepted:?}"
            );
        }
    }
}
