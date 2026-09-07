//! Amazon Bedrock through the AWS SDK's `ConverseStream`.
//!
//! Two halves. The transport builds a request from a [`ProviderRequest`],
//! sends it, and hands the SDK's event receiver to the normaliser. The
//! [`Normaliser`] is a pure function from one SDK event to zero or more
//! [`TurnEvent`]s, with no I/O and no clock, which is what makes it
//! testable against events built with the SDK's own builders: Bedrock's
//! event stream is binary and the SDK types are not deserialisable, so
//! recorded fixtures are not an option here.
//!
//! The event union is imported as [`StreamEvent`], and the operation output
//! of the same name is never imported: it appears only as the value
//! `send_with` returns.

use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error as StdError;
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use aws_config::retry::RetryConfig;
use aws_config::timeout::TimeoutConfig;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_bedrockruntime::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_bedrockruntime::operation::converse_stream::builders::ConverseStreamInputBuilder;
use aws_sdk_bedrockruntime::operation::converse_stream::{
    ConverseStreamError, ConverseStreamInput,
};
use aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError;
use aws_sdk_bedrockruntime::types::ConverseStreamOutput as StreamEvent;
use aws_sdk_bedrockruntime::types::{
    CachePointBlock, CachePointType, ContentBlock, ContentBlockDelta, ContentBlockStart,
    ConversationRole, DocumentBlock, DocumentFormat as BedrockDocumentFormat, DocumentSource,
    ImageBlock, ImageFormat, ImageSource, InferenceConfiguration, Message as BedrockMessage,
    ReasoningContentBlock, ReasoningContentBlockDelta, ReasoningTextBlock,
    StopReason as BedrockStopReason, SystemContentBlock, TokenUsage,
};
use aws_sdk_bedrockruntime::Client;
use aws_smithy_types::{Blob, Document};
use futures_util::future::BoxFuture;
use futures_util::{Stream, StreamExt};
use serde_json::json;

use crate::conversation::{self, base64_decode, base64_encode, Block, Message, Role};
use crate::provider::{
    Provider, ProviderError, ProviderRequest, StopReason, ThinkingMode, TurnEvent, TurnStream,
    Usage,
};

/// The provider tag on the canonical blocks this adapter produces.
pub const PROVIDER_NAME: &str = "bedrock";
/// Attempts the SDK makes before `send` fails: the first plus three retries.
pub const RETRY_MAX_ATTEMPTS: u32 = 4;
/// The first retry's back-off.
pub const RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// The back-off ceiling.
pub const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long a TCP connect may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one attempt may take up to the response head. The stream body
/// is the loop's, under its own idle timeout.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);
/// The key under which a redacted reasoning block's bytes are held in an
/// opaque block's `raw`, base64 encoded.
pub const REDACTED_KEY: &str = "redactedContent";

// ---------- the normaliser ----------

/// What an open content block index holds.
#[derive(Debug)]
enum Open {
    Thinking {
        id: String,
        signature: Option<String>,
        started: bool,
    },
    ToolUse {
        id: String,
    },
    Text,
}

/// One SDK event in, zero or more [`TurnEvent`]s out. One per reply.
#[derive(Debug)]
pub struct Normaliser {
    model: String,
    open: HashMap<i32, Open>,
    /// Kinds this normaliser did not recognise, for the transport to
    /// report; the normaliser itself writes nothing.
    unknown: Vec<&'static str>,
}

