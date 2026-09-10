//! `mezame init`, the bootstrap a first start falls into, and the user
//! commands.
//!
//! `init` asks, or takes from its flags, the bind address, the admin's
//! name and password, and the Bedrock model with its region and profile,
//! then `apply` turns the answers into the master key, the datastore, the
//! admin row, the Bedrock credential (sealed, with its grant) and the
//! global profile, the configuration file, and a summary that names no
//! password, no hash, no region and no profile. The user commands share
//! the password reading with it and open the key and the datastore that
//! are there, creating neither.

use std::io::{self, BufRead as _, IsTerminal as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use dialoguer::{theme::ColorfulTheme, Input, Password, Select};
use serde_json::json;

use crate::auth::{check_password, hash_password};
use crate::config::{
    self, config_path, default_bind, read_existing_config, validate_bind_entry, Config,
    ExistingConfig, TransportConfig, CONFIG_VERSION, DEFAULT_PORT,
};
use crate::store::crypto::{Keys, MasterKey};
use crate::store::sqlite::SqliteStore;
use crate::store::{NewProfile, Role, Store, UserRow, USER_NAME_MAX_CHARS};

/// The `provider` column of the Bedrock credential row.
pub const BEDROCK_PROVIDER: &str = "bedrock";
/// The `label` of the Bedrock credential row: a fixed string, never a
/// payload value.
pub const BEDROCK_LABEL: &str = "Bedrock";

const MEZAME_ART: &str = r#"
 ███╗   ███╗███████╗███████╗ █████╗ ███╗   ███╗███████╗
 ████╗ ████║██╔════╝╚══███╔╝██╔══██╗████╗ ████║██╔════╝
 ██╔████╔██║█████╗    ███╔╝ ███████║██╔████╔██║█████╗  
 ██║╚██╔╝██║██╔══╝   ███╔╝  ██╔══██║██║╚██╔╝██║██╔══╝  
 ██║ ╚═╝ ██║███████╗███████╗██║  ██║██║ ╚═╝ ██║███████╗
 ╚═╝     ╚═╝╚══════╝╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝╚══════╝
 目覚め!
"#;

// ---------- the flags ----------

/// What follows `init` on the command line, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InitArgs {
    pub bind: Option<String>,
    pub admin: Option<String>,
    pub password_stdin: bool,
    pub model: Option<String>,
    pub region: Option<String>,
    pub profile: Option<String>,
}

impl InitArgs {
    /// No flag at all: the interactive setup runs.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The flags `init` takes, each with the example its error shows; an empty
/// example marks a flag that takes no value. One table: the parser, the
/// error text and the help all read it.
pub const INIT_FLAGS: [(&str, &str); 6] = [
    ("--bind", "0.0.0.0:9510"),
    ("--admin", "alice"),
    ("--password-stdin", ""),
    ("--model", "global.anthropic.claude-sonnet-5"),
    ("--region", "us-east-1"),
    ("--profile", "work"),
];

/// The flags as the error names them: `--bind ADDR`, `--admin NAME`,
/// `--password-stdin`, `--model ID`, `--region NAME` and `--profile NAME`.
pub const INIT_FLAGS_TEXT: &str =
    "`--bind ADDR`, `--admin NAME`, `--password-stdin`, `--model ID`, \
     `--region NAME` and `--profile NAME`";

/// Parse what follows `init`: nothing, or any of the six flags, each value
/// flag in either of its two spellings, each flag at most once, and
/// `--password-stdin` only beside `--admin`, whose password it reads.
///
/// Anything else is an error naming the token, so a typo is refused
/// instead of dropping into the prompt, and a flag the run would not
/// honour is refused rather than consumed with nothing said. Pure, so it
/// has tests.
pub fn parse_init_args(args: &[String]) -> Result<InitArgs> {
    let mut parsed = InitArgs::default();
    let mut tokens = args.iter();
    while let Some(token) = tokens.next() {
        let Some((flag, example)) = INIT_FLAGS
            .iter()
            .find(|(flag, _)| token == *flag || token.starts_with(&format!("{flag}=")))
        else {
            bail!(
                "Unknown argument `{token}`. `mezame init` takes {INIT_FLAGS_TEXT}, and nothing \
                 else."
            );
        };
        if example.is_empty() {
            if token != *flag {
                bail!("`{flag}` takes no value");
            }
            if parsed.password_stdin {
                bail!("`{flag}` given twice");
            }
            parsed.password_stdin = true;
            continue;
        }
        let value = if token == flag {
            match tokens.next() {
                Some(v) if !v.starts_with('-') => v.clone(),
                _ => bail!("`{flag}` needs a value, e.g. `mezame init {flag} {example}`"),
            }
        } else {
            token[flag.len() + 1..].to_string()
        };
        let slot = match *flag {
            "--bind" => &mut parsed.bind,
            "--admin" => &mut parsed.admin,
            "--model" => &mut parsed.model,
            "--region" => &mut parsed.region,
            _ => &mut parsed.profile,
        };
        if slot.is_some() {
            bail!("`{flag}` given twice");
        }
        *slot = Some(value);
    }
    if parsed.password_stdin && parsed.admin.is_none() {
        bail!(
            "`--password-stdin` needs `--admin`: the first line of standard input is the \
             admin's password, e.g. `echo 'the password' | mezame init --admin NAME \
             --password-stdin`"
        );
    }
    Ok(parsed)
}

/// A flag's value with the whitespace trimmed, refused when nothing is
/// left. One rule for every flag: no flag clears a setting. The hosts can
/// be edited in the file. The region and the profile live sealed in the
/// datastore, where a re-run of `init` keeps them; clearing one is not
/// possible in this alpha.
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

// ---------- the answers ----------

/// The Bedrock settings `init` writes to the datastore: the model, and the
/// region and profile when given. Both `None` names the ambient AWS chain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BedrockAnswers {
    pub model: String,
    pub region: Option<String>,
    pub profile: Option<String>,
}

