//! Live smoke tests against Amazon Bedrock. Every case is `#[ignore]` and
//! returns early with a printed reason unless `MEZAME_LIVE_BEDROCK_MODEL`
//! names a model; CI holds no credentials and never runs them. Run by hand:
//!
//! ```sh
//! MEZAME_LIVE_BEDROCK_MODEL=global.anthropic.claude-sonnet-5 \
//!   cargo test --test live_bedrock -- --ignored --nocapture
//! ```
//!
//! `MEZAME_LIVE_BEDROCK_REGION` and `MEZAME_LIVE_BEDROCK_PROFILE` are read
//! when set; otherwise the AWS default chain decides. These cases cost
//! money: a few thousand tokens per run.

mod support;

use std::sync::Arc;

use mezame::backend::{Backend, EntryBody};
use mezame::provider::bedrock::{build_client, thinking_rule, BedrockProvider};
use mezame::provider::{LoopSettings, Provider, ThinkingMode};
use mezame::turn::LoopBackend;
use serde_json::{json, Value};
use tokio::sync::mpsc;

fn model() -> Option<String> {
    match std::env::var("MEZAME_LIVE_BEDROCK_MODEL") {
        Ok(model) if !model.trim().is_empty() => Some(model),
        _ => {
            eprintln!("MEZAME_LIVE_BEDROCK_MODEL is unset; skipping the live case");
            None
        }
    }
}

async fn backend(model: &str) -> LoopBackend {
    let region = std::env::var("MEZAME_LIVE_BEDROCK_REGION").ok();
    let profile = std::env::var("MEZAME_LIVE_BEDROCK_PROFILE").ok();
    let client = build_client(region.as_deref(), profile.as_deref()).await;
    let provider: Arc<dyn Provider> = Arc::new(BedrockProvider::new(client));
    LoopBackend::new(
        provider,
        LoopSettings {
            model: model.to_string(),
            models: vec![model.to_string()],
            thinking: None,
            thinking_budget: 4096,
            max_output_tokens: 16384,
        },
        "live",
        "live",
        None,
    )
}

/// One turn: the frames streamed and the outcome.
async fn turn(
    backend: &LoopBackend,
    text: &str,
) -> (Vec<Value>, anyhow::Result<mezame::backend::TurnOutcome>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let result = backend
        .prompt(vec![json!({ "type": "text", "text": text })], tx)
        .await;
    let mut frames = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        frames.push(frame);
    }
    (frames, result)
}

fn joined(frames: &[Value], kind: &str) -> String {
    frames
        .iter()
        .filter(|f| f["type"] == kind)
        .filter_map(|f| f["text"].as_str())
        .collect()
}

#[tokio::test]
#[ignore = "reaches Bedrock with real credentials"]
async fn a_streamed_turn_yields_text_and_usage() {
    let Some(model) = model() else { return };
    let backend = backend(&model).await;
    let (frames, result) = turn(&backend, "Reply with exactly the word: pong").await;
    let outcome = result.expect("the turn resolves");
    let text = joined(&frames, "append");
    eprintln!("answer: {text:?}\nusage: {:?}", outcome.usage);
    assert!(!text.trim().is_empty(), "some text streamed: {frames:?}");
    let usage = outcome.usage.expect("usage reported");
    assert!(usage.output > 0);
    let history = backend.history().await;
    assert!(matches!(&history[0].body, EntryBody::User { text } if text.contains("pong")));
    assert!(history
        .iter()
        .any(|e| matches!(&e.body, EntryBody::Agent { .. })));
}

#[tokio::test]
#[ignore = "reaches Bedrock with real credentials"]
async fn a_reasoning_prompt_fills_the_thought_pane_where_the_model_allows() {
    let Some(model) = model() else { return };
    let backend = backend(&model).await;
    let (frames, result) = turn(
        &backend,
        "A farmer has 17 sheep. All but 9 run away. Then each remaining sheep has 2 lambs. How many \
         animals does the farmer have now? Think it through step by step before answering.",
    )
    .await;
    result.expect("the turn resolves");
    let thoughts = frames.iter().filter(|f| f["type"] == "thought").count();
    let text = joined(&frames, "append");
    eprintln!("thought frames: {thoughts}\nanswer: {text:?}");
    assert!(text.contains("27"), "the answer holds 27: {text:?}");
    // Adaptive thinking may decide not to think; the count is reported
    // above. A budgeted model always thinks.
    if thinking_rule(&model) == ThinkingMode::Enabled {
        assert!(thoughts > 0, "a budgeted model always thinks");
    }
}

#[tokio::test]
#[ignore = "reaches Bedrock with real credentials"]
async fn a_second_identical_request_reads_from_the_cache() {
    let Some(model) = model() else { return };
    let backend = backend(&model).await;
    // Over 4,096 tokens on every Claude tokenizer, the largest current
    // minimum, so the conversation checkpoint has something to cache.
    let filler: String = (0..400)
        .map(|i| format!("Paragraph {i}: the quick brown fox jumps over the lazy dog near the river bank at dawn. "))
        .collect();
    let prompt = format!("{filler}\nReply with exactly the word: ready");
    let (_, first) = turn(&backend, &prompt).await;
    let first = first.expect("first turn").usage.expect("usage");
    let (_, second) = turn(&backend, "Reply with exactly the word: again").await;
    let second = second.expect("second turn").usage.expect("usage");
    eprintln!("first usage: {first:?}\nsecond usage: {second:?}");
    assert!(
        second.cache_read > 0,
        "the second request read the cached prefix: {second:?}"
    );
}
