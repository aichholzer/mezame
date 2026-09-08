//! On-disk configuration and interactive setup.
//!
//! Config lives at `~/.mezame/config.json` and holds server settings
//! only. The file carries `"version": 2`; a file of another or no version
//! is refused at startup with one line pointing at `mezame init`, which
//! rewrites it. Within a version, add fields with `#[serde(default)]` and
//! leave the existing ones where they are. Transports live in a list
//! (`TransportConfig`) internally tagged on `kind`; see the architecture
//! document's configuration reference.
//!
//! Everything under `~/.mezame` is created owner-only on Unix: the
//! directory `0700` and its files `0600`, each file written to a fresh
//! `O_EXCL` sibling and renamed into place, so a symlink at the target is
//! replaced rather than followed and a reader never sees a partial file.
//! An existing directory keeps its mode. The state endpoint writes through
//! the same two helpers.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Input, Select};
use serde::{Deserialize, Serialize};

use crate::provider::bedrock::{thinking_rule, vendor};
use crate::provider::{LoopSettings, ThinkingMode};

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

/// The bind `init` writes when neither a flag nor an existing file gives
/// one: loopback on the default port.
pub fn default_bind() -> String {
    format!("127.0.0.1:{DEFAULT_PORT}")
}

/// The budget an `enabled` thinking request sends when the file sets none.
pub const DEFAULT_THINKING_BUDGET: u32 = 4096;
/// The smallest budget the provider accepts.
pub const MIN_THINKING_BUDGET: u32 = 1024;
/// The output ceiling of a request when the file sets none.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16_384;

/// The version of the file this release reads and writes.
pub const CONFIG_VERSION: u32 = 2;

/// The one datastore backend of this release.
pub const SQLITE_BACKEND: &str = "sqlite";

/// The `datastore` object of `config.json`: which backend holds the
/// persistent state. `sqlite` is the one value this release accepts; a
/// second backend is a second value and a second `Store` implementation,
/// not a schema change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatastoreConfig {
    #[serde(default = "default_backend")]
    pub backend: String,
}

fn default_backend() -> String {
    SQLITE_BACKEND.to_string()
}

impl Default for DatastoreConfig {
    fn default() -> Self {
        Self {
            backend: default_backend(),
        }
    }
}

/// Server settings, as they sit at `~/.mezame/config.json`.
///
/// No `deny_unknown_fields`: a file written by a later release of the same
/// version carries keys this binary knows nothing about, and they are
/// ignored, the file is left on disk untouched, and the parsed settings
/// are served. A file without a `bedrock` section loads exactly as before
/// the section existed and selects the echo backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Always [`CONFIG_VERSION`] once loaded: the loader checks it before
    /// anything else is read.
    pub version: u32,
    pub transports: Vec<TransportConfig>,
    #[serde(default)]
    pub datastore: DatastoreConfig,
    /// The URL browsers reach Mezame at, when a tunnel or proxy fronts it.
    /// An `https://` value marks the session cookie `Secure`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// The model ids the picker offers besides the profile's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// The Bedrock model and, when the AWS setup needs them, the region and
    /// profile. Absent means the echo backend. The section moves into the
    /// datastore once the profile row exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock: Option<BedrockConfig>,
}

impl Config {
    /// Refuse a file the process could not run on, naming the key, the
    /// accepted values and the file.
    pub fn validate(&self, path: &Path) -> Result<()> {
        let at = path.display();
        if self.datastore.backend != SQLITE_BACKEND {
            bail!(
                "`datastore.backend` `{}` is not supported; `{SQLITE_BACKEND}` is the one backend \
                 in this release, in {at}",
                self.datastore.backend
            );
        }
        if self.models.iter().any(|m| m.trim().is_empty()) {
            bail!("`models` holds an empty entry, in {at}");
        }
        if let Some(padded) = self.models.iter().find(|m| m.as_str() != m.trim()) {
            bail!("`models` entry `{padded}` has leading or trailing whitespace, in {at}");
        }
        if let Some(url) = &self.public_url {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                bail!("`public_url` must begin with `http://` or `https://`, in {at}");
            }
        }
        if let Some(bedrock) = &self.bedrock {
            bedrock.validate(path)?;
        }
        Ok(())
    }

    /// The bind of the first transport, when there is one.
    pub fn bind(&self) -> Option<&str> {
        self.transports.first().map(|t| match t {
            TransportConfig::Cloudflared { bind, .. } => bind.as_str(),
        })
    }

    /// Every `hosts` entry of every transport, in order: the same walk
    /// the request guard makes, so a re-run of `init` carries forward
    /// exactly what the guard was serving.
    pub fn hosts(&self) -> Vec<String> {
        self.transports
            .iter()
            .flat_map(|t| match t {
                TransportConfig::Cloudflared { hosts, .. } => hosts.iter().cloned(),
            })
            .collect()
    }
}

