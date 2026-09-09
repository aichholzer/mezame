//! Mezame: an agent harness with a browser front end.
//!
//! A browser opens a session over a WebSocket, sends a prompt, and reads
//! the turn as it streams. Several browsers can attach to one session at
//! once and see the same conversation; a session outlives a reconnect.
//! What produces a turn sits behind one trait, `backend::Backend`. With a
//! Bedrock profile in the datastore a session runs `turn::LoopBackend`
//! over Amazon Bedrock; without one it runs `backend::EchoBackend`, which
//! returns each prompt's text as the reply and reaches no provider.
//!
//! See the README for architecture, the wire protocol and transports.
//! In-code extension points are marked with `TODO:`.
//!
//! Layout:
//!   - `backend`: the Backend seam, the transcript types, the `EchoBackend`
//!     a session gets with no provider configured
//!   - `auth`:    passwords, the session cookie and the login limiter
//!   - `config`:  the on-disk settings file and the paths under `~/.mezame`
//!   - `init`:    `mezame init`, the first-start bootstrap and the user
//!     commands
//!   - `history`: the transcript entries a stored message row stands for
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
//!   - `store`:   the `Store` trait, its SQLite implementation on its own
//!     thread, the migrations and the master key
//!   - `turn`:    the turn loop, a Backend over a Provider
//!   - `ws`:      the upgrade, the per-attach loop and the client command
//!     set
//!   - `unix`:    the three libc calls this crate needs, on Unix only
//!
//! The crate is exposed as a library so integration tests in `tests/` can
//! import internals. The thin binary in `src/main.rs` calls `run()`.

pub mod auth;
pub mod init;

pub mod backend;
pub mod config;
pub mod conversation;
pub mod guard;
pub mod history;
pub mod http;
pub mod hub;
pub mod prompt;
pub mod provider;
pub mod store;
pub mod turn;
pub mod ws;

#[cfg(unix)]
pub mod unix;

use anyhow::{anyhow, bail, Context, Result};

use std::sync::Arc;

use crate::backend::{TRANSCRIPT_BUDGET_BYTES, TRANSCRIPT_MAX_ENTRIES};
use crate::config::{config_path, load_config, Config, TransportConfig};
use crate::http::run_cloudflared;
use crate::hub::{BackendFactory, HubRegistry, NewBackend};
use crate::init::{
    parse_init_args, parse_user_add_args, InitArgs, BEDROCK_LABEL, BEDROCK_PROVIDER,
};
use crate::provider::bedrock::{build_client, BedrockProvider};
use crate::provider::{LoopSettings, Provider, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_THINKING_BUDGET};
use crate::store::{MessageWindow, ProfileRow, Store};
use crate::turn::LoopBackend;

/// Top-level CLI entry point. Synchronous because `init` and the user
/// commands read standard input and we do not want a tokio runtime
/// blocking a thread on that. The runtime is built only once we know
/// which transport to run.
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
            if init_args.is_empty() && !init::has_terminal() {
                bail!(
                    "`mezame init` asks its questions on a terminal and none is attached. Without \
                     one, pass the flags: `mezame init --bind ADDR --admin NAME --password-stdin \
                     [--model ID] [--region NAME] [--profile NAME]`, the password on the first line \
                     of standard input."
                );
            }
            return init::run(&init_args);
        }
        Some("user") => {
            return match args.get(2).map(String::as_str) {
                Some("add") => init::user_add(&parse_user_add_args(&args[3..])?),
                Some("list") if args.len() == 3 => init::user_list(),
                Some("list") => bail!("`mezame user list` takes no argument"),
                _ => bail!("`mezame user` takes `add NAME [--admin] [--password-stdin]` or `list`"),
            };
        }
        Some("passwd") => {
            let mut name = None;
            let mut password_stdin = false;
            for token in &args[2..] {
                match token.as_str() {
                    "--password-stdin" => password_stdin = true,
                    other if other.starts_with('-') => {
                        bail!("Unknown argument `{other}`. `mezame passwd NAME [--password-stdin]`")
                    }
                    other if name.is_some() => {
                        bail!("Unexpected argument `{other}`: one name at a time")
                    }
                    other => name = Some(other.to_string()),
                }
            }
            let Some(name) = name else {
                bail!("`mezame passwd` needs a name: `mezame passwd NAME [--password-stdin]`");
            };
            return init::passwd(&name, password_stdin);
        }
        Some(other) => bail!("Unknown command `{other}`. `mezame --help` lists the commands."),
        None => {}
    }

    let path = config_path()?;
    if !path.exists() {
        eprintln!("No config at {}", path.display());
        if !init::has_terminal() {
            // Under a service manager or `docker compose up -d` the prompt
            // cannot be answered; the log says what to run instead.
            bail!(
                "Setup has not run. In a terminal, run `mezame init`; without one, `mezame init \
                 --bind ADDR --admin NAME --password-stdin [--model ID]` writes {} with no prompt",
                path.display()
            );
        }
        eprintln!("Let's set one up:");
        init::run(&InitArgs::default()).with_context(|| {
            format!(
                "Setup did not complete. In a terminal, run `mezame init`; without one, `mezame \
                 init --bind ADDR --admin NAME --password-stdin [--model ID]` writes {} with no \
                 prompt",
                path.display()
            )
        })?;
    }
    let cfg = load_config()?;

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
                    // The key beside the datastore, then the datastore;
                    // then a user, asked for when there is none and a
                    // terminal to ask on; then the Bedrock rows, read once.
                    let (store, keys) = init::open_store()?;
                    if store.count_users().await.map_err(|e| anyhow!("{e}"))? == 0 {
                        if init::has_terminal() {
                            init::bootstrap(store.as_ref()).await?;
                        } else {
                            bail!("{}", init::NO_USER_NO_TERMINAL);
                        }
                    }
                    let backend = load_backend(&cfg, store.as_ref()).await?;
                    let hubs = build_registry(backend, Arc::clone(&store)).await;
                    let users = store.count_users().await.map_err(|e| anyhow!("{e}"))?;
                    eprintln!(
                        "Datastore: {} {} ({users} user{})",
                        store.backend_name(),
                        crate::config::datastore_path()?.display(),
                        if users == 1 { "" } else { "s" }
                    );
                    let workspace_root = workspace_root();
                    run_cloudflared(cfg, bind, hubs, store, keys, workspace_root).await
                }
            },
            _ => bail!(
                "Running more than one transport at once is not yet supported. \
                 Leave a single entry in `transports` until multi-transport ships."
            ),
        }
    })
}

