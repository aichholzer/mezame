//! The turn loop without tools: a [`Backend`] over a [`Provider`].
//!
//! One turn is: map the prompt's blocks to a user message, record it,
//! open the provider's stream, apply its events to an accumulator while
//! forwarding text as `append` frames and reasoning as `thought` frames,
//! then persist what the stream produced and resolve. The loop owns the
//! conversation the next request is built from, with the transcript
//! recorded beside it, and writes the store's rows after each transition.
//! A persisting deployment serves `GET /history` from those rows, rebuilt
//! through `Conversation::restore`; the transcript held here answers
//! `Backend::history`, which the route reads only in the echo fallback.
//!
//! Sessions must not brick. Every way a turn can end resolves it: a stream
//! that stalls is cut after [`IDLE_TIMEOUT`], a cancel interrupts the
//! pending request as well as the stream, a shutdown closes the loop so a
//! turn resolving afterwards persists nothing, and every lock recovers
//! from poison. No lock is held across an await.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};

use crate::backend::{extract_user_text, now_ms, Backend, EntryBody, HistoryEntry, TurnOutcome};
use crate::conversation::{self, Block, Conversation, Message, Role};
use crate::prompt::{assemble, preamble, today_utc, SystemPrompt};
use crate::provider::bedrock::{thinking_rule, PROVIDER_NAME};
use crate::provider::{
    LoopSettings, Provider, ProviderError, ProviderRequest, StopReason, ThinkingMode, TurnEvent,
    Usage,
};
use crate::store::{MessageWindow, Store, StoreError};

/// How long the stream may go without an event before the turn is cut.
///
/// Five minutes rather than the two the plan first named: a model whose
/// thinking is hidden (`thinking: off` on a family that always thinks, or
/// a summary the provider does not stream) sends nothing at all while it
/// reasons, and a hard prompt reasons for minutes. The timer bounds a
/// stall, not a long reply; a user who wants out sooner has the Stop
/// button.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// What the browser is told when the model asked for a tool.
pub const TOOL_ERROR: &str = "The model asked for a tool; tools arrive in a later release.";
/// What the browser is told when the stream closed without a stop reason.
pub const STREAM_ENDED_ERROR: &str = "The stream ended before the model finished.";
/// What a prompt is told when it arrives after the session shut down.
pub const CLOSED_ERROR: &str = "The session is shutting down.";
/// The stop-reason texts, in the order of the design's table. The
/// context-window one is shared with the provider, which uses it when the
/// service refuses a request as too long.
pub use crate::provider::CONTEXT_WINDOW_ERROR;
pub const CONTENT_FILTERED_ERROR: &str =
    "Bedrock filtered the reply (content filter or guardrail).";
pub const REFUSAL_ERROR: &str = "The model refused the request.";

/// The `session_info.info` object for `settings` with `current` selected:
/// what the factory seeds a hub with and what `set_model` returns.
pub fn session_info_for(settings: &LoopSettings, current: &str) -> Value {
    let models: Vec<Value> = settings
        .models
        .iter()
        .map(|id| {
            let mode = settings.thinking.unwrap_or_else(|| thinking_rule(id));
            json!({
                "modelId": id,
                "name": id,
                "description": describe_thinking(mode),
            })
        })
        .collect();
    json!({
        "models": {
            "currentModelId": current,
            "availableModels": models
        }
    })
}

fn describe_thinking(mode: ThinkingMode) -> &'static str {
    match mode {
        ThinkingMode::Adaptive => "adaptive thinking",
        ThinkingMode::Enabled => "thinking with a budget",
        ThinkingMode::Off => "no thinking requested",
    }
}

/// The cancel of one turn: a flag the turn polls and a wake for the poll.
#[derive(Clone, Default)]
struct CancelHandle {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelHandle {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn same_as(&self, other: &CancelHandle) -> bool {
        Arc::ptr_eq(&self.cancelled, &other.cancelled)
    }

    /// Resolves once cancelled. Checks the flag before it waits, so a
    /// cancel that landed before the wait is never missed.
    async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            self.notify.notified().await;
        }
    }
}