/// The `bedrock` object of `config.json`. `model` is required; every other
/// key is optional and left out of a written file when unset.
///
/// `model` is passed to the API unchanged: a base id, an inference profile
/// id under `global.`, `us.`, `eu.` or another geo prefix, or an ARN. The
/// forms differ by model generation and newer models refuse the base id,
/// so nothing here second-guesses it; the API's own error, relayed to the
/// browser, is the validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BedrockConfig {
    #[serde(default)]
    pub model: String,
    /// The ids the picker offers. `model` is always among them, first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// `adaptive`, `enabled` or `off`; unset derives the mode from the
    /// model id. Held as the string the file holds so a wrong value is
    /// refused by [`validate`](Self::validate) with the key, the accepted
    /// values and the file named; `ThinkingMode::from_str` is the one place
    /// the accepted set lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

impl BedrockConfig {
    /// A section naming `model` and nothing else.
    pub fn for_model(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            models: Vec::new(),
            region: None,
            profile: None,
            thinking: None,
            thinking_budget: None,
            max_output_tokens: None,
        }
    }

    /// Refuse a section the loop could not run, naming the key, the
    /// accepted values or range, and the file.
    pub fn validate(&self, path: &Path) -> Result<()> {
        let at = path.display();
        if self.model.trim().is_empty() {
            bail!("`bedrock.model` must name a model id, in {at}");
        }
        if self.model != self.model.trim() {
            bail!("`bedrock.model` has leading or trailing whitespace, in {at}");
        }
        if self.models.iter().any(|m| m.trim().is_empty()) {
            bail!("`bedrock.models` holds an empty entry, in {at}");
        }
        if let Some(padded) = self.models.iter().find(|m| m.as_str() != m.trim()) {
            bail!("`bedrock.models` entry `{padded}` has leading or trailing whitespace, in {at}");
        }
        // An empty region or profile is not the AWS default: the SDK takes
        // it literally and every request fails. Removing the key is the
        // way to the default.
        if self.region.as_deref().is_some_and(|r| r.trim().is_empty()) {
            bail!("`bedrock.region` is empty; remove the key to use the AWS default, in {at}");
        }
        if self.profile.as_deref().is_some_and(|p| p.trim().is_empty()) {
            bail!(
                "`bedrock.profile` is empty; remove the key to use the default credential chain, \
                 in {at}"
            );
        }
        let configured = match &self.thinking {
            Some(text) => Some(
                text.parse::<ThinkingMode>()
                    .map_err(|why| anyhow!("`bedrock.thinking` {why}, in {at}"))?,
            ),
            None => None,
        };
        // A thinking request is an Anthropic request field. Asking for one
        // on another vendor's model would fail every request instead of
        // failing here.
        if matches!(
            configured,
            Some(ThinkingMode::Adaptive | ThinkingMode::Enabled)
        ) {
            if let Some(other) = self
                .model_list()
                .into_iter()
                .find(|id| vendor(id).is_some_and(|v| v != "anthropic"))
            {
                bail!(
                    "`bedrock.thinking` `{}` needs an Anthropic model, and `{other}` names \
                     another vendor, in {at}",
                    configured.map_or("", ThinkingMode::as_str)
                );
            }
        }
        let max_output_tokens = self.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        if max_output_tokens < 1 {
            bail!("`bedrock.max_output_tokens` must be at least 1, in {at}");
        }
        // The budget is checked when a request would carry it: when it is
        // set, or when the mode that sends it is in force for any model the
        // picker offers. A user who sets a small `max_output_tokens` under
        // `adaptive` thinking is not told off about a budget nothing sends.
        let budget_in_force = self.thinking_budget.is_some()
            || match configured {
                Some(mode) => mode == ThinkingMode::Enabled,
                None => self
                    .model_list()
                    .iter()
                    .any(|id| thinking_rule(id) == ThinkingMode::Enabled),
            };
        let budget = self.thinking_budget.unwrap_or(DEFAULT_THINKING_BUDGET);
        if budget_in_force && (budget < MIN_THINKING_BUDGET || budget >= max_output_tokens) {
            bail!(
                "`bedrock.thinking_budget` must be at least {MIN_THINKING_BUDGET} and below \
                 `bedrock.max_output_tokens` ({max_output_tokens}), not {budget}, in {at}"
            );
        }
        Ok(())
    }

    /// The configured mode, or `None` to derive it per model. Only valid
    /// after [`validate`](Self::validate): a string the enum does not
    /// spell reads as unset.
    pub fn thinking_mode(&self) -> Option<ThinkingMode> {
        self.thinking.as_deref().and_then(|t| t.parse().ok())
    }

    /// The mode a request for `model` would carry.
    pub fn thinking_mode_or_rule(&self) -> ThinkingMode {
        self.thinking_mode()
            .unwrap_or_else(|| thinking_rule(&self.model))
    }

    /// `model` first, then the configured list in order, no duplicates.
    pub fn model_list(&self) -> Vec<String> {
        let mut list = vec![self.model.clone()];
        for id in &self.models {
            if !list.contains(id) {
                list.push(id.clone());
            }
        }
        list
    }

    /// What the loop is configured with, defaults applied.
    pub fn settings(&self) -> LoopSettings {
        LoopSettings {
            model: self.model.clone(),
            models: self.model_list(),
            thinking: self.thinking_mode(),
            thinking_budget: self.thinking_budget.unwrap_or(DEFAULT_THINKING_BUDGET),
            max_output_tokens: self.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        }
    }
}

