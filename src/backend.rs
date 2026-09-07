//! The seam between the Hub and whatever produces a turn.
//!
//! The Hub owns the wire: it counts subscribers, fans events out to every
//! attached browser, stamps the targeted ones, and derives the frames that
//! open and close a turn. A Backend owns the turn itself. Everything the
//! two share is in this module: the [`Backend`] trait, the [`TurnOutcome`]
//! a resolved turn reports, the transcript types a `GET /history` response
//! is built from, and the one derivation of the user's plain text that both
//! the Hub's echo and a Backend's transcript are defined against.
//!
//! [`EchoBackend`] is the implementation this build ships. It answers every
//! prompt with the text it was given and talks to no provider.

use std::collections::VecDeque;
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// What a resolved turn reports to the Hub.
///
/// Empty in this phase. The Hub reads it when building `prompt_done`, and a
/// later phase adds fields here to populate `prompt_done.usage` with no
/// change to any signature and no change to any call site.
#[derive(Debug, Default)]
pub struct TurnOutcome {}

/// Everything that stands between the Hub and whatever produces a turn.
///
/// Six operations: run one turn, cancel the turn in flight, answer a
/// permission request, change the selected model, report the transcript,
/// and shut down.
///
/// # Obligations an implementation takes on
///
/// - `prompt` streams only `append` with `role` `agent` or `sys`,
///   `thought`, `tool_call` and `permission_request` events. `ready`,
///   `session_info`, `prompt_done`, `error` and the `user` echo belong to
///   the Hub.
/// - No event is streamed outside a turn. The Hub closes the event channel
///   when the turn resolves; anything sent after that is dropped, and an
///   implementation must not rely on a retained clone of `events`
///   outliving the turn.
/// - Failure is reported by resolving the turn with `Err`. The Hub derives
///   the `error` frame from it. The Hub catches a panic in the polled
///   `prompt` future, and nothing else: an implementation that spawns its
///   own tasks must fold their failure into the turn's `Err` rather than
///   await a side channel that a dead task never answers.
/// - `cancel` while a turn is open must cause that turn to resolve
///   promptly, `Err` or `Ok`; the Hub derives the `prompt_done` that
///   acknowledges a cancel from that resolution. `cancel` with no turn
///   open is a no-op.
/// - `shutdown` may arrive while a turn is open, on the capped-hold exit
///   of a detached hub. It must cause that turn to resolve promptly and
///   must be idempotent, so the turn task, its `Arc` on the Backend and
///   the outbound sender are released with the hub instead of living to
///   the end of the process.
/// - Every `permission_request` id is minted by the implementation and is
///   unique for the life of the hub; the Hub's first-answer-wins set is
///   keyed on it.
/// - `history` returns promptly while a turn is open. An implementation
///   that held its transcript lock across a turn would stall the history
///   endpoint for the length of a turn.
///
/// # Shapes no Rust type in this module pins
///
/// A `permission_request` carries `id`, `title` and an `options` array
/// whose members hold `optionId` and optionally `name` and `kind`.
///
/// `set_model` returns the whole `session_info.info` object on success: an
/// object whose only key is `models`, holding `currentModelId` and an
/// `availableModels` array whose members hold `modelId` and optionally
/// `name` and `description`.
pub trait Backend: Send + Sync + 'static {
    /// Run one turn for `blocks`, streaming its events into `events`.
    fn prompt(
        &self,
        blocks: Vec<Value>,
        events: mpsc::UnboundedSender<Value>,
    ) -> BoxFuture<'_, Result<TurnOutcome>>;

    /// Cancel the turn in flight. A no-op with no turn open.
    fn cancel(&self) -> BoxFuture<'_, ()>;

    /// Answer the `permission_request` minted under `id` with `option_id`.
    fn permission_response(&self, id: Value, option_id: String) -> BoxFuture<'_, ()>;

    /// Change the selected model.
    ///
    /// `Ok` returns the whole `session_info.info` object, an object whose
    /// only key is `models`.
    fn set_model(&self, model_id: String) -> BoxFuture<'_, Result<Value>>;

    /// Report the transcript, in recorded order.
    ///
    /// An implementation bounds what it retains and reports the retained
    /// window. A transcript lives in memory for the life of its hub, and
    /// one with no bound is one a browser can grow at wire speed.
    fn history(&self) -> BoxFuture<'_, Vec<HistoryEntry>>;

    /// Release everything this Backend holds. Idempotent.
    fn shutdown(&self) -> BoxFuture<'_, ()>;
}

