//! Mezame: an ACP client that bridges a local agent to a browser UI.
//!
//! One WebSocket connection = one agent subprocess = one ACP session.
//! The agent is killed when the browser disconnects (`kill_on_drop(true)`).
//!
//! See the README for architecture, wire protocol, transports, and
//! extension points. In-code extension points are marked with `TODO:`.
//!
//! Layout:
//!   - `config`  — on-disk config and interactive setup
//!   - `agent`   — ACP subprocess wrapper and JSON-RPC framing
//!   - `session` — session resume and stale-lock recovery
//!   - `http`    — cloudflared transport (HTTP/WS server, UI assets, /state, /history)
//!   - `ws`      — per-WebSocket session loop and agent-message dispatch
//!
//! The crate is exposed as a library so integration tests in `tests/` can
//! import internals. The thin binary in `src/main.rs` calls `run()`.

pub mod agent;
pub mod config;
pub mod http;
pub mod session;
pub mod ws;

#[cfg(unix)]
pub mod unix;

use anyhow::{bail, Result};

use crate::config::{config_path, init_config, load_config, TransportConfig};
use crate::http::run_cloudflared;

/// Top-level CLI entry point. Synchronous because `init_config` reads
/// stdin and we do not want a tokio runtime blocking a thread on that.
/// The runtime is built only once we know which transport to run.
pub fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).map(String::as_str);

    match sub {
        Some("--version") | Some("-V") => {
            println!("mezame {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help") | Some("-h") => {
            print_help();
            return Ok(());
        }
        Some("init") => {
            init_config()?;
            return Ok(());
        }
        _ => {}
    }

    let cfg = if config_path()?.exists() {
        load_config()?
    } else {
        eprintln!("No config at {}", config_path()?.display());
        eprintln!("Let's set one up:");
        init_config()?
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        // Single-transport runtime for now: pick the first entry, bail on
        // empty or multi-entry configs. When multi-transport lands
        // (todo #19), iterate the list and spawn one task per entry.
        match cfg.transports.as_slice() {
            [] => bail!("No transports configured. Re-run `mezame init`."),
            [one] => match one.clone() {
                TransportConfig::Cloudflared { bind } => run_cloudflared(cfg, bind).await,
            },
            _ => bail!(
                "Running more than one transport at once is not yet supported. \
                 Leave a single entry in `transports` until multi-transport ships."
            ),
        }
    })
}

fn print_help() {
    println!(
        "mezame {version} -- ACP client that bridges a local agent to a browser UI

USAGE:
    mezame [SUBCOMMAND]

SUBCOMMANDS:
    init        Run interactive setup and write ~/.mezame/config.json
    (none)      Load the saved config and start serving

FLAGS:
    -h, --help      Print this message
    -V, --version   Print the version and exit

ENVIRONMENT:
    MEZAME_DEBUG_ACP=1     Echo every inbound ACP frame to stderr
    MEZAME_SKIP_UI_BUILD=1 Skip the Vite build (developer use only)
",
        version = env!("CARGO_PKG_VERSION")
    );
}