/// Transport entries are internally tagged by `kind`. Each variant holds
/// its own config with no separate top-level section. A new transport
/// takes three steps: add a variant here, add an arm in `run`
/// (`src/lib.rs`), implement its `run_*` entry point.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TransportConfig {
    Cloudflared {
        bind: String,
        /// Hostnames this server answers to besides IP literals, `localhost`
        /// and `.local` names: the public hostname a tunnel or proxy in
        /// front of Mezame carries in `Host`, and the origin its pages
        /// present. A request naming any other hostname is answered 421;
        /// see `guard.rs`. A file written before this key existed reads as
        /// an empty list, and `mezame init` writes none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        hosts: Vec<String>,
    },
    // Telegram { token: String }: commented out until a Telegram
    // transport ships. An enabled variant would have to round-trip through the
    // config, advertising a transport that does nothing.
}

/// `~/.mezame`, the directory every file this module names sits in.
pub fn mezame_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".mezame"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(mezame_dir()?.join("config.json"))
}

/// The SQLite datastore.
pub fn datastore_path() -> Result<PathBuf> {
    Ok(mezame_dir()?.join("mezame.db"))
}

/// The master key the credential and cookie keys are derived from.
pub fn master_key_path() -> Result<PathBuf> {
    Ok(mezame_dir()?.join("master.key"))
}

/// Path to the persistent browser state (currently-open tabs, history list,
/// active id, next numeric label). Server-side so any device hitting Mezame
/// sees the same list.
pub fn state_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".mezame/state.json"))
}

pub fn load_config() -> Result<Config> {
    load_config_from(&config_path()?)
}