/// One entry of a Backend's transcript. Serialises directly into a
/// `GET /history` entry.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryEntry {
    /// The role-tagged payload.
    #[serde(flatten)]
    pub body: EntryBody,
    /// Milliseconds since the Unix epoch.
    pub timestamp: i64,
}

/// The payload of a transcript entry, tagged on `role`.
///
/// Serde serialises a newtype variant holding a struct as that struct's
/// map plus the tag, so a tool-call entry emits `role: "tool_call"`
/// alongside the payload's own keys with no duplicated field list.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum EntryBody {
    /// What the user said, with no `> ` prefix and no trailing newline.
    /// The browser adds both when it renders the entry.
    User { text: String },
    /// What the Backend answered.
    Agent { text: String },
    /// A notice the Hub or the Backend raised.
    Sys { text: String },
    /// Reasoning text.
    Thought { text: String },
    /// One tool call and whatever is known of its result.
    ToolCall(ToolCall),
}

/// A tool call, in the one shape the wire event and the history entry
/// share.
///
/// `kind`, `content` and `locations` take no `skip_serializing_if`: the
/// wire contract requires them present, holding JSON null, when absent.
/// The browser reads a null field as "no change" and keeps the value it
/// already holds.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    /// The id every update for this call is keyed on.
    pub tool_call_id: String,
    /// What to show the user.
    pub title: String,
    /// Where the call has got to.
    pub status: ToolCallStatus,
    /// The tool's category, when the producer supplied one.
    pub kind: Option<String>,
    /// The arguments the call was made with. Any JSON value.
    pub raw_input: Value,
    /// The call's output, when there is any.
    pub content: Option<Vec<ToolContent>>,
    /// The files the call touched, when the producer reported any.
    pub locations: Option<Vec<ToolLocation>>,
}

/// The four states a tool call can be in. An enum, so a string outside
/// the four is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Not started.
    Pending,
    /// Running.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Finished with a failure.
    Failed,
}

/// One block of a tool call's output.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolContent {
    /// Plain text.
    Text { text: String },
}

/// One file a tool call touched.
#[derive(Debug, Clone, Serialize)]
pub struct ToolLocation {
    /// The path, as the producer reported it.
    pub path: String,
    /// The line within that path. The one optional field in the wire
    /// contract, and so the one field that is omitted rather than nulled
    /// when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// The wire form of a [`ToolCall`]. Three fields, and the reason the wire
/// shape and the history shape cannot drift: they are one struct, read
/// twice.
#[derive(Debug, Serialize)]
pub struct ToolCallEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    #[serde(flatten)]
    body: &'a ToolCall,
}

impl ToolCall {
    /// The only way to build the wire form; `type` is fixed to
    /// `tool_call`.
    pub fn wire(&self) -> ToolCallEvent<'_> {
        ToolCallEvent {
            event_type: "tool_call",
            body: self,
        }
    }
}

/// The user's plain text, pulled out of a prompt block list.
///
/// `None` when no block carries a `text` string under a `type` of `text`,
/// which is the image-only, resource-only and empty-list case. Otherwise
/// the `text` fields of the text blocks in command order, joined by
/// exactly one newline, which is `Some("")` for a single text block whose
/// text is empty. `image` and `resource` blocks contribute nothing.
///
/// The distinction the `Option` carries is what a Backend branches on:
/// `None` is a prompt with nothing to answer, and `Some("")` is an empty
/// thing said.
pub fn extract_user_text(blocks: &[Value]) -> Option<String> {
    let mut texts = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            texts.push(text);
        }
    }
    if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    }
}