/// The working directory as the root of a user's default workspace, when
/// it can be one. Decided once per start and said once: the line names the
/// root, or the reason there is none.
fn workspace_root() -> Option<std::path::PathBuf> {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            eprintln!("Workspace: none (the working directory could not be read: {e})");
            return None;
        }
    };
    let dir = crate::config::mezame_dir().ok()?;
    match crate::config::eligible_workspace_root(&cwd, &dir) {
        Ok(root) => {
            eprintln!("Workspace: {}", root.display());
            Some(root)
        }
        Err(why) => {
            eprintln!("Workspace: none ({why})");
            None
        }
    }
}

/// The Bedrock settings the datastore holds: the global profile as the
/// loop's settings, and its credential's region and AWS profile for the
/// client. `None` selects the echo.
pub struct BackendSpec {
    pub settings: LoopSettings,
    pub region: Option<String>,
    pub profile: Option<String>,
}

/// The global profile and its credential, read once at startup. A
/// credential this key cannot open stops the start: falling back to the
/// echo would hide a datastore that no longer matches its key.
async fn load_backend(cfg: &Config, store: &dyn Store) -> Result<Option<BackendSpec>> {
    let Some(profile) = store.global_profile().await.map_err(|e| anyhow!("{e}"))? else {
        return Ok(None);
    };
    let (region, aws_profile) = match &profile.credential_id {
        Some(id) => {
            let label = store
                .credentials(None, BEDROCK_PROVIDER)
                .await
                .map_err(|e| anyhow!("{e}"))?
                .into_iter()
                .find(|row| &row.id == id)
                .map_or_else(|| BEDROCK_LABEL.to_string(), |row| row.label);
            let payload = store.credential_payload(id).await.map_err(|e| {
                anyhow!(
                    "The Bedrock credential `{label}` could not be opened: {e}. Run `mezame init \
                     --model {} --region R --profile P` to replace it.",
                    profile.model
                )
            })?;
            (
                payload["region"].as_str().map(str::to_string),
                payload["profile"].as_str().map(str::to_string),
            )
        }
        None => (None, None),
    };
    Ok(Some(BackendSpec {
        settings: settings_from_profile(&profile, &cfg.models),
        region,
        profile: aws_profile,
    }))
}

/// The loop's settings from the global profile row and the configuration's
/// model list: the profile's model first, then the list without
/// duplicates; a null column takes the phase 1 default; a `thinking` value
/// the enum does not spell reads as unset.
pub fn settings_from_profile(profile: &ProfileRow, catalogue: &[String]) -> LoopSettings {
    let mut models = vec![profile.model.clone()];
    for id in catalogue {
        if !models.contains(id) {
            models.push(id.clone());
        }
    }
    LoopSettings {
        model: profile.model.clone(),
        models,
        thinking: profile.thinking.as_deref().and_then(|t| t.parse().ok()),
        thinking_budget: profile.thinking_budget.unwrap_or(DEFAULT_THINKING_BUDGET),
        max_output_tokens: profile
            .max_output_tokens
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
    }
}