/// Read and validate the configuration at `path`. The version is checked
/// on the raw document before anything else is read, so a file another
/// release wrote is answered with one line pointing at `mezame init` and
/// not with a parse error; a `bedrock` section the loop could not run is
/// refused here too, so the process never starts on a file it would fail
/// on later.
pub fn load_config_from(path: &Path) -> Result<Config> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("Reading {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("Parsing config.json at {}", path.display()))?;
    check_version(&value, path)?;
    let cfg: Config = serde_json::from_value(value)
        .with_context(|| format!("Parsing config.json at {}", path.display()))?;
    cfg.validate(path)?;
    Ok(cfg)
}

/// The version a raw document declares, as text for a message: `none` when
/// the key is absent, the number when it is one, the JSON otherwise.
fn version_text(value: &serde_json::Value) -> String {
    match value.get("version") {
        None => "none".to_string(),
        Some(v) => v.to_string(),
    }
}

/// Refuse a document of another or no version with one line naming the
/// file, the version found, and the way out.
pub fn check_version(value: &serde_json::Value, path: &Path) -> Result<()> {
    let at = path.display();
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(v) if v == u64::from(CONFIG_VERSION) => Ok(()),
        _ => bail!(
            "{at} has version {} and this release ({}) reads version {CONFIG_VERSION}; run \
             `mezame init` to rewrite it (the hosts are kept)",
            version_text(value),
            env!("CARGO_PKG_VERSION")
        ),
    }
}

/// Read the configuration at `~/.mezame/config.json` without validating
/// it: what `init` starts from, so a file with one bad value is carried
/// forward and reported rather than treated as absent and dropped.
pub fn read_config() -> Result<Config> {
    read_config_from(&config_path()?)
}

/// Read the configuration at `path` without validating it.
pub fn read_config_from(path: &Path) -> Result<Config> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("Reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("Parsing config.json at {}", path.display()))
}

/// The configuration `init` starts from.
#[derive(Debug, Clone)]
pub struct ExistingConfig {
    pub config: Config,
    /// `Some(version text)` when the file was written by another release:
    /// only its `transports` (the hosts) were carried, and `init` says so.
    pub legacy: Option<String>,
}

/// What `init` starts from: the configuration on disk, `None` when there
/// is no file, and an error when there is a file that cannot be read or
/// parsed. A broken file is never taken for an absent one: `init` would
/// otherwise write a fresh file over it and drop the hosts and the
/// `bedrock` section it held, with nothing said. A file of another or no
/// version is carried as far as its `transports` parse, so the server's
/// pointer at `mezame init` is a working step and the hosts survive it.
pub fn read_existing_config() -> Result<Option<ExistingConfig>> {
    read_existing_config_from(&config_path()?)
}

/// [`read_existing_config`] at `path`.
pub fn read_existing_config_from(path: &Path) -> Result<Option<ExistingConfig>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Reading {}", path.display())),
    };
    let broken = || {
        format!(
            "{} exists but does not parse; fix it, or delete it and run `mezame init` again",
            path.display()
        )
    };
    let value: serde_json::Value = serde_json::from_str(&raw).with_context(broken)?;
    if check_version(&value, path).is_ok() {
        let config: Config = serde_json::from_value(value).with_context(broken)?;
        return Ok(Some(ExistingConfig {
            config,
            legacy: None,
        }));
    }
    let transports: Vec<TransportConfig> = value
        .get("transports")
        .cloned()
        .and_then(|t| serde_json::from_value(t).ok())
        .unwrap_or_default();
    Ok(Some(ExistingConfig {
        config: Config {
            version: CONFIG_VERSION,
            transports,
            datastore: DatastoreConfig::default(),
            public_url: None,
            models: Vec::new(),
            bedrock: None,
        },
        legacy: Some(version_text(&value)),
    }))
}

/// What `init` says about a file another release wrote, once.
fn note_legacy(existing: Option<&ExistingConfig>) -> Result<()> {
    if let Some(ExistingConfig {
        legacy: Some(version),
        ..
    }) = existing
    {
        println!(
            "Existing {} was written by an earlier release (version {version}); keeping its \
             hosts and dropping the rest.",
            config_path()?.display()
        );
    }
    Ok(())
}