/// What the loop guards under its lock.
struct State {
    conversation: Conversation,
    model: String,
    thinking: ThinkingMode,
    /// The system prompt every request of this conversation carries,
    /// assembled when the conversation is empty and frozen after that. A
    /// reasoning block's signature is bound to the system prompt it was
    /// produced under, so the date line may change between conversations
    /// and never within one; the frozen text is also what keeps the
    /// system cache point warm.
    system: Option<SystemPrompt>,
    /// Set by `shutdown`: a later `prompt` resolves at once and a turn
    /// resolving afterwards persists nothing.
    closed: bool,
}

/// A [`Backend`] that runs each turn against a [`Provider`].
///
/// With a store, every transition of the conversation is written after
/// it: the user row before the request, the assistant row and the
/// rejection flags after the reply. The in-memory conversation stays
/// authoritative for the hub's lifetime; a failed write is logged once per
/// session and the turn resolves as if it had succeeded, so the store can
/// lose a row and never a session.
pub struct LoopBackend {
    provider: Arc<dyn Provider>,
    settings: LoopSettings,
    session_id: String,
    /// The owner's name, for the per-turn log line.
    owner_name: String,
    store: Option<Arc<dyn Store>>,
    /// Set by the first failed write; the one that sets it writes the log
    /// line, the rest are counted and silent.
    store_failed: AtomicBool,
    store_failures: AtomicUsize,
    store_failure_lines: AtomicUsize,
    idle_timeout: Duration,
    state: Mutex<State>,
    turn: Mutex<Option<CancelHandle>>,
}

