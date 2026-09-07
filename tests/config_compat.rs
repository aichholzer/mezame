//! Requirement 15 criterion 4: a `config.json` written by 0.13.x loads, its
//! agent keys are ignored, the file is left byte-identical, and Mezame
//! serves the bind it holds. Named here so the fixture in
//! `tests/config_paths.rs` may lose its 0.13 keys, as Requirement 17
//! criterion 7 allows, without taking this coverage with it.
//!
//! The in-process cases mutate `HOME` under a process-wide mutex. The
//! serving case spawns the binary with its own `HOME` and never touches
//! this process's environment.

use std::sync::OnceLock;

use mezame::config::{load_config, TransportConfig};
use tempfile::TempDir;
use tokio::sync::Mutex;

/// The exact bytes 0.13.4's `init_config` wrote: `serde_json::to_string_pretty`
/// of its `Config` in field order, no trailing newline.
const CONFIG_0_13: &str = r#"{
  "transports": [
    {
      "kind": "cloudflared",
      "bind": "127.0.0.1:9510"
    }
  ],
  "agent_cmd": "kiro-cli",
  "agent_args": [
    "acp"
  ]
}"#;

fn home_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A temporary `HOME` holding `body` at `.mezame/config.json`.
fn home_with(body: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), body).unwrap();
    tmp
}

#[tokio::test]
async fn a_0_13_config_loads_and_yields_its_bind_with_no_hosts() {
    let _g = home_lock().lock().await;
    for body in [
        CONFIG_0_13.to_string(),
        // 0.13 had `#[serde(default)]` on `agent_args`, so files without
        // it exist.
        CONFIG_0_13.replace(",\n  \"agent_args\": [\n    \"acp\"\n  ]", ""),
    ] {
        let tmp = home_with(&body);
        std::env::set_var("HOME", tmp.path());
        let cfg = load_config().expect("a 0.13 file loads");
        assert_eq!(cfg.transports.len(), 1);
        let TransportConfig::Cloudflared { bind, hosts } = &cfg.transports[0];
        assert_eq!(bind, "127.0.0.1:9510");
        assert!(hosts.is_empty(), "no hosts key reads as no extra hosts");
    }
}

#[tokio::test]
async fn loading_a_0_13_config_leaves_the_file_byte_identical() {
    // The tripwire for a migration-on-load: any rewrite, key stripping or
    // version stamp changes the bytes or the mtime.
    let _g = home_lock().lock().await;
    let tmp = home_with(CONFIG_0_13);
    std::env::set_var("HOME", tmp.path());
    let path = tmp.path().join(".mezame/config.json");
    let before = std::fs::read(&path).unwrap();
    let modified_before = std::fs::metadata(&path).unwrap().modified().unwrap();

    load_config().expect("loads");

    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the bytes are unchanged"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified_before,
        "the file was not rewritten"
    );
    assert!(
        String::from_utf8_lossy(&before).contains("agent_cmd"),
        "the fixture still carries the 0.13 keys"
    );
}

#[cfg(unix)]
mod serving {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use mezame::unix::send_signal;

    use super::{home_with, CONFIG_0_13};

    /// Kills the child if a failing assertion leaves it running.
    struct Reaper(Option<Child>);

    impl Drop for Reaper {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[test]
    fn a_0_13_config_starts_serving_on_its_bind_and_is_left_unmodified() {
        // Clause 3 of the criterion, end to end through `run()`: the binary
        // starts with a 0.13 file and prints the listening line for the
        // bind it holds. Port 0 keeps the case free of collisions. The
        // child is stopped with SIGTERM so the graceful arm runs and, under
        // llvm-cov, its profile is written; a SIGTERM that lands before the
        // handler is installed kills the child by signal instead, which
        // costs that run's profile and nothing else.
        let body = CONFIG_0_13.replace("127.0.0.1:9510", "127.0.0.1:0");
        let tmp = home_with(&body);
        let path = tmp.path().join(".mezame/config.json");
        let before = std::fs::read(&path).unwrap();

        let child = Command::new(env!("CARGO_BIN_EXE_mezame"))
            .env("HOME", tmp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mezame");
        let mut reaper = Reaper(Some(child));
        let child = reaper.0.as_mut().unwrap();
        let pid = child.id() as i32;

        let stderr = child.stderr.take().expect("stderr is piped");
        let (lines_tx, lines_rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if lines_tx.send(line).is_err() {
                    break;
                }
            }
        });

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut seen = Vec::new();
        let listening = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match lines_rx.recv_timeout(remaining) {
                Ok(line) => {
                    let hit = line.contains("Mezame is listening on: http://127.0.0.1:0");
                    seen.push(line);
                    if hit {
                        break true;
                    }
                }
                Err(_) => break false,
            }
        };
        assert!(
            listening,
            "the binary serves the bind the 0.13 file holds; stderr was: {seen:?}"
        );

        assert_eq!(send_signal(pid, 15), 0, "SIGTERM is delivered");
        let child = reaper.0.as_mut().unwrap();
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait on the child") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                break child.wait().expect("reap the child");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        reaper.0 = None;
        assert!(
            status.success() || status.signal() == Some(15),
            "the child exits on SIGTERM, got {status:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "the file is left unmodified"
        );
    }
}
