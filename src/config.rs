//! On-disk configuration and interactive setup.
//!
//! Config lives at `~/.mezame/config.json` and holds server settings
//! only. A schema change breaks existing users: add fields with
//! `#[serde(default)]` and leave the existing ones where they are.
//! Transports live in a list (`TransportConfig`) internally tagged on
//! `kind`; see the architecture document's configuration reference.
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

/// `mezame init` with no arguments: ask for the bind address, then write
/// the config.
pub(crate) fn init_config() -> Result<Config> {
    write_config(prompt_bind()?)
}

/// `mezame init --bind ADDR`: write the config for `addr` with no prompt,
/// for a service unit or a container started before setup.
///
/// `addr` is held to the same check as the free-form prompt entry and is
/// otherwise not parsed: an address that does not resolve fails at
/// `TcpListener::bind` on the next start with the operating system's own
/// message, which says more than a guess made here would.
pub(crate) fn init_config_with_bind(addr: &str) -> Result<Config> {
    validate_bind_entry(addr).map_err(|message| anyhow!(message))?;
    write_config(addr.trim().to_string())
}

/// What follows `init` on the command line: nothing, or `--bind ADDR` in
/// either of its two spellings.
///
/// Anything else is an error naming the token, so a typo is refused
/// instead of dropping into the prompt. Pure, so it has tests.
pub fn parse_init_args(args: &[String]) -> Result<Option<String>> {
    let mut bind: Option<String> = None;
    let mut tokens = args.iter();
    while let Some(token) = tokens.next() {
        let value = if token == "--bind" {
            match tokens.next() {
                Some(v) if !v.starts_with('-') => v.clone(),
                _ => bail!("`--bind` needs an address, e.g. `mezame init --bind 0.0.0.0:9510`"),
            }
        } else if let Some(v) = token.strip_prefix("--bind=") {
            v.to_string()
        } else {
            bail!(
                "Unknown argument `{token}`. `mezame init` takes `--bind ADDR` and nothing else."
            );
        };
        if bind.is_some() {
            bail!("`--bind` given twice");
        }
        bind = Some(value);
    }
    Ok(bind)
}

/// The one prompt: the bind address, with the two common choices and a
/// free-form entry.
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
    Ok(bind)
}

/// Write `~/.mezame/config.json` for `bind`, creating `~/.mezame`
/// owner-only when it is absent, and return the config it holds.
///
/// An existing file is replaced, the same as re-running the prompt, with
/// one exception: `hosts` is the key a user edits by hand, and a tunnel
/// user's list dropped on a re-run left every request answered 421 with
/// nothing said. A readable existing file's list is kept and named; a
/// file that does not parse is what `init` exists to replace.
pub(crate) fn write_config(bind: String) -> Result<Config> {
    let hosts = load_config()
        .ok()
        .into_iter()
        .flat_map(|old| old.transports)
        .find_map(|transport| match transport {
            TransportConfig::Cloudflared { hosts, .. } if !hosts.is_empty() => Some(hosts),
            _ => None,
        })
        .unwrap_or_default();
    if !hosts.is_empty() {
        println!(
            "Keeping hosts from the existing config: {}",
            hosts.join(", ")
        );
    }
    let cfg = Config {
        transports: vec![TransportConfig::Cloudflared { bind, hosts }],
    };

    let path = config_path()?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent).with_context(|| format!("Creating {}", parent.display()))?;
    }
    write_private_atomic(&path, serde_json::to_string_pretty(&cfg)?.as_bytes(), true)
        .with_context(|| format!("Writing {}", path.display()))?;
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
    use super::{parse_init_args, validate_bind_entry};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn init_takes_no_arguments_or_a_bind_address() {
        assert_eq!(parse_init_args(&args(&[])).unwrap(), None);
        assert_eq!(
            parse_init_args(&args(&["--bind", "0.0.0.0:9510"])).unwrap(),
            Some("0.0.0.0:9510".to_string())
        );
        assert_eq!(
            parse_init_args(&args(&["--bind=127.0.0.1:9511"])).unwrap(),
            Some("127.0.0.1:9511".to_string())
        );
        // An empty value is accepted here and refused by the bind check.
        assert_eq!(
            parse_init_args(&args(&["--bind", ""])).unwrap(),
            Some(String::new())
        );
    }

    #[test]
    fn init_refuses_what_it_does_not_understand() {
        for (refused, names) in [
            (vec!["--bind"], "--bind"),
            (vec!["--bind", "--other"], "--bind"),
            (vec!["--bind", "a", "--bind", "b"], "twice"),
            (vec!["--bogus"], "Unknown argument"),
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
