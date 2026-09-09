//! `mezame init` on the binary in a temporary home: the flags, what they
//! write (the config, the key, the datastore and its rows), what they
//! refuse, and what the summary says and does not say.
//!
//! Every case runs the binary with its own temporary `HOME` and its output
//! captured, so standard error is a pipe. `dialoguer` checks that stream
//! before it prompts and would otherwise take keys from `/dev/tty`, not
//! standard input, so an accidental prompt fails with `not a terminal`
//! instead of hanging. Standard input is closed unless a case pipes a
//! password in.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use mezame::store::crypto::MasterKey;
use mezame::store::sqlite::SqliteStore;
use mezame::store::{Role, Store};
use serde_json::{json, Value};
use tempfile::TempDir;

const PASSWORD: &str = "correct horse battery";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mezame")
}

fn run_with_home(args: &[&str], home: &Path) -> Output {
    Command::new(bin())
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .expect("spawn mezame")
}

/// Run with `input` on standard input, for `--password-stdin`.
fn run_with_stdin(args: &[&str], home: &Path, input: &str) -> Output {
    let mut child = Command::new(bin())
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mezame");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("write the input");
    child.wait_with_output().expect("mezame exits")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn config_at(home: &Path) -> PathBuf {
    home.join(".mezame/config.json")
}

fn read_config(home: &Path) -> Value {
    let raw = std::fs::read_to_string(config_at(home)).expect("config.json exists");
    serde_json::from_str(&raw).expect("config.json is JSON")
}

fn home_with(body: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), body).unwrap();
    tmp
}

/// The datastore under `home`, opened with the key beside it, for looking
/// at what `init` wrote.
fn open_store(home: &Path) -> Arc<SqliteStore> {
    let keys = MasterKey::load(&home.join(".mezame/master.key"))
        .expect("the key loads")
        .keys();
    Arc::new(SqliteStore::open(&home.join(".mezame/mezame.db"), keys).expect("the store opens"))
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// The rows a Bedrock setup leaves: the profile's model, the credentials
/// (id, label) and, for the first credential, its payload and the count
/// of grants on it.
struct BedrockRows {
    model: Option<String>,
    credentials: Vec<(String, String)>,
    payload: Option<Value>,
    users: Vec<(String, Role)>,
}

fn bedrock_rows(home: &Path) -> BedrockRows {
    let store = open_store(home);
    block_on(async {
        let profile = store.global_profile().await.unwrap();
        let credentials: Vec<(String, String)> = store
            .credentials(None, "bedrock")
            .await
            .unwrap()
            .into_iter()
            .map(|c| (c.id, c.label))
            .collect();
        let payload = match credentials.first() {
            Some((id, _)) => Some(store.credential_payload(id).await.unwrap()),
            None => None,
        };
        let users = store
            .list_users()
            .await
            .unwrap()
            .into_iter()
            .map(|u| (u.name, u.role))
            .collect();
        BedrockRows {
            model: profile.map(|p| p.model),
            credentials,
            payload,
            users,
        }
    })
}

/// Rows of `grants` for `credential_id`, read straight from the file: the
/// store exposes no grant listing yet.
fn grant_count(home: &Path, credential_id: &str) -> i64 {
    let conn = rusqlite::Connection::open(home.join(".mezame/mezame.db")).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM grants WHERE credential_id = ?1",
        [credential_id],
        |row| row.get(0),
    )
    .unwrap()
}

fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "exit 0, got {:?}: {}",
        out.status,
        stderr(out)
    );
}

// ---------- phase 0 and 1: the bind and the file ----------

#[test]
fn init_with_bind_writes_the_config_without_a_prompt() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert_success(&out);
    assert!(
        stdout(&out).contains("Wrote"),
        "the path written is reported"
    );

    let cfg = read_config(tmp.path());
    let keys: Vec<&String> = cfg.as_object().expect("an object").keys().collect();
    // `serde_json::Value` holds an object's keys in sorted order.
    assert_eq!(
        keys,
        vec!["datastore", "transports", "version"],
        "the version-2 key set and nothing else"
    );
    assert_eq!(
        cfg["transports"],
        json!([{ "kind": "cloudflared", "bind": "0.0.0.0:9510" }]),
        "the entry holds the kind and the bind, and no hosts key"
    );
    // With no admin asked for, the summary says how one is made.
    let printed = stdout(&out);
    assert!(printed.contains("Admin: none yet"), "{printed}");
    assert!(
        printed.contains("--admin NAME --password-stdin"),
        "{printed}"
    );
    assert!(printed.contains("Backend: echo"), "{printed}");
}