/// Lock a mutex, recovering from poison: a holder panicking is a bug
/// elsewhere and must not turn every later lock into a panic that wedges
/// the hub.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl LoopBackend {
    /// A loop for `session_id` over `provider`, with `settings.model`
    /// selected, owned by `owner_name` and writing to `store` when one is
    /// given. One per hub.
    pub fn new(
        provider: Arc<dyn Provider>,
        settings: LoopSettings,
        session_id: &str,
        owner_name: &str,
        store: Option<Arc<dyn Store>>,
    ) -> Self {
        let thinking = settings
            .thinking
            .unwrap_or_else(|| thinking_rule(&settings.model));
        Self {
            provider,
            session_id: session_id.to_string(),
            owner_name: owner_name.to_string(),
            store,
            store_failed: AtomicBool::new(false),
            store_failures: AtomicUsize::new(0),
            store_failure_lines: AtomicUsize::new(0),
            idle_timeout: IDLE_TIMEOUT,
            state: Mutex::new(State {
                conversation: Conversation::new(),
                model: settings.model.clone(),
                thinking,
                system: None,
                closed: false,
            }),
            turn: Mutex::new(None),
            settings,
        }
    }

    /// Rebuild the conversation from the session's stored rows, before the
    /// first turn: see [`Conversation::restore`]. What the window holds is
    /// what the next request replays, less every reasoning block.
    pub fn restore(&mut self, window: MessageWindow) {
        lock(&self.state).conversation.restore(window);
    }

    /// Test-only: how many store writes have failed.
    #[doc(hidden)]
    pub fn store_failures_for_test(&self) -> usize {
        self.store_failures.load(Ordering::SeqCst)
    }

    /// Test-only: how many failure lines were written; one at most.
    #[doc(hidden)]
    pub fn store_failure_lines_for_test(&self) -> usize {
        self.store_failure_lines.load(Ordering::SeqCst)
    }

    /// Test-only: a shorter idle timeout, so a paused-clock test drives
    /// the stall in milliseconds of virtual time.
    #[doc(hidden)]
    pub fn with_idle_timeout_for_test(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    /// Test-only: a conversation under a lowered budget.
    #[doc(hidden)]
    pub fn with_conversation_for_test(self, conversation: Conversation) -> Self {
        lock(&self.state).conversation = conversation;
        self
    }

    /// Test-only: whether the back exchange awaits its reply.
    #[doc(hidden)]
    pub fn has_open_exchange_for_test(&self) -> bool {
        lock(&self.state).conversation.has_open_exchange()
    }

    /// The model selected for the next request.
    pub fn current_model(&self) -> String {
        lock(&self.state).model.clone()
    }

    /// The `session_info.info` object for the model selected now: what the
    /// factory seeds a hub with, from the same state the first request
    /// reads.
    pub fn session_info(&self) -> Value {
        session_info_for(&self.settings, &lock(&self.state).model)
    }

    /// Run one turn. The numbered steps are the design's; step 0, the
    /// cancel handle, ran in `prompt` before this future existed.
    async fn run_turn(
        &self,
        blocks: Vec<Value>,
        events: mpsc::UnboundedSender<Value>,
        cancel: CancelHandle,
    ) -> Result<TurnOutcome> {
        let started = Instant::now();
        // Whatever way this future ends, the slot stops naming this turn's
        // handle, so a later cancel cannot reach a turn that is over.
        let _slot = HandleSlot {
            slot: &self.turn,
            handle: cancel.clone(),
        };

        // 1. Map the blocks and hold them to the provider's limits. Nothing
        //    is recorded on a refusal.
        let user = conversation::user_message_from_blocks(&blocks).map_err(|e| anyhow!("{e}"))?;
        {
            let state = lock(&self.state);
            if state.closed {
                return Err(anyhow!(CLOSED_ERROR));
            }
            let mut merged: Vec<&Message> = state.conversation.messages();
            merged.push(&user);
            conversation::check_limits(merged).map_err(|e| anyhow!("{e}"))?;
        }

        // 2. Record the user side, then snapshot what the request needs.
        //    The store's copy of the user side follows, with the lock
        //    released: the write is the one await between the record and
        //    the request, and the row it names is set on the exchange
        //    once the id is back.
        let text = extract_user_text(&blocks).unwrap_or_default();
        let user_blocks = self.store.is_some().then(|| user.blocks.clone());
        let (messages, system, model, thinking, timestamp) = {
            let mut state = lock(&self.state);
            if state.closed {
                return Err(anyhow!(CLOSED_ERROR));
            }
            let timestamp = clamped_now(&state.conversation);
            // A fresh conversation takes today's prompt; a running one
            // keeps the prompt it started under.
            if state.conversation.exchange_count() == 0 || state.system.is_none() {
                state.system = Some(assemble(&[preamble()], today_utc()));
            }
            state.conversation.begin(
                user,
                HistoryEntry {
                    body: EntryBody::User { text: text.clone() },
                    timestamp,
                    usage: None,
                },
            );
            let messages: Vec<Message> =
                state.conversation.messages().into_iter().cloned().collect();
            let system = state.system.clone().expect("set above");
            (
                messages,
                system,
                state.model.clone(),
                state.thinking,
                timestamp,
            )
        };
        //    A user row that could not be written holds back the turn's
        //    reply, see `persist`.
        let mut user_written = true;
        if let (Some(store), Some(user_blocks)) = (&self.store, user_blocks) {
            match store
                .append_user(&self.session_id, &user_blocks, &text, timestamp)
                .await
            {
                Ok(id) => lock(&self.state).conversation.set_user_row(id),
                Err(e) => {
                    user_written = false;
                    self.store_failure("append_user", &e);
                }
            }
        }

        // 3. Open the stream, under the cancel handle: the SDK's retries
        //    could otherwise run for minutes with the composer locked. A
        //    request refused for the reasoning it replayed is sent once
        //    more without any reasoning: the recovery the provider allows.
        let mut request = ProviderRequest {
            model: model.clone(),
            system,
            messages,
            thinking,
            thinking_budget: self.settings.thinking_budget,
            max_output_tokens: self.settings.max_output_tokens,
        };
        let mut accumulator = Accumulator::new(&model);
        let mut retried = false;
        let stream = loop {
            let attempt = request.clone();
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return self
                        .resolve(Ended::Cancelled, accumulator, &events, &model, started, user_written)
                        .await;
                }
                opened = self.provider.stream(attempt) => match opened {
                    Ok(stream) => break stream,
                    Err(error) if error.is_stale_reasoning() && !retried => {
                        retried = true;
                        let dropped = {
                            let mut state = lock(&self.state);
                            let dropped = state.conversation.strip_reasoning();
                            request.messages =
                                state.conversation.messages().into_iter().cloned().collect();
                            dropped
                        };
                        crate::hub::log(&format!(
                            "turn session={} user={} model={model} dropped {dropped} reasoning \
                             block(s) the model could not read and retried the request",
                            self.session_id, self.owner_name
                        ));
                    }
                    Err(error) => {
                        return self
                            .resolve(
                                Ended::BeforeStream(error),
                                accumulator,
                                &events,
                                &model,
                                started,
                                user_written,
                            )
                            .await;
                    }
                }
            }
        };

        // 4. Consume the stream under the cancel handle and the idle timer.
        let mut stream = stream;
        let ended = loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break Ended::Cancelled,
                next = tokio::time::timeout(self.idle_timeout, stream.next()) => match next {
                    Err(_elapsed) => break Ended::Idle,
                    Ok(None) => break Ended::Complete,
                    Ok(Some(event)) => {
                        if let Some(ended) = accumulator.apply(event, &events) {
                            break ended;
                        }
                    }
                },
            }
        };
        // Dropping the stream aborts the connection.
        drop(stream);

        // 5. Record, write and resolve.
        self.resolve(ended, accumulator, &events, &model, started, user_written)
            .await
    }
}