impl Normaliser {
    /// A normaliser for a reply from `model`.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            open: HashMap::new(),
            unknown: Vec::new(),
        }
    }

    /// Normalise one event.
    pub fn feed(&mut self, event: StreamEvent) -> Vec<TurnEvent> {
        match event {
            StreamEvent::MessageStart(_) => vec![TurnEvent::MessageStart {
                model: self.model.clone(),
            }],
            StreamEvent::ContentBlockStart(start) => {
                let index = start.content_block_index();
                match start.start() {
                    Some(ContentBlockStart::ToolUse(tool)) => {
                        let id = tool.tool_use_id().to_string();
                        self.open.insert(index, Open::ToolUse { id: id.clone() });
                        vec![TurnEvent::ToolUseStart {
                            id,
                            name: tool.name().to_string(),
                        }]
                    }
                    Some(ContentBlockStart::Image(_)) | Some(ContentBlockStart::ToolResult(_)) => {
                        self.open.insert(index, Open::Text);
                        Vec::new()
                    }
                    Some(_) => {
                        self.unknown.push("content block start");
                        Vec::new()
                    }
                    None => Vec::new(),
                }
            }
            StreamEvent::ContentBlockDelta(delta) => {
                let index = delta.content_block_index();
                match delta.delta() {
                    Some(ContentBlockDelta::Text(text)) => {
                        self.open.entry(index).or_insert(Open::Text);
                        vec![TurnEvent::TextDelta(text.clone())]
                    }
                    Some(ContentBlockDelta::ReasoningContent(reasoning)) => {
                        self.reasoning(index, reasoning)
                    }
                    Some(ContentBlockDelta::ToolUse(input)) => match self.open.get(&index) {
                        Some(Open::ToolUse { id }) => vec![TurnEvent::ToolInputDelta {
                            id: id.clone(),
                            json_fragment: input.input().to_string(),
                        }],
                        _ => Vec::new(),
                    },
                    Some(ContentBlockDelta::Image(_))
                    | Some(ContentBlockDelta::ToolResult(_))
                    | Some(ContentBlockDelta::Citation(_)) => Vec::new(),
                    Some(_) => {
                        self.unknown.push("content block delta");
                        Vec::new()
                    }
                    None => Vec::new(),
                }
            }
            StreamEvent::ContentBlockStop(stop) => {
                match self.open.remove(&stop.content_block_index()) {
                    Some(Open::Thinking { id, signature, .. }) => {
                        vec![TurnEvent::ThinkingEnd { id, signature }]
                    }
                    Some(Open::ToolUse { id }) => vec![TurnEvent::ToolUseEnd { id }],
                    Some(Open::Text) | None => Vec::new(),
                }
            }
            StreamEvent::MessageStop(stop) => {
                vec![TurnEvent::Stop(map_stop_reason(stop.stop_reason()))]
            }
            StreamEvent::Metadata(metadata) => match metadata.usage() {
                Some(usage) => vec![TurnEvent::Usage(usage_from(usage))],
                None => Vec::new(),
            },
            _ => {
                self.unknown.push("stream event");
                Vec::new()
            }
        }
    }

    /// The kinds seen since the last call that this normaliser does not
    /// know, drained.
    pub fn take_unknown_kinds(&mut self) -> Vec<&'static str> {
        std::mem::take(&mut self.unknown)
    }

    fn reasoning(&mut self, index: i32, delta: &ReasoningContentBlockDelta) -> Vec<TurnEvent> {
        // A redacted block and an unknown delta open nothing: a redacted
        // block is complete in itself, and an index that never opened
        // emits nothing at its stop.
        match delta {
            ReasoningContentBlockDelta::RedactedContent(blob) => {
                return vec![TurnEvent::OpaqueBlock {
                    raw: json!({ REDACTED_KEY: base64_encode(blob.as_ref()) }),
                }];
            }
            ReasoningContentBlockDelta::Text(_) | ReasoningContentBlockDelta::Signature(_) => {}
            _ => {
                self.unknown.push("reasoning delta");
                return Vec::new();
            }
        }
        let entry = self.open.entry(index).or_insert_with(|| Open::Thinking {
            id: index.to_string(),
            signature: None,
            started: false,
        });
        let Open::Thinking {
            id,
            signature,
            started,
        } = entry
        else {
            return Vec::new();
        };
        match delta {
            ReasoningContentBlockDelta::Text(text) => {
                let mut out = Vec::with_capacity(2);
                if !*started {
                    *started = true;
                    out.push(TurnEvent::ThinkingStart { id: id.clone() });
                }
                out.push(TurnEvent::ThinkingDelta {
                    id: id.clone(),
                    text: text.clone(),
                });
                out
            }
            ReasoningContentBlockDelta::Signature(value) => {
                *signature = Some(value.clone());
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

/// The SDK's stop reason in the internal vocabulary. The SDK enum is
/// `#[non_exhaustive]`; a value it does not know arrives as `Unknown` and
/// is carried by name.
pub fn map_stop_reason(reason: &BedrockStopReason) -> StopReason {
    match reason {
        BedrockStopReason::EndTurn => StopReason::EndTurn,
        BedrockStopReason::ToolUse => StopReason::ToolUse,
        BedrockStopReason::MaxTokens => StopReason::MaxTokens,
        BedrockStopReason::StopSequence => StopReason::StopSequence,
        BedrockStopReason::ModelContextWindowExceeded => StopReason::ContextWindowExceeded,
        BedrockStopReason::ContentFiltered | BedrockStopReason::GuardrailIntervened => {
            StopReason::ContentFiltered
        }
        other => StopReason::Other(other.as_str().to_string()),
    }
}

/// The SDK's token counts as [`Usage`]. Absent cache counts read as zero;
/// a negative count, which the API does not send, reads as zero too.
pub fn usage_from(usage: &TokenUsage) -> Usage {
    let clamp = |n: i32| n.max(0) as u32;
    Usage {
        input: clamp(usage.input_tokens()),
        output: clamp(usage.output_tokens()),
        cache_read: clamp(usage.cache_read_input_tokens().unwrap_or(0)),
        cache_write: clamp(usage.cache_write_input_tokens().unwrap_or(0)),
    }
}

/// Write one line per unknown kind per process. The normaliser records;
/// the transport, which owns the I/O, reports.
fn report_unknown_kinds(kinds: Vec<&'static str>) {
    static REPORTED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    if kinds.is_empty() {
        return;
    }
    let reported = REPORTED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut reported = reported
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for kind in kinds {
        if reported.insert(kind) {
            crate::hub::warn(&format!(
                "Bedrock sent a {kind} this build does not know; it was ignored. A newer Mezame may understand it."
            ));
        }
    }
}

// ---------- error classification ----------

/// What a service error is, read off its variant. The response metadata's
/// `code()` is not used: the event-stream path reads the payload alone, and
/// an error built with the SDK's builders carries no metadata at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    AccessDenied,
    Validation,
    ResourceNotFound,
    Throttling,
    /// A shape the SDK's retry policy already retried.
    Transient,
    Other,
}

/// Implemented for the two error enums `ConverseStream` produces.
///
/// The message is read off the variant's own field, not off the metadata
/// `ProvideErrorMetadata` exposes: the metadata is filled from the HTTP
/// response, so on the event-stream path and on an error built with the
/// SDK's builders it is empty while the field is set.
pub trait ServiceErrorKind {
    /// Which [`ServiceKind`] this error is.
    fn service_kind(&self) -> ServiceKind;
    /// The service's own message, when the variant carries one.
    fn service_message(&self) -> Option<&str>;
}

impl ServiceErrorKind for ConverseStreamError {
    fn service_message(&self) -> Option<&str> {
        match self {
            Self::AccessDeniedException(e) => e.message(),
            Self::ValidationException(e) => e.message(),
            Self::ResourceNotFoundException(e) => e.message(),
            Self::ThrottlingException(e) => e.message(),
            Self::ServiceUnavailableException(e) => e.message(),
            Self::InternalServerException(e) => e.message(),
            Self::ModelTimeoutException(e) => e.message(),
            Self::ModelStreamErrorException(e) => e.message(),
            Self::ModelNotReadyException(e) => e.message(),
            Self::ModelErrorException(e) => e.message(),
            _ => None,
        }
    }

    fn service_kind(&self) -> ServiceKind {
        match self {
            Self::AccessDeniedException(_) => ServiceKind::AccessDenied,
            Self::ValidationException(_) => ServiceKind::Validation,
            Self::ResourceNotFoundException(_) => ServiceKind::ResourceNotFound,
            Self::ThrottlingException(_) => ServiceKind::Throttling,
            Self::ServiceUnavailableException(_)
            | Self::InternalServerException(_)
            | Self::ModelTimeoutException(_)
            | Self::ModelStreamErrorException(_)
            | Self::ModelNotReadyException(_) => ServiceKind::Transient,
            _ => ServiceKind::Other,
        }
    }
}

impl ServiceErrorKind for ConverseStreamOutputError {
    fn service_message(&self) -> Option<&str> {
        match self {
            Self::ValidationException(e) => e.message(),
            Self::ThrottlingException(e) => e.message(),
            Self::ServiceUnavailableException(e) => e.message(),
            Self::InternalServerException(e) => e.message(),
            Self::ModelStreamErrorException(e) => e.message(),
            _ => None,
        }
    }

    fn service_kind(&self) -> ServiceKind {
        match self {
            Self::ValidationException(_) => ServiceKind::Validation,
            Self::ThrottlingException(_) => ServiceKind::Throttling,
            Self::ServiceUnavailableException(_)
            | Self::InternalServerException(_)
            | Self::ModelStreamErrorException(_) => ServiceKind::Transient,
            _ => ServiceKind::Other,
        }
    }
}

/// What a browser is told about a failure, and what the loop does with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    /// The SDK's policy would retry this shape; information for the text
    /// and the log line, never a signal to retry again.
    pub retryable: bool,
    /// The service refused the request for its content.
    pub rejected: bool,
    /// The text shown.
    pub text: String,
}

/// Classify a service error by its variant and message.
pub fn classify_service<E>(err: &E, model: &str) -> Classified
where
    E: ServiceErrorKind + ProvideErrorMetadata + StdError,
{
    let message = err
        .service_message()
        .or_else(|| err.message())
        .map(str::to_string);
    let fallback = || {
        message
            .clone()
            .or_else(|| err.code().map(str::to_string))
            .unwrap_or_else(|| chain_text(err))
    };
    let names_throughput = message
        .as_deref()
        .is_some_and(|m| m.to_ascii_lowercase().contains("on-demand throughput"));
    match err.service_kind() {
        ServiceKind::AccessDenied => Classified {
            retryable: false,
            rejected: false,
            text: format!(
                "Bedrock refused the request: check that model access is enabled for `{model}` in \
                 this region and that the credentials may call \
                 `bedrock:InvokeModelWithResponseStream`."
            ),
        },
        ServiceKind::Validation | ServiceKind::ResourceNotFound if names_throughput => Classified {
            retryable: false,
            rejected: false,
            text: format!(
                "`{model}` needs an inference profile id here: try `global.{model}` or a geo \
                 prefix such as `us.`."
            ),
        },
        ServiceKind::Validation => Classified {
            retryable: false,
            rejected: true,
            text: format!(
                "Bedrock rejected the request: {}.",
                fallback().trim_end_matches('.')
            ),
        },
        ServiceKind::Throttling => Classified {
            retryable: true,
            rejected: false,
            text: format!("Bedrock is throttling requests for `{model}`; try again shortly."),
        },
        ServiceKind::Transient => Classified {
            retryable: true,
            rejected: false,
            text: fallback(),
        },
        ServiceKind::ResourceNotFound | ServiceKind::Other => Classified {
            retryable: false,
            rejected: false,
            text: fallback(),
        },
    }
}

/// Classify any failure of a `ConverseStream` call, before or during the
/// stream. One function for both, because both carry the same shapes.
///
/// `format!("{err}")` on an `SdkError` yields a kind label such as
/// `dispatch failure`; the detail lives in the `source()` chain, which is
/// what the text rows read. A missing region and missing credentials both
/// arrive as a dispatch failure of the `other` kind: the SDK resolves both
/// inside the first attempt, after the request has entered its transmit
/// phase.
pub fn classify_error<E, R>(err: &SdkError<E, R>, model: &str) -> Classified
where
    E: ServiceErrorKind + ProvideErrorMetadata + StdError + 'static,
    R: Debug,
{
    match err {
        SdkError::ServiceError(service) => classify_service(service.err(), model),
        SdkError::TimeoutError(_) => Classified {
            retryable: true,
            rejected: false,
            text: format!("Could not reach Bedrock: {}.", chain_text(err)),
        },
        SdkError::DispatchFailure(dispatch) => {
            let connector = dispatch.as_connector_error();
            let transient = connector.is_some_and(|c| c.is_timeout() || c.is_io());
            if transient {
                return Classified {
                    retryable: true,
                    rejected: false,
                    text: format!("Could not reach Bedrock: {}.", chain_text(err)),
                };
            }
            let chain = chain_text(err);
            let lower = chain.to_ascii_lowercase();
            let text = if lower.contains("credential") {
                "No AWS credentials were found: run `aws configure` or `aws sso login`, or set \
                 `AWS_PROFILE` or the `AWS_ACCESS_KEY_ID` variables."
                    .to_string()
            } else if lower.contains("missing region") {
                "No AWS region is set: add `region` to the `bedrock` section of config.json or \
                 set `AWS_REGION`."
                    .to_string()
            } else {
                chain
            };
            Classified {
                retryable: false,
                rejected: false,
                text,
            }
        }
        _ => Classified {
            retryable: false,
            rejected: false,
            text: chain_text(err),
        },
    }
}

/// The `Display` of every error below `err` in its `source()` chain,
/// joined with `: `; `err`'s own text when it has no source.
pub fn chain_text(err: &dyn StdError) -> String {
    let mut parts = Vec::new();
    let mut current = err.source();
    while let Some(source) = current {
        parts.push(source.to_string());
        current = source.source();
    }
    if parts.is_empty() {
        err.to_string()
    } else {
        parts.join(": ")
    }
}

// ---------- the stream adapter ----------

/// Normalise a stream of SDK results into a [`TurnStream`].
///
/// Pure over `events`: a test feeds it an iterator of builder-built events
/// with a trailing `Err`. `Ok` items go through one [`Normaliser`]; the
/// first `Err` becomes one trailing [`TurnEvent::Error`] and ends the
/// stream. Unknown kinds the normaliser records are reported once per
/// process.
pub fn normalised_stream<S, R>(model: String, events: S) -> TurnStream
where
    S: Stream<Item = Result<StreamEvent, SdkError<ConverseStreamOutputError, R>>> + Send + 'static,
    R: Debug + Send + 'static,
{
    struct State<S> {
        events: Pin<Box<S>>,
        normaliser: Normaliser,
        pending: VecDeque<TurnEvent>,
        done: bool,
        model: String,
    }
    let state = State {
        events: Box::pin(events),
        normaliser: Normaliser::new(model.clone()),
        pending: VecDeque::new(),
        done: false,
        model,
    };
    Box::pin(futures_util::stream::unfold(
        state,
        |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((event, state));
                }
                if state.done {
                    return None;
                }
                match state.events.next().await {
                    Some(Ok(event)) => {
                        let out = state.normaliser.feed(event);
                        report_unknown_kinds(state.normaliser.take_unknown_kinds());
                        state.pending.extend(out);
                    }
                    Some(Err(err)) => {
                        let classified = classify_error(&err, &state.model);
                        crate::hub::warn(&format!(
                            "Bedrock stream for {} failed: {}",
                            state.model,
                            DisplayErrorContext(&err)
                        ));
                        state.pending.push_back(TurnEvent::Error {
                            retryable: classified.retryable,
                            message: classified.text,
                        });
                        state.done = true;
                    }
                    None => state.done = true,
                }
            }
        },
    ))
}

