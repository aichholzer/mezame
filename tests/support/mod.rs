#![allow(dead_code)]

//! `ScriptedBackend`: a Backend a test drives frame by frame.
//!
//! It exists so the surviving transport behaviour can be asserted with no
//! subprocess, no socket and no file. A test scripts the events each turn
//! streams and how that turn resolves, then reads back every method the
//! Hub invoked, in order, with the arguments each invocation received.
//!
//! `cancel` and `shutdown` release only a turn that is open. With no turn
//! open they record their invocation and leave nothing behind, which is
//! the trait's no-op; an earlier shape stored a release the next `Pending`
//! turn consumed at once. A test that cancels a turn must first sync on
//! something the turn produced (its echo, its first event, or
//! `prompt_count()`), so the turn is open when the cancel lands.
//!
//! Interior mutability is `std::sync::Mutex` throughout and no lock is
//! held across an await. Reading the invocation log while a turn is
//! unresolved is therefore a plain function call from any task. A
//! `tokio::sync::Mutex` would make every assertion helper async and would
//! invite holding a guard across an await, which is the shape that stalls
//! the history endpoint for a real Backend.
//!
//! The module sits under `tests/`, so cargo compiles it into the test
//! binaries that declare `mod support;` and into nothing else. It is
//! absent from the library's public API and from the shipped binary.

use std::collections::VecDeque;
use std::sync::Mutex;

use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use mezame::backend::{Backend, HistoryEntry, TurnOutcome};
use mezame::conversation::{Message, Role};
use mezame::provider::{
    Provider, ProviderError, ProviderRequest, ThinkingMode, TurnEvent, TurnStream,
};
use serde_json::Value;
use tokio::sync::{mpsc, Notify};

/// One scripted turn: the events it streams, and how it ends.
pub struct ScriptedTurn {
    /// Streamed into the Hub's channel in this order, before the turn
    /// resolves. Zero events is a valid script.
    pub events: Vec<Value>,
    /// How the turn ends once its events are away.
    pub resolution: Resolution,
}

impl ScriptedTurn {
    /// Stream `events`, then resolve with success.
    pub fn success(events: Vec<Value>) -> Self {
        Self {
            events,
            resolution: Resolution::Success,
        }
    }

    /// Stream `events`, then resolve with an error holding `message`.
    pub fn error(events: Vec<Value>, message: impl Into<String>) -> Self {
        Self {
            events,
            resolution: Resolution::Error(message.into()),
        }
    }

    /// Stream `events`, then panic with `message`.
    pub fn panicking(events: Vec<Value>, message: impl Into<String>) -> Self {
        Self {
            events,
            resolution: Resolution::Panic(message.into()),
        }
    }

    /// Stream `events`, then stay open until the test releases the turn.
    pub fn pending(events: Vec<Value>) -> Self {
        Self {
            events,
            resolution: Resolution::Pending,
        }
    }
}

/// How a scripted turn ends.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// Resolve with success.
    Success,
    /// Resolve with an error holding this text.
    Error(String),
    /// Panic with this text. Reaches the Hub's `catch_unwind`.
    Panic(String),
    /// Stay unresolved until the test calls
    /// [`ScriptedBackend::release_turn`], on no timer of its own.
    Pending,
}

/// How a test ends a [`Resolution::Pending`] turn.
#[derive(Debug, Clone)]
pub enum Release {
    /// Resolve with success.
    Ok,
    /// Resolve with an error holding this text.
    Err(String),
    /// Panic with this text.
    Panic(String),
}

/// One recorded call into the Backend, with what it was called with.
#[derive(Debug, Clone, PartialEq)]
pub enum Invocation {
    /// `prompt`, with the block list it received.
    Prompt(Vec<Value>),
    /// `cancel`.
    Cancel,
    /// `permission_response`, with both arguments.
    PermissionResponse { id: Value, option_id: String },
    /// `set_model`, with the model id.
    SetModel(String),
    /// `shutdown`.
    Shutdown,
}

/// The hand-off a blocked call waits on: a `Pending` turn, or a
/// `set_model` on a Backend built with `set_model_pending`.
///
/// `Notify` stores one permit, so a release that lands before the call has
/// parked is not lost. The waiting side checks the slot before it waits,
/// so a permit consumed by an earlier call cannot leave a later one
/// parked.
struct Slot<T> {
    slot: Mutex<Option<T>>,
    notify: Notify,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self {
            slot: Mutex::new(None),
            notify: Notify::new(),
        }
    }
}

