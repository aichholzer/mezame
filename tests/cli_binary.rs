//! End-to-end tests for the CLI entry points in `src/main.rs` and
//! `mezame::run()` (`src/lib.rs`). Those two files are pure process glue:
//! argument dispatch, help/version output, config discovery, and the
//! transport-selection `match`. None of it is reachable from in-process
//! unit tests without standing up a tokio runtime and a real server, so
//! we drive the compiled binary as a subprocess instead.
//!
//! Cargo exposes the built binary to integration tests via
//! `CARGO_BIN_EXE_mezame`. Running it as a child process still counts
//! toward coverage under `cargo llvm-cov`: the child inherits the
//! profile-file pattern and writes its own `.profraw`.
//!
//! Each test points the child at its own temp `HOME` via `Command::env`,
//! so nothing here mutates the parent process environment and the tests
//! need no shared lock.

use std::io::Write;
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// Absolute path to the freshly built `mezame` binary, injected by Cargo
/// for integration tests.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mezame")
}

/// Run the binary with `args` and an explicit `HOME`, with stdin closed.
/// An accidental interactive prompt then fails fast; open stdin would
/// leave the test hanging.
fn run_with_home(args: &[&str], home: &std::path::Path) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .expect("spawn mezame")
}

/// Write a `config.json` under `<home>/.mezame/` and return the temp dir
/// that owns it. Keep the `TempDir` alive for the duration of the test.
fn home_with_config(body: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join("config.json")).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    tmp
}

#[test]
fn version_flag_prints_version_and_exits_zero() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["--version"], tmp.path());

    assert!(out.status.success(), "--version should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("mezame {}", env!("CARGO_PKG_VERSION"))),
        "unexpected --version output: {stdout}"
    );
}

#[test]
fn short_version_flag_matches_long_form() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["-V"], tmp.path());

    assert!(out.status.success(), "-V should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "unexpected -V output: {stdout}"
    );
}

#[test]
fn help_flag_prints_usage_and_subcommands() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["--help"], tmp.path());

    assert!(out.status.success(), "--help should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("USAGE"), "help missing USAGE: {stdout}");
    assert!(
        stdout.contains("SUBCOMMANDS"),
        "help missing SUBCOMMANDS: {stdout}"
    );
    assert!(
        stdout.contains("init"),
        "help should mention the init subcommand: {stdout}"
    );
}

#[test]
fn short_help_flag_matches_long_form() {
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&["-h"], tmp.path());

    assert!(out.status.success(), "-h should exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("USAGE"), "help missing USAGE: {stdout}");
}