// ---------- the thinking rule ----------

/// The pieces of a Claude model id this module reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeId {
    /// `opus`, `sonnet`, `haiku`, `fable`, `mythos`, when the id names one.
    pub family: Option<String>,
    pub major: u32,
    pub minor: Option<u32>,
}

/// Read the version out of a Bedrock model id, or `None` for an id that
/// is not a Claude model.
///
/// Everything up to and including the last `anthropic.` is dropped, so a
/// base id, an inference profile id and an ARN that embeds one all parse
/// alike. A part is a version number when it parses as an integer under
/// 1000, which keeps a date such as `20250929` out of the version.
pub fn parse_claude_id(model_id: &str) -> Option<ClaudeId> {
    let index = model_id.rfind("anthropic.")?;
    let tail = &model_id[index + "anthropic.".len()..];
    let rest = tail.strip_prefix("claude-")?;
    let parts: Vec<&str> = rest.split('-').collect();
    let as_version = |part: &str| part.parse::<u32>().ok().filter(|n| *n < 1000);
    let position = parts.iter().position(|part| as_version(part).is_some())?;
    let major = as_version(parts[position])?;
    let minor = parts.get(position + 1).and_then(|part| as_version(part));
    let family = parts
        .iter()
        .find(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|part| part.to_string());
    Some(ClaudeId {
        family,
        major,
        minor,
    })
}