impl<T> Slot<T> {
    fn put(&self, value: T) {
        *self.slot.lock().expect("release slot") = Some(value);
        self.notify.notify_one();
    }

    async fn take(&self) -> T {
        loop {
            let taken = self.slot.lock().expect("release slot").take();
            if let Some(value) = taken {
                return value;
            }
            self.notify.notified().await;
        }
    }
}

/// The release hand-off for a `Pending` turn, gated on a turn being open.
///
/// `release_turn` stores unconditionally: a release that lands before the
/// turn parks must not be lost. `cancel` and `shutdown` store only while a
/// turn is open, which is the trait's "a cancel with no turn open is a
/// no-op". The open flag lives under the same mutex as the value, so the
/// check and the store are one step against the turn closing on any
/// runtime flavour. Closing the turn discards a release nobody took.
#[derive(Default)]
struct TurnSlot {
    state: Mutex<TurnState>,
    notify: Notify,
}

#[derive(Default)]
struct TurnState {
    open: bool,
    release: Option<Release>,
}

impl TurnSlot {
    /// Mark a turn open for as long as the returned guard lives.
    fn open(&self) -> OpenTurn<'_> {
        self.state.lock().expect("turn slot").open = true;
        OpenTurn(self)
    }

    /// Store a release for the open turn. With no turn open, store
    /// nothing and report it.
    fn put_if_open(&self, release: Release) -> bool {
        let mut state = self.state.lock().expect("turn slot");
        if !state.open {
            return false;
        }
        state.release = Some(release);
        drop(state);
        self.notify.notify_one();
        true
    }

    /// Store a release whether or not a turn is open.
    fn put(&self, release: Release) {
        self.state.lock().expect("turn slot").release = Some(release);
        self.notify.notify_one();
    }

    async fn take(&self) -> Release {
        loop {
            let taken = self.state.lock().expect("turn slot").release.take();
            if let Some(release) = taken {
                return release;
            }
            self.notify.notified().await;
        }
    }
}

/// Closes the turn on drop: on resolve, on a panic unwind, and on a turn
/// future dropped mid-flight. A release aimed at this turn and never
/// consumed goes with it.
struct OpenTurn<'a>(&'a TurnSlot);

impl Drop for OpenTurn<'_> {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.open = false;
        state.release = None;
    }
}

/// A Backend whose every answer a test supplies up front.
#[derive(Default)]
pub struct ScriptedBackend {
    turns: Mutex<VecDeque<ScriptedTurn>>,
    invocations: Mutex<Vec<Invocation>>,
    set_model: Mutex<Option<Result<Value, String>>>,
    /// When set, every `set_model` parks until the test calls
    /// [`ScriptedBackend::release_set_model`]. Stands in for a Backend
    /// whose model change does I/O.
    set_model_blocks: bool,
    /// When set, every `shutdown` parks after recording its invocation
    /// until the test calls [`ScriptedBackend::release_shutdown`]. Stands
    /// in for a Backend whose shutdown does I/O.
    shutdown_blocks: bool,
    transcript: Mutex<Vec<HistoryEntry>>,
    turn: TurnSlot,
    model_release: Slot<Result<Value, String>>,
    shutdown_release: Slot<()>,
}

impl ScriptedBackend {
    /// A Backend with nothing scripted. An unscripted turn and an
    /// unscripted model change each resolve with an error naming the
    /// omission.
    pub fn new() -> Self {
        Self::default()
    }

