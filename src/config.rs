//! On-disk configuration: the file, its version gate, and the paths under
//! `~/.mezame`. The setup that writes the file lives in `crate::init`.
//!
//! Config lives at `~/.mezame/config.json` and holds server settings
//! only. The file carries `"version": 2`; a file of another or no version
//! is refused at startup with one line pointing at `mezame init`, which
//! rewrites it. Within a version, add fields with `#[serde(default)]` and
//! leave the existing ones where they are. Transports live in a list
//! (`TransportConfig`) internally tagged on `kind`; see the architecture
//! document's configuration reference. The Bedrock model, region and
//! profile live in the datastore, not here: a file carrying the `bedrock`
//! key an earlier release wrote is refused with a pointer at `mezame init`.
//!
//! Everything under `~/.mezame` is created owner-only on Unix: the
//! directory `0700` and its files `0600`, each file written to a fresh
//! `O_EXCL` sibling and renamed into place, so a symlink at the target is
//! replaced rather than followed and a reader never sees a partial file.
//! An existing directory keeps its mode. The helpers live here and their
//! callers elsewhere: `init.rs` writes the configuration through
//! `write_private_atomic`, `crypto.rs` takes the master key's sibling from
//! `temp_sibling`, and `init.rs` and the SQLite store call
//! `ensure_private_dir` before they create anything under the directory.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 9510;

/// The bind `init` writes when neither a flag nor an existing file gives
/// one: loopback on the default port.
pub fn default_bind() -> String {
    format!("127.0.0.1:{DEFAULT_PORT}")
}

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
/// are served. The one key refused is `bedrock`, which the loader checks
/// on the raw document: its settings moved to the datastore.
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

/// `~/.mezame`, the directory every file this module names sits in. An
/// empty `HOME` is refused like an unset one: joined as it is, it would
/// name `.mezame` relative to whatever directory the process runs in, and
/// the key and the datastore would be created there.
pub fn mezame_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .ok()
        .filter(|home| !home.is_empty())
        .context("HOME not set")?;
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

/// Why the server's working directory cannot be a user's default
/// workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceIneligible {
    /// The filesystem root.
    Root,
    /// The home directory.
    Home,
    /// `~/.mezame` itself, a directory holding it, or one inside it.
    MezameDir,
}

impl std::fmt::Display for WorkspaceIneligible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            WorkspaceIneligible::Root => "the working directory is the filesystem root",
            WorkspaceIneligible::Home => "the working directory is the home directory",
            WorkspaceIneligible::MezameDir => {
                "the working directory is, holds or is inside ~/.mezame"
            }
        })
    }
}

/// `cwd` as a default workspace root, or why it cannot be one: the root,
/// the home directory (`mezame_dir`'s parent), and any directory that is,
/// holds or sits inside `mezame_dir` are refused, since a workspace there
/// would put the key and the datastore under a tool's reach.
pub fn eligible_workspace_root(
    cwd: &Path,
    mezame_dir: &Path,
) -> std::result::Result<PathBuf, WorkspaceIneligible> {
    if cwd.parent().is_none() {
        return Err(WorkspaceIneligible::Root);
    }
    if mezame_dir.parent() == Some(cwd) {
        return Err(WorkspaceIneligible::Home);
    }
    if mezame_dir.starts_with(cwd) || cwd.starts_with(mezame_dir) {
        return Err(WorkspaceIneligible::MezameDir);
    }
    Ok(cwd.to_path_buf())
}