/// Create `dir` and any missing parent, owner-only (`0700`) on Unix.
///
/// An existing directory is left as it is, mode included: a directory a
/// 0.13.x release created stays `0755` until its owner runs `chmod`. A
/// regular file at the path is an error, which is what keeps `PUT /state`
/// answering 500 there. `0700` because the directory will hold credential
/// material and transcripts, and nothing else on the machine needs to
/// read it; the umask only ever removes bits from it.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// A temporary sibling of `target` unique to one write:
/// `.{name}.{hex}.tmp`, with 64 bits of OS entropy in the hex.
///
/// Same directory, so the rename that follows stays on one filesystem;
/// the leading dot keeps a listing of `~/.mezame` clean. Two writes that
/// overlap never share a file, which the one fixed `.tmp` name they used
/// to share let happen: one writer renamed a file the other had just
/// truncated, and the loser's rename failed with a 500.
pub fn temp_sibling(target: &Path) -> io::Result<PathBuf> {
    let name = target.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "the target has no file name")
    })?;
    use std::fmt::Write as _;

    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    let hex = bytes.iter().fold(String::with_capacity(16), |mut s, b| {
        // Writing into a String cannot fail.
        let _ = write!(s, "{b:02x}");
        s
    });
    Ok(target.with_file_name(format!(".{name}.{hex}.tmp")))
}

/// Write `data` to `target` through a fresh owner-only (`0600`) sibling
/// opened `O_EXCL`, then rename it into place.
///
/// The target is never opened for writing, so a symlink planted there is
/// replaced by the rename, not followed, and a reader sees either the old
/// file or the whole new one. `durable` adds an `fsync` of the sibling
/// before the rename and, best effort, one of the directory after it, for
/// a file whose loss after a power cut would need `init` to run again: the
/// directory entry the rename creates is durable only once the directory
/// itself is synced. The state file skips both, since a torn or missing
/// state reads as `{}` and on Apple targets an `fsync` is a full device
/// flush. A failure leaves the target as it was and removes the sibling.
pub fn write_private_atomic(target: &Path, data: &[u8], durable: bool) -> io::Result<()> {
    let tmp = temp_sibling(target)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(data)?;
        if durable {
            file.sync_all()?;
        }
        drop(file);
        std::fs::rename(&tmp, target)?;
        if durable {
            // Best effort: the bytes are already on disk, and a filesystem
            // that refuses to sync a directory handle must not fail `init`.
            #[cfg(unix)]
            if let Some(parent) = target.parent() {
                let dir = if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                };
                let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
            }
        }
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// `mezame init` with no arguments: ask for the bind address and the
/// Bedrock settings, then write the config.
pub(crate) fn init_config() -> Result<Config> {
    let existing = read_existing_config()?;
    note_legacy(existing.as_ref())?;
    let existing = existing.map(|e| e.config);
    let bind = prompt_bind()?;
    let bedrock = prompt_bedrock(existing.as_ref().and_then(|c| c.bedrock.clone()))?;
    write_config(&assemble(existing.as_ref(), bind, bedrock), false)
}

/// One transport with `bind`, the hosts, datastore, public URL and model
/// list an existing file carried, and the Bedrock section: the shape both
/// `init` paths write, always at the current version.
fn assemble(existing: Option<&Config>, bind: String, bedrock: Option<BedrockConfig>) -> Config {
    Config {
        version: CONFIG_VERSION,
        transports: vec![TransportConfig::Cloudflared {
            bind,
            hosts: existing.map(Config::hosts).unwrap_or_default(),
        }],
        datastore: existing.map(|c| c.datastore.clone()).unwrap_or_default(),
        public_url: existing.and_then(|c| c.public_url.clone()),
        models: existing.map(|c| c.models.clone()).unwrap_or_default(),
        bedrock,
    }
}

/// What follows `init` on the command line, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InitArgs {
    pub bind: Option<String>,
    pub model: Option<String>,
    pub region: Option<String>,
    pub profile: Option<String>,
}

impl InitArgs {
    /// No flag at all: the interactive setup runs.
    pub fn is_empty(&self) -> bool {
        self.bind.is_none()
            && self.model.is_none()
            && self.region.is_none()
            && self.profile.is_none()
    }
}