    /// A Backend that will run `turns`, in order.
    pub fn with_turns(turns: Vec<ScriptedTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
            ..Self::default()
        }
    }

    /// A Backend that will run one turn.
    pub fn with_turn(turn: ScriptedTurn) -> Self {
        Self::with_turns(vec![turn])
    }

    /// Set the transcript `history` reports, unchanged, on every call.
    pub fn transcript(mut self, transcript: Vec<HistoryEntry>) -> Self {
        self.transcript = Mutex::new(transcript);
        self
    }

    /// Set what `set_model` resolves with. `Ok` carries the whole
    /// `session_info.info` value.
    pub fn set_model_outcome(mut self, outcome: Result<Value, String>) -> Self {
        self.set_model = Mutex::new(Some(outcome));
        self
    }

    /// Make every `set_model` park until [`ScriptedBackend::release_set_model`]
    /// is called, one release per call.
    pub fn set_model_pending(mut self) -> Self {
        self.set_model_blocks = true;
        self
    }

    /// End the `set_model` call that is parked, with `outcome`.
    pub fn release_set_model(&self, outcome: Result<Value, String>) {
        self.model_release.put(outcome);
    }

    /// Make every `shutdown` park until [`ScriptedBackend::release_shutdown`]
    /// is called, one release per call.
    pub fn shutdown_pending(mut self) -> Self {
        self.shutdown_blocks = true;
        self
    }

    /// End the `shutdown` call that is parked.
    pub fn release_shutdown(&self) {
        self.shutdown_release.put(());
    }

    /// Append a turn to the script. Usable while a turn is unresolved.
    pub fn push_turn(&self, turn: ScriptedTurn) {
        self.turns.lock().expect("turns").push_back(turn);
    }

    /// End the open `Pending` turn. A release that lands before the turn
    /// parks is kept for it.
    pub fn release_turn(&self, release: Release) {
        self.turn.put(release);
    }

    /// Every recorded invocation, in invocation order. Readable while a
    /// turn is unresolved.
    pub fn invocations(&self) -> Vec<Invocation> {
        self.invocations.lock().expect("invocations").clone()
    }

    /// How many `prompt` calls have been recorded.
    pub fn prompt_count(&self) -> usize {
        self.invocations()
            .iter()
            .filter(|i| matches!(i, Invocation::Prompt(_)))
            .count()
    }

    /// Whether an invocation equal to `wanted` has been recorded.
    pub fn saw(&self, wanted: &Invocation) -> bool {
        self.invocations().iter().any(|i| i == wanted)
    }

    /// How many recorded invocations equal `wanted`.
    pub fn count_of(&self, wanted: &Invocation) -> usize {
        self.invocations().iter().filter(|i| *i == wanted).count()
    }
}

impl Backend for ScriptedBackend {
    fn prompt(
        &self,
        blocks: Vec<Value>,
        events: mpsc::UnboundedSender<Value>,
    ) -> BoxFuture<'_, Result<TurnOutcome>> {
        Box::pin(async move {
            let turn = {
                self.invocations
                    .lock()
                    .expect("invocations")
                    .push(Invocation::Prompt(blocks));
                self.turns.lock().expect("turns").pop_front()
            };
            // Open for the whole poll, the unscripted turn included, so a
            // cancel that lands while this future is alive is delivered.
            let _open = self.turn.open();
            let Some(turn) = turn else {
                return Err(anyhow!(
                    "ScriptedBackend: no turn was scripted for this prompt"
                ));
            };
            for event in turn.events {
                // A send failure means the Hub closed the channel, which
                // it does when the turn is over.
                let _ = events.send(event);
            }
            match turn.resolution {
                Resolution::Success => Ok(TurnOutcome::default()),
                Resolution::Error(message) => Err(anyhow!(message)),
                Resolution::Panic(message) => panic!("{message}"),
                Resolution::Pending => match self.turn.take().await {
                    Release::Ok => Ok(TurnOutcome::default()),
                    Release::Err(message) => Err(anyhow!(message)),
                    Release::Panic(message) => panic!("{message}"),
                },
            }
        })
    }

    fn cancel(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.invocations
                .lock()
                .expect("invocations")
                .push(Invocation::Cancel);
            // The trait obligation: a cancel while a turn is open makes
            // that turn resolve promptly, and a cancel with no turn open
            // is a no-op. The invocation is recorded either way.
            self.turn.put_if_open(Release::Err("cancelled".to_string()));
        })
    }

    fn permission_response(&self, id: Value, option_id: String) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.invocations
                .lock()
                .expect("invocations")
                .push(Invocation::PermissionResponse { id, option_id });
        })
    }

    fn set_model(&self, model_id: String) -> BoxFuture<'_, Result<Value>> {
        Box::pin(async move {
            self.invocations
                .lock()
                .expect("invocations")
                .push(Invocation::SetModel(model_id));
            if self.set_model_blocks {
                return match self.model_release.take().await {
                    Ok(info) => Ok(info),
                    Err(message) => Err(anyhow!(message)),
                };
            }
            let configured = self.set_model.lock().expect("set_model").clone();
            match configured {
                Some(Ok(info)) => Ok(info),
                Some(Err(message)) => Err(anyhow!(message)),
                None => Err(anyhow!(
                    "ScriptedBackend: no model-change outcome was scripted"
                )),
            }
        })
    }

    fn history(&self) -> BoxFuture<'_, Vec<HistoryEntry>> {
        Box::pin(async move { self.transcript.lock().expect("transcript").clone() })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.invocations
                .lock()
                .expect("invocations")
                .push(Invocation::Shutdown);
            self.turns.lock().expect("turns").clear();
            // Idempotent: a second call finds no turn open and stores
            // nothing.
            self.turn.put_if_open(Release::Err("cancelled".to_string()));
            if self.shutdown_blocks {
                self.shutdown_release.take().await;
            }
        })
    }
}