/// Clears the cancel slot on drop when it still names this turn's handle.
struct HandleSlot<'a> {
    slot: &'a Mutex<Option<CancelHandle>>,
    handle: CancelHandle,
}

impl Drop for HandleSlot<'_> {
    fn drop(&mut self) {
        let mut slot = lock(self.slot);
        if slot.as_ref().is_some_and(|held| held.same_as(&self.handle)) {
            *slot = None;
        }
    }
}

/// One write the store receives after a turn's in-memory record, in the
/// order the loop issues them.
enum StoreWrite {
    /// The assistant row: the kept blocks, the counts when known, flagged
    /// when the reply was refused.
    Assistant {
        blocks: Vec<Block>,
        usage: Option<Usage>,
        rejected: bool,
        created: i64,
    },
    /// The user rows of the exchanges a rejection covered.
    Rejected { user_ids: Vec<i64> },
}

impl LoopBackend {
    /// Record what the turn produced, write the store's copy of it, and
    /// resolve. The record and the log line are synchronous under the
    /// lock; the writes follow with the lock released and the turn still
    /// open, so a browser learns of `prompt_done` after the rows exist.
    /// `user_written` says whether the turn's user row landed.
    async fn resolve(
        &self,
        ended: Ended,
        accumulator: Accumulator,
        events: &mpsc::UnboundedSender<Value>,
        model: &str,
        started: Instant,
        user_written: bool,
    ) -> Result<TurnOutcome> {
        let (outcome, writes) = self.finish(ended, accumulator, events, model, started);
        self.persist(writes, user_written).await;
        // A hub torn down while this turn ran (an archive, a delete, the
        // grace timer) left the conversation to us so the reply could be
        // recorded and written; it is ours to clear now.
        let mut state = lock(&self.state);
        if state.closed {
            state.conversation.clear();
        }
        outcome
    }

