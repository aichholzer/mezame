//! Tests for `Config::resolve_agent` / `default_agent` — the per-session
//! agent selection the WS `?agent=` param drives. Pure (no filesystem or
//! `HOME` mutation), so these need no shared lock.

use mezame::config::{AgentConfig, Config, TransportConfig};

fn agent(name: &str, command: &str) -> AgentConfig {
    AgentConfig {
        name: name.into(),
        command: command.into(),
        args: vec![],
        env: Default::default(),
    }
}

fn config_with(agents: Vec<AgentConfig>) -> Config {
    Config {
        transports: vec![TransportConfig::Cloudflared {
            bind: "127.0.0.1:0".into(),
        }],
        agents,
        agent_cmd: None,
        agent_args: vec![],
    }
}

#[test]
fn resolve_agent_none_returns_first_as_default() {
    let cfg = config_with(vec![
        agent("kiro", "kiro-cli"),
        agent("claude", "claude-agent-acp"),
    ]);
    let resolved = cfg.resolve_agent(None).expect("default agent");
    assert_eq!(resolved.name, "kiro");
    assert_eq!(cfg.default_agent().unwrap().name, "kiro");
}

#[test]
fn resolve_agent_by_name_picks_the_named_entry() {
    let cfg = config_with(vec![
        agent("kiro", "kiro-cli"),
        agent("claude", "claude-agent-acp"),
    ]);
    let resolved = cfg.resolve_agent(Some("claude")).expect("named agent");
    assert_eq!(resolved.name, "claude");
    assert_eq!(resolved.command, "claude-agent-acp");
}

#[test]
fn resolve_agent_unknown_name_errors_with_the_name() {
    let cfg = config_with(vec![agent("kiro", "kiro-cli")]);
    let err = cfg
        .resolve_agent(Some("does-not-exist"))
        .expect_err("unknown agent should error");
    assert!(
        err.to_string().contains("does-not-exist"),
        "error should name the missing agent: {err}"
    );
}

#[test]
fn resolve_agent_errors_when_no_agents_configured() {
    let cfg = config_with(vec![]);
    assert!(cfg.default_agent().is_none());
    let err = cfg
        .resolve_agent(None)
        .expect_err("empty agents should error");
    assert!(
        err.to_string().contains("No agents configured"),
        "unexpected error: {err}"
    );
}

#[test]
fn normalize_is_idempotent_for_list_form_configs() {
    let mut cfg = config_with(vec![agent("kiro", "kiro-cli")]);
    cfg.normalize();
    assert_eq!(cfg.agents.len(), 1);
    assert_eq!(cfg.agents[0].name, "kiro");
    assert!(cfg.agent_cmd.is_none());
}