// ---------- ScriptedProvider ----------

/// One scripted stream: what `Provider::stream` returns for one request.
pub enum ScriptedStream {
    /// Yield every event, then end.
    Events(Vec<TurnEvent>),
    /// Yield `first`, then hold the stream open until
    /// [`ScriptedProvider::release_stream`] supplies the rest, after which
    /// the stream ends.
    Pending { first: Vec<TurnEvent> },
    /// Park the `stream()` future itself until
    /// [`ScriptedProvider::release_send`] supplies a script, or until it is
    /// dropped. Stands in for a request the SDK is still retrying.
    PendingSend,
    /// Fail before any event.
    BeforeStream {
        retryable: bool,
        rejected: bool,
        message: String,
    },
    /// Fail before any event because the replayed reasoning could not be
    /// read: the loop drops its reasoning blocks and retries once.
    StaleReasoning,
    /// Panic when the `stream()` future is polled.
    Panicking(String),
}

/// What one request looked like, for a test to assert on.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestLog {
    pub model: String,
    pub thinking: ThinkingMode,
    pub thinking_budget: u32,
    pub max_output_tokens: u32,
    pub roles: Vec<Role>,
    pub messages: Vec<Message>,
    pub system_static: String,
    pub date_line: String,
}

/// A Provider whose every stream a test supplies up front.
#[derive(Default)]
pub struct ScriptedProvider {
    streams: Mutex<VecDeque<ScriptedStream>>,
    requests: Mutex<Vec<RequestLog>>,
    stream_release: Arc<Slot<Vec<TurnEvent>>>,
    send_release: Arc<Slot<ScriptedStream>>,
}

impl ScriptedProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_streams(streams: Vec<ScriptedStream>) -> Self {
        Self {
            streams: Mutex::new(streams.into()),
            ..Self::default()
        }
    }

    pub fn with_stream(stream: ScriptedStream) -> Self {
        Self::with_streams(vec![stream])
    }

    pub fn push_stream(&self, stream: ScriptedStream) {
        self.streams.lock().expect("scripts").push_back(stream);
    }

    /// Supply the rest of a `Pending` stream; the stream ends after them.
    pub fn release_stream(&self, rest: Vec<TurnEvent>) {
        self.stream_release.put(rest);
    }

    /// Supply the script a `PendingSend` request resolves to.
    pub fn release_send(&self, script: ScriptedStream) {
        self.send_release.put(script);
    }

    /// Every request received, in order.
    pub fn requests(&self) -> Vec<RequestLog> {
        self.requests.lock().expect("requests").clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().expect("requests").len()
    }

    fn record(&self, request: &ProviderRequest) {
        self.requests.lock().expect("requests").push(RequestLog {
            model: request.model.clone(),
            thinking: request.thinking,
            thinking_budget: request.thinking_budget,
            max_output_tokens: request.max_output_tokens,
            roles: request.messages.iter().map(|m| m.role).collect(),
            messages: request.messages.clone(),
            system_static: request.system.static_text.clone(),
            date_line: request.system.date_line.clone(),
        });
    }

    fn open(&self, script: ScriptedStream) -> Result<TurnStream, ProviderError> {
        match script {
            ScriptedStream::Events(events) => Ok(Box::pin(futures_util::stream::iter(events))),
            ScriptedStream::Pending { first } => {
                let release = Arc::clone(&self.stream_release);
                let rest = futures_util::stream::once(async move { release.take().await })
                    .flat_map(futures_util::stream::iter);
                Ok(Box::pin(futures_util::stream::iter(first).chain(rest)))
            }
            ScriptedStream::BeforeStream {
                retryable,
                rejected,
                message,
            } => Err(ProviderError::BeforeStream {
                retryable,
                rejected,
                stale_reasoning: false,
                message,
            }),
            ScriptedStream::StaleReasoning => Err(ProviderError::BeforeStream {
                retryable: false,
                rejected: false,
                stale_reasoning: true,
                message: "Invalid `signature` in `thinking` block.".to_string(),
            }),
            ScriptedStream::Panicking(message) => panic!("{message}"),
            ScriptedStream::PendingSend => unreachable!("resolved by the caller"),
        }
    }
}