    /// Record what the turn produced, resolve it, and write the log line.
    /// Returns the outcome and the writes the store is owed for it.
    fn finish(
        &self,
        ended: Ended,
        accumulator: Accumulator,
        events: &mpsc::UnboundedSender<Value>,
        model: &str,
        started: Instant,
    ) -> (Result<TurnOutcome>, Vec<StoreWrite>) {
        let usage = accumulator.usage;
        let stop = accumulator.stop.clone();
        let mut writes = Vec::new();
        let outcome: Result<TurnOutcome> = {
            // A turn resolving after `shutdown` finds the conversation it
            // began with: `shutdown` leaves it for a turn in flight, so the
            // reply is recorded and written like any other, and `resolve`
            // clears it afterwards.
            let mut state = lock(&self.state);
            match &ended {
                Ended::BeforeStream(error) => {
                    if error.is_rejected() {
                        if let Some(user_ids) = state.conversation.reject_open(Vec::new()) {
                            writes.push(StoreWrite::Rejected { user_ids });
                        }
                    } else {
                        state.conversation.complete(None, Vec::new());
                    }
                    Err(anyhow!("{}", error.message()))
                }
                _ => {
                    let timestamp = clamped_now(&state.conversation);
                    let (assistant, entries) = accumulator.kept(timestamp, usage);
                    let blocks = assistant.as_ref().map(|message| message.blocks.clone());
                    // A reply the model refused or the service filtered is
                    // content the next request must not carry again: the
                    // exchange is rejected, its entries kept for the
                    // transcript, however the stream ended after the stop
                    // (a cancel or a stall before the counts arrive
                    // included). Everything else closes the exchange.
                    let refused = matches!(
                        &stop,
                        Some(StopReason::Refusal) | Some(StopReason::ContentFiltered)
                    );
                    if refused {
                        if let Some(user_ids) = state.conversation.reject_open(entries) {
                            if let Some(blocks) = blocks {
                                writes.push(StoreWrite::Assistant {
                                    blocks,
                                    usage,
                                    rejected: true,
                                    created: timestamp,
                                });
                            }
                            writes.push(StoreWrite::Rejected { user_ids });
                        }
                    } else if state.conversation.complete(assistant, entries) {
                        if let Some(blocks) = blocks {
                            writes.push(StoreWrite::Assistant {
                                blocks,
                                usage,
                                rejected: false,
                                created: timestamp,
                            });
                        }
                    }
                    match &ended {
                        Ended::Complete => match &stop {
                            Some(StopReason::EndTurn) | Some(StopReason::StopSequence) => {
                                Ok(TurnOutcome { usage })
                            }
                            Some(StopReason::MaxTokens) => {
                                let _ = events.send(json!({
                                    "type": "append",
                                    "role": "sys",
                                    "text": format!(
                                        "\n[The reply stopped at the output limit of {} tokens.]\n",
                                        self.settings.max_output_tokens
                                    )
                                }));
                                Ok(TurnOutcome { usage })
                            }
                            Some(StopReason::ContextWindowExceeded) => {
                                Err(anyhow!(CONTEXT_WINDOW_ERROR))
                            }
                            Some(StopReason::ContentFiltered) => {
                                Err(anyhow!(CONTENT_FILTERED_ERROR))
                            }
                            Some(StopReason::Refusal) => Err(anyhow!(REFUSAL_ERROR)),
                            // `apply` ends a turn on `Stop(ToolUse)` as `Ended::Tool`.
                            Some(StopReason::ToolUse) => Err(anyhow!(TOOL_ERROR)),
                            Some(StopReason::Other(reason)) => Err(anyhow!(
                                "The model stopped for an unexpected reason: {reason}."
                            )),
                            None => Err(anyhow!(STREAM_ENDED_ERROR)),
                        },
                        Ended::Failed(message) => Err(anyhow!("{message}")),
                        Ended::Idle => Err(anyhow!(
                            "No data from the provider for {} seconds; the reply was cut off.",
                            self.idle_timeout.as_secs()
                        )),
                        Ended::Tool => Err(anyhow!(TOOL_ERROR)),
                        Ended::Cancelled => Ok(TurnOutcome { usage: None }),
                        Ended::BeforeStream(_) => unreachable!("handled above"),
                    }
                }
            }
        };
        let log = TurnLog {
            session: &self.session_id,
            user: &self.owner_name,
            model,
            outcome: match (&ended, &outcome) {
                (Ended::Cancelled, _) => "cancelled",
                (_, Ok(_)) => "ok",
                (_, Err(_)) => "error",
            },
            stop: stop.as_ref(),
            usage,
            elapsed: started.elapsed(),
            error: outcome.as_ref().err().map(|e| e.to_string()),
        };
        crate::hub::log(&log.render());
        (outcome, writes)
    }