impl BedrockAnswers {
    /// The credential payload: the region and the profile, null when unset.
    fn payload(&self) -> serde_json::Value {
        json!({ "region": self.region, "profile": self.profile })
    }
}

/// What `init` decided, before anything is written.
#[derive(Debug, Clone)]
pub struct Answers {
    pub bind: String,
    /// The admin to create, name and password; `None` keeps the users
    /// there are.
    pub admin: Option<(String, String)>,
    /// `Some` replaces the global Bedrock credential and profile; `None`
    /// keeps what the datastore holds.
    pub bedrock: Option<BedrockAnswers>,
    /// The file on disk, when there is one.
    pub existing: Option<ExistingConfig>,
}

/// The key, the store and what opening them did.
pub struct Opened {
    pub store: Arc<dyn Store>,
    pub keys: Keys,
    /// Credential rows dropped because the datastore had no key.
    pub dropped: u64,
}

/// What `apply` did, for the summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub config_path: PathBuf,
    pub datastore_path: PathBuf,
    pub users: u64,
    pub admin: AdminOutcome,
    /// The global profile's model, or `None` for the echo.
    pub model: Option<String>,
    pub removed_state_file: Option<PathBuf>,
}

/// What became of the admin question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminOutcome {
    Created(String),
    /// Users existed; the question was skipped.
    Kept,
    /// No user yet and none asked for: the first start asks, or the flags.
    None,
}

// ---------- `mezame init` ----------