/// [`eligible_workspace_root`] on the paths as the filesystem spells them.
/// `getcwd` returns the working directory with every symlink resolved,
/// while `mezame_dir` is built from `HOME` as it was given, so a home
/// reached through a symlink would compare unequal to itself and pass
/// every check. The working directory is canonicalized whole. `mezame_dir`
/// is compared in two spellings, since the directory itself may be a
/// symlink to a directory elsewhere, such as a datastore kept on another
/// volume, and then its canonical parent is no longer the home: the home
/// canonicalized with the directory's name appended as given, on which the
/// home and a directory holding it are refused, and the directory
/// canonicalized whole through its nearest existing ancestor (it need not
/// exist yet), on which the directory the link points at, and a directory
/// holding or inside that one, are refused as well. A path that cannot be
/// canonicalized is compared as given. The root returned is the canonical
/// one.
pub fn resolve_workspace_root(
    cwd: &Path,
    mezame_dir: &Path,
) -> std::result::Result<PathBuf, WorkspaceIneligible> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let by_home = match (mezame_dir.parent(), mezame_dir.file_name()) {
        (Some(home), Some(name)) => canonicalize_through_existing_ancestor(home).join(name),
        _ => mezame_dir.to_path_buf(),
    };
    let root = eligible_workspace_root(&cwd, &by_home)?;
    let resolved = canonicalize_through_existing_ancestor(mezame_dir);
    if cwd.starts_with(&resolved) || resolved.starts_with(&cwd) {
        return Err(WorkspaceIneligible::MezameDir);
    }
    Ok(root)
}

/// `path` with its nearest existing ancestor canonicalized and the rest of
/// its components appended as given; `path` itself when no ancestor
/// resolves.
fn canonicalize_through_existing_ancestor(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(canonical) = ancestor.canonicalize() {
            return match path.strip_prefix(ancestor) {
                Ok(rest) => canonical.join(rest),
                Err(_) => canonical,
            };
        }
    }
    path.to_path_buf()
}

pub fn load_config() -> Result<Config> {
    load_config_from(&config_path()?)
}

/// Read and validate the configuration at `path`. The version is checked
/// on the raw document before anything else is read, so a file another
/// release wrote is answered with one line pointing at `mezame init` and
/// not with a parse error; a `bedrock` key is refused on the raw document
/// too, with the pointer at the datastore.
pub fn load_config_from(path: &Path) -> Result<Config> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("Reading {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("Parsing config.json at {}", path.display()))?;
    check_version(&value, path)?;
    check_no_bedrock(&value, path)?;
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

/// Refuse a document carrying the `bedrock` key an earlier release wrote:
/// the model, region and profile live in the datastore now, and a file
/// that still names them would be read as saying something it does not.
pub fn check_no_bedrock(value: &serde_json::Value, path: &Path) -> Result<()> {
    if value.get("bedrock").is_some() {
        bail!(
            "{} has a `bedrock` section; the Bedrock model, region and profile now live in the \
             datastore. Run `mezame init --model ID [--region R] [--profile P]` to set them and \
             rewrite the file without the section",
            path.display()
        );
    }
    Ok(())
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
    /// Whether the file carried the `bedrock` key an earlier release wrote,
    /// which `init` drops and says so.
    pub had_bedrock: bool,
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
    let had_bedrock = value.get("bedrock").is_some();
    if check_version(&value, path).is_ok() {
        let config: Config = serde_json::from_value(value).with_context(broken)?;
        return Ok(Some(ExistingConfig {
            config,
            legacy: None,
            had_bedrock,
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
        },
        legacy: Some(version_text(&value)),
        had_bedrock,
    }))
}

/// Create `dir` and any missing parent, owner-only (`0700`) on Unix.
///
/// An existing directory is left as it is, mode included: a directory a
/// 0.13.x release created stays `0755` until its owner runs `chmod`. A
/// regular file at the path is an error, on which the callers stop:
/// `init` and a first server start before they write the key, `init`
/// before it writes the configuration, and the SQLite store before it
/// opens the datastore. `0700` because the directory holds
/// the master key, the datastore and the configuration, and nothing else
/// on the machine needs to read them; the umask only ever removes bits
/// from it.
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
/// itself is synced. `init` passes it for the configuration; a file that
/// can be rebuilt may skip both, since on Apple targets an `fsync` is a
/// full device flush. A failure leaves the target as it was and removes
/// the sibling.
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

/// The check the free-form bind entry is held to.
///
/// A pure function so it has a test. The interactive body in `crate::init`
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