/// The flags `init` takes, each with the example its error shows. One
/// table: the parser, the error text and the slot all read it.
const INIT_FLAGS: [(&str, &str); 4] = [
    ("--bind", "0.0.0.0:9510"),
    ("--model", "global.anthropic.claude-sonnet-5"),
    ("--region", "us-east-1"),
    ("--profile", "work"),
];

/// Parse what follows `init`: nothing, or any of `--bind ADDR`, `--model
/// ID`, `--region NAME` and `--profile NAME`, each in either of its two
/// spellings and each at most once.
///
/// Anything else is an error naming the token, so a typo is refused
/// instead of dropping into the prompt. Pure, so it has tests.
pub fn parse_init_args(args: &[String]) -> Result<InitArgs> {
    let mut parsed = InitArgs::default();
    let mut tokens = args.iter();
    while let Some(token) = tokens.next() {
        let Some((index, (flag, example))) = INIT_FLAGS
            .iter()
            .enumerate()
            .find(|(_, (flag, _))| token == *flag || token.starts_with(&format!("{flag}=")))
        else {
            bail!(
                "Unknown argument `{token}`. `mezame init` takes `--bind ADDR`, `--model ID`, \
                 `--region NAME` and `--profile NAME`, and nothing else."
            );
        };
        let value = if token == flag {
            match tokens.next() {
                Some(v) if !v.starts_with('-') => v.clone(),
                _ => bail!("`{flag}` needs a value, e.g. `mezame init {flag} {example}`"),
            }
        } else {
            token[flag.len() + 1..].to_string()
        };
        let slots = [
            &mut parsed.bind,
            &mut parsed.model,
            &mut parsed.region,
            &mut parsed.profile,
        ];
        let slot = slots
            .into_iter()
            .nth(index)
            .expect("one slot per flag in the table");
        if slot.is_some() {
            bail!("`{flag}` given twice");
        }
        *slot = Some(value);
    }
    Ok(parsed)
}

/// A flag's value with the whitespace trimmed, refused when nothing is
/// left. One rule for every flag: no flag clears a key. Removing a key is
/// a hand edit of the file, as it is for `hosts`.
fn non_empty(flag: &str, value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        let example = INIT_FLAGS
            .iter()
            .find(|(f, _)| *f == flag)
            .map_or("", |(_, example)| example);
        bail!("`{flag}` needs a value, e.g. `mezame init {flag} {example}`");
    }
    Ok(trimmed.to_string())
}

/// `mezame init` with flags: write the config with no prompt, for a
/// service unit or a container started before setup.
///
/// The bind comes from `--bind`, else from an existing file, else
/// [`default_bind`]; it is held to the same check as the free-form prompt
/// entry and otherwise not parsed. The Bedrock keys the flags set replace
/// those of an existing section and the rest is carried forward; a run
/// that sets none keeps the section whole and says so. `--region` or
/// `--profile` with no model anywhere is refused: there is nothing to
/// attach them to.
pub(crate) fn init_config_with_args(args: &InitArgs) -> Result<Config> {
    let existing = read_existing_config()?;
    note_legacy(existing.as_ref())?;
    let existing = existing.map(|e| e.config);
    let bind = match &args.bind {
        Some(addr) => {
            validate_bind_entry(addr).map_err(|message| anyhow!(message))?;
            addr.trim().to_string()
        }
        None => existing
            .as_ref()
            .and_then(|c| c.bind().map(str::to_string))
            .unwrap_or_else(default_bind),
    };
    let mut bedrock = existing.as_ref().and_then(|c| c.bedrock.clone());
    let touches_bedrock = args.model.is_some() || args.region.is_some() || args.profile.is_some();
    let kept = bedrock.is_some() && !touches_bedrock;
    if let Some(model) = &args.model {
        let model = non_empty("--model", model)?;
        match &mut bedrock {
            Some(section) => section.model = model,
            None => bedrock = Some(BedrockConfig::for_model(model)),
        }
    }
    if args.region.is_some() || args.profile.is_some() {
        let Some(section) = &mut bedrock else {
            bail!(
                "`--region` and `--profile` need a model: pass `--model ID` with them, or \
                 configure one first"
            );
        };
        if let Some(region) = &args.region {
            section.region = Some(non_empty("--region", region)?);
        }
        if let Some(profile) = &args.profile {
            section.profile = Some(non_empty("--profile", profile)?);
        }
    }
    write_config(&assemble(existing.as_ref(), bind, bedrock), kept)
}