#[test]
fn init_with_bind_in_equals_form_is_accepted() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--bind=127.0.0.1:9511"], tmp.path());
    assert_success(&out);
    assert_eq!(
        read_config(tmp.path())["transports"][0]["bind"],
        "127.0.0.1:9511"
    );
}

#[test]
fn init_with_a_blank_bind_writes_nothing_and_exits_non_zero() {
    // The flag is held to the same check as the prompt's free-form entry.
    for blank in ["", "   "] {
        let tmp = TempDir::new().unwrap();
        let out = run_with_home(&["init", "--bind", blank], tmp.path());
        assert!(!out.status.success(), "a blank bind {blank:?} is refused");
        assert!(
            stderr(&out).contains("Bind address is required"),
            "the refusal names the check: {}",
            stderr(&out)
        );
        assert!(!config_at(tmp.path()).exists(), "nothing is written");
    }
}

#[test]
fn init_with_bind_missing_its_value_is_refused() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--bind"], tmp.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--bind"));
    assert!(!config_at(tmp.path()).exists());
}

#[test]
fn init_with_an_unknown_argument_is_refused() {
    // A typo used to drop into the prompt as if no argument were given.
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--bogus"], tmp.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Unknown argument"));
    assert!(!config_at(tmp.path()).exists());
}

#[test]
fn init_with_bind_overwrites_an_existing_config() {
    // The same re-run semantics as the prompt: the file is replaced, and
    // the keys a 0.13.x release wrote go with it.
    let tmp = home_with(
        r#"{"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510"}],"agent_cmd":"kiro-cli","agent_args":["acp"]}"#,
    );
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert_success(&out);
    let cfg = read_config(tmp.path());
    assert_eq!(cfg["version"], 2, "rewritten at the current version");
    assert_eq!(cfg["transports"][0]["bind"], "0.0.0.0:9510");
    assert!(cfg.get("agent_cmd").is_none(), "the old keys are gone");
    let printed = stdout(&out);
    assert!(
        printed.contains("written by an earlier release (version none)"),
        "{printed}"
    );
    assert!(printed.contains("dropping the rest"), "{printed}");
}

#[test]
fn init_over_a_versionless_file_keeps_its_hosts() {
    // The server refuses such a file with a pointer at `init`; `init` has
    // to be a working step, so the transports' hosts survive it.
    let tmp = home_with(
        r#"{"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510","hosts":["mezame.example.com"]}],"bedrock":{"model":"m"}}"#,
    );
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert_success(&out);
    let cfg = read_config(tmp.path());
    assert_eq!(cfg["version"], 2);
    assert_eq!(cfg["transports"][0]["hosts"], json!(["mezame.example.com"]));
    assert!(
        cfg.get("bedrock").is_none(),
        "only the transports are carried from another version"
    );
    assert!(stdout(&out).contains("version none"), "{}", stdout(&out));
}

#[cfg(unix)]
#[test]
fn init_writes_an_owner_only_directory_and_owner_only_files() {
    // The directory, the config, the key and the datastore, each mode
    // checked end to end through the binary.
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let out = run_with_stdin(
        &[
            "init",
            "--bind",
            "127.0.0.1:9510",
            "--admin",
            "alice",
            "--password-stdin",
        ],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&out);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let dir = tmp.path().join(".mezame");
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&config_at(tmp.path())), 0o600);
    assert_eq!(mode(&dir.join("master.key")), 0o600);
    assert_eq!(
        std::fs::metadata(dir.join("master.key")).unwrap().len(),
        32,
        "the key holds 32 bytes"
    );
    assert_eq!(mode(&dir.join("mezame.db")), 0o600);
}