    /// Issue the store's writes for a turn, in order, with no lock held.
    /// A failure is counted, reported once per session, and otherwise
    /// ignored: the conversation in memory is the one the session runs
    /// on.
    ///
    /// A turn whose user row was not written holds its reply back and
    /// counts it as a failed write. A rebuild pairs the rows by position,
    /// a user row with the assistant row after it, so a reply whose
    /// question is missing would be read as the answer to the question
    /// before it; held back, that question stays with no reply, which is
    /// what the rows say. A rejection's flags go through regardless: with
    /// no row of its own, the turn's ids name only the earlier unanswered
    /// rows the refused request merged in, which exist, and left unflagged
    /// they would be replayed as questions after a rebuild.
    async fn persist(&self, writes: Vec<StoreWrite>, user_written: bool) {
        let Some(store) = &self.store else {
            return;
        };
        for write in writes {
            match write {
                StoreWrite::Assistant { .. } if !user_written => {
                    // The user row's failure wrote the session's line, so
                    // the held-back reply is counted and not reported.
                    self.store_failures.fetch_add(1, Ordering::SeqCst);
                }
                StoreWrite::Assistant {
                    blocks,
                    usage,
                    rejected,
                    created,
                } => {
                    if let Err(e) = store
                        .append_assistant(&self.session_id, &blocks, usage, rejected, created)
                        .await
                    {
                        self.store_failure("append_assistant", &e);
                    }
                }
                StoreWrite::Rejected { user_ids } => {
                    if user_ids.is_empty() {
                        continue;
                    }
                    if let Err(e) = store.mark_rejected(&user_ids).await {
                        self.store_failure("mark_rejected", &e);
                    }
                }
            }
        }
    }

    /// Count a failed write and report the first one of the session.
    fn store_failure(&self, operation: &str, error: &StoreError) {
        self.store_failures.fetch_add(1, Ordering::SeqCst);
        if !self.store_failed.swap(true, Ordering::SeqCst) {
            self.store_failure_lines.fetch_add(1, Ordering::SeqCst);
            crate::hub::log(&format!(
                "turn session={} user={} store write {operation} failed: {error}; the session \
                 continues in memory and later failures on it are not reported",
                self.session_id, self.owner_name
            ));
        }
    }
}

/// One timestamp for a turn's entries, clamped upward against the last
/// recorded one so the non-decreasing rule survives a clock that steps
/// back.
fn clamped_now(conversation: &Conversation) -> i64 {
    now_ms().max(conversation.last_timestamp().unwrap_or(i64::MIN))
}

