//! CodSpeed benchmarks for Mezame's pure, CPU-bound helpers.
//!
//! The file keeps its name and its `[[bench]]` entry: CodSpeed identifies
//! a benchmark by `{file}::{function}`, so moving it would reset every
//! baseline in it, `bench_mime_for` included.
//!
//! Three subjects, each on a hot path and each isolated, deterministic and
//! allocation-light, which is the shape CodSpeed's CPU simulation suits:
//!   - `mime_for`:          the content-type lookup for every served asset
//!   - `extract_user_text`: the derivation every prompt runs through, once
//!     for the hub's echo and once for the Backend's transcript
//!   - `HistoryEntry` serialisation: what `GET /history` costs per turn

use mezame::backend::{
    extract_user_text, EntryBody, HistoryEntry, ToolCall, ToolCallStatus, ToolContent, ToolLocation,
};
use mezame::http::mime_for;
use serde_json::{json, Value};

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

/// A prompt block list of `blocks` members, two thirds text and the rest
/// attachments, which is the mix a composer with a pasted image produces.
fn make_blocks(blocks: usize) -> Vec<Value> {
    (0..blocks)
        .map(|i| match i % 3 {
            0 => json!({
                "type": "image",
                "mimeType": "image/png",
                "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8AAAwAB"
            }),
            _ => json!({
                "type": "text",
                "text": format!("block number {i} with some surrounding context")
            }),
        })
        .collect()
}

#[divan::bench(args = [1, 8, 64])]
fn bench_extract_user_text(bencher: divan::Bencher, blocks: usize) {
    let blocks = make_blocks(blocks);
    bencher.bench(|| extract_user_text(divan::black_box(&blocks)));
}

/// A transcript of `entries` members cycling through the five roles, so
/// the tagged enum, the flattened body and the tool-call payload are all
/// measured.
fn make_transcript(entries: usize) -> Vec<HistoryEntry> {
    (0..entries)
        .map(|i| {
            let timestamp = 1_700_000_000_000 + i as i64;
            let body = match i % 5 {
                0 => EntryBody::User {
                    text: format!("question number {i} with some surrounding context"),
                },
                1 => EntryBody::Agent {
                    text: format!("answer to question {i}"),
                },
                2 => EntryBody::Thought {
                    text: format!("reasoning about question {i}"),
                },
                3 => EntryBody::Sys {
                    text: format!("a notice raised during turn {i}"),
                },
                _ => EntryBody::ToolCall(ToolCall {
                    tool_call_id: format!("tool-{i}"),
                    title: "Read".to_string(),
                    status: ToolCallStatus::Completed,
                    kind: Some("read".to_string()),
                    raw_input: json!({ "path": format!("/src/file{i}.rs") }),
                    content: Some(vec![ToolContent::Text {
                        text: format!("the contents of file {i}"),
                    }]),
                    locations: Some(vec![ToolLocation {
                        path: format!("/src/file{i}.rs"),
                        line: Some(42),
                    }]),
                }),
            };
            HistoryEntry { body, timestamp }
        })
        .collect()
}

#[divan::bench(args = [1, 16, 128])]
fn bench_serialise_transcript(bencher: divan::Bencher, entries: usize) {
    let transcript = make_transcript(entries);
    bencher.bench(|| serde_json::to_value(divan::black_box(&transcript)));
}