#[test]
fn missing_config_without_a_terminal_names_the_flags() {
    // Under a service manager or `docker compose up -d` the prompt cannot
    // be answered. The exit is non-zero, nothing is written, and the log
    // says what to run instead of only that the prompt failed.
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&[], tmp.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("No config at"), "{err}");
    assert!(err.contains("--bind"), "the way out is named: {err}");
    assert!(err.contains("--admin NAME --password-stdin"), "{err}");
    assert!(err.contains("--model ID"), "{err}");
    assert!(!config_at(tmp.path()).exists());
    assert!(
        !tmp.path().join(".mezame/master.key").exists(),
        "a refused setup writes no key"
    );
}

#[test]
fn init_without_flags_and_without_a_terminal_is_refused_before_it_writes() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init"], tmp.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("terminal"), "{err}");
    assert!(err.contains("--admin NAME --password-stdin"), "{err}");
    assert!(!tmp.path().join(".mezame").exists(), "nothing is created");
}

#[test]
fn init_with_bind_keeps_the_hosts_and_the_models_of_an_existing_config() {
    // `hosts` and `models` are keys a user edits by hand. A re-run keeps
    // both and names them.
    let tmp = home_with(
        r#"{"version":2,"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510","hosts":["mezame.example.com"]}],"models":["anthropic.claude-opus-5"],"public_url":"https://mezame.example.com"}"#,
    );
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert_success(&out);
    let printed = stdout(&out);
    assert!(printed.contains("Keeping hosts"), "{printed}");
    assert!(
        printed.contains("Keeping models from the existing config: anthropic.claude-opus-5"),
        "{printed}"
    );
    let cfg = read_config(tmp.path());
    assert_eq!(cfg["transports"][0]["bind"], "0.0.0.0:9510");
    assert_eq!(
        cfg["transports"][0]["hosts"],
        json!(["mezame.example.com"]),
        "the hosts list survives the re-run"
    );
    assert_eq!(cfg["models"], json!(["anthropic.claude-opus-5"]));
    assert_eq!(cfg["public_url"], "https://mezame.example.com");
}

#[test]
fn init_over_a_file_with_a_bedrock_section_drops_it_and_says_so() {
    // Phase 2 Requirement 9 criterion 1: the section's settings live in
    // the datastore; a version-2 file still carrying it is rewritten
    // without it, and the hosts are kept.
    let tmp = home_with(
        r#"{"version":2,"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510","hosts":["mezame.example.com"]}],"bedrock":{"model":"anthropic.claude-sonnet-5","region":"us-east-1"}}"#,
    );
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert_success(&out);
    let printed = stdout(&out);
    assert!(
        printed.contains("Dropping the `bedrock` section"),
        "{printed}"
    );
    let cfg = read_config(tmp.path());
    assert!(cfg.get("bedrock").is_none());
    assert_eq!(cfg["transports"][0]["hosts"], json!(["mezame.example.com"]));
    // The section is not migrated: the backend is the echo until
    // `--model` sets a profile.
    assert!(printed.contains("Backend: echo"), "{printed}");
}

#[test]
fn a_file_with_a_bedrock_section_refuses_to_start_pointing_at_the_datastore() {
    let tmp = home_with(
        r#"{"version":2,"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510"}],"bedrock":{"model":"m"}}"#,
    );
    let out = run_with_home(&[], tmp.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("`bedrock` section"), "{err}");
    assert!(err.contains("datastore"), "{err}");
    assert!(err.contains("mezame init --model ID"), "{err}");
    assert!(err.contains("config.json"), "{err}");
}

#[test]
fn a_rerun_over_a_file_that_does_not_parse_refuses_and_keeps_the_file() {
    // A broken file is not an absent one. Treating it as absent wrote a
    // fresh file with no hosts over it, with nothing said; now the run
    // refuses, names the file, and writes nothing.
    for body in [
        r#"{"version":2,"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510","hosts":["mezame.example.com"]}],"models":5}"#,
        r#"{"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510"}],}"#,
    ] {
        let tmp = home_with(body);
        for args in [
            &["init", "--bind", "0.0.0.0:9510"][..],
            &["init", "--model", "anthropic.claude-sonnet-5"][..],
        ] {
            let out = run_with_home(args, tmp.path());
            assert!(!out.status.success(), "{body}: {}", stdout(&out));
            let err = stderr(&out);
            assert!(err.contains("does not parse"), "{err}");
            assert!(err.contains("config.json"), "{err}");
            assert_eq!(
                std::fs::read_to_string(config_at(tmp.path())).unwrap(),
                body,
                "nothing was written"
            );
        }
    }
}

