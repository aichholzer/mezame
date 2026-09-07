//! `mezame init` with flags, the non-interactive setup (phase 0
//! Requirement 15 criterion 12; phase 1 Requirement 3 adds `--model`,
//! `--region` and `--profile`), and what the missing-config path says when
//! no terminal is attached. A new file rather than cases in
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

// ---------- phase 1: the Bedrock flags ----------

fn home_with(body: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), body).unwrap();
    tmp
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn init_with_model_region_and_profile_writes_the_section_in_both_flag_forms() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(
        &[
            "init",
            "--bind",
            "0.0.0.0:9510",
            "--model=global.anthropic.claude-sonnet-5",
            "--region",
            "eu-west-1",
            "--profile=work",
        ],
        tmp.path(),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let cfg = read_config(tmp.path());
    assert_eq!(cfg["transports"][0]["bind"], "0.0.0.0:9510");
    assert_eq!(
        cfg["bedrock"],
        serde_json::json!({ "model": "global.anthropic.claude-sonnet-5", "region": "eu-west-1", "profile": "work" }),
        "the three keys and nothing unset"
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
        !printed.contains("global.global."),
        "a profile id gets no second prefix: {printed}"
    );
    assert!(
        printed
            .contains("use an inference profile id: the base id under a `global.` or geo prefix."),
        "{printed}"
    );

    // A bare base id gets the worked example.
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(
        &["init", "--model", "anthropic.claude-sonnet-5"],
        tmp.path(),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out)
            .contains("use an inference profile id such as global.anthropic.claude-sonnet-5."),
        "{}",
        stdout(&out)
    );
}

#[test]
fn init_with_bind_alone_names_the_echo_backend() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--bind", "127.0.0.1:9510"], tmp.path());
    assert!(out.status.success(), "{}", stderr(&out));
    let printed = stdout(&out);
    assert!(printed.contains("Backend: echo"), "{printed}");
    assert!(!printed.contains("Credentials come from"), "{printed}");
    assert!(read_config(tmp.path()).get("bedrock").is_none());
}

#[test]
fn init_with_model_alone_and_no_file_writes_the_default_bind() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(
        &["init", "--model", "anthropic.claude-sonnet-5"],
        tmp.path(),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let cfg = read_config(tmp.path());
    assert_eq!(cfg["transports"][0]["bind"], "127.0.0.1:9510");
    assert_eq!(cfg["bedrock"]["model"], "anthropic.claude-sonnet-5");
}

#[test]
fn init_with_model_alone_keeps_an_existing_bind_and_hosts() {
    let tmp = home_with(
        r#"{"transports":[{"kind":"cloudflared","bind":"0.0.0.0:9511","hosts":["mezame.example.com"]}]}"#,
    );
    let out = run_with_home(&["init", "--model", "anthropic.claude-opus-5"], tmp.path());
    assert!(out.status.success(), "{}", stderr(&out));
    let cfg = read_config(tmp.path());
    assert_eq!(cfg["transports"][0]["bind"], "0.0.0.0:9511");
    assert_eq!(
        cfg["transports"][0]["hosts"],
        serde_json::json!(["mezame.example.com"])
    );
    assert_eq!(cfg["bedrock"]["model"], "anthropic.claude-opus-5");
}

#[test]
fn init_with_bind_alone_keeps_an_existing_bedrock_section_and_says_so() {
    let tmp = home_with(
        r#"{"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510"}],
            "bedrock":{"model":"anthropic.claude-sonnet-5","models":["anthropic.claude-opus-5"],"region":"us-east-1","thinking":"off","thinking_budget":2048,"max_output_tokens":9000}}"#,
    );
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Keeping the Bedrock settings from the existing config"),
        "{}",
        stdout(&out)
    );
    let cfg = read_config(tmp.path());
    assert_eq!(cfg["transports"][0]["bind"], "0.0.0.0:9510");
    assert_eq!(
        cfg["bedrock"],
        serde_json::json!({ "model": "anthropic.claude-sonnet-5", "models": ["anthropic.claude-opus-5"], "region": "us-east-1", "thinking": "off", "thinking_budget": 2048, "max_output_tokens": 9000 }),
        "the section survives untouched"
    );
}