/// `mezame init`. With no flag every question is asked; with any flag
/// nothing is asked, each question answered by its flag or by existing
/// state, and the admin created only when `--admin` names one.
pub fn run(args: &InitArgs) -> Result<()> {
    let interactive = args.is_empty();
    let existing = read_existing_config()?;
    note_legacy(existing.as_ref())?;
    if interactive {
        println!("{MEZAME_ART}");
    }

    // The key and the store come before the questions: whether the admin
    // is asked for depends on the user count, and the model prompt's
    // default on the profile row.
    let opened = open_or_create_store()?;
    let store = Arc::clone(&opened.store);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the runtime for init")?;
    let summary = runtime.block_on(async move {
        if opened.dropped > 0 {
            println!(
                "Dropped {} credential row(s) and the profiles that used them: they were sealed \
                 under a key that is not {}, so they could not be opened. Re-enter the Bedrock \
                 settings below or with `--model`.",
                opened.dropped,
                config::master_key_path()?.display()
            );
        }
        let has_admin = oldest_admin(store.as_ref()).await?.is_some();
        let current = current_bedrock(store.as_ref()).await?;

        let bind = match &args.bind {
            Some(addr) => {
                validate_bind_entry(addr).map_err(|message| anyhow!(message))?;
                addr.trim().to_string()
            }
            None if interactive => prompt_bind()?,
            None => existing
                .as_ref()
                .and_then(|e| e.config.bind().map(str::to_string))
                .unwrap_or_else(default_bind),
        };

        // The admin question is settled by an admin row, not by any user
        // row: a datastore `mezame user add` filled with plain users still
        // needs one, since the Bedrock credential is granted to an admin.
        let admin = if has_admin {
            if args.admin.is_some() {
                println!(
                    "The datastore already holds an admin; `--admin` is ignored. `mezame user \
                     add NAME --admin` adds another."
                );
            } else if interactive {
                println!("The datastore already holds an admin; skipping the admin questions.");
            }
            None
        } else if interactive {
            Some(prompt_admin()?)
        } else {
            match (&args.admin, args.password_stdin) {
                (Some(name), true) => Some((name.trim().to_string(), read_password_stdin()?)),
                (Some(_), false) => bail!(
                    "`--admin` needs `--password-stdin`: with no terminal the password is read \
                     from the first line of standard input, e.g. `echo 'the password' | mezame \
                     init --admin NAME --password-stdin`"
                ),
                (None, _) => None,
            }
        };

        let bedrock = if interactive {
            prompt_bedrock(current.as_ref())?
        } else {
            bedrock_from_flags(args, current.as_ref())?
        };

        apply(
            store.as_ref(),
            Answers {
                bind,
                admin,
                bedrock,
                existing,
            },
            crate::store::now_ms(),
        )
        .await
    })?;
    print_summary(&summary);
    Ok(())
}

fn plural(n: u64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
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

/// The Bedrock settings the datastore holds now: the global profile's model
/// with its credential's region and profile. `None` for the echo.
async fn current_bedrock(store: &dyn Store) -> Result<Option<BedrockAnswers>> {
    let Some(profile) = store.global_profile().await.map_err(|e| anyhow!("{e}"))? else {
        return Ok(None);
    };
    let (region, aws_profile) = match &profile.credential_id {
        Some(id) => match store.credential_payload(id).await {
            Ok(payload) => (
                payload["region"].as_str().map(str::to_string),
                payload["profile"].as_str().map(str::to_string),
            ),
            // A credential this key cannot open, or one whose row is gone:
            // the model is kept as the default and the rest starts blank.
            Err(_) => (None, None),
        },
        None => (None, None),
    };
    Ok(Some(BedrockAnswers {
        model: profile.model,
        region,
        profile: aws_profile,
    }))
}

/// The Bedrock answers from the flags over what the datastore holds: a
/// flag replaces its setting and the rest is carried; no flag at all keeps
/// the rows as they are; `--region` or `--profile` with no model anywhere
/// is refused, since there is nothing to attach them to.
fn bedrock_from_flags(
    args: &InitArgs,
    current: Option<&BedrockAnswers>,
) -> Result<Option<BedrockAnswers>> {
    if args.model.is_none() && args.region.is_none() && args.profile.is_none() {
        return Ok(None);
    }
    let model = match &args.model {
        Some(model) => non_empty("--model", model)?,
        None => match current {
            Some(current) => current.model.clone(),
            None => bail!(
                "`--region` and `--profile` need a model: pass `--model ID` with them, or \
                 configure one first"
            ),
        },
    };
    let region = match &args.region {
        Some(region) => Some(non_empty("--region", region)?),
        None => current.and_then(|c| c.region.clone()),
    };
    let profile = match &args.profile {
        Some(profile) => Some(non_empty("--profile", profile)?),
        None => current.and_then(|c| c.profile.clone()),
    };
    Ok(Some(BedrockAnswers {
        model,
        region,
        profile,
    }))
}

// ---------- the prompts ----------

/// The one prompt for the transport: the bind address, with the two common
/// choices and a free-form entry.
fn prompt_bind() -> Result<String> {
    let theme = ColorfulTheme::default();
    let loopback = default_bind();
    let all = format!("0.0.0.0:{DEFAULT_PORT}");
    let bind_options = [
        format!("{loopback}  (loopback only, default)"),
        format!("{all}    (all IPv4 interfaces, reachable from LAN)"),
        "Custom          (type an address:port)".to_string(),
    ];
    let bind_idx = Select::with_theme(&theme)
        .with_prompt("Bind address")
        .items(&bind_options)
        .default(0)
        .interact()?;
    Ok(match bind_idx {
        0 => loopback,
        1 => all,
        _ => {
            let s: String = Input::with_theme(&theme)
                .with_prompt("Bind address")
                .validate_with(|input: &String| validate_bind_entry(input))
                .interact_text()?;
            s.trim().to_string()
        }
    })
}

/// The admin's name and password, the password masked and entered twice.
fn prompt_admin() -> Result<(String, String)> {
    let theme = ColorfulTheme::default();
    let name: String = Input::with_theme(&theme)
        .with_prompt("Admin username")
        .validate_with(|input: &String| check_user_name(input))
        .interact_text()?;
    let password = prompt_password(&theme, "Admin password")?;
    Ok((name.trim().to_string(), password))
}

/// A password entered twice, held to the length rules.
fn prompt_password(theme: &ColorfulTheme, prompt: &str) -> Result<String> {
    let password = Password::with_theme(theme)
        .with_prompt(prompt)
        .with_confirmation("Confirm", "The two entries differ; try again")
        .validate_with(|input: &String| check_password(input).map_err(|e| e.to_string()))
        .interact()?;
    Ok(password)
}

/// The rule a user name is held to before the store sees it: not empty
/// and at most [`USER_NAME_MAX_CHARS`] characters.
pub fn check_user_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("a user name is required".to_string());
    }
    if trimmed.chars().count() > USER_NAME_MAX_CHARS {
        return Err(format!(
            "a user name is at most {USER_NAME_MAX_CHARS} characters"
        ));
    }
    Ok(())
}