/// The registry every session is built from: hubs over Bedrock when the
/// datastore holds a profile, over the echo otherwise. One line on stderr
/// says which, naming the model and nothing of the credential.
///
/// Building the client reads files and the environment and makes no
/// request for credentials; those resolve on the first turn, so a machine
/// with no AWS setup still serves the browser and reports the problem
/// there. With no region anywhere the SDK's chain ends at the instance
/// metadata service, which costs about a second here outside EC2.
///
/// A Bedrock hub is built in two phases: the session's stored rows are
/// loaded first, with no registry lock held, under the same bounds the
/// loop keeps in memory with headroom for the stored encoding; then the
/// loop is built from them and writes every later turn to `store`.
async fn build_registry(backend: Option<BackendSpec>, store: Arc<dyn Store>) -> HubRegistry {
    let Some(spec) = backend else {
        eprintln!(
            "Backend: echo (no Bedrock profile in the datastore; `mezame init --model ID` sets one)"
        );
        return HubRegistry::new();
    };
    let client = build_client(spec.region.as_deref(), spec.profile.as_deref()).await;
    eprintln!("Backend: Bedrock {}", spec.settings.model);
    let provider: Arc<dyn Provider> = Arc::new(BedrockProvider::new(client));
    HubRegistry::with_factory(persistent_factory(provider, spec.settings, store))
}

/// The factory of a persisting deployment: `prepare` loads the session's
/// window, `build` rebuilds a [`LoopBackend`] from it for the owner.
pub fn persistent_factory(
    provider: Arc<dyn Provider>,
    settings: LoopSettings,
    store: Arc<dyn Store>,
) -> BackendFactory {
    let loader = Arc::clone(&store);
    BackendFactory::persistent(
        move |session_id| {
            let store = Arc::clone(&loader);
            let session_id = session_id.to_string();
            Box::pin(async move {
                match store
                    .load_window(
                        &session_id,
                        TRANSCRIPT_MAX_ENTRIES,
                        2 * TRANSCRIPT_BUDGET_BYTES,
                    )
                    .await
                {
                    Ok(window) => window,
                    Err(e) => {
                        crate::hub::warn(&format!(
                            "Session {session_id}: the stored conversation could not be loaded \
                             ({e}); the session starts empty"
                        ));
                        MessageWindow::default()
                    }
                }
            })
        },
        move |session_id, window, owner| {
            let mut backend = LoopBackend::new(
                Arc::clone(&provider),
                settings.clone(),
                session_id,
                &owner.user_name,
                Some(Arc::clone(&store)),
            );
            backend.restore(window);
            Ok(NewBackend {
                session_info: Some(backend.session_info()),
                backend: Arc::new(backend),
            })
        },
    )
}

fn print_help() {
    println!(
        "mezame {version}: an agent harness with a browser front end

USAGE:
    mezame [SUBCOMMAND]

SUBCOMMANDS:
    init                 Run the setup in a terminal: bind address, admin account, Bedrock model
    init [FLAGS]         Set up with no prompt; a setting no flag names keeps its current value
    user add NAME [--admin] [--password-stdin]
                         Add a user, asking for the password twice unless it comes on standard input
    user list            List the users: name, role, creation date
    passwd NAME [--password-stdin]
                         Set a user's password and sign out every device of theirs
    (none)               Load the saved config and start serving

INIT FLAGS (any combination):
    --bind ADDR          The address to serve on (default 127.0.0.1:9510)
    --admin NAME         The admin to create, when the datastore holds no user yet
    --password-stdin     Read the password from the first line of standard input
    --model ID           The Bedrock model id; without one, prompts are echoed
    --region NAME        The AWS region, else the AWS default chain decides
    --profile NAME       The AWS profile, else the default credential chain

FLAGS:
    -h, --help      Print this message
    -V, --version   Print the version and exit

FILES (under ~/.mezame, created owner-only):
    config.json     The server settings: version, transports, datastore, public_url, models
    mezame.db       The datastore: users, sessions and their messages, the Bedrock profile
                    and its credential (encrypted)
    master.key      32 random bytes the credential and cookie keys derive from; the
                    datastore cannot be opened without it, so back the two up together

ENVIRONMENT:
    HOME                   Resolves ~/.mezame
    MEZAME_SKIP_UI_BUILD=1 Skip the Vite build (developer use only)
",
        version = env!("CARGO_PKG_VERSION")
    );
}