#[test]
fn init_with_a_bedrock_flag_replaces_that_key_and_carries_the_rest() {
    let tmp = home_with(
        r#"{"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510"}],
            "bedrock":{"model":"anthropic.claude-sonnet-5","region":"us-east-1","profile":"old","thinking":"off"}}"#,
    );
    let out = run_with_home(&["init", "--profile", "new"], tmp.path());
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        !stdout(&out).contains("Keeping the Bedrock settings"),
        "{}",
        stdout(&out)
    );
    let cfg = read_config(tmp.path());
    assert_eq!(
        cfg["bedrock"],
        serde_json::json!({ "model": "anthropic.claude-sonnet-5", "region": "us-east-1", "profile": "new", "thinking": "off" })
    );
    // No flag clears a key: an empty value is refused like an empty bind.
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
        read_config(tmp.path())["bedrock"]["region"],
        "us-east-1",
        "the file is untouched"
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
fn an_unknown_flag_lists_the_four_accepted_ones() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["init", "--models", "x"], tmp.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    for flag in [
        "--bind ADDR",
        "--model ID",
        "--region NAME",
        "--profile NAME",
    ] {
        assert!(err.contains(flag), "{err}");
    }
}

#[test]
fn help_names_the_four_init_flags_and_the_no_terminal_remedy_names_model() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["--help"], tmp.path());
    let help = stdout(&out);
    for flag in [
        "--bind ADDR",
        "--model ID",
        "--region NAME",
        "--profile NAME",
    ] {
        assert!(help.contains(flag), "{help}");
    }
    let out = run_with_home(&[], tmp.path());
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--model ID"), "{}", stderr(&out));
}

#[test]
fn a_file_whose_bedrock_section_is_invalid_refuses_to_start() {
    let tmp = home_with(
        r#"{"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510"}],"bedrock":{"model":"m","thinking":"budget"}}"#,
    );
    let out = run_with_home(&[], tmp.path());
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("`bedrock.thinking`"), "{err}");
    assert!(err.contains("config.json"), "{err}");
}

#[test]
fn a_bind_only_rerun_on_a_file_with_a_bad_bedrock_value_refuses_and_keeps_the_file() {
    // The existing file parses but its section fails one check. A re-run
    // used to read it through the validating loader, treat it as absent,
    // and silently drop both the hosts and the section. Now the section is
    // carried forward and refused with the key named; the file stays.
    let body = r#"{"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510","hosts":["mezame.example.com"]}],"bedrock":{"model":"m","thinking":"budget"}}"#;
    let tmp = home_with(body);
    let out = run_with_home(&["init", "--bind", "0.0.0.0:9510"], tmp.path());
    assert!(!out.status.success(), "{}", stdout(&out));
    let err = stderr(&out);
    assert!(err.contains("`bedrock.thinking`"), "{err}");
    assert_eq!(
        std::fs::read_to_string(config_at(tmp.path())).unwrap(),
        body,
        "nothing was written"
    );
    // Fixing the value is a hand edit; a flag that sets the model alone
    // still carries the bad key and is still refused.
    let out = run_with_home(
        &["init", "--model", "anthropic.claude-sonnet-5"],
        tmp.path(),
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("`bedrock.thinking`"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn init_never_writes_a_section_the_next_start_would_refuse() {
    // A small output ceiling is fine under an adaptive model. Switching
    // the model to one that sends a budget makes the carried ceiling too
    // small for the default budget, and the write is refused rather than
    // leaving a file that fails on the next start.
    let tmp = home_with(
        r#"{"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510"}],"bedrock":{"model":"anthropic.claude-sonnet-5","max_output_tokens":2000}}"#,
    );
    let out = run_with_home(
        &[
            "init",
            "--model",
            "anthropic.claude-haiku-4-5-20251001-v1:0",
        ],
        tmp.path(),
    );
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(
        stderr(&out).contains("`bedrock.thinking_budget`"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        read_config(tmp.path())["bedrock"]["model"],
        "anthropic.claude-sonnet-5",
        "the file is untouched"
    );
    // And what init writes, mezame starts on: the written file loads.
    let out = run_with_home(
        &[
            "init",
            "--model",
            "anthropic.claude-sonnet-5",
            "--region",
            "us-east-1",
        ],
        tmp.path(),
    );
    assert!(out.status.success(), "{}", stderr(&out));
}