/// The byte length of the text [`extract_user_text`] derives for
/// `blocks`, with no allocation: the text blocks' lengths plus one for
/// each newline the join inserts. Zero for a list with no text block.
pub fn user_text_len(blocks: &[Value]) -> usize {
    let mut total = 0;
    let mut texts = 0usize;
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = block.get("text").and_then(Value::as_str) {
            total += text.len();
            texts += 1;
        }
    }
    total + texts.saturating_sub(1)
}

/// The hub-owned user echo, defined in terms of [`extract_user_text`].
///
/// `text` is the derived join, prefixed once with `> `, followed by one
/// newline. A block list [`extract_user_text`] answers `None` for yields
/// `"> \n"`, so a peer browser sees a user turn and locks its composer for
/// an image-only prompt too.
pub fn user_echo_event(blocks: &[Value]) -> Value {
    let text = extract_user_text(blocks).unwrap_or_default();
    json!({
        "type": "append",
        "role": "user",
        "text": format!("> {text}\n")
    })
}

/// What an [`EchoBackend`] answers a prompt that carries no text block.
const NO_TEXT_BLOCK_REPLY: &str = "The prompt held no text block, so there is nothing to echo.";

/// Milliseconds since the Unix epoch, or 0 for a clock set before it.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Ceiling on the text an [`EchoBackend`] transcript retains, 16 MiB.
///
/// A transcript lives in memory for the life of its hub, and every turn
/// adds the prompt's text twice: once as the user entry, once as the
/// echoed reply. Past the ceiling the oldest turn is evicted, a turn at a
/// time, until the transcript fits again; the newest turn is always kept.
/// The transcript therefore holds the most recent conversation and never
/// more than this much text plus the newest turn. Against the hub's 1 MiB
/// prompt text ceiling that is at least eight turns of the largest
/// prompt, and thousands of ordinary ones.
pub const TRANSCRIPT_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// Ceiling on the entries an [`EchoBackend`] transcript retains, 10,000,
/// which is 5,000 turns. Bytes alone would let a run of empty prompts
/// grow the entry count without bound.
pub const TRANSCRIPT_MAX_ENTRIES: usize = 10_000;

/// The Backend this build ships: it answers every prompt with the text it
/// was given.
///
/// It exists to prove the transport. A browser connects, sends a prompt,
/// and sees its own words come back as an agent turn, on every attached
/// device at once, with the transcript surviving a reload for as long as
/// the hub does. No provider is contacted and no model is selectable.
pub struct EchoBackend {
    transcript: Mutex<Transcript>,
}

impl EchoBackend {
    /// A fresh Backend with an empty transcript under the shipped
    /// ceilings, [`TRANSCRIPT_BUDGET_BYTES`] and
    /// [`TRANSCRIPT_MAX_ENTRIES`]. One per hub.
    pub fn new() -> Self {
        Self::with_budget(TRANSCRIPT_BUDGET_BYTES, TRANSCRIPT_MAX_ENTRIES)
    }

    /// A fresh Backend whose transcript is held under `budget_bytes` of
    /// entry text and `max_entries` entries.
    pub fn with_budget(budget_bytes: usize, max_entries: usize) -> Self {
        Self {
            transcript: Mutex::new(Transcript::new(budget_bytes, max_entries)),
        }
    }
}

/// The entries an [`EchoBackend`] retains, the running byte count, and
/// the two ceilings they are held under.
struct Transcript {
    entries: VecDeque<HistoryEntry>,
    /// The sum of [`entry_text_len`] over `entries`.
    bytes: usize,
    budget_bytes: usize,
    max_entries: usize,
}