// ---------- phase 2: the admin, the key, the datastore ----------

#[test]
fn init_with_admin_and_password_stdin_creates_the_admin_and_the_files_end_to_end() {
    // Requirement 10 criterion 8: the flag path with no prompt, and what it
    // leaves behind. The password appears nowhere in the output.
    let tmp = TempDir::new().unwrap();
    let out = run_with_stdin(
        &[
            "init",
            "--bind",
            "0.0.0.0:9510",
            "--admin",
            "alice",
            "--password-stdin",
        ],
        tmp.path(),
        &format!("{PASSWORD}\nsecond line is ignored\n"),
    );
    assert_success(&out);
    let printed = stdout(&out);
    let dir = tmp.path().join(".mezame");
    assert!(dir.join("config.json").exists());
    assert!(dir.join("master.key").exists());
    assert!(dir.join("mezame.db").exists());
    assert!(printed.contains("Wrote "), "{printed}");
    assert!(
        printed.contains(&format!(
            "Datastore: sqlite {} (1 user)",
            dir.join("mezame.db").display()
        )),
        "{printed}"
    );
    assert!(printed.contains("Admin: created `alice`"), "{printed}");
    assert!(printed.contains("Backend: echo"), "{printed}");
    let all = format!("{printed}{}", stderr(&out));
    assert!(!all.contains(PASSWORD), "{all}");
    assert!(!all.contains("$argon2"), "{all}");

    let rows = bedrock_rows(tmp.path());
    assert_eq!(rows.users, vec![("alice".to_string(), Role::Admin)]);
    assert!(rows.model.is_none());
    assert!(rows.credentials.is_empty());
}

#[test]
fn init_with_admin_but_without_password_stdin_is_refused_naming_both_flags() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(
        &["init", "--bind", "0.0.0.0:9510", "--admin", "alice"],
        tmp.path(),
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("`--admin`"), "{err}");
    assert!(err.contains("`--password-stdin`"), "{err}");
    assert!(!config_at(tmp.path()).exists(), "nothing is written");
}

#[test]
fn init_with_password_stdin_and_an_empty_first_line_is_refused() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_stdin(
        &["init", "--admin", "alice", "--password-stdin"],
        tmp.path(),
        "\n",
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("first line"), "{}", stderr(&out));
    assert!(!config_at(tmp.path()).exists());
    assert!(bedrock_rows(tmp.path()).users.is_empty());
}

#[test]
fn init_refuses_a_short_password_and_creates_no_user() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_stdin(
        &["init", "--admin", "alice", "--password-stdin"],
        tmp.path(),
        "short\n",
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("at least 8 characters"),
        "{}",
        stderr(&out)
    );
    assert!(!config_at(tmp.path()).exists());
    assert!(bedrock_rows(tmp.path()).users.is_empty());
}