/// The Bedrock prompts, in one of two shapes each. With no current value,
/// an empty answer means "none": the echo backend for the model, the AWS
/// default for the region and the profile. With a current value, that
/// value is the default and the Enter key keeps it. A blank answer never
/// clears a setting.
fn prompt_bedrock(current: Option<&BedrockAnswers>) -> Result<Option<BedrockAnswers>> {
    let theme = ColorfulTheme::default();
    let model = match current {
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
        Some(current) => Input::with_theme(&theme)
            .with_prompt("Bedrock model id (Enter keeps the current one)")
            .default(current.model.clone())
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
            Ok(Some(entered.trim().to_string())
                .filter(|s| !s.is_empty())
                .or_else(|| current.cloned()))
        };
    let region = optional(
        "AWS region",
        current.and_then(|c| c.region.as_ref()),
        "the AWS default",
    )?;
    let profile = optional(
        "AWS profile",
        current.and_then(|c| c.profile.as_ref()),
        "the default credential chain",
    )?;
    Ok(Some(BedrockAnswers {
        model,
        region,
        profile,
    }))
}

/// The first line of standard input as the password, for `--password-stdin`.
pub fn read_password_stdin() -> Result<String> {
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading the password from standard input")?;
    let password = line.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        bail!("`--password-stdin` was given but the first line of standard input was empty");
    }
    Ok(password.to_string())
}

/// Whether the process can ask a question: standard input is a terminal.
pub fn has_terminal() -> bool {
    io::stdin().is_terminal()
}

// ---------- the key and the store ----------

/// Refuse a datastore found without its key, with one line naming both
/// paths and pointing at the `~/.mezame` backup set and at `mezame init`.
/// Nothing sealed in the datastore can be opened without the key, so a
/// path that would make a new key beside it stops here instead: the
/// server's start, a start with no configuration that would fall into the
/// setup, and the user commands. Only `mezame init` itself goes on, since
/// dropping the sealed rows is the operator's call to make. The check
/// reads two paths and writes nothing.
pub fn refuse_keyless_datastore(datastore: &Path, key_path: &Path) -> Result<()> {
    if datastore.exists() && !key_path.exists() {
        bail!(
            "{} exists but {} does not: the datastore's credentials cannot be opened without \
             it. Restore ~/.mezame from its backup, or run `mezame init` to create a new key \
             and re-enter the Bedrock credential.",
            datastore.display(),
            key_path.display()
        );
    }
    Ok(())
}

