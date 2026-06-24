//! CodSpeed benchmarks for Mezame's pure HTTP-layer helpers.
//!
//! These cover the CPU-bound parsing/formatting functions on the hot
//! path of serving the UI and replaying chat history:
//!   - `mime_for`            — content-type lookup for every served asset
//!   - `extract_text_blocks` — pull text out of ACP content arrays
//!   - `parse_kiro_history`  — parse a Kiro session JSONL log on resume
//!
//! They are isolated, deterministic, and allocation-light, which makes
//! them a good fit for CodSpeed's CPU simulation instrument.

use mezame::http::{extract_text_blocks, mime_for, parse_kiro_history};
use serde_json::json;

fn main() {
    divan::main();
}

#[divan::bench(args = [
    "index.html",
    "a/b/c.js",
    "style.css",
    "logo.png",
    "favicon.ico",
    "font.woff2",
    "manifest.webmanifest",
    "blob.unknownext",
    "noextension",
])]
fn bench_mime_for(path: &str) -> &'static str {
    mime_for(divan::black_box(path))
}

#[divan::bench]
fn bench_extract_text_blocks(bencher: divan::Bencher) {
    let data = json!({
        "content": [
            { "kind": "text", "data": "first line" },
            { "kind": "tool_call", "data": "ignored" },
            { "kind": "text", "data": "second line" },
            { "kind": "image", "data": "ignored too" },
            { "kind": "text", "data": "third line" },
        ]
    });
    bencher.bench(|| extract_text_blocks(divan::black_box(&data)));
}

/// Build a realistic Kiro session JSONL log with `turns` prompt/reply
/// pairs so the parser is exercised over a representative input size.
fn make_history(turns: usize) -> String {
    let mut out = String::new();
    for i in 0..turns {
        out.push_str(&format!(
            r#"{{"kind":"Prompt","data":{{"content":[{{"kind":"text","data":"question number {i} with some surrounding context"}}],"meta":{{"timestamp":{ts}}}}}}}"#,
            i = i,
            ts = 1_700_000_000 + i as i64,
        ));
        out.push('\n');
        out.push_str(r#"{"kind":"ToolResults","data":{}}"#);
        out.push('\n');
        out.push_str(&format!(
            r#"{{"kind":"AssistantMessage","data":{{"content":[{{"kind":"text","data":"answer to question {i}"}}]}}}}"#,
            i = i,
        ));
        out.push('\n');
    }
    out
}

#[divan::bench(args = [1, 16, 128])]
fn bench_parse_kiro_history(bencher: divan::Bencher, turns: usize) {
    let raw = make_history(turns);
    bencher.bench(|| parse_kiro_history(divan::black_box(&raw)));
}