#[test]
fn init_with_the_bedrock_flags_writes_the_rows_and_names_no_region_or_profile() {
    // Requirement 10 criterion 8: the profile row, the credential row with
    // its one grant, the sealed payload, and a summary that names the
    // model and nothing of the credential.
    let tmp = TempDir::new().unwrap();
    let out = run_with_stdin(
        &[
            "init",
            "--bind",
            "0.0.0.0:9510",
            "--admin",
            "alice",
            "--password-stdin",
            "--model=global.anthropic.claude-sonnet-5",
            "--region",
            "eu-west-1",
            "--profile=prof-xyz-77",
        ],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&out);
    let cfg = read_config(tmp.path());
    assert_eq!(cfg["transports"][0]["bind"], "0.0.0.0:9510");
    assert!(
        cfg.get("bedrock").is_none(),
        "nothing of Bedrock in the file"
    );

    let printed = stdout(&out);
    assert!(printed.contains("Wrote "), "{printed}");
    assert!(
        printed.contains("Backend: Bedrock global.anthropic.claude-sonnet-5"),
        "{printed}"
    );
    assert!(printed.contains("Credentials come from the AWS chain: aws configure, aws sso login, AWS_PROFILE or the AWS_ACCESS_KEY_ID variables."), "{printed}");
    assert!(printed.contains("Enable access to global.anthropic.claude-sonnet-5 in the Bedrock console for the region you use."), "{printed}");
    assert!(
        printed
            .contains("use an inference profile id: the base id under a `global.` or geo prefix."),
        "{printed}"
    );
    let all = format!("{printed}{}", stderr(&out));
    assert!(!all.contains("eu-west-1"), "no region in the output: {all}");
    assert!(
        !all.contains("prof-xyz-77"),
        "no profile in the output: {all}"
    );
    assert!(!all.contains(PASSWORD), "{all}");

    let rows = bedrock_rows(tmp.path());
    assert_eq!(
        rows.model.as_deref(),
        Some("global.anthropic.claude-sonnet-5")
    );
    assert_eq!(rows.credentials.len(), 1);
    assert_eq!(
        rows.credentials[0].1, "Bedrock",
        "the label is the fixed string"
    );
    assert_eq!(
        rows.payload,
        Some(json!({ "region": "eu-west-1", "profile": "prof-xyz-77" }))
    );
    assert_eq!(grant_count(tmp.path(), &rows.credentials[0].0), 1);

    // A bare base id gets the worked example.
    let tmp = TempDir::new().unwrap();
    let out = run_with_stdin(
        &[
            "init",
            "--admin",
            "alice",
            "--password-stdin",
            "--model",
            "anthropic.claude-sonnet-5",
        ],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&out);
    assert!(
        stdout(&out)
            .contains("use an inference profile id such as global.anthropic.claude-sonnet-5."),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        read_config(tmp.path())["transports"][0]["bind"],
        "127.0.0.1:9510",
        "no file and no flag: the default bind"
    );
}

#[test]
fn a_second_init_keeps_the_admin_and_replaces_the_credential_leaving_one_grant() {
    // Requirement 10 criterion 2: replacing, not adding. The second run
    // names no admin and skips the question; a flag replaces its setting
    // and the rest of the payload is carried.
    let tmp = TempDir::new().unwrap();
    let first = run_with_stdin(
        &[
            "init",
            "--admin",
            "alice",
            "--password-stdin",
            "--model",
            "anthropic.claude-sonnet-5",
            "--region",
            "us-east-1",
            "--profile",
            "old-profile",
        ],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&first);
    let before = bedrock_rows(tmp.path());
    let old_id = before.credentials[0].0.clone();

    let second = run_with_home(&["init", "--profile", "new-profile"], tmp.path());
    assert_success(&second);
    let printed = stdout(&second);
    assert!(printed.contains("Admin: kept"), "{printed}");
    assert!(
        printed.contains("Backend: Bedrock anthropic.claude-sonnet-5"),
        "{printed}"
    );
    assert!(!printed.contains("new-profile"), "{printed}");

    let after = bedrock_rows(tmp.path());
    assert_eq!(
        after.users,
        vec![("alice".to_string(), Role::Admin)],
        "the user is kept"
    );
    assert_eq!(
        after.model.as_deref(),
        Some("anthropic.claude-sonnet-5"),
        "the model is carried"
    );
    assert_eq!(after.credentials.len(), 1, "one credential, not two");
    assert_ne!(after.credentials[0].0, old_id, "a new row");
    assert_eq!(
        after.payload,
        Some(json!({ "region": "us-east-1", "profile": "new-profile" })),
        "the region is carried and the profile replaced"
    );
    assert_eq!(
        grant_count(tmp.path(), &after.credentials[0].0),
        1,
        "one grant on the new id"
    );
    assert_eq!(
        grant_count(tmp.path(), &old_id),
        0,
        "the old grant went with its row"
    );

    // `--admin` on a datastore with a user is said to be ignored, not refused.
    let third = run_with_stdin(
        &["init", "--admin", "bob", "--password-stdin"],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&third);
    assert!(
        stdout(&third).contains("`--admin` is ignored"),
        "{}",
        stdout(&third)
    );
    assert_eq!(bedrock_rows(tmp.path()).users.len(), 1);

    // A model alone replaces the model and keeps the credential's payload.
    let fourth = run_with_home(&["init", "--model", "anthropic.claude-opus-5"], tmp.path());
    assert_success(&fourth);
    let rows = bedrock_rows(tmp.path());
    assert_eq!(rows.model.as_deref(), Some("anthropic.claude-opus-5"));
    assert_eq!(rows.credentials.len(), 1);
    assert_eq!(
        rows.payload,
        Some(json!({ "region": "us-east-1", "profile": "new-profile" }))
    );

    // No flag clears a setting: an empty value is refused and the rows stand.
    for blank in ["", "  "] {
        let out = run_with_home(&["init", "--region", blank], tmp.path());
        assert!(!out.status.success(), "{blank:?}");
        assert!(
            stderr(&out).contains("`--region` needs a value"),
            "{}",
            stderr(&out)
        );
    }
    assert_eq!(
        bedrock_rows(tmp.path()).payload,
        Some(json!({ "region": "us-east-1", "profile": "new-profile" }))
    );
}

