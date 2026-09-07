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

use anyhow::{anyhow, Result};
use futures_util::future::BoxFuture;
use mezame::backend::{Backend, HistoryEntry, TurnOutcome};
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
    transcript: Mutex<Vec<HistoryEntry>>,
    turn: TurnSlot,
    model_release: Slot<Result<Value, String>>,
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
        })
    }
}