/// The default [`ThinkingMode`] for a model id.
///
/// Claude 5 and later, and 4.6 and later, take `adaptive`; 4.5 and
/// earlier, and 3.7, take `enabled` with a budget; everything else,
/// non-Anthropic ids and unparseable ids included, takes `off`, which sends
/// no thinking field, a request every model accepts. The `thinking` key of
/// the configuration overrides the rule.
pub fn thinking_rule(model_id: &str) -> ThinkingMode {
    let Some(id) = parse_claude_id(model_id) else {
        return ThinkingMode::Off;
    };
    match (id.major, id.minor) {
        (5.., _) => ThinkingMode::Adaptive,
        (4, Some(minor)) if minor >= 6 => ThinkingMode::Adaptive,
        (4, _) => ThinkingMode::Enabled,
        (3, Some(7)) => ThinkingMode::Enabled,
        _ => ThinkingMode::Off,
    }
}

/// Whether the model always thinks and refuses `disabled`: the Fable and
/// Mythos families.
pub fn always_thinks(model_id: &str) -> bool {
    parse_claude_id(model_id)
        .and_then(|id| id.family)
        .is_some_and(|family| family == "fable" || family == "mythos")
}

/// The vendor segment of a Bedrock model id: `anthropic` in
/// `us.anthropic.claude-sonnet-5`, `amazon` in `amazon.nova-pro-v1:0`, read
/// off the last `/`-separated part so an ARN that embeds an id reads like
/// the id. `None` when the id has no such segment, as an application
/// inference profile ARN or a custom model name has not.
pub fn vendor(model_id: &str) -> Option<&str> {
    let resource = model_id.rsplit('/').next().unwrap_or(model_id);
    let parts: Vec<&str> = resource.split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    Some(parts[parts.len() - 2])
}