/// The one prompt for the transport: the bind address, with the two common
/// choices and a free-form entry.
fn prompt_bind() -> Result<String> {
    // Transport prompt commented out while Cloudflared is the only
    // implemented option. When a Telegram transport ships, rewrite this to
    // build the `transports` list interactively: ask for Cloudflared, offer
    // to add another, loop. The single-choice block below is a record of
    // what was there.
    //
    // let transport_idx = Select::with_theme(&ColorfulTheme::default())
    //     .with_prompt("Which transport?")
    //     .items(&["Cloudflared  (serve a terminal-like web UI; front with your tunnel)",
    //              "Telegram     (long-poll a Telegram bot)  [not yet implemented]"])
    //     .default(0)
    //     .interact()?;

    let theme = ColorfulTheme::default();

    let loopback = default_bind();
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
    Ok(bind)
}

/// The Bedrock prompts, in one of two shapes each. With no existing
/// value, an empty answer is accepted and means "none": the echo backend
/// for the model, the AWS default for the region and the profile. With an
/// existing value, that value is the default and the Enter key keeps it;
/// `dialoguer` returns the default on an empty answer, so the two shapes
/// cannot be one prompt. Nothing here removes a section or clears a key,
/// a blank answer included: that is a hand edit of the file, as it is for
/// `hosts`.
fn prompt_bedrock(existing: Option<BedrockConfig>) -> Result<Option<BedrockConfig>> {
    let theme = ColorfulTheme::default();
    let model = match &existing {
        None => {
            let entered: String = Input::with_theme(&theme)
                .with_prompt("Bedrock model id (leave empty to keep the echo backend)")
                .allow_empty(true)
                .interact_text()?;
            let entered = entered.trim().to_string();
            if entered.is_empty() {
                return Ok(None);
            }
            entered
        }
        Some(section) => Input::with_theme(&theme)
            .with_prompt("Bedrock model id (Enter keeps the current one)")
            .default(section.model.clone())
            // The default applies to an empty line only; a line of spaces
            // reaches the validator, which sends it back.
            .validate_with(|input: &String| {
                if input.trim().is_empty() {
                    Err("A model id is required; Enter keeps the current one")
                } else {
                    Ok(())
                }
            })
            .interact_text()?
            .trim()
            .to_string(),
    };
    let optional =
        |label: &str, current: Option<&String>, default_text: &str| -> Result<Option<String>> {
            let entered: String = match current {
                None => Input::with_theme(&theme)
                    .with_prompt(format!("{label} (leave empty to use {default_text})"))
                    .allow_empty(true)
                    .interact_text()?,
                Some(value) => Input::with_theme(&theme)
                    .with_prompt(format!("{label} (Enter keeps the current one)"))
                    .default(value.clone())
                    .interact_text()?,
            };
            // A blank answer keeps what there was: nothing here clears a key.
            Ok(Some(entered.trim().to_string())
                .filter(|s| !s.is_empty())
                .or_else(|| current.cloned()))
        };
    let region = optional(
        "AWS region",
        existing.as_ref().and_then(|s| s.region.as_ref()),
        "the AWS default",
    )?;
    let profile = optional(
        "AWS profile",
        existing.as_ref().and_then(|s| s.profile.as_ref()),
        "the default credential chain",
    )?;
    let mut section = existing.unwrap_or_else(|| BedrockConfig::for_model(&model));
    section.model = model;
    section.region = region;
    section.profile = profile;
    Ok(Some(section))
}