#[test]
fn a_bind_only_rerun_keeps_the_bedrock_rows_and_names_the_model() {
    let tmp = TempDir::new().unwrap();
    let first = run_with_stdin(
        &[
            "init",
            "--admin",
            "alice",
            "--password-stdin",
            "--model",
            "anthropic.claude-sonnet-5",
        ],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&first);
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert_success(&out);
    assert!(
        stdout(&out).contains("Backend: Bedrock anthropic.claude-sonnet-5"),
        "{}",
        stdout(&out)
    );
    let rows = bedrock_rows(tmp.path());
    assert_eq!(rows.model.as_deref(), Some("anthropic.claude-sonnet-5"));
    assert_eq!(rows.credentials.len(), 1);
    assert_eq!(
        read_config(tmp.path())["transports"][0]["bind"],
        "0.0.0.0:9510"
    );
}

#[test]
fn init_with_region_or_profile_and_no_model_anywhere_is_refused() {
    for flags in [
        &["init", "--region", "us-east-1"][..],
        &["init", "--profile=work"][..],
    ] {
        let tmp = TempDir::new().unwrap();
        let out = run_with_home(flags, tmp.path());
        assert!(!out.status.success(), "{flags:?}");
        assert!(stderr(&out).contains("--model"), "{}", stderr(&out));
        assert!(!config_at(tmp.path()).exists(), "nothing is written");
    }
}

#[test]
fn a_model_with_no_admin_anywhere_is_refused_naming_the_admin_flags() {
    // The credential is granted to an admin; with none there is nothing to
    // grant it to, and the file is not written either.
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(
        &["init", "--model", "anthropic.claude-sonnet-5"],
        tmp.path(),
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("--admin NAME --password-stdin"), "{err}");
    assert!(!config_at(tmp.path()).exists());
    let rows = bedrock_rows(tmp.path());
    assert!(rows.model.is_none());
    assert!(rows.credentials.is_empty());
}

#[test]
fn init_with_a_blank_model_or_a_repeated_flag_is_refused() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--model", "  "], tmp.path());
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("`--model` needs a value"),
        "{}",
        stderr(&out)
    );
    let out = run_with_home(&["init", "--model", "a", "--model=b"], tmp.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains("twice"), "{}", stderr(&out));
    assert!(!config_at(tmp.path()).exists());
}

#[test]
fn an_unknown_flag_lists_the_six_accepted_ones() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--models", "x"], tmp.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    for flag in [
        "--bind ADDR",
        "--admin NAME",
        "--password-stdin",
        "--model ID",
        "--region NAME",
        "--profile NAME",
    ] {
        assert!(err.contains(flag), "{err}");
    }
}

#[test]
fn help_names_the_six_init_flags_the_commands_and_the_files() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["--help"], tmp.path());
    let help = stdout(&out);
    for text in [
        "--bind ADDR",
        "--admin NAME",
        "--password-stdin",
        "--model ID",
        "--region NAME",
        "--profile NAME",
        "user add NAME",
        "user list",
        "passwd NAME",
        "FILES",
        "mezame.db",
        "master.key",
    ] {
        assert!(help.contains(text), "{help}");
    }
    let out = run_with_home(&[], tmp.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--model ID"), "{}", stderr(&out));
}