impl Transcript {
    fn new(budget_bytes: usize, max_entries: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            budget_bytes,
            max_entries,
        }
    }

    /// Append one turn's two entries, then evict the oldest turns until
    /// the transcript is inside both ceilings again. The turn just
    /// recorded is never evicted, so a single turn larger than the whole
    /// budget is retained on its own.
    fn record_turn(&mut self, user: HistoryEntry, agent: HistoryEntry) {
        self.bytes += entry_text_len(&user) + entry_text_len(&agent);
        self.entries.push_back(user);
        self.entries.push_back(agent);
        while self.entries.len() > 2
            && (self.bytes > self.budget_bytes || self.entries.len() > self.max_entries)
        {
            // Entries are recorded in pairs, so the front two are one
            // turn.
            for _ in 0..2 {
                if let Some(evicted) = self.entries.pop_front() {
                    self.bytes -= entry_text_len(&evicted);
                }
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

/// The bytes an entry's text occupies, which is what the transcript
/// budget counts. An echo transcript holds text entries only; the
/// tool-call arm measures the serialised call so the count stays honest
/// should one ever be recorded.
fn entry_text_len(entry: &HistoryEntry) -> usize {
    match &entry.body {
        EntryBody::User { text }
        | EntryBody::Agent { text }
        | EntryBody::Sys { text }
        | EntryBody::Thought { text } => text.len(),
        EntryBody::ToolCall(call) => serde_json::to_vec(call).map_or(0, |v| v.len()),
    }
}

impl Default for EchoBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for EchoBackend {
    fn prompt(
        &self,
        blocks: Vec<Value>,
        events: mpsc::UnboundedSender<Value>,
    ) -> BoxFuture<'_, Result<TurnOutcome>> {
        Box::pin(async move {
            let (user_text, reply) = match extract_user_text(&blocks) {
                Some(text) => (text.clone(), text),
                None => (String::new(), NO_TEXT_BLOCK_REPLY.to_string()),
            };

            // Record before streaming, so a history request racing this
            // turn's `prompt_done` never shows less than a browser saw.
            // One timestamp per turn, clamped upward against the last
            // recorded value: the non-decreasing rule then holds as an
            // invariant of the transcript, whatever a system clock that
            // steps backwards does.
            {
                let mut transcript = self.transcript.lock().expect("transcript lock");
                let timestamp =
                    now_ms().max(transcript.entries.back().map_or(i64::MIN, |e| e.timestamp));
                transcript.record_turn(
                    HistoryEntry {
                        body: EntryBody::User { text: user_text },
                        timestamp,
                    },
                    HistoryEntry {
                        body: EntryBody::Agent {
                            text: reply.clone(),
                        },
                        timestamp,
                    },
                );
            }

            // A send failure means the Hub has closed the channel, which
            // it does when the turn is over. Nothing to report.
            let _ = events.send(json!({
                "type": "append",
                "role": "agent",
                "text": reply
            }));
            Ok(TurnOutcome::default())
        })
    }

    fn cancel(&self) -> BoxFuture<'_, ()> {
        // A turn resolves in one poll. There is never one to cancel.
        Box::pin(std::future::ready(()))
    }

    fn permission_response(&self, _id: Value, _option_id: String) -> BoxFuture<'_, ()> {
        // Nothing here ever asks for a permission.
        Box::pin(std::future::ready(()))
    }

    fn set_model(&self, _model_id: String) -> BoxFuture<'_, Result<Value>> {
        Box::pin(std::future::ready(Err(anyhow!(
            "No model is selectable: this build answers every prompt with an echo and talks to no provider."
        ))))
    }

    fn history(&self) -> BoxFuture<'_, Vec<HistoryEntry>> {
        Box::pin(async move {
            self.transcript
                .lock()
                .expect("transcript lock")
                .entries
                .iter()
                .cloned()
                .collect()
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.transcript.lock().expect("transcript lock").clear();
        })
    }
}