impl Provider for ScriptedProvider {
    fn stream(&self, request: ProviderRequest) -> BoxFuture<'_, Result<TurnStream, ProviderError>> {
        Box::pin(async move {
            self.record(&request);
            let script = self
                .streams
                .lock()
                .expect("scripts")
                .pop_front()
                .unwrap_or_else(|| ScriptedStream::BeforeStream {
                    retryable: false,
                    rejected: false,
                    message: "no stream was scripted for this request".to_string(),
                });
            let script = match script {
                ScriptedStream::PendingSend => self.send_release.take().await,
                other => other,
            };
            self.open(script)
        })
    }
}

// ---------- store wrappers ----------

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use mezame::conversation::Block;
use mezame::provider::Usage;
use mezame::store::{
    CredentialRow, MessageStats, MessageWindow, NewProfile, ProfileRow, SessionList, SessionRow,
    Store, StoreError, StoreFuture, UserRow, WorkspaceRow,
};

/// Forward every `Store` method of `$wrapper` to `self.inner`, after running
/// `$before` (a closure over `&self` returning `Result<(), StoreError>`).
macro_rules! forward_store {
    ($wrapper:ty, $before:expr) => {
        #[allow(unused_variables)]
        impl Store for $wrapper {
            fn backend_name(&self) -> &'static str {
                self.inner.backend_name()
            }
            fn health(&self) -> StoreFuture<'_, ()> {
                let gate = ($before)(self);
                Box::pin(async move {
                    gate?;
                    self.inner.health().await
                })
            }
            fn create_user(
                &self,
                name: &str,
                password_hash: &str,
                role: mezame::store::Role,
                now: i64,
            ) -> StoreFuture<'_, UserRow> {
                let gate = ($before)(self);
                let (name, hash) = (name.to_string(), password_hash.to_string());
                Box::pin(async move {
                    gate?;
                    self.inner.create_user(&name, &hash, role, now).await
                })
            }
            fn user_by_name(&self, name: &str) -> StoreFuture<'_, Option<UserRow>> {
                let gate = ($before)(self);
                let name = name.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.user_by_name(&name).await
                })
            }
            fn user_by_id(&self, id: &str) -> StoreFuture<'_, Option<UserRow>> {
                let gate = ($before)(self);
                let id = id.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.user_by_id(&id).await
                })
            }
            fn list_users(&self) -> StoreFuture<'_, Vec<UserRow>> {
                let gate = ($before)(self);
                Box::pin(async move {
                    gate?;
                    self.inner.list_users().await
                })
            }
            fn count_users(&self) -> StoreFuture<'_, u64> {
                let gate = ($before)(self);
                Box::pin(async move {
                    gate?;
                    self.inner.count_users().await
                })
            }
            fn password_hash_of(&self, name: &str) -> StoreFuture<'_, Option<String>> {
                let gate = ($before)(self);
                let name = name.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.password_hash_of(&name).await
                })
            }
            fn set_password_hash(&self, id: &str, hash: &str) -> StoreFuture<'_, ()> {
                let gate = ($before)(self);
                let (id, hash) = (id.to_string(), hash.to_string());
                Box::pin(async move {
                    gate?;
                    self.inner.set_password_hash(&id, &hash).await
                })
            }
            fn bump_session_epoch(&self, id: &str) -> StoreFuture<'_, u64> {
                let gate = ($before)(self);
                let id = id.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.bump_session_epoch(&id).await
                })
            }
            fn settings(&self, id: &str) -> StoreFuture<'_, Value> {
                let gate = ($before)(self);
                let id = id.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.settings(&id).await
                })
            }
            fn set_settings(&self, id: &str, settings: &Value) -> StoreFuture<'_, ()> {
                let gate = ($before)(self);
                let (id, settings) = (id.to_string(), settings.clone());
                Box::pin(async move {
                    gate?;
                    self.inner.set_settings(&id, &settings).await
                })
            }
            fn default_workspace(&self, user_id: &str) -> StoreFuture<'_, Option<WorkspaceRow>> {
                let gate = ($before)(self);
                let user_id = user_id.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.default_workspace(&user_id).await
                })
            }
            fn create_session(
                &self,
                user_id: &str,
                id: &str,
                workspace_root: Option<&Path>,
                now: i64,
            ) -> StoreFuture<'_, SessionRow> {
                let gate = ($before)(self);
                let (user_id, id, root) = (
                    user_id.to_string(),
                    id.to_string(),
                    workspace_root.map(Path::to_path_buf),
                );
                Box::pin(async move {
                    gate?;
                    self.inner
                        .create_session(&user_id, &id, root.as_deref(), now)
                        .await
                })
            }
            fn session(&self, id: &str) -> StoreFuture<'_, Option<SessionRow>> {
                let gate = ($before)(self);
                let id = id.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.session(&id).await
                })
            }
            fn list_sessions(&self, user_id: &str) -> StoreFuture<'_, SessionList> {
                let gate = ($before)(self);
                let user_id = user_id.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.list_sessions(&user_id).await
                })
            }
            fn set_title(&self, id: &str, title: &str, now: i64) -> StoreFuture<'_, ()> {
                let gate = ($before)(self);
                let (id, title) = (id.to_string(), title.to_string());
                Box::pin(async move {
                    gate?;
                    self.inner.set_title(&id, &title, now).await
                })
            }
            fn set_title_if_null(&self, id: &str, title: &str, now: i64) -> StoreFuture<'_, bool> {
                let gate = ($before)(self);
                self.on_title_write();

                let (id, title) = (id.to_string(), title.to_string());
                Box::pin(async move {
                    gate?;
                    self.inner.set_title_if_null(&id, &title, now).await
                })
            }
            fn set_archived(&self, id: &str, archived: bool, now: i64) -> StoreFuture<'_, ()> {
                let gate = ($before)(self);
                let id = id.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.set_archived(&id, archived, now).await
                })
            }
            fn delete_session(&self, id: &str) -> StoreFuture<'_, ()> {
                let gate = ($before)(self);
                let id = id.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.delete_session(&id).await
                })
            }
            fn append_user(
                &self,
                session_id: &str,
                blocks: &[Block],
                text: &str,
                created: i64,
            ) -> StoreFuture<'_, i64> {
                let gate = ($before)(self);
                let (session_id, blocks, text) =
                    (session_id.to_string(), blocks.to_vec(), text.to_string());
                Box::pin(async move {
                    gate?;
                    self.inner
                        .append_user(&session_id, &blocks, &text, created)
                        .await
                })
            }
            fn append_assistant(
                &self,
                session_id: &str,
                blocks: &[Block],
                usage: Option<Usage>,
                rejected: bool,
                created: i64,
            ) -> StoreFuture<'_, i64> {
                let gate = ($before)(self);
                let (session_id, blocks) = (session_id.to_string(), blocks.to_vec());
                Box::pin(async move {
                    gate?;
                    self.inner
                        .append_assistant(&session_id, &blocks, usage, rejected, created)
                        .await
                })
            }
            fn mark_rejected(&self, message_ids: &[i64]) -> StoreFuture<'_, ()> {
                let gate = ($before)(self);
                let ids = message_ids.to_vec();
                Box::pin(async move {
                    gate?;
                    self.inner.mark_rejected(&ids).await
                })
            }
            fn load_window(
                &self,
                session_id: &str,
                max_rows: usize,
                max_bytes: usize,
            ) -> StoreFuture<'_, MessageWindow> {
                let gate = ($before)(self);
                self.on_load();
                let session_id = session_id.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner
                        .load_window(&session_id, max_rows, max_bytes)
                        .await
                })
            }
            fn message_stats(&self, session_id: &str) -> StoreFuture<'_, MessageStats> {
                let gate = ($before)(self);
                let session_id = session_id.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.message_stats(&session_id).await
                })
            }
            fn create_credential(
                &self,
                owner: Option<&str>,
                creator: &str,
                provider: &str,
                label: &str,
                payload: &Value,
                now: i64,
            ) -> StoreFuture<'_, CredentialRow> {
                let gate = ($before)(self);
                let (owner, creator, provider, label, payload) = (
                    owner.map(str::to_string),
                    creator.to_string(),
                    provider.to_string(),
                    label.to_string(),
                    payload.clone(),
                );
                Box::pin(async move {
                    gate?;
                    self.inner
                        .create_credential(
                            owner.as_deref(),
                            &creator,
                            &provider,
                            &label,
                            &payload,
                            now,
                        )
                        .await
                })
            }
            fn credentials(
                &self,
                owner: Option<&str>,
                provider: &str,
            ) -> StoreFuture<'_, Vec<CredentialRow>> {
                let gate = ($before)(self);
                let (owner, provider) = (owner.map(str::to_string), provider.to_string());
                Box::pin(async move {
                    gate?;
                    self.inner.credentials(owner.as_deref(), &provider).await
                })
            }
            fn credential_payload(&self, id: &str) -> StoreFuture<'_, Value> {
                let gate = ($before)(self);
                let id = id.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.credential_payload(&id).await
                })
            }
            fn delete_credential(&self, id: &str) -> StoreFuture<'_, ()> {
                let gate = ($before)(self);
                let id = id.to_string();
                Box::pin(async move {
                    gate?;
                    self.inner.delete_credential(&id).await
                })
            }
            fn drop_all_credentials(&self) -> StoreFuture<'_, u64> {
                let gate = ($before)(self);
                Box::pin(async move {
                    gate?;
                    self.inner.drop_all_credentials().await
                })
            }
            fn global_profile(&self) -> StoreFuture<'_, Option<ProfileRow>> {
                let gate = ($before)(self);
                Box::pin(async move {
                    gate?;
                    self.inner.global_profile().await
                })
            }
            fn upsert_global_profile(&self, profile: &NewProfile) -> StoreFuture<'_, ProfileRow> {
                let gate = ($before)(self);
                let profile = profile.clone();
                Box::pin(async move {
                    gate?;
                    self.inner.upsert_global_profile(&profile).await
                })
            }
        }
    };
}

