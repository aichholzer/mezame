//! The seam between the turn loop and a model API.
//!
//! A [`Provider`] opens one stream for one request. Whatever the API sends
//! back is normalised into [`TurnEvent`], the one vocabulary the loop
//! consumes, so the loop is written once and a second provider is a
//! transport and a normaliser. [`bedrock`] is the implementation this
//! phase ships.
//!
//! Two rules every implementation keeps: an [`TurnEvent::Error`] is the
//! last item its stream yields, and dropping the stream is the cancel.

pub mod bedrock;

use std::fmt;
use std::pin::Pin;
use std::str::FromStr;

use futures_util::future::BoxFuture;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conversation::Message;
use crate::prompt::SystemPrompt;

/// One event of a model's reply, provider-neutral.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    /// The reply has started; `model` is the id the request named.
    MessageStart { model: String },
    /// A fragment of the visible answer.
    TextDelta(String),
    /// A reasoning block opened. `id` is unique within the reply.
    ThinkingStart { id: String },
    /// A fragment of the reasoning block's summary text.
    ThinkingDelta { id: String, text: String },
    /// The reasoning block closed. A block without a signature cannot be
    /// replayed, and the loop keeps only the signed ones.
    ThinkingEnd {
        id: String,
        signature: Option<String>,
    },
    /// A provider-opaque block, replayed to the same provider unchanged.
    /// `raw` is JSON so a later store can hold it as-is.
    OpaqueBlock { raw: Value },
    /// A tool call opened. Phase 1 declares no tool, so the loop treats
    /// this as a failure; the variant exists because the normaliser maps
    /// what the stream can carry.
    ToolUseStart { id: String, name: String },
    /// A fragment of the tool call's JSON input.
    ToolInputDelta { id: String, json_fragment: String },
    /// The tool call closed.
    ToolUseEnd { id: String },
    /// The token counts of the turn.
    Usage(Usage),
    /// Why the model stopped.
    Stop(StopReason),
    /// The stream failed. Always the last item.
    Error { retryable: bool, message: String },
}

/// Why a reply ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    ContextWindowExceeded,
    ContentFiltered,
    Refusal,
    /// A reason the provider named and this enum does not.
    Other(String),
}

/// The four counts a turn reports. `input` is what the provider billed as
/// uncached input; the whole prompt is `input + cache_read + cache_write`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u32,
    pub output: u32,
    pub cache_read: u32,
    pub cache_write: u32,
}

/// How a request asks the model to think.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingMode {
    /// The model decides when and how much; the summary is shown.
    Adaptive,
    /// A fixed token budget, for the models that take one.
    Enabled,
    /// No thinking is asked for. On models that think by default the
    /// reasoning still runs, hidden.
    Off,
}

impl ThinkingMode {
    /// The spellings `bedrock.thinking` takes, in the order the error names
    /// them.
    pub const NAMES: [&'static str; 3] = ["adaptive", "enabled", "off"];

    /// The configuration spelling of this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingMode::Adaptive => "adaptive",
            ThinkingMode::Enabled => "enabled",
            ThinkingMode::Off => "off",
        }
    }
}

impl FromStr for ThinkingMode {
    type Err = String;

    /// Exact, lowercase spellings only; the error names the accepted set.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "adaptive" => Ok(ThinkingMode::Adaptive),
            "enabled" => Ok(ThinkingMode::Enabled),
            "off" => Ok(ThinkingMode::Off),
            other => Err(format!(
                "must be one of `adaptive`, `enabled` or `off`, not `{other}`"
            )),
        }
    }
}

/// What a turn loop is configured with. `BedrockConfig::settings` builds
/// it with the defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopSettings {
    /// The model selected at startup.
    pub model: String,
    /// The ids the picker offers; `model` is among them.
    pub models: Vec<String>,
    /// The configured override, or `None` to derive the mode per model.
    pub thinking: Option<ThinkingMode>,
    /// The budget `ThinkingMode::Enabled` sends.
    pub thinking_budget: u32,
    /// The output ceiling of every request.
    pub max_output_tokens: u32,
}

/// Everything one request is built from.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRequest {
    pub model: String,
    pub system: SystemPrompt,
    /// The conversation so far, ending with the user message of this turn.
    pub messages: Vec<Message>,
    pub thinking: ThinkingMode,
    /// Read only when `thinking` is `Enabled`.
    pub thinking_budget: u32,
    pub max_output_tokens: u32,
}

/// What a browser is told when the conversation no longer fits the model,
/// whether the model said so at the end of a reply or the service refused
/// the request as too long before any event.
pub const CONTEXT_WINDOW_ERROR: &str =
    "The conversation no longer fits the model's context window; start a new session.";

/// The events of one reply, in order. Dropping it aborts the request.
pub type TurnStream = Pin<Box<dyn Stream<Item = TurnEvent> + Send>>;

/// Why a stream could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// The request failed before any event. `rejected` marks a request the
    /// service refused for its content: the loop keeps the user's text in
    /// the transcript and never sends that message again.
    BeforeStream {
        retryable: bool,
        rejected: bool,
        message: String,
    },
}

impl ProviderError {
    /// The text a browser is shown.
    pub fn message(&self) -> &str {
        match self {
            ProviderError::BeforeStream { message, .. } => message,
        }
    }

    /// Whether the service refused the request for its content.
    pub fn is_rejected(&self) -> bool {
        match self {
            ProviderError::BeforeStream { rejected, .. } => *rejected,
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ProviderError {}

/// One operation: open the stream for a request.
pub trait Provider: Send + Sync + 'static {
    /// Send `request` and resolve to its event stream, or to why it could
    /// not be sent.
    fn stream(&self, request: ProviderRequest) -> BoxFuture<'_, Result<TurnStream, ProviderError>>;
}