/// The master key and the datastore as the server opens them: a datastore
/// found without its key stops with the backup set named, since nothing
/// sealed in it could be opened; with neither present both are created;
/// a key of the wrong mode or length is refused with the path named.
pub fn open_store() -> Result<(Arc<dyn Store>, Keys)> {
    let dir = config::mezame_dir()?;
    let datastore = config::datastore_path()?;
    let key_path = config::master_key_path()?;
    refuse_keyless_datastore(&datastore, &key_path)?;
    config::ensure_private_dir(&dir).with_context(|| format!("Creating {}", dir.display()))?;
    let key = if datastore.exists() {
        MasterKey::load(&key_path).map_err(|e| anyhow!("{e}"))?
    } else {
        MasterKey::load_or_create(&key_path).map_err(|e| anyhow!("{e}"))?
    };
    let keys = key.keys();
    let store = SqliteStore::open(&datastore, keys.clone()).map_err(|e| anyhow!("{e}"))?;
    Ok((Arc::new(store), keys))
}

/// The master key and the datastore as the user commands open them: both
/// have to be there already. The commands print rows or refuse, and
/// neither is a reason to make a key or a datastore; `mezame init` and
/// the first start do that, and the line says so. A datastore without its
/// key is refused with the server's line.
fn open_existing_store() -> Result<(Arc<dyn Store>, Keys)> {
    let datastore = config::datastore_path()?;
    let key_path = config::master_key_path()?;
    if !datastore.exists() {
        bail!(
            "No datastore yet ({} does not exist): run `mezame init` first.",
            datastore.display()
        );
    }
    refuse_keyless_datastore(&datastore, &key_path)?;
    let key = MasterKey::load(&key_path).map_err(|e| anyhow!("{e}"))?;
    let keys = key.keys();
    let store = SqliteStore::open(&datastore, keys.clone()).map_err(|e| anyhow!("{e}"))?;
    Ok((Arc::new(store), keys))
}

/// The master key and the datastore as `init` opens them: an absent key is
/// created whether or not a datastore is there, and every credential row
/// the key in hand cannot open goes, with the profiles that used it,
/// since nothing sealed under another key can ever be read again. That
/// is decided on the rows themselves, not on whether this run made the
/// key, so a run that failed between making the key and dropping the rows
/// leaves the drop to the next one. An existing key is read under the
/// same checks as the server's and never rewritten.
pub fn open_or_create_store() -> Result<Opened> {
    let dir = config::mezame_dir()?;
    config::ensure_private_dir(&dir).with_context(|| format!("Creating {}", dir.display()))?;
    let datastore = config::datastore_path()?;
    let key_path = config::master_key_path()?;
    let key = MasterKey::load_or_create(&key_path).map_err(|e| anyhow!("{e}"))?;
    let keys = key.keys();
    let store: Arc<dyn Store> =
        Arc::new(SqliteStore::open(&datastore, keys.clone()).map_err(|e| anyhow!("{e}"))?);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the runtime for init")?;
    let dropped = runtime
        .block_on(store.drop_unopenable_credentials())
        .map_err(|e| anyhow!("{e}"))?;
    Ok(Opened {
        store,
        keys,
        dropped,
    })
}

// ---------- apply ----------

/// Turn the answers into rows and files: the admin, the Bedrock credential
/// with its grant and the global profile (replacing the ones there were),
/// the configuration at version 2 with the hosts, `models` and public URL
/// an existing file held, and the removal of `state.json`. The key and the
/// datastore are open already.
///
/// The configuration is assembled and checked first, under the same rule
/// the write applies: the values carried forward from an existing file
/// were read without validation, and one the loader would refuse has to
/// stop the run before the admin is created or the credential replaced.
/// Otherwise the run reports a failure having already changed the
/// datastore, and every re-run does the same until the file is fixed.
pub async fn apply(store: &dyn Store, answers: Answers, now: i64) -> Result<Summary> {
    let config_path = config_path()?;
    let cfg = assemble(answers.existing.as_ref().map(|e| &e.config), answers.bind);
    cfg.validate(&config_path)?;

    let admin = match &answers.admin {
        Some((name, password)) => {
            let hash = hash_password(password).map_err(|e| anyhow!("{e}"))?;
            let row = store
                .create_user(name, &hash, Role::Admin, now)
                .await
                .map_err(|e| anyhow!("could not create the admin: {e}"))?;
            AdminOutcome::Created(row.name)
        }
        None => {
            if oldest_admin(store).await?.is_some() {
                AdminOutcome::Kept
            } else {
                AdminOutcome::None
            }
        }
    };

    let model = match &answers.bedrock {
        Some(bedrock) => {
            replace_bedrock(store, bedrock, now).await?;
            Some(bedrock.model.clone())
        }
        None => store
            .global_profile()
            .await
            .map_err(|e| anyhow!("{e}"))?
            .map(|profile| profile.model),
    };

    if answers.existing.as_ref().is_some_and(|e| e.had_bedrock) {
        println!(
            "Dropping the `bedrock` section from {}: the model, region and profile live in the \
             datastore now.",
            config_path.display()
        );
    }
    write_config(&cfg, &config_path)?;
    let removed_state_file = remove_legacy_state_file(&config::mezame_dir()?)
        .context("removing the state file releases before 0.14 kept")?;

    Ok(Summary {
        config_path,
        datastore_path: config::datastore_path()?,
        users: store.count_users().await.map_err(|e| anyhow!("{e}"))?,
        admin,
        model,
        removed_state_file,
    })
}