/// Whether reasoning blocks this adapter recorded are replayed to
/// `model_id`: yes unless the id names a vendor other than Anthropic, whose
/// models refuse them. An id with no readable vendor, such as an
/// application inference profile ARN, is taken to front the Anthropic
/// model that produced the blocks, because only an Anthropic model does.
pub fn replays_reasoning(model_id: &str) -> bool {
    vendor(model_id).is_none_or(|v| v == "anthropic")
}

/// The `additionalModelRequestFields` document for a thinking mode, or
/// `None` when nothing is sent.
///
/// `adaptive` asks for the summary to be shown, which is what fills the
/// thought pane on models whose default display is `omitted`. `off` sends
/// `disabled` only to a model the rule marks `adaptive` and that is not of
/// a family that always thinks; everywhere else it sends nothing.
pub fn thinking_fields(mode: ThinkingMode, model_id: &str, budget: u32) -> Option<Document> {
    let object = |pairs: Vec<(&str, Document)>| {
        Document::Object(
            pairs
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect::<HashMap<_, _>>(),
        )
    };
    let thinking = match mode {
        ThinkingMode::Adaptive => object(vec![
            ("type", Document::from("adaptive")),
            ("display", Document::from("summarized")),
        ]),
        ThinkingMode::Enabled => object(vec![
            ("type", Document::from("enabled")),
            ("budget_tokens", Document::from(u64::from(budget))),
        ]),
        ThinkingMode::Off => {
            if thinking_rule(model_id) == ThinkingMode::Adaptive && !always_thinks(model_id) {
                object(vec![("type", Document::from("disabled"))])
            } else {
                return None;
            }
        }
    };
    Some(object(vec![("thinking", thinking)]))
}

