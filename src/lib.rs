//! Mezame: an agent harness with a browser front end.
//!
//! A browser opens a session over a WebSocket, sends a prompt, and reads
//! the turn as it streams. Several browsers can attach to one session at
//! once and see the same conversation; a session outlives a reconnect.
//! What produces a turn sits behind one trait, `backend::Backend`. With a
//! `bedrock` section in the configuration a session runs `turn::LoopBackend`
//! over Amazon Bedrock; without one it runs `backend::EchoBackend`, which
//! returns each prompt's text as the reply and reaches no provider.
//!
//! See the README for architecture, the wire protocol and transports.
//! In-code extension points are marked with `TODO:`.
//!
//! Layout:
//!   - `backend`: the Backend seam, the transcript types, the `EchoBackend`
//!     a session gets with no provider configured
//!   - `config`:  on-disk settings and interactive setup
//!   - `conversation`: the canonical content blocks, the wire-to-block
//!     mapping, and the conversation store coupled to the transcript
//!   - `guard`:   the `Host` allowlist and the `Origin` check, the one
//!     layer in front of every route
//!   - `http`:    cloudflared transport (HTTP/WS server, UI assets,
//!     `/state`, `/history`)
//!   - `hub`:     the per-session hub, its registry and its owner loop
//!   - `prompt`:  the system prompt assembly, date last
//!   - `provider`: the `TurnEvent` vocabulary, the `Provider` trait and
//!     the Bedrock implementation
//!   - `turn`:    the turn loop, a Backend over a Provider
//!   - `ws`:      the upgrade, the per-attach loop and the client command
//!     set
//!   - `unix`:    the three libc calls this crate needs, on Unix only
//!
//! The crate is exposed as a library so integration tests in `tests/` can
//! import internals. The thin binary in `src/main.rs` calls `run()`.

pub mod backend;
pub mod config;
pub mod conversation;
pub mod guard;
pub mod http;
pub mod hub;
pub mod prompt;
pub mod provider;
pub mod turn;
pub mod ws;

#[cfg(unix)]
pub mod unix;

use anyhow::{bail, Context, Result};

use std::sync::Arc;

use crate::config::{
    config_path, init_config, init_config_with_args, load_config, parse_init_args, BedrockConfig,
    TransportConfig,
};
use crate::http::run_cloudflared;
use crate::hub::{HubRegistry, NewBackend};
use crate::provider::bedrock::{build_client, BedrockProvider};
use crate::provider::Provider;
use crate::turn::LoopBackend;

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
            let init_args = parse_init_args(&args[2..])?;
            if init_args.is_empty() {
                init_config()?;
            } else {
                init_config_with_args(&init_args)?;
            }
            return Ok(());
        }
        _ => {}
    }

    let path = config_path()?;
    let cfg = if path.exists() {
        load_config()?
    } else {
        eprintln!("No config at {}", path.display());
        eprintln!("Let's set one up:");
        // The context stays true whatever stopped the setup: no terminal
        // to ask on under a service manager, an interrupted prompt, or a
        // directory that could not be written. Under `Restart=on-failure`
        // this line is what the operator reads in the log.
        init_config().with_context(|| {
            format!(
                "Setup did not complete. In a terminal, run `mezame init`; without one, \
                 `mezame init --bind ADDR [--model ID]` writes {} with no prompt",
                path.display()
            )
        })?
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        // Single-transport runtime for now: pick the first entry, bail on
        // empty or multi-entry configs. When multi-transport lands
        // (todo #19), iterate the list and spawn one task per entry. The
        // registry is built inside the served arm, so a config error is
        // reported before the SDK's region lookup runs.
        match cfg.transports.as_slice() {
            [] => bail!("No transports configured. Re-run `mezame init`."),
            [one] => match one.clone() {
                TransportConfig::Cloudflared { bind, .. } => {
                    let hubs = build_registry(cfg.bedrock.as_ref(), &path).await;
                    run_cloudflared(cfg, bind, hubs).await
                }
            },
            _ => bail!(
                "Running more than one transport at once is not yet supported. \
                 Leave a single entry in `transports` until multi-transport ships."
            ),
        }
    })
}

/// The registry every session is built from: hubs over Bedrock when the
/// configuration names a model, over the echo otherwise. One line on
/// stderr says which.
///
/// Building the client reads files and the environment and makes no
/// request for credentials; those resolve on the first turn, so a machine
/// with no AWS setup still serves the browser and reports the problem
/// there. With no region anywhere the SDK's chain ends at the instance
/// metadata service, which costs about a second here outside EC2.
async fn build_registry(bedrock: Option<&BedrockConfig>, path: &std::path::Path) -> HubRegistry {
    let Some(section) = bedrock else {
        eprintln!("Backend: echo (no `bedrock` section in {})", path.display());
        return HubRegistry::new();
    };
    let client = build_client(section.region.as_deref(), section.profile.as_deref()).await;
    eprintln!(
        "Backend: Bedrock {} (region: {}, profile: {})",
        section.model,
        section.region.as_deref().unwrap_or("AWS default"),
        section.profile.as_deref().unwrap_or("default chain")
    );
    let provider: Arc<dyn Provider> = Arc::new(BedrockProvider::new(client));
    let settings = section.settings();
    HubRegistry::with_factory(Arc::new(move |session_id| {
        let backend = LoopBackend::new(Arc::clone(&provider), settings.clone(), session_id);
        NewBackend {
            session_info: Some(backend.session_info()),
            backend: Arc::new(backend),
        }
    }))
}

fn print_help() {
    println!(
        "mezame {version}: an agent harness with a browser front end

USAGE:
    mezame [SUBCOMMAND]

SUBCOMMANDS:
    init                 Run interactive setup and write ~/.mezame/config.json
    init [FLAGS]         Write ~/.mezame/config.json from the flags with no prompt
    (none)               Load the saved config and start serving

INIT FLAGS (any combination; a key not given keeps the existing value):
    --bind ADDR          The address to serve on (default 127.0.0.1:9510)
    --model ID           The Bedrock model id; without one, prompts are echoed
    --region NAME        The AWS region, else the AWS default chain decides
    --profile NAME       The AWS profile, else the default credential chain

FLAGS:
    -h, --help      Print this message
    -V, --version   Print the version and exit

ENVIRONMENT:
    HOME                   Resolves ~/.mezame/config.json and state.json
    MEZAME_SKIP_UI_BUILD=1 Skip the Vite build (developer use only)
",
        version = env!("CARGO_PKG_VERSION")
    );
}