/// Replace the global Bedrock credential and profile: the new credential
/// (sealed, with a grant to its creator) and the profile pointing at it
/// first, then the credentials it replaces, whose grants go with them.
async fn replace_bedrock(store: &dyn Store, bedrock: &BedrockAnswers, now: i64) -> Result<()> {
    let creator = oldest_admin(store).await?.ok_or_else(|| {
        anyhow!(
            "a Bedrock credential needs an admin to own it and none exists yet: run `mezame \
             init --admin NAME --password-stdin --model ID`, or `mezame user add NAME --admin` \
             and then `mezame init --model ID`"
        )
    })?;
    let previous = store
        .credentials(None, BEDROCK_PROVIDER)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let credential = store
        .create_credential(
            None,
            &creator.id,
            BEDROCK_PROVIDER,
            BEDROCK_LABEL,
            &bedrock.payload(),
            now,
        )
        .await
        .map_err(|e| anyhow!("could not store the Bedrock credential: {e}"))?;
    store
        .upsert_global_profile(&NewProfile {
            model: bedrock.model.clone(),
            credential_id: Some(credential.id.clone()),
            ..NewProfile::default()
        })
        .await
        .map_err(|e| anyhow!("could not store the Bedrock profile: {e}"))?;
    for old in previous {
        store
            .delete_credential(&old.id)
            .await
            .map_err(|e| anyhow!("could not remove the replaced credential: {e}"))?;
    }
    Ok(())
}

/// The admin the Bedrock credential is granted to: the oldest `admin` row.
async fn oldest_admin(store: &dyn Store) -> Result<Option<UserRow>> {
    let users = store.list_users().await.map_err(|e| anyhow!("{e}"))?;
    Ok(users
        .into_iter()
        .filter(|user| user.role == Role::Admin)
        .min_by_key(|user| (user.created, user.id.clone())))
}

/// One transport with `bind`, the hosts, datastore, public URL and model
/// list an existing file carried: the shape `init` writes, always at the
/// current version.
fn assemble(existing: Option<&Config>, bind: String) -> Config {
    Config {
        version: CONFIG_VERSION,
        transports: vec![TransportConfig::Cloudflared {
            bind,
            hosts: existing.map(Config::hosts).unwrap_or_default(),
        }],
        datastore: existing.map(|c| c.datastore.clone()).unwrap_or_default(),
        public_url: existing.and_then(|c| c.public_url.clone()),
        models: existing.map(|c| c.models.clone()).unwrap_or_default(),
    }
}

/// Write `cfg` to `path`, naming the hosts carried forward, and say what
/// was written.
fn write_config(cfg: &Config, path: &Path) -> Result<()> {
    let hosts = cfg.hosts();
    if !hosts.is_empty() {
        println!(
            "Keeping hosts from the existing config: {}",
            hosts.join(", ")
        );
    }
    if !cfg.models.is_empty() {
        println!(
            "Keeping models from the existing config: {}",
            cfg.models.join(", ")
        );
    }
    cfg.validate(path)?;
    if let Some(parent) = path.parent() {
        config::ensure_private_dir(parent)
            .with_context(|| format!("Creating {}", parent.display()))?;
    }
    config::write_private_atomic(path, serde_json::to_string_pretty(cfg)?.as_bytes(), true)
        .with_context(|| format!("Writing {}", path.display()))?;
    println!("Wrote {}", path.display());
    Ok(())
}