/// A Store whose every operation fails on demand, for the loop's failed
/// write path (Requirement 8.2). Forwards to `inner` until `fail()` is
/// called; `recover()` forwards again.
pub struct FailingStore {
    inner: Arc<dyn Store>,
    failing: AtomicBool,
}

impl FailingStore {
    pub fn new(inner: Arc<dyn Store>) -> Self {
        Self {
            inner,
            failing: AtomicBool::new(false),
        }
    }

    pub fn fail(&self) {
        self.failing.store(true, Ordering::SeqCst);
    }

    pub fn recover(&self) {
        self.failing.store(false, Ordering::SeqCst);
    }

    fn gate(&self) -> Result<(), StoreError> {
        if self.failing.load(Ordering::SeqCst) {
            Err(StoreError::Internal(
                "the failing store refused".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    fn on_load(&self) {}
    fn on_title_write(&self) {}
}

forward_store!(FailingStore, |s: &FailingStore| s.gate());

/// A Store that counts `load_window` calls, for the two-phase factory's
/// once-per-build assertion, and `set_title_if_null` calls, for the
/// once-per-session title write.
pub struct CountingStore {
    inner: Arc<dyn Store>,
    loads: AtomicUsize,
    title_writes: AtomicUsize,
}

impl CountingStore {
    pub fn new(inner: Arc<dyn Store>) -> Self {
        Self {
            inner,
            loads: AtomicUsize::new(0),
            title_writes: AtomicUsize::new(0),
        }
    }

    pub fn loads(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }

    pub fn title_writes(&self) -> usize {
        self.title_writes.load(Ordering::SeqCst)
    }

    fn on_load(&self) {
        self.loads.fetch_add(1, Ordering::SeqCst);
    }

    fn on_title_write(&self) {
        self.title_writes.fetch_add(1, Ordering::SeqCst);
    }
}

forward_store!(CountingStore, |_: &CountingStore| Ok::<(), StoreError>(()));