// ---------- the request builder ----------

/// Merge consecutive same-role messages, so the request's roles alternate
/// starting with `user`. Borrows every block: nothing is copied until the
/// SDK's `Blob` needs owned bytes.
pub fn alternate(messages: &[Message]) -> Vec<(Role, Vec<&Block>)> {
    let mut out: Vec<(Role, Vec<&Block>)> = Vec::with_capacity(messages.len());
    for message in messages {
        match out.last_mut() {
            Some((role, blocks)) if *role == message.role => blocks.extend(message.blocks.iter()),
            _ => out.push((message.role, message.blocks.iter().collect())),
        }
    }
    out
}

/// A cache checkpoint of the default type.
pub fn cache_point() -> CachePointBlock {
    CachePointBlock::builder()
        .r#type(CachePointType::Default)
        .build()
        .expect("the type is set")
}

/// The Bedrock content for canonical blocks sent to `model_id`. A
/// thinking or opaque block is replayed when this adapter produced it and
/// the target reads it (see [`replays_reasoning`]); an empty text block
/// is left out.
pub fn to_bedrock_blocks(blocks: &[&Block], model_id: &str) -> Vec<ContentBlock> {
    let replay = replays_reasoning(model_id);
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            Block::Text { text } => {
                if !text.is_empty() {
                    out.push(ContentBlock::Text(text.clone()));
                }
            }
            Block::Image { media_type, data } => {
                let format = ImageFormat::from(media_type.trim_start_matches("image/"));
                out.push(ContentBlock::Image(
                    ImageBlock::builder()
                        .format(format)
                        .source(ImageSource::Bytes(Blob::new(data.clone())))
                        .build()
                        .expect("format and source are set"),
                ));
            }
            Block::Document { format, name, data } => out.push(ContentBlock::Document(
                DocumentBlock::builder()
                    .format(BedrockDocumentFormat::from(format.as_str()))
                    .name(name)
                    .source(DocumentSource::Bytes(Blob::new(data.clone())))
                    .build()
                    .expect("format, name and source are set"),
            )),
            Block::Thinking {
                text,
                signature,
                provider,
                ..
            } => {
                if replay && provider == PROVIDER_NAME {
                    out.push(ContentBlock::ReasoningContent(
                        ReasoningContentBlock::ReasoningText(
                            ReasoningTextBlock::builder()
                                .text(text)
                                .set_signature(signature.clone())
                                .build()
                                .expect("the text is set"),
                        ),
                    ));
                }
            }
            Block::Opaque { raw, provider, .. } => {
                if replay && provider == PROVIDER_NAME {
                    if let Some(bytes) = raw
                        .get(REDACTED_KEY)
                        .and_then(serde_json::Value::as_str)
                        .and_then(base64_decode)
                    {
                        out.push(ContentBlock::ReasoningContent(
                            ReasoningContentBlock::RedactedContent(Blob::new(bytes)),
                        ));
                    }
                }
            }
        }
    }
    out
}