/// The lines after `Wrote ...`: the datastore, the admin, the backend and
/// the hints. No password, hash, region or profile is among them.
fn print_summary(summary: &Summary) {
    println!(
        "Datastore: sqlite {} ({} user{})",
        summary.datastore_path.display(),
        summary.users,
        plural(summary.users)
    );
    match &summary.admin {
        AdminOutcome::Created(name) => println!("Admin: created `{name}`"),
        AdminOutcome::Kept => println!("Admin: kept"),
        AdminOutcome::None => println!(
            "Admin: none yet. The first start in a terminal asks for one; without a terminal, \
             run `mezame init --admin NAME --password-stdin`."
        ),
    }
    match &summary.model {
        Some(model) => {
            println!("Backend: Bedrock {model}");
            println!(
                "Credentials come from the AWS chain: aws configure, aws sso login, AWS_PROFILE or \
                 the AWS_ACCESS_KEY_ID variables."
            );
            println!("Enable access to {model} in the Bedrock console for the region you use.");
            // The example is built only from a bare base id; a profile id
            // or an ARN already carries its routing.
            if model.starts_with("anthropic.") {
                println!(
                    "If a base id is refused with an on-demand-throughput error, use an \
                     inference profile id such as global.{model}."
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
    if let Some(path) = &summary.removed_state_file {
        println!(
            "Removed {}: the tab list and the settings live in the datastore now.",
            path.display()
        );
    }
    println!();
}

/// Remove `state.json` under `dir` when it is there: the path removed, or
/// `None` when there was nothing to remove. Only a failure other than the
/// file's absence is an error.
pub fn remove_legacy_state_file(dir: &Path) -> io::Result<Option<PathBuf>> {
    let path = dir.join("state.json");
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(Some(path)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

// ---------- the bootstrap ----------

/// The admin questions alone, for a server that starts on a datastore
/// with no user and has a terminal to ask on. Returns the row created.
pub async fn bootstrap(store: &dyn Store) -> Result<UserRow> {
    println!("{MEZAME_ART}");
    println!("No user yet. Create the admin account to continue:");
    let (name, password) = prompt_admin()?;
    let hash = hash_password(&password).map_err(|e| anyhow!("{e}"))?;
    let row = store
        .create_user(&name, &hash, Role::Admin, crate::store::now_ms())
        .await
        .map_err(|e| anyhow!("could not create the admin: {e}"))?;
    println!("Admin: created `{}`", row.name);
    println!();
    Ok(row)
}

/// The line a server with no user and no terminal exits with.
pub const NO_USER_NO_TERMINAL: &str = "No user yet, and no terminal to ask on. Run `mezame init \
     --admin NAME --password-stdin` (the password on the first line of standard input), or start \
     `mezame` in a terminal to be asked.";

// ---------- the user commands ----------

/// What follows `user add NAME`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserAddArgs {
    pub name: String,
    pub admin: bool,
    pub password_stdin: bool,
}

/// Parse what follows `user add`: the name, then `--admin` and
/// `--password-stdin` in any order, each at most once.
pub fn parse_user_add_args(args: &[String]) -> Result<UserAddArgs> {
    let mut parsed = UserAddArgs::default();
    let mut name_seen = false;
    for token in args {
        match token.as_str() {
            "--admin" if parsed.admin => bail!("`--admin` given twice"),
            "--admin" => parsed.admin = true,
            "--password-stdin" if parsed.password_stdin => bail!("`--password-stdin` given twice"),
            "--password-stdin" => parsed.password_stdin = true,
            other if other.starts_with('-') => bail!(
                "Unknown argument `{other}`. `mezame user add` takes a name, then `--admin` and \
                 `--password-stdin`, and nothing else."
            ),
            other if name_seen => bail!("Unexpected argument `{other}`: one name at a time"),
            other => {
                parsed.name = other.to_string();
                name_seen = true;
            }
        }
    }
    if !name_seen {
        bail!(
            "`mezame user add` needs a name: `mezame user add NAME [--admin] [--password-stdin]`"
        );
    }
    Ok(parsed)
}

/// `mezame user add NAME [--admin] [--password-stdin]`.
pub fn user_add(args: &UserAddArgs) -> Result<()> {
    check_user_name(&args.name).map_err(|why| anyhow!("{why}"))?;
    let (store, _keys) = open_existing_store()?;
    let password = read_password(args.password_stdin, "Password")?;
    let hash = hash_password(&password).map_err(|e| anyhow!("{e}"))?;
    let role = if args.admin { Role::Admin } else { Role::User };
    let row = block_on(async {
        store
            .create_user(&args.name, &hash, role, crate::store::now_ms())
            .await
            .map_err(|e| anyhow!("{e}"))
    })?;
    println!("Created user `{}` ({})", row.name, row.role.as_str());
    Ok(())
}

/// `mezame user list`: one line per user, `<name>  <role>  <created>`.
pub fn user_list() -> Result<()> {
    let (store, _keys) = open_existing_store()?;
    let users = block_on(async { store.list_users().await.map_err(|e| anyhow!("{e}")) })?;
    if users.is_empty() {
        println!("No users yet. `mezame init --admin NAME --password-stdin` creates the first.");
        return Ok(());
    }
    let width = users
        .iter()
        .map(|u| u.name.chars().count())
        .max()
        .unwrap_or(0);
    for user in users {
        println!(
            "{:<width$}  {:<5}  {}",
            user.name,
            user.role.as_str(),
            crate::prompt::civil_from_days(user.created.div_euclid(86_400_000)),
        );
    }
    Ok(())
}

/// `mezame passwd NAME [--password-stdin]`: a new password, and every
/// cookie of the user ends with the epoch it bumps.
pub fn passwd(name: &str, password_stdin: bool) -> Result<()> {
    check_user_name(name).map_err(|why| anyhow!("{why}"))?;
    let (store, _keys) = open_existing_store()?;
    let user = block_on(async { store.user_by_name(name).await.map_err(|e| anyhow!("{e}")) })?
        .ok_or_else(|| anyhow!("no user named `{name}`; `mezame user list` shows them"))?;
    let password = read_password(password_stdin, &format!("New password for {name}"))?;
    let hash = hash_password(&password).map_err(|e| anyhow!("{e}"))?;
    block_on(async {
        store
            .set_password_hash(&user.id, &hash)
            .await
            .map_err(|e| anyhow!("{e}"))
    })?;
    println!("Password changed for `{name}`; every device is signed out.");
    Ok(())
}

/// A password from standard input when asked, else from a masked prompt,
/// which needs a terminal.
fn read_password(from_stdin: bool, prompt: &str) -> Result<String> {
    if from_stdin {
        return read_password_stdin();
    }
    if !has_terminal() {
        bail!(
            "No terminal to ask for the password on. Pass `--password-stdin` and the password on \
             the first line of standard input."
        );
    }
    prompt_password(&ColorfulTheme::default(), prompt)
}

/// Run one store future to completion on a runtime of its own.
fn block_on<T>(future: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the runtime")?
        .block_on(future)
}

#[cfg(test)]
mod tests {
    use super::{parse_init_args, parse_user_add_args, InitArgs, UserAddArgs};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn init_takes_no_arguments_or_any_of_the_six_flags() {
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
                "--admin",
                "alice",
                "--password-stdin",
                "--model",
                "global.anthropic.claude-sonnet-5",
                "--region=eu-west-1",
                "--profile",
                "work",
            ]))
            .unwrap(),
            InitArgs {
                bind: None,
                admin: Some("alice".to_string()),
                password_stdin: true,
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
            (vec!["--admin"], "--admin"),
            (vec!["--password-stdin=yes"], "takes no value"),
            (vec!["--password-stdin", "--password-stdin"], "twice"),
            (vec!["--bogus"], "Unknown argument"),
            (vec!["--bogus"], "--profile NAME"),
            (vec!["--bogus"], "--password-stdin"),
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
    fn user_add_takes_a_name_and_two_flags() {
        assert_eq!(
            parse_user_add_args(&args(&["bob"])).unwrap(),
            UserAddArgs {
                name: "bob".to_string(),
                admin: false,
                password_stdin: false,
            }
        );
        assert_eq!(
            parse_user_add_args(&args(&["--password-stdin", "bob", "--admin"])).unwrap(),
            UserAddArgs {
                name: "bob".to_string(),
                admin: true,
                password_stdin: true,
            }
        );
        for (refused, names) in [
            (vec![], "needs a name"),
            (vec!["--admin"], "needs a name"),
            (vec!["a", "b"], "one name"),
            (vec!["a", "--admin", "--admin"], "twice"),
            (vec!["a", "--nope"], "Unknown argument"),
        ] {
            let err = parse_user_add_args(&args(&refused))
                .unwrap_err()
                .to_string();
            assert!(err.contains(names), "{refused:?}: {err}");
        }
    }
}