#[test]
fn the_legacy_state_file_is_removed_and_reported() {
    // Requirement 7 criterion 7 through the binary, and the helper alone.
    let tmp =
        home_with(r#"{"version":2,"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510"}]}"#);
    let state = tmp.path().join(".mezame/state.json");
    std::fs::write(&state, b"{}").unwrap();
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert_success(&out);
    assert!(!state.exists());
    assert!(
        stdout(&out).contains(&format!("Removed {}", state.display())),
        "{}",
        stdout(&out)
    );
    // A second run finds nothing and says nothing of it.
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert_success(&out);
    assert!(!stdout(&out).contains("Removed"), "{}", stdout(&out));

    use mezame::init::remove_legacy_state_file;
    let dir = tmp.path().join(".mezame");
    std::fs::write(dir.join("state.json"), b"{}").unwrap();
    assert_eq!(
        remove_legacy_state_file(&dir).expect("removal succeeds"),
        Some(dir.join("state.json"))
    );
    assert!(dir.join("config.json").exists(), "only the one file goes");
    assert_eq!(
        remove_legacy_state_file(&dir).expect("absence is fine"),
        None
    );
    assert_eq!(
        remove_legacy_state_file(&tmp.path().join("nowhere")).expect("absence"),
        None
    );
}

#[test]
fn init_never_rewrites_an_existing_key() {
    // Requirement 4 criterion 1: a key that is there is read, byte for
    // byte, on every run.
    let tmp = TempDir::new().unwrap();
    let first = run_with_stdin(
        &[
            "init",
            "--admin",
            "alice",
            "--password-stdin",
            "--model",
            "anthropic.claude-sonnet-5",
        ],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&first);
    let key_path = tmp.path().join(".mezame/master.key");
    let key = std::fs::read(&key_path).unwrap();
    assert_eq!(key.len(), 32);

    let second = run_with_home(&["init", "--bind", "0.0.0.0:9511"], tmp.path());
    assert_success(&second);
    assert_eq!(
        std::fs::read(&key_path).unwrap(),
        key,
        "the key is untouched"
    );
    // And the credential sealed under it still opens.
    let rows = bedrock_rows(tmp.path());
    assert_eq!(
        rows.payload,
        Some(json!({ "region": null, "profile": null }))
    );
}

#[cfg(unix)]
#[test]
fn a_key_of_the_wrong_mode_or_length_is_refused_by_init_too() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    let key_path = dir.join("master.key");
    std::fs::write(&key_path, [7u8; 32]).unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("master.key"), "{err}");
    assert!(err.contains("0600"), "{err}");
    assert!(!config_at(tmp.path()).exists());

    std::fs::write(&key_path, [7u8; 31]).unwrap();
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains("32"), "{}", stderr(&out));
}

#[test]
fn init_beside_a_datastore_with_no_key_drops_its_credentials_and_says_how_many() {
    // Requirement 4 criterion 1, the last clause: rows sealed under a key
    // that is gone cannot be opened, so they go, with their profiles, and
    // the run says so.
    let tmp = TempDir::new().unwrap();
    let first = run_with_stdin(
        &[
            "init",
            "--admin",
            "alice",
            "--password-stdin",
            "--model",
            "anthropic.claude-sonnet-5",
        ],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&first);
    let key_path = tmp.path().join(".mezame/master.key");
    std::fs::remove_file(&key_path).unwrap();

    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert_success(&out);
    let printed = stdout(&out);
    assert!(
        printed.contains("Dropped 1 credential row(s)"),
        "the count is named: {printed}"
    );
    assert!(
        printed.contains("sealed under a key that is not"),
        "{printed}"
    );
    assert!(
        printed.contains("Backend: echo"),
        "the profile went with the row: {printed}"
    );
    assert!(key_path.exists(), "a new key was written");
    let rows = bedrock_rows(tmp.path());
    assert!(rows.credentials.is_empty());
    assert!(rows.model.is_none());
    assert_eq!(rows.users.len(), 1, "the users stay");

    // With the new key a model can be set again.
    let again = run_with_home(&["init", "--model", "anthropic.claude-opus-5"], tmp.path());
    assert_success(&again);
    let rows = bedrock_rows(tmp.path());
    assert_eq!(rows.model.as_deref(), Some("anthropic.claude-opus-5"));
    assert_eq!(rows.credentials.len(), 1);
}