/// The request's messages: roles alternated, blocks mapped, a message
/// whose every block was left out dropped and its neighbours merged, and
/// one cache point as the last content block of the last message.
///
/// The last message has role `user` whenever the loop calls this, because
/// the exchange is begun before any request; the assertion records the
/// invariant.
pub fn to_bedrock_messages(messages: &[Message], model_id: &str) -> Vec<BedrockMessage> {
    let mut mapped: Vec<(Role, Vec<ContentBlock>)> = Vec::with_capacity(messages.len());
    for (role, blocks) in alternate(messages) {
        let content = to_bedrock_blocks(&blocks, model_id);
        if content.is_empty() {
            continue;
        }
        match mapped.last_mut() {
            Some((last_role, last_content)) if *last_role == role => last_content.extend(content),
            _ => mapped.push((role, content)),
        }
    }
    if let Some((role, blocks)) = mapped.last_mut() {
        debug_assert_eq!(*role, Role::User, "a request ends with the user's message");
        blocks.push(ContentBlock::CachePoint(cache_point()));
    }
    mapped
        .into_iter()
        .map(|(role, blocks)| {
            BedrockMessage::builder()
                .role(match role {
                    Role::User => ConversationRole::User,
                    Role::Assistant => ConversationRole::Assistant,
                })
                .set_content(Some(blocks))
                .build()
                .expect("role and content are set")
        })
        .collect()
}

