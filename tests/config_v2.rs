//! `config.json` version 2: the version gate on the raw document, the
//! datastore backend, the model list, the public URL, and what `init`
//! starts from over a file another release wrote.
//!
//! The in-process cases that read `~/.mezame` set `HOME` under a
//! process-wide lock; the binary case spawns `mezame` with its own `HOME`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use mezame::config::{
    load_config_from, read_existing_config_from, Config, DatastoreConfig, TransportConfig,
    CONFIG_VERSION,
};
use serde_json::{json, Value};
use tempfile::TempDir;

fn home_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write(tmp: &TempDir, body: &str) -> PathBuf {
    let path = tmp.path().join("config.json");
    std::fs::write(&path, body).unwrap();
    path
}

fn load(body: &str) -> Result<Config, String> {
    let tmp = TempDir::new().unwrap();
    let path = write(&tmp, body);
    load_config_from(&path).map_err(|e| format!("{e:#}"))
}

const TRANSPORT: &str = r#""transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510"}]"#;

#[test]
fn a_version_2_file_loads_with_its_defaults() {
    let cfg = load(&format!("{{\"version\":2,{TRANSPORT}}}")).unwrap();
    assert_eq!(cfg.version, CONFIG_VERSION);
    assert_eq!(cfg.datastore, DatastoreConfig::default());
    assert_eq!(cfg.datastore.backend, "sqlite");
    assert_eq!(cfg.public_url, None);
    assert!(cfg.models.is_empty());
    assert_eq!(cfg.bind(), Some("127.0.0.1:9510"));
}

#[test]
fn every_key_is_read_when_present() {
    let cfg = load(&format!(
        "{{\"version\":2,{TRANSPORT},\"datastore\":{{\"backend\":\"sqlite\"}},\
         \"public_url\":\"https://mezame.example.com\",\"models\":[\"a\",\"b\"]}}"
    ))
    .unwrap();
    assert_eq!(
        cfg.public_url.as_deref(),
        Some("https://mezame.example.com")
    );
    assert_eq!(cfg.models, vec!["a", "b"]);
}

#[test]
fn a_file_of_another_or_no_version_is_refused_with_the_pointer_line() {
    for (body, found) in [
        (format!("{{{TRANSPORT}}}"), "version none"),
        (format!("{{\"version\":1,{TRANSPORT}}}"), "version 1"),
        (format!("{{\"version\":3,{TRANSPORT}}}"), "version 3"),
        (
            format!("{{\"version\":\"2\",{TRANSPORT}}}"),
            "version \"2\"",
        ),
    ] {
        let err = load(&body).unwrap_err();
        assert!(err.contains(found), "{body}: {err}");
        assert!(err.contains("reads version 2"), "{err}");
        assert!(err.contains("`mezame init`"), "{err}");
        assert!(err.contains("config.json"), "{err}");
        assert!(
            !err.contains("missing field"),
            "the version is checked before the shape: {err}"
        );
    }
}

#[test]
fn the_binary_exits_with_one_pointer_line_on_a_versionless_file() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), format!("{{{TRANSPORT}}}")).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mezame"))
        .env("HOME", tmp.path())
        .output()
        .expect("spawn mezame");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("version none"), "{stderr}");
    assert!(stderr.contains("`mezame init`"), "{stderr}");
    assert!(!stderr.contains("missing field"), "{stderr}");
    assert_eq!(
        stderr.lines().filter(|l| !l.trim().is_empty()).count(),
        1,
        "one line, no parse error: {stderr}"
    );
}

#[test]
fn an_unknown_datastore_backend_is_refused_naming_the_value() {
    let err = load(&format!(
        "{{\"version\":2,{TRANSPORT},\"datastore\":{{\"backend\":\"postgres\"}}}}"
    ))
    .unwrap_err();
    assert!(err.contains("`datastore.backend` `postgres`"), "{err}");
    assert!(err.contains("`sqlite` is the one backend"), "{err}");
    assert!(err.contains("config.json"), "{err}");
}

#[test]
fn models_entries_must_be_non_empty_and_trimmed() {
    let err = load(&format!(
        "{{\"version\":2,{TRANSPORT},\"models\":[\"a\",\"\"]}}"
    ))
    .unwrap_err();
    assert!(err.contains("`models` holds an empty entry"), "{err}");
    let err = load(&format!(
        "{{\"version\":2,{TRANSPORT},\"models\":[\" a \"]}}"
    ))
    .unwrap_err();
    assert!(
        err.contains("`models` entry ` a ` has leading or trailing whitespace"),
        "{err}"
    );
}