impl Backend for LoopBackend {
    fn prompt(
        &self,
        blocks: Vec<Value>,
        events: mpsc::UnboundedSender<Value>,
    ) -> BoxFuture<'_, Result<TurnOutcome>> {
        // 0. A fresh cancel handle before the future exists, so a cancel
        //    that lands before the turn task's first poll is seen by the
        //    turn it was aimed at and reaches nothing else.
        let cancel = CancelHandle::default();
        *lock(&self.turn) = Some(cancel.clone());
        Box::pin(self.run_turn(blocks, events, cancel))
    }

    fn cancel(&self) -> BoxFuture<'_, ()> {
        if let Some(handle) = lock(&self.turn).clone() {
            handle.cancel();
        }
        Box::pin(std::future::ready(()))
    }

    fn permission_response(&self, _id: Value, _option_id: String) -> BoxFuture<'_, ()> {
        // Nothing here ever asks for a permission.
        Box::pin(std::future::ready(()))
    }

    fn set_model(&self, model_id: String) -> BoxFuture<'_, Result<Value>> {
        let result = if self.settings.models.contains(&model_id) {
            let mut state = lock(&self.state);
            state.model = model_id.clone();
            state.thinking = self
                .settings
                .thinking
                .unwrap_or_else(|| thinking_rule(&model_id));
            Ok(session_info_for(&self.settings, &model_id))
        } else {
            Err(anyhow!(
                "`{model_id}` is not a configured model; the configured ids are {}",
                self.settings
                    .models
                    .iter()
                    .map(|id| format!("`{id}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        };
        Box::pin(std::future::ready(result))
    }

    fn history(&self) -> BoxFuture<'_, Vec<HistoryEntry>> {
        let entries = lock(&self.state).conversation.history();
        Box::pin(std::future::ready(entries))
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        // A turn in flight is cancelled and left to finish: it records
        // what the stream produced, writes the store's copy, and clears
        // the conversation itself once it sees `closed`. With no turn open
        // the clear happens here. Either way a later `prompt` resolves at
        // once and nothing of the conversation outlives the hub.
        let in_flight = match lock(&self.turn).as_ref() {
            Some(handle) => {
                handle.cancel();
                true
            }
            None => false,
        };
        let mut state = lock(&self.state);
        state.closed = true;
        if !in_flight {
            state.conversation.clear();
        }
        Box::pin(std::future::ready(()))
    }
}

/// How the stream ended.
enum Ended {
    /// The stream yielded `None`.
    Complete,
    /// The stream yielded an `Error` event.
    Failed(String),
    /// No event within the idle timeout.
    Idle,
    /// The model asked for a tool.
    Tool,
    /// Cancelled or shut down, before or after the stream opened.
    Cancelled,
    /// The request could not be sent.
    BeforeStream(ProviderError),
}

/// What the stream has produced so far.
struct Accumulator {
    model: String,
    /// Blocks in stream order; text accumulates into the last text block.
    blocks: Vec<Block>,
    /// The thinking block open now: its id and text.
    open_thinking: Option<(String, String)>,
    usage: Option<Usage>,
    stop: Option<StopReason>,
}

impl Accumulator {
    fn new(model: &str) -> Self {
        Self {
            model: model.to_string(),
            blocks: Vec::new(),
            open_thinking: None,
            usage: None,
            stop: None,
        }
    }

    /// Apply one event, forwarding what the browser sees. `Some` ends the
    /// turn.
    fn apply(&mut self, event: TurnEvent, events: &mpsc::UnboundedSender<Value>) -> Option<Ended> {
        match event {
            TurnEvent::MessageStart { .. } => {}
            TurnEvent::TextDelta(text) => {
                match self.blocks.last_mut() {
                    Some(Block::Text { text: current }) => current.push_str(&text),
                    _ => self.blocks.push(Block::Text { text: text.clone() }),
                }
                let _ = events.send(json!({ "type": "append", "role": "agent", "text": text }));
            }
            TurnEvent::ThinkingStart { id } => {
                self.open_thinking = Some((id, String::new()));
            }
            TurnEvent::ThinkingDelta { id, text } => {
                match &mut self.open_thinking {
                    Some((open, current)) if *open == id => current.push_str(&text),
                    _ => self.open_thinking = Some((id, text.clone())),
                }
                let _ = events.send(json!({ "type": "thought", "text": text }));
            }
            TurnEvent::ThinkingEnd { id, signature } => {
                // A block without a signature cannot be replayed and is
                // dropped; an end for an id never opened closes an empty
                // block, which is what a hidden-display block looks like.
                let text = match self.open_thinking.take() {
                    Some((open, text)) if open == id => text,
                    other => {
                        self.open_thinking = other;
                        String::new()
                    }
                };
                if let Some(signature) = signature {
                    self.blocks.push(Block::Thinking {
                        text,
                        signature: Some(signature),
                        provider: PROVIDER_NAME.to_string(),
                        model: self.model.clone(),
                    });
                }
            }
            TurnEvent::OpaqueBlock { raw } => self.blocks.push(Block::Opaque {
                provider: PROVIDER_NAME.to_string(),
                model: self.model.clone(),
                raw,
            }),
            TurnEvent::ToolUseStart { .. } => return Some(Ended::Tool),
            TurnEvent::ToolInputDelta { .. } | TurnEvent::ToolUseEnd { .. } => {}
            TurnEvent::Usage(usage) => {
                self.usage = Some(usage);
                // The stop and the counts are the last two things a reply
                // carries. With both in hand the turn is complete, whatever
                // the trailing end-of-body does: a cancel clicked now or a
                // transport error reading the tail must not turn a whole,
                // billed answer into a cancelled or failed one.
                if self.stop.is_some() {
                    return Some(Ended::Complete);
                }
            }
            TurnEvent::Stop(StopReason::ToolUse) => {
                self.stop = Some(StopReason::ToolUse);
                return Some(Ended::Tool);
            }
            TurnEvent::Stop(reason) => {
                self.stop = Some(reason);
                if self.usage.is_some() {
                    return Some(Ended::Complete);
                }
            }
            TurnEvent::Error { message, .. } => {
                return Some(if self.stop.is_some() {
                    Ended::Complete
                } else {
                    Ended::Failed(message)
                });
            }
        }
        None
    }

    /// The assistant message and transcript entries the turn keeps: the
    /// text so far, the signed thinking blocks and the opaque blocks, in
    /// stream order; a `thought` entry per kept thinking block with text,
    /// then an `agent` entry when there is text. An open thinking block
    /// has no signature and is dropped. A reply that produced no text at
    /// all is not persisted: an assistant message of reasoning alone has
    /// nothing to replay and the provider refuses an empty one.
    fn kept(self, timestamp: i64, usage: Option<Usage>) -> (Option<Message>, Vec<HistoryEntry>) {
        let has_text = self
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text { text } if !text.is_empty()));
        if !has_text {
            return (None, Vec::new());
        }
        let mut entries = Vec::new();
        let mut agent_text = String::new();
        for block in &self.blocks {
            match block {
                Block::Text { text } => agent_text.push_str(text),
                Block::Thinking { text, .. } if !text.is_empty() => entries.push(HistoryEntry {
                    body: EntryBody::Thought { text: text.clone() },
                    timestamp,
                    usage: None,
                }),
                _ => {}
            }
        }
        if !agent_text.is_empty() {
            entries.push(HistoryEntry {
                body: EntryBody::Agent { text: agent_text },
                timestamp,
                usage,
            });
        }
        let assistant = if self.blocks.is_empty() {
            None
        } else {
            Some(Message {
                role: Role::Assistant,
                blocks: self.blocks,
            })
        };
        (assistant, entries)
    }
}