/// Build the request. Pure over `request`: no client, no I/O, and equal
/// inputs give equal builders, which a test compares after `build()`.
pub fn build_request(request: &ProviderRequest) -> ConverseStreamInputBuilder {
    let mut builder = ConverseStreamInput::builder()
        .model_id(&request.model)
        .system(SystemContentBlock::Text(request.system.static_text.clone()))
        .system(SystemContentBlock::CachePoint(cache_point()))
        .system(SystemContentBlock::Text(request.system.date_line.clone()));
    for message in to_bedrock_messages(&request.messages, &request.model) {
        builder = builder.messages(message);
    }
    let max_tokens = request.max_output_tokens.min(i32::MAX as u32) as i32;
    builder = builder.inference_config(
        InferenceConfiguration::builder()
            .max_tokens(max_tokens)
            .build(),
    );
    if let Some(fields) = thinking_fields(request.thinking, &request.model, request.thinking_budget)
    {
        builder = builder.additional_model_request_fields(fields);
    }
    builder
}

// ---------- the transport ----------

/// One client for the process, from the AWS default configuration chain.
///
/// Loading reads files and the environment; credentials resolve on the
/// first request. The one exception is the region: with none in the
/// arguments, the environment or the profile, the chain's last resort is
/// the instance metadata service, which costs about a second on a machine
/// outside EC2 before it gives up.
pub async fn build_client(region: Option<&str>, profile: Option<&str>) -> Client {
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .retry_config(
            RetryConfig::standard()
                .with_max_attempts(RETRY_MAX_ATTEMPTS)
                .with_initial_backoff(RETRY_INITIAL_BACKOFF)
                .with_max_backoff(RETRY_MAX_BACKOFF),
        )
        .timeout_config(
            TimeoutConfig::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .operation_attempt_timeout(ATTEMPT_TIMEOUT)
                .build(),
        );
    if let Some(profile) = profile {
        loader = loader.profile_name(profile);
    }
    if let Some(region) = region {
        loader = loader.region(Region::new(region.to_string()));
    }
    let config = loader.load().await;
    Client::new(&config)
}

/// The [`Provider`] over Bedrock.
#[derive(Debug, Clone)]
pub struct BedrockProvider {
    client: Client,
}

impl BedrockProvider {
    /// A provider over `client`, shared by every hub.
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

impl Provider for BedrockProvider {
    fn stream(&self, request: ProviderRequest) -> BoxFuture<'_, Result<TurnStream, ProviderError>> {
        Box::pin(async move {
            // The loop refused an over-limit message before recording
            // anything; this only records the invariant.
            debug_assert!(conversation::check_limits(request.messages.iter()).is_ok());
            let output = build_request(&request)
                .send_with(&self.client)
                .await
                .map_err(|err| {
                    let classified = classify_error(&err, &request.model);
                    crate::hub::warn(&format!(
                        "Bedrock request for {} failed: {}",
                        request.model,
                        DisplayErrorContext(&err)
                    ));
                    ProviderError::BeforeStream {
                        retryable: classified.retryable,
                        rejected: classified.rejected,
                        message: classified.text,
                    }
                })?;
            let events = futures_util::stream::unfold(output.stream, |mut receiver| async move {
                receiver
                    .recv()
                    .await
                    .transpose()
                    .map(|item| (item, receiver))
            });
            Ok(normalised_stream(request.model, events))
        })
    }
}
