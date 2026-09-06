#![allow(dead_code)]

//! `ScriptedBackend`: a Backend a test drives frame by frame.
//!
//! It exists so the surviving transport behaviour can be asserted with no
//! subprocess, no socket and no file. A test scripts the events each turn
//! streams and how that turn resolves, then reads back every method the
//! Hub invoked, in order, with the arguments each invocation received.
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

/// The hand-off a `Pending` turn waits on.
///
/// `Notify` stores one permit, so a release that lands before the turn has
/// parked is not lost. The waiting side checks the slot before it waits,
/// so a permit consumed by an earlier turn cannot leave a later one
/// parked.
#[derive(Default)]
struct ReleaseSlot {
    slot: Mutex<Option<Release>>,
    notify: Notify,
}

impl ReleaseSlot {
    fn put(&self, release: Release) {
        *self.slot.lock().expect("release slot") = Some(release);
        self.notify.notify_one();
    }

    async fn take(&self) -> Release {
        loop {
            let taken = self.slot.lock().expect("release slot").take();
            if let Some(release) = taken {
                return release;
            }
            self.notify.notified().await;
        }
    }
}

/// A Backend whose every answer a test supplies up front.
#[derive(Default)]
pub struct ScriptedBackend {
    turns: Mutex<VecDeque<ScriptedTurn>>,
    invocations: Mutex<Vec<Invocation>>,
    set_model: Mutex<Option<Result<Value, String>>>,
    transcript: Mutex<Vec<HistoryEntry>>,
    release: ReleaseSlot,
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

    /// Append a turn to the script. Usable while a turn is unresolved.
    pub fn push_turn(&self, turn: ScriptedTurn) {
        self.turns.lock().expect("turns").push_back(turn);
    }

    /// End the open `Pending` turn.
    pub fn release_turn(&self, release: Release) {
        self.release.put(release);
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
                Resolution::Pending => match self.release.take().await {
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
            // that turn resolve promptly. A cancel with no turn open
            // leaves a release the next `Pending` turn consumes, which is
            // why no test sends one.
            self.release.put(Release::Err("cancelled".to_string()));
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
            self.release.put(Release::Err("cancelled".to_string()));
        })
    }
}