/// The per-turn log line, rendered by a pure function so a test pins it.
pub struct TurnLog<'a> {
    pub session: &'a str,
    /// The owner's name.
    pub user: &'a str,
    pub model: &'a str,
    pub outcome: &'a str,
    pub stop: Option<&'a StopReason>,
    pub usage: Option<Usage>,
    pub elapsed: Duration,
    pub error: Option<String>,
}

impl TurnLog<'_> {
    /// `turn session=<id> user=<name> model=<id> outcome=<ok|cancelled|error>
    /// stop=<reason|-> in=<n> out=<n> cache_read=<n> cache_write=<n> ms=<n>`
    /// followed by ` error=<text>` on error; an unknown count is `-`.
    pub fn render(&self) -> String {
        let count = |n: Option<u32>| n.map_or("-".to_string(), |n| n.to_string());
        let stop = self.stop.map_or("-".to_string(), stop_name);
        let mut line = format!(
            "turn session={} user={} model={} outcome={} stop={} in={} out={} cache_read={} cache_write={} ms={}",
            self.session,
            self.user,
            self.model,
            self.outcome,
            stop,
            count(self.usage.map(|u| u.input)),
            count(self.usage.map(|u| u.output)),
            count(self.usage.map(|u| u.cache_read)),
            count(self.usage.map(|u| u.cache_write)),
            self.elapsed.as_millis(),
        );
        if let Some(error) = &self.error {
            // One line: the text is the browser's, and a line break in it
            // would split the log entry.
            line.push_str(" error=");
            line.push_str(&error.replace(['\n', '\r'], " "));
        }
        line
    }
}

/// The stop reason's name in the log line.
pub fn stop_name(stop: &StopReason) -> String {
    match stop {
        StopReason::EndTurn => "end_turn".to_string(),
        StopReason::ToolUse => "tool_use".to_string(),
        StopReason::MaxTokens => "max_tokens".to_string(),
        StopReason::StopSequence => "stop_sequence".to_string(),
        StopReason::ContextWindowExceeded => "context_window_exceeded".to_string(),
        StopReason::ContentFiltered => "content_filtered".to_string(),
        StopReason::Refusal => "refusal".to_string(),
        StopReason::Other(other) => other.clone(),
    }
}