#[test]
fn public_url_must_be_http_or_https() {
    for ok in ["http://mezame.lan:9510", "https://mezame.example.com"] {
        load(&format!(
            "{{\"version\":2,{TRANSPORT},\"public_url\":\"{ok}\"}}"
        ))
        .unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
    let err = load(&format!(
        "{{\"version\":2,{TRANSPORT},\"public_url\":\"ftp://x\"}}"
    ))
    .unwrap_err();
    assert!(err.contains("`public_url` must begin with"), "{err}");
}

#[test]
fn unknown_keys_are_ignored() {
    let cfg = load(&format!(
        "{{\"version\":2,{TRANSPORT},\"future_key\":true}}"
    ))
    .unwrap();
    assert_eq!(cfg.bind(), Some("127.0.0.1:9510"));
}

#[test]
fn a_written_config_carries_its_version_and_datastore_and_loads_back() {
    let cfg = Config {
        version: CONFIG_VERSION,
        transports: vec![TransportConfig::Cloudflared {
            bind: "127.0.0.1:9510".into(),
            hosts: vec!["mezame.example.com".into()],
        }],
        datastore: DatastoreConfig::default(),
        public_url: Some("https://mezame.example.com".into()),
        models: vec!["m".into()],
    };
    let value: Value = serde_json::to_value(&cfg).unwrap();
    assert_eq!(
        value,
        json!({
            "version": 2,
            "transports": [{ "kind": "cloudflared", "bind": "127.0.0.1:9510", "hosts": ["mezame.example.com"] }],
            "datastore": { "backend": "sqlite" },
            "public_url": "https://mezame.example.com",
            "models": ["m"]
        })
    );
    let tmp = TempDir::new().unwrap();
    let path = write(&tmp, &serde_json::to_string_pretty(&cfg).unwrap());
    let back = load_config_from(&path).unwrap();
    assert_eq!(back.public_url, cfg.public_url);
    assert_eq!(back.models, cfg.models);
    assert_eq!(back.hosts(), vec!["mezame.example.com"]);
}

#[test]
fn what_init_starts_from() {
    let _guard = home_lock();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("config.json");

    // No file: nothing to start from, and not an error.
    assert!(read_existing_config_from(&path).unwrap().is_none());

    // A current file is carried whole.
    std::fs::write(
        &path,
        format!("{{\"version\":2,{TRANSPORT},\"models\":[\"m\"],\"public_url\":\"http://x\"}}"),
    )
    .unwrap();
    let existing = read_existing_config_from(&path).unwrap().unwrap();
    assert!(existing.legacy.is_none());
    assert!(!existing.had_bedrock);
    assert_eq!(existing.config.models, vec!["m"]);
    assert_eq!(existing.config.public_url.as_deref(), Some("http://x"));

    // A file of another release: the transports and their hosts survive,
    // the rest does not, and the version found is reported.
    std::fs::write(
        &path,
        r#"{"transports":[{"kind":"cloudflared","bind":"0.0.0.0:9511","hosts":["mezame.example.com"]}],"bedrock":{"model":"m"},"agent_cmd":"kiro-cli"}"#,
    )
    .unwrap();
    let existing = read_existing_config_from(&path).unwrap().unwrap();
    assert_eq!(existing.legacy.as_deref(), Some("none"));
    assert_eq!(existing.config.version, CONFIG_VERSION);
    assert_eq!(existing.config.bind(), Some("0.0.0.0:9511"));
    assert_eq!(existing.config.hosts(), vec!["mezame.example.com"]);
    assert!(
        existing.had_bedrock,
        "the section it carried is noted, for `init` to say so"
    );
    assert!(existing.config.models.is_empty());

    std::fs::write(&path, format!("{{\"version\":1,{TRANSPORT}}}")).unwrap();
    assert_eq!(
        read_existing_config_from(&path)
            .unwrap()
            .unwrap()
            .legacy
            .as_deref(),
        Some("1")
    );

    // Transports that do not parse are dropped rather than refused: `init`
    // asks for the bind anyway.
    std::fs::write(&path, r#"{"transports":5}"#).unwrap();
    let existing = read_existing_config_from(&path).unwrap().unwrap();
    assert_eq!(existing.legacy.as_deref(), Some("none"));
    assert!(existing.config.transports.is_empty());

    // A file that is not JSON is an error naming the way out, never an
    // absent file.
    std::fs::write(&path, "{").unwrap();
    let err = format!("{:#}", read_existing_config_from(&path).unwrap_err());
    assert!(err.contains("does not parse"), "{err}");
    assert!(
        err.contains("delete it and run `mezame init` again"),
        "{err}"
    );

    // A current-version file with a wrong type is the same error, not a
    // legacy file.
    std::fs::write(
        &path,
        format!("{{\"version\":2,{TRANSPORT},\"models\":\"m\"}}"),
    )
    .unwrap();
    let err = format!("{:#}", read_existing_config_from(&path).unwrap_err());
    assert!(err.contains("does not parse"), "{err}");
    let _ = Path::new("unused");
}

// ---------- step 6: the `bedrock` key goes, and the cases the section's
// suite held that were never about the section ----------

#[test]
fn a_bedrock_key_is_refused_with_the_datastore_pointer() {
    // Requirement 9 criterion 1: the settings live in the datastore now,
    // and a file still carrying them is not read as saying nothing.
    let err = load(&format!(
        "{{\"version\":2,{TRANSPORT},\"bedrock\":{{\"model\":\"anthropic.claude-sonnet-5\"}}}}"
    ))
    .unwrap_err();
    assert!(err.contains("`bedrock` section"), "{err}");
    assert!(err.contains("datastore"), "{err}");
    assert!(err.contains("mezame init --model ID"), "{err}");
    assert!(err.contains("config.json"), "{err}");
    // An empty object under the key is refused the same way: the key is
    // what is checked, before anything in it is read.
    let err = load(&format!("{{\"version\":2,{TRANSPORT},\"bedrock\":{{}}}}")).unwrap_err();
    assert!(err.contains("`bedrock` section"), "{err}");
    // The version gate comes first: a versionless file with the key gets
    // the version line, whose `init` drops the key.
    let err = load(&format!("{{{TRANSPORT},\"bedrock\":{{}}}}")).unwrap_err();
    assert!(err.contains("has version none"), "{err}");
}

#[test]
fn hosts_walks_every_transport_as_the_guard_does() {
    let cfg = load(r#"{"version":2,"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510","hosts":[]},{"kind":"cloudflared","bind":"0.0.0.0:9511","hosts":["a.example","b.example"]}]}"#).unwrap();
    assert_eq!(cfg.hosts(), vec!["a.example", "b.example"]);
    assert_eq!(cfg.bind(), Some("127.0.0.1:9510"));
}

#[test]
fn read_config_from_parses_without_validating() {
    use mezame::config::read_config_from;
    let tmp = TempDir::new().unwrap();
    let path = write(
        &tmp,
        &format!("{{\"version\":2,{TRANSPORT},\"public_url\":\"ftp://x\"}}"),
    );
    let cfg = read_config_from(&path).unwrap();
    assert_eq!(cfg.public_url.as_deref(), Some("ftp://x"));
    assert!(load_config_from(&path).is_err());
}

#[test]
fn a_malformed_value_is_a_parse_error_naming_the_file() {
    let err = load(&format!(
        "{{\"version\":2,{TRANSPORT},\"models\":\"lots\"}}"
    ))
    .unwrap_err();
    assert!(err.contains("Parsing config.json"), "{err}");
    let err = load(&format!("{{\"version\":2,{TRANSPORT},\"public_url\":5}}")).unwrap_err();
    assert!(err.contains("Parsing config.json"), "{err}");
}

#[test]
fn a_written_file_holds_only_the_keys_that_are_set() {
    let echo = Config {
        version: 2,
        transports: vec![TransportConfig::Cloudflared {
            bind: "127.0.0.1:9510".into(),
            hosts: Vec::new(),
        }],
        datastore: Default::default(),
        public_url: None,
        models: vec![],
    };
    assert_eq!(
        serde_json::to_value(&echo).unwrap(),
        json!({
            "version": 2,
            "transports": [{ "kind": "cloudflared", "bind": "127.0.0.1:9510" }],
            "datastore": { "backend": "sqlite" }
        }),
        "no `bedrock` key, no empty `models`, no null `public_url`"
    );
}