#[test]
fn a_datastore_holding_only_plain_users_still_gets_its_admin_from_init() {
    // `mezame user add` runs before any `init` and creates the datastore
    // itself. The admin question is settled by an admin row, not by any
    // user row, so `--admin` still creates one here and the credential has
    // an owner to be granted to.
    let tmp = TempDir::new().unwrap();
    let out = run_with_stdin(
        &["user", "add", "bob", "--password-stdin"],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&out);
    let out = run_with_stdin(
        &[
            "init",
            "--admin",
            "root",
            "--password-stdin",
            "--model",
            "anthropic.claude-sonnet-5",
        ],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&out);
    let printed = stdout(&out);
    assert!(printed.contains("Admin: created `root`"), "{printed}");
    assert!(
        printed.contains("Backend: Bedrock anthropic.claude-sonnet-5"),
        "{printed}"
    );
    let rows = bedrock_rows(tmp.path());
    assert_eq!(
        rows.users,
        vec![
            ("bob".to_string(), Role::User),
            ("root".to_string(), Role::Admin)
        ]
    );
    assert_eq!(rows.credentials.len(), 1);
    assert_eq!(grant_count(tmp.path(), &rows.credentials[0].0), 1);

    // With an admin in place, `--admin` is skipped and the message names
    // the command that adds another.
    let out = run_with_stdin(
        &["init", "--admin", "other", "--password-stdin"],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&out);
    assert!(
        stdout(&out).contains("`mezame user add NAME --admin`"),
        "{}",
        stdout(&out)
    );
    assert_eq!(bedrock_rows(tmp.path()).users.len(), 2);

    // Without one, `--model` alone names both ways to get an admin.
    let tmp = TempDir::new().unwrap();
    let out = run_with_stdin(
        &["user", "add", "bob", "--password-stdin"],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&out);
    let out = run_with_home(
        &["init", "--model", "anthropic.claude-sonnet-5"],
        tmp.path(),
    );
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("--admin NAME --password-stdin"), "{err}");
    assert!(err.contains("mezame user add NAME --admin"), "{err}");
    assert!(!stdout(&out).contains("Admin: kept"), "{}", stdout(&out));
}

#[test]
fn a_key_that_cannot_open_the_rows_drops_them_even_when_the_datastore_has_sessions() {
    // The rows sealed under a lost key go, with the profile that used them,
    // and a session that ran on that profile stays, unlinked. The session
    // is the case that used to fail: its row referenced the profile.
    let tmp = TempDir::new().unwrap();
    let first = run_with_stdin(
        &[
            "init",
            "--admin",
            "alice",
            "--password-stdin",
            "--model",
            "anthropic.claude-sonnet-5",
        ],
        tmp.path(),
        PASSWORD,
    );
    assert_success(&first);
    {
        let store = open_store(tmp.path());
        block_on(async {
            let alice = store.user_by_name("alice").await.unwrap().unwrap();
            let session = store
                .create_session(&alice.id, "s1", None, 5)
                .await
                .unwrap();
            assert!(
                session.profile_id.is_some(),
                "the session runs on the profile"
            );
        });
    }
    let key_path = tmp.path().join(".mezame/master.key");

    // The key is gone entirely.
    std::fs::remove_file(&key_path).unwrap();
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert_success(&out);
    assert!(
        stdout(&out).contains("Dropped 1 credential row(s)"),
        "{}",
        stdout(&out)
    );
    let rows = bedrock_rows(tmp.path());
    assert!(rows.credentials.is_empty());
    assert!(rows.model.is_none());
    {
        let store = open_store(tmp.path());
        block_on(async {
            let session = store
                .session("s1")
                .await
                .unwrap()
                .expect("the session stays");
            assert_eq!(
                session.profile_id, None,
                "unlinked from the dropped profile"
            );
        });
    }

    // A run that made a fresh key but failed before the drop: the rows are
    // still unopenable, so the next run drops them all the same.
    let again = run_with_home(&["init", "--model", "anthropic.claude-opus-5"], tmp.path());
    assert_success(&again);
    std::fs::remove_file(&key_path).unwrap();
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&key_path)
            .unwrap()
            .write_all(&[3u8; 32])
            .unwrap();
    }
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert_success(&out);
    assert!(
        stdout(&out).contains("Dropped 1 credential row(s)"),
        "{}",
        stdout(&out)
    );
    assert!(stdout(&out).contains("Backend: echo"), "{}", stdout(&out));
    assert!(bedrock_rows(tmp.path()).credentials.is_empty());
}