#[test]
fn empty_transports_config_bails() {
    // A well-formed config with no transports must fail loudly. Doing
    // nothing in silence is the failure mode this guards. It exercises
    // the `[]` arm of run()'s transport match, plus the full
    // config-discovery and runtime-build path that precedes it.
    let tmp = home_with_config(r#"{ "version": 2, "transports": [] }"#);
    let out = run_with_home(&[], tmp.path());

    assert!(
        !out.status.success(),
        "empty transports should be a non-zero exit"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("No transports configured"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn multiple_transports_config_bails() {
    // Multi-transport is parsed and not yet runnable. run() bails on the
    // `_` arm. Serving only the first entry in silence is the failure
    // mode this guards.
    let body = r#"{
        "version": 2,
        "transports": [
            { "kind": "cloudflared", "bind": "127.0.0.1:9510" },
            { "kind": "cloudflared", "bind": "127.0.0.1:9511" }
        ]
    }"#;
    let tmp = home_with_config(body);
    let out = run_with_home(&[], tmp.path());

    assert!(
        !out.status.success(),
        "multiple transports should be a non-zero exit"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("more than one transport"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn missing_config_reports_and_attempts_setup() {
    // No config on disk: run() announces the missing file and drops into
    // interactive setup. With stdin closed the prompt cannot succeed, so
    // the process exits non-zero, but it must first print where it looked.
    let tmp = TempDir::new().unwrap();
    let out = run_with_home(&[], tmp.path());

    assert!(
        !out.status.success(),
        "missing config with no stdin should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("No config at"),
        "should report the missing config path: {stderr}"
    );
    // A prompt that cannot be answered writes nothing. A half-written
    // config would be served on the next start.
    assert!(
        !tmp.path().join(".mezame/config.json").exists(),
        "no config.json is written when standard input cannot be read"
    );
}

#[test]
fn a_start_with_no_config_beside_a_keyless_datastore_refuses_and_writes_nothing() {
    // Requirement 4 criterion 1: with no config the start would fall into
    // the setup, and the setup writes a new key and drops every sealed
    // row. A datastore whose key is gone is refused first, with the line
    // the served start uses, and nothing under the home changes.
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("mezame.db");
    let key_path = dir.join("master.key");
    std::fs::write(&db_path, b"whatever the datastore holds").unwrap();

    let out = run_with_home(&[], tmp.path());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&key_path.display().to_string()), "{stderr}");
    assert!(stderr.contains(&db_path.display().to_string()), "{stderr}");
    assert!(stderr.contains("backup"), "{stderr}");
    assert!(stderr.contains("mezame init"), "{stderr}");
    assert_eq!(
        stderr.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "one line, before the setup is offered: {stderr}"
    );
    assert!(!key_path.exists(), "no key is written");
    assert!(!dir.join("config.json").exists(), "no config is written");
    assert_eq!(
        std::fs::read(&db_path).unwrap(),
        b"whatever the datastore holds",
        "the datastore is untouched"
    );
}

// ---------- phase 2: startup over the datastore ----------

/// Run `init` with `flags` and the admin `alice` under `home`, the
/// password piped in.
fn init_with_admin(home: &std::path::Path, flags: &[&str]) {
    let mut args = vec!["init", "--admin", "alice", "--password-stdin"];
    args.extend_from_slice(flags);
    let mut child = Command::new(bin())
        .args(&args)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mezame");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"correct horse battery\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "init: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Spawn the binary on `home` with no AWS credentials in its environment
/// and a region set, read stderr until it reports listening or exits,
/// fetch `/`, and return the `Backend:` line, the `Datastore:` line, the
/// response status line and whether it started.
fn start_and_fetch_root(home: &std::path::Path, port: u16) -> (String, String, String, bool) {
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    let mut child = Command::new(bin())
        .env("HOME", home)
        // No credentials: startup must not need any. A region, so the
        // SDK's chain never probes the instance metadata service.
        .env_remove("AWS_ACCESS_KEY_ID")
        .env_remove("AWS_SECRET_ACCESS_KEY")
        .env_remove("AWS_SESSION_TOKEN")
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_BEARER_TOKEN_BEDROCK")
        .env("AWS_REGION", "us-east-1")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mezame");
    let stderr = child.stderr.take().unwrap();
    let mut lines = BufReader::new(stderr).lines();
    let mut backend_line = String::new();
    let mut datastore_line = String::new();
    let mut started = false;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            Instant::now() < deadline,
            "the binary did not report startup in time"
        );
        let Some(line) = lines.next() else {
            break; // stderr closed: the process exited
        };
        let line = line.expect("a line");
        if line.starts_with("Backend:") {
            backend_line = line.clone();
        }
        if line.starts_with("Datastore:") {
            datastore_line = line.clone();
        }
        if line.contains("listening on") {
            started = true;
            break;
        }
    }
    let mut status = String::new();
    if started {
        for _ in 0..50 {
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                write!(
                    stream,
                    "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                let mut response = String::new();
                let _ = stream.read_to_string(&mut response);
                status = response.lines().next().unwrap_or_default().to_string();
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    (backend_line, datastore_line, status, started)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn a_bedrock_profile_starts_with_no_credentials_and_names_the_model_alone() {
    // Phase 2 Requirement 10 criterion 7: the startup line names the model
    // and drops phase 1's region and profile suffix.
    let port = free_port();
    let tmp = TempDir::new().unwrap();
    init_with_admin(
        tmp.path(),
        &[
            "--bind",
            &format!("127.0.0.1:{port}"),
            "--model",
            "anthropic.claude-sonnet-5",
            "--region",
            "us-east-1",
            "--profile",
            "prof-xyz-77",
        ],
    );
    let (backend, datastore, status, started) = start_and_fetch_root(tmp.path(), port);
    assert!(started);
    assert_eq!(backend, "Backend: Bedrock anthropic.claude-sonnet-5");
    assert!(!backend.contains("prof-xyz-77"), "{backend}");
    assert!(
        datastore.starts_with("Datastore: sqlite ") && datastore.ends_with("(1 user)"),
        "{datastore}"
    );
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
}

#[test]
fn a_datastore_without_a_profile_names_the_echo() {
    let port = free_port();
    let tmp = TempDir::new().unwrap();
    init_with_admin(tmp.path(), &["--bind", &format!("127.0.0.1:{port}")]);
    let (backend, _, status, started) = start_and_fetch_root(tmp.path(), port);
    assert!(started);
    assert!(backend.starts_with("Backend: echo"), "{backend}");
    assert!(backend.contains("mezame init --model ID"), "{backend}");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
}

#[test]
fn a_datastore_with_no_user_and_no_terminal_exits_naming_the_flags() {
    // Requirement 10 criterion 4: no user and nothing to ask on.
    let port = free_port();
    let body = format!(
        r#"{{"version":2,"transports":[{{"kind":"cloudflared","bind":"127.0.0.1:{port}"}}]}}"#
    );
    let tmp = home_with_config(&body);
    let out = run_with_home(&[], tmp.path());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("No user yet"), "{stderr}");
    assert!(
        stderr.contains("mezame init --admin NAME --password-stdin"),
        "{stderr}"
    );
    assert!(!stderr.contains("listening on"), "{stderr}");
}

#[test]
fn a_datastore_with_no_key_refuses_to_start_naming_the_key_path_and_changes_nothing() {
    // Requirement 4 criterion 1: nothing sealed in the datastore could be
    // opened, so the start stops, writes no key and alters no byte.
    let port = free_port();
    let tmp = TempDir::new().unwrap();
    init_with_admin(
        tmp.path(),
        &[
            "--bind",
            &format!("127.0.0.1:{port}"),
            "--model",
            "anthropic.claude-sonnet-5",
        ],
    );
    let key_path = tmp.path().join(".mezame/master.key");
    let db_path = tmp.path().join(".mezame/mezame.db");
    std::fs::remove_file(&key_path).unwrap();
    let before = std::fs::read(&db_path).unwrap();

    let out = run_with_home(&[], tmp.path());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&key_path.display().to_string()), "{stderr}");
    assert!(stderr.contains("backup"), "{stderr}");
    assert!(stderr.contains("mezame init"), "{stderr}");
    assert!(!key_path.exists(), "no key is written");
    assert_eq!(
        std::fs::read(&db_path).unwrap(),
        before,
        "the datastore is untouched"
    );
}

#[test]
fn a_credential_sealed_under_another_key_stops_the_start_naming_the_label() {
    // Requirement 4 criterion 8: no fall back to the echo.
    let port = free_port();
    let tmp = TempDir::new().unwrap();
    init_with_admin(
        tmp.path(),
        &[
            "--bind",
            &format!("127.0.0.1:{port}"),
            "--model",
            "anthropic.claude-sonnet-5",
        ],
    );
    let key_path = tmp.path().join(".mezame/master.key");
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
            .write_all(&[9u8; 32])
            .unwrap();
    }
    let out = run_with_home(&[], tmp.path());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("credential `Bedrock`"), "{stderr}");
    assert!(stderr.contains("mezame init"), "{stderr}");
    assert!(
        stderr.contains("--model anthropic.claude-sonnet-5"),
        "{stderr}"
    );
    assert!(!stderr.contains("Backend: echo"), "{stderr}");
    assert!(!stderr.contains("listening on"), "{stderr}");
}

#[test]
fn a_profile_row_with_null_columns_yields_the_phase_1_defaults() {
    use mezame::provider::{ThinkingMode, DEFAULT_MAX_OUTPUT_TOKENS, DEFAULT_THINKING_BUDGET};
    use mezame::settings_from_profile;
    use mezame::store::ProfileRow;
    let row = ProfileRow {
        id: "p".into(),
        user_id: None,
        credential_id: None,
        model: "anthropic.claude-sonnet-5".into(),
        thinking: None,
        thinking_budget: None,
        max_output_tokens: None,
    };
    let settings = settings_from_profile(
        &row,
        &[
            "anthropic.claude-opus-5".into(),
            "anthropic.claude-sonnet-5".into(),
        ],
    );
    assert_eq!(settings.model, "anthropic.claude-sonnet-5");
    assert_eq!(
        settings.models,
        vec!["anthropic.claude-sonnet-5", "anthropic.claude-opus-5"],
        "the profile's model first, the catalogue after it, no duplicate"
    );
    assert_eq!(settings.thinking, None);
    assert_eq!(settings.thinking_budget, DEFAULT_THINKING_BUDGET);
    assert_eq!(settings.max_output_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
    assert_eq!(
        (DEFAULT_THINKING_BUDGET, DEFAULT_MAX_OUTPUT_TOKENS),
        (4096, 16384)
    );

    let set = ProfileRow {
        thinking: Some("off".into()),
        thinking_budget: Some(2048),
        max_output_tokens: Some(9000),
        ..row.clone()
    };
    let settings = settings_from_profile(&set, &[]);
    assert_eq!(settings.thinking, Some(ThinkingMode::Off));
    assert_eq!(settings.thinking_budget, 2048);
    assert_eq!(settings.max_output_tokens, 9000);
    let odd = ProfileRow {
        thinking: Some("budget".into()),
        ..row
    };
    assert_eq!(
        settings_from_profile(&odd, &[]).thinking,
        None,
        "an unknown spelling reads as unset"
    );
}