/// Write `cfg` to `~/.mezame/config.json`, creating `~/.mezame` owner-only
/// when it is absent, and say what was written and which backend it
/// selects.
///
/// An existing file is replaced. Two things the callers carry forward are
/// named on the way, because dropping them on a re-run was silent and
/// costly: `hosts`, the key a tunnel user edits by hand, and the `bedrock`
/// section a bind-only run leaves untouched (`kept`).
pub(crate) fn write_config(cfg: &Config, kept: bool) -> Result<Config> {
    let hosts = cfg.hosts();
    if !hosts.is_empty() {
        println!(
            "Keeping hosts from the existing config: {}",
            hosts.join(", ")
        );
    }
    if kept {
        println!("Keeping the Bedrock settings from the existing config");
    }

    let path = config_path()?;
    // What is written is what the next start loads, so it is held to the
    // same check here. A section carried forward from a file with a bad
    // value, or a flag that made one, is refused with the key named and
    // the file left as it was.
    cfg.validate(&path)?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent).with_context(|| format!("Creating {}", parent.display()))?;
    }
    write_private_atomic(&path, serde_json::to_string_pretty(cfg)?.as_bytes(), true)
        .with_context(|| format!("Writing {}", path.display()))?;
    println!("Wrote {}", path.display());
    match &cfg.bedrock {
        Some(section) => {
            println!("Backend: Bedrock {}", section.model);
            println!(
                "Credentials come from the AWS chain: aws configure, aws sso login, AWS_PROFILE or \
                 the AWS_ACCESS_KEY_ID variables."
            );
            println!(
                "Enable access to {} in the Bedrock console for the region you use.",
                section.model
            );
            // The example is built only from a bare base id; a profile id
            // or an ARN already carries its routing.
            if section.model.starts_with("anthropic.") {
                println!(
                    "If a base id is refused with an on-demand-throughput error, use an \
                     inference profile id such as global.{}.",
                    section.model
                );
            } else {
                println!(
                    "If the id is refused with an on-demand-throughput error, use an inference \
                     profile id: the base id under a `global.` or geo prefix."
                );
            }
        }
        None => println!("Backend: echo"),
    }
    println!();
    Ok(cfg.clone())
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
    use super::{parse_init_args, validate_bind_entry, InitArgs};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn init_takes_no_arguments_or_any_of_the_four_flags() {
        assert!(parse_init_args(&args(&[])).unwrap().is_empty());
        assert_eq!(
            parse_init_args(&args(&["--bind", "0.0.0.0:9510"])).unwrap(),
            InitArgs {
                bind: Some("0.0.0.0:9510".to_string()),
                ..InitArgs::default()
            }
        );
        assert_eq!(
            parse_init_args(&args(&["--bind=127.0.0.1:9511"])).unwrap(),
            InitArgs {
                bind: Some("127.0.0.1:9511".to_string()),
                ..InitArgs::default()
            }
        );
        assert_eq!(
            parse_init_args(&args(&[
                "--model",
                "global.anthropic.claude-sonnet-5",
                "--region=eu-west-1",
                "--profile",
                "work",
            ]))
            .unwrap(),
            InitArgs {
                bind: None,
                model: Some("global.anthropic.claude-sonnet-5".to_string()),
                region: Some("eu-west-1".to_string()),
                profile: Some("work".to_string()),
            }
        );
        // An empty value is accepted here and refused by the later check.
        assert_eq!(
            parse_init_args(&args(&["--bind", ""])).unwrap().bind,
            Some(String::new())
        );
    }

    #[test]
    fn init_refuses_what_it_does_not_understand() {
        for (refused, names) in [
            (vec!["--bind"], "--bind"),
            (vec!["--bind", "--other"], "--bind"),
            (vec!["--bind", "a", "--bind", "b"], "twice"),
            (vec!["--model"], "--model"),
            (vec!["--model", "a", "--model=b"], "twice"),
            (vec!["--region", "--profile", "x"], "--region"),
            (vec!["--bogus"], "Unknown argument"),
            (vec!["--bogus"], "--profile NAME"),
            (vec!["extra"], "Unknown argument"),
            (vec!["--bind=a", "trailing"], "Unknown argument"),
        ] {
            let err = parse_init_args(&args(&refused)).unwrap_err().to_string();
            assert!(
                err.contains(names),
                "{refused:?} should name {names:?}: {err}"
            );
        }
    }

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
