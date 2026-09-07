//! `mezame init --bind ADDR`, the non-interactive setup (Requirement 15
//! criterion 12, added 2026-09-07), and what the missing-config path says
//! when no terminal is attached. A new file rather than cases in
//! `tests/cli_binary.rs`, which Requirement 17 criterion 7 holds to its
//! merge-base cases.
//!
//! Every case runs the binary with its own temporary `HOME` and its output
//! captured, so standard error is a pipe. `dialoguer` checks that stream
//! before it prompts and would otherwise take keys from `/dev/tty`, not
//! standard input, so an accidental prompt fails with `not a terminal`
//! instead of hanging. Standard input is closed as well, for anything that
//! reads it directly.

use std::io::Write as _;
use std::process::{Command, Stdio};

use serde_json::Value;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mezame")
}

fn run_with_home(args: &[&str], home: &std::path::Path) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .expect("spawn mezame")
}

fn config_at(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".mezame/config.json")
}

fn read_config(home: &std::path::Path) -> Value {
    let raw = std::fs::read_to_string(config_at(home)).expect("config.json exists");
    serde_json::from_str(&raw).expect("config.json is JSON")
}

#[test]
fn init_with_bind_writes_the_config_without_a_prompt() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert!(
        out.status.success(),
        "exit 0, got {:?}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Wrote"),
        "the path written is reported"
    );

    let cfg = read_config(tmp.path());
    let keys: Vec<&String> = cfg.as_object().expect("an object").keys().collect();
    assert_eq!(keys, vec!["transports"], "transports is the only key");
    assert_eq!(
        cfg["transports"],
        serde_json::json!([{ "kind": "cloudflared", "bind": "0.0.0.0:9510" }]),
        "the entry holds the kind and the bind, and no hosts key"
    );
}

#[test]
fn init_with_bind_in_equals_form_is_accepted() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--bind=127.0.0.1:9511"], tmp.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
            String::from_utf8_lossy(&out.stderr).contains("Bind address is required"),
            "the refusal names the check: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!config_at(tmp.path()).exists(), "nothing is written");
    }
}

#[test]
fn init_with_bind_missing_its_value_is_refused() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--bind"], tmp.path());
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--bind"));
    assert!(!config_at(tmp.path()).exists());
}

#[test]
fn init_with_an_unknown_argument_is_refused() {
    // A typo used to drop into the prompt as if no argument were given.
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--bogus"], tmp.path());
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Unknown argument"));
    assert!(!config_at(tmp.path()).exists());
}

#[test]
fn init_with_bind_overwrites_an_existing_config() {
    // The same re-run semantics as the prompt: the file is replaced, and
    // the keys a 0.13.x release wrote go with it.
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join("config.json")).unwrap();
    f.write_all(
        br#"{"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510"}],"agent_cmd":"kiro-cli","agent_args":["acp"]}"#,
    )
    .unwrap();
    drop(f);

    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cfg = read_config(tmp.path());
    assert_eq!(cfg["transports"][0]["bind"], "0.0.0.0:9510");
    assert!(cfg.get("agent_cmd").is_none(), "the old keys are gone");
}

#[cfg(unix)]
#[test]
fn init_with_bind_writes_an_owner_only_directory_and_file() {
    // Requirement 15 criterion 9 as amended, end to end through the binary.
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--bind", "127.0.0.1:9510"], tmp.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&tmp.path().join(".mezame")), 0o700);
    assert_eq!(mode(&config_at(tmp.path())), 0o600);
}

#[test]
fn missing_config_without_a_terminal_names_the_bind_flag() {
    // Under a service manager or `docker compose up -d` the prompt cannot
    // be answered. The exit is non-zero, nothing is written, and the log
    // now says what to run instead of only that the prompt failed.
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&[], tmp.path());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("No config at"), "{stderr}");
    assert!(stderr.contains("--bind"), "the way out is named: {stderr}");
    assert!(!config_at(tmp.path()).exists());
}

#[test]
fn help_names_the_bind_flag() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["--help"], tmp.path());
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--bind"));
}

#[test]
fn init_with_bind_keeps_the_hosts_of_an_existing_config() {
    // `hosts` is the key a user edits by hand. A re-run used to drop it,
    // and a tunnel user then had every request answered 421 with nothing
    // said; the list is kept and named.
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        br#"{"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510","hosts":["mezame.example.com"]}]}"#,
    )
    .unwrap();

    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Keeping hosts"),
        "the kept list is named on stdout"
    );
    let cfg = read_config(tmp.path());
    assert_eq!(cfg["transports"][0]["bind"], "0.0.0.0:9510");
    assert_eq!(
        cfg["transports"][0]["hosts"],
        serde_json::json!(["mezame.example.com"]),
        "the hosts list survives the re-run"
    );
}
