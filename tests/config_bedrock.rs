//! The `bedrock` section of `config.json`: how it loads, what it defaults,
//! what it refuses, and what a written file holds. Every case reads and
//! writes a file of its own through `load_config_from`, so no test touches
//! the process's `HOME`.

use std::path::Path;

use std::str::FromStr as _;

use mezame::config::{
    load_config_from, BedrockConfig, Config, TransportConfig, DEFAULT_MAX_OUTPUT_TOKENS,
    DEFAULT_THINKING_BUDGET,
};
use mezame::provider::ThinkingMode;
use serde_json::{json, Value};
use tempfile::TempDir;

fn write(tmp: &TempDir, body: &str) -> std::path::PathBuf {
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

fn with_bedrock(section: &str) -> String {
    format!("{{{TRANSPORT},\"bedrock\":{section}}}")
}

#[test]
fn a_file_without_the_section_loads_as_before_and_selects_the_echo() {
    let cfg = load(&format!("{{{TRANSPORT}}}")).unwrap();
    assert!(cfg.bedrock.is_none());
    assert_eq!(cfg.bind(), Some("127.0.0.1:9510"));
    // Unknown keys at the top level are still ignored.
    let cfg = load(&format!("{{{TRANSPORT},\"agent_cmd\":\"kiro-cli\"}}")).unwrap();
    assert!(cfg.bedrock.is_none());
}

#[test]
fn a_section_with_only_a_model_takes_every_default() {
    let cfg = load(&with_bedrock(r#"{"model":"anthropic.claude-sonnet-5"}"#)).unwrap();
    let section = cfg.bedrock.unwrap();
    assert_eq!(section.model, "anthropic.claude-sonnet-5");
    assert!(section.models.is_empty());
    assert_eq!(section.model_list(), vec!["anthropic.claude-sonnet-5"]);
    assert_eq!(section.region, None);
    assert_eq!(section.profile, None);
    assert_eq!(section.thinking, None);
    assert_eq!(section.thinking_mode(), None);
    assert_eq!(section.thinking_mode_or_rule(), ThinkingMode::Adaptive);
    let settings = section.settings();
    assert_eq!(settings.model, "anthropic.claude-sonnet-5");
    assert_eq!(settings.models, vec!["anthropic.claude-sonnet-5"]);
    assert_eq!(settings.thinking, None);
    assert_eq!(settings.thinking_budget, DEFAULT_THINKING_BUDGET);
    assert_eq!(settings.max_output_tokens, DEFAULT_MAX_OUTPUT_TOKENS);
    assert_eq!(
        (DEFAULT_THINKING_BUDGET, DEFAULT_MAX_OUTPUT_TOKENS),
        (4096, 16384)
    );
}

#[test]
fn every_key_is_read_and_the_settings_carry_them() {
    let cfg = load(&with_bedrock(
        r#"{"model":"global.anthropic.claude-haiku-4-5-20251001-v1:0","models":["global.anthropic.claude-sonnet-5"],
            "region":"eu-west-1","profile":"work","thinking":"enabled","thinking_budget":2048,
            "max_output_tokens":8000,"future_key":true}"#,
    ))
    .unwrap();
    let section = cfg.bedrock.unwrap();
    assert_eq!(section.region.as_deref(), Some("eu-west-1"));
    assert_eq!(section.profile.as_deref(), Some("work"));
    assert_eq!(section.thinking_mode(), Some(ThinkingMode::Enabled));
    let settings = section.settings();
    assert_eq!(settings.thinking, Some(ThinkingMode::Enabled));
    assert_eq!(settings.thinking_budget, 2048);
    assert_eq!(settings.max_output_tokens, 8000);
    assert_eq!(
        settings.models,
        vec![
            "global.anthropic.claude-haiku-4-5-20251001-v1:0",
            "global.anthropic.claude-sonnet-5"
        ],
        "the model comes first, then the list"
    );
}

#[test]
fn models_that_omit_the_model_get_it_prepended_and_duplicates_dropped() {
    let cfg = load(&with_bedrock(r#"{"model":"a","models":["b","a","c","b"]}"#)).unwrap();
    assert_eq!(cfg.bedrock.unwrap().model_list(), vec!["a", "b", "c"]);
}

#[test]
fn the_model_id_passes_through_unchanged_whatever_its_form() {
    for id in [
        "anthropic.claude-sonnet-5",
        "global.anthropic.claude-sonnet-5",
        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-sonnet-5",
        "amazon.nova-pro-v1:0",
    ] {
        let cfg = load(&with_bedrock(&format!(r#"{{"model":"{id}"}}"#))).unwrap();
        assert_eq!(cfg.bedrock.unwrap().model, id);
    }
}

#[test]
fn each_refusal_names_the_key_and_the_file() {
    let cases = [
        (r#"{}"#, "`bedrock.model`"),
        (r#"{"model":"  "}"#, "`bedrock.model`"),
        (r#"{"model":"m","models":["ok",""]}"#, "`bedrock.models`"),
        (
            r#"{"model":"m","thinking":"budget"}"#,
            "`bedrock.thinking` must be one of `adaptive`, `enabled` or `off`, not `budget`",
        ),
        (
            r#"{"model":"m","thinking":"Adaptive"}"#,
            "`bedrock.thinking`",
        ),
        (
            r#"{"model":"m","thinking_budget":512}"#,
            "`bedrock.thinking_budget` must be at least 1024",
        ),
        (
            r#"{"model":"m","thinking_budget":16384}"#,
            "below `bedrock.max_output_tokens` (16384)",
        ),
        (
            r#"{"model":"m","thinking":"enabled","max_output_tokens":4096}"#,
            "`bedrock.thinking_budget`",
        ),
        (
            r#"{"model":"m","max_output_tokens":0}"#,
            "`bedrock.max_output_tokens` must be at least 1",
        ),
    ];
    for (section, names) in cases {
        let err = load(&with_bedrock(section)).unwrap_err();
        assert!(
            err.contains(names),
            "{section} should name {names:?}: {err}"
        );
        assert!(
            err.contains("config.json"),
            "{section} should name the file: {err}"
        );
    }
}

#[test]
fn a_budget_nothing_would_send_is_not_checked() {
    // `adaptive` thinking sends no budget, so a small output ceiling under
    // it is fine even though the default budget would not fit.
    let cfg = load(&with_bedrock(
        r#"{"model":"anthropic.claude-sonnet-5","max_output_tokens":2000}"#,
    ))
    .unwrap();
    assert_eq!(cfg.bedrock.unwrap().settings().max_output_tokens, 2000);
    // An `enabled` model with the same ceiling would send the default
    // budget, which does not fit, so it is refused.
    let err = load(&with_bedrock(
        r#"{"model":"anthropic.claude-haiku-4-5-20251001-v1:0","max_output_tokens":2000}"#,
    ))
    .unwrap_err();
    assert!(err.contains("`bedrock.thinking_budget`"), "{err}");
    // So is an `enabled` model anywhere in the picker: a switch to it
    // would send the budget too.
    let err = load(&with_bedrock(
        r#"{"model":"anthropic.claude-sonnet-5","models":["anthropic.claude-haiku-4-5-20251001-v1:0"],"max_output_tokens":2000}"#,
    ))
    .unwrap_err();
    assert!(err.contains("`bedrock.thinking_budget`"), "{err}");
}

#[test]
fn a_thinking_request_on_another_vendor_s_model_is_refused_at_load() {
    for mode in ["enabled", "adaptive"] {
        let err = load(&with_bedrock(&format!(
            r#"{{"model":"amazon.nova-pro-v1:0","thinking":"{mode}"}}"#
        )))
        .unwrap_err();
        assert!(
            err.contains("`bedrock.thinking`") && err.contains("amazon.nova-pro-v1:0"),
            "{err}"
        );
    }
    // `off` asks for nothing and is fine anywhere; so is an unset key.
    load(&with_bedrock(
        r#"{"model":"amazon.nova-pro-v1:0","thinking":"off"}"#,
    ))
    .unwrap();
    load(&with_bedrock(r#"{"model":"amazon.nova-pro-v1:0"}"#)).unwrap();
    // A picker entry from another vendor is refused too.
    let err = load(&with_bedrock(
        r#"{"model":"anthropic.claude-sonnet-5","models":["amazon.nova-pro-v1:0"],"thinking":"adaptive"}"#,
    ))
    .unwrap_err();
    assert!(err.contains("amazon.nova-pro-v1:0"), "{err}");
}

#[test]
fn read_config_from_parses_without_validating() {
    use mezame::config::read_config_from;
    let tmp = TempDir::new().unwrap();
    let path = write(&tmp, &with_bedrock(r#"{"model":"m","thinking":"budget"}"#));
    let cfg = read_config_from(&path).unwrap();
    assert_eq!(cfg.bedrock.unwrap().thinking.as_deref(), Some("budget"));
    assert!(load_config_from(&path).is_err());
}

#[test]
fn a_malformed_value_is_a_parse_error_naming_the_file() {
    let err = load(&with_bedrock(r#"{"model":"m","thinking_budget":"lots"}"#)).unwrap_err();
    assert!(err.contains("Parsing config.json"), "{err}");
    let err = load(&with_bedrock(r#"{"model":"m","thinking_budget":-5}"#)).unwrap_err();
    assert!(err.contains("Parsing config.json"), "{err}");
}

#[test]
fn a_written_file_holds_only_the_keys_that_are_set() {
    let cfg = Config {
        transports: vec![TransportConfig::Cloudflared {
            bind: "127.0.0.1:9510".into(),
            hosts: Vec::new(),
        }],
        bedrock: Some(BedrockConfig::for_model("anthropic.claude-sonnet-5")),
    };
    let value: Value = serde_json::to_value(&cfg).unwrap();
    assert_eq!(
        value,
        json!({
            "transports": [{ "kind": "cloudflared", "bind": "127.0.0.1:9510" }],
            "bedrock": { "model": "anthropic.claude-sonnet-5" }
        })
    );
    let echo = Config {
        transports: cfg.transports.clone(),
        bedrock: None,
    };
    assert_eq!(
        serde_json::to_value(&echo).unwrap(),
        json!({ "transports": [{ "kind": "cloudflared", "bind": "127.0.0.1:9510" }] }),
        "no `bedrock` key at all for the echo"
    );
    let full = BedrockConfig {
        model: "m".into(),
        models: vec!["n".into()],
        region: Some("us-east-1".into()),
        profile: Some("work".into()),
        thinking: Some("off".into()),
        thinking_budget: Some(2048),
        max_output_tokens: Some(4096),
    };
    assert_eq!(
        serde_json::to_value(&full).unwrap(),
        json!({ "model": "m", "models": ["n"], "region": "us-east-1", "profile": "work",
                "thinking": "off", "thinking_budget": 2048, "max_output_tokens": 4096 })
    );
    // And a written file loads back to the same section.
    let tmp = TempDir::new().unwrap();
    let path = write(
        &tmp,
        &serde_json::to_string_pretty(&Config {
            transports: cfg.transports.clone(),
            bedrock: Some(full.clone()),
        })
        .unwrap(),
    );
    assert_eq!(
        load_config_from(Path::new(&path)).unwrap().bedrock,
        Some(full)
    );
}

#[test]
fn hosts_walks_every_transport_as_the_guard_does() {
    let cfg = load(r#"{"transports":[{"kind":"cloudflared","bind":"127.0.0.1:9510","hosts":[]},{"kind":"cloudflared","bind":"0.0.0.0:9511","hosts":["a.example","b.example"]}]}"#).unwrap();
    assert_eq!(cfg.hosts(), vec!["a.example", "b.example"]);
    assert_eq!(cfg.bind(), Some("127.0.0.1:9510"));
    assert_eq!(ThinkingMode::from_str("off"), Ok(ThinkingMode::Off));
    assert!(ThinkingMode::from_str("Off").is_err());
    assert_eq!(ThinkingMode::NAMES, ["adaptive", "enabled", "off"]);
}
