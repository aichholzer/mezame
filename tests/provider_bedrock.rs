//! The Bedrock provider without Bedrock: the normaliser over events built
//! with the SDK's builders, the request builder against a hand-built
//! input, the thinking rule, the block mapping and its limits, and the
//! error classifier over errors built the same way. Nothing here reaches a
//! network.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;

use aws_sdk_bedrockruntime::error::{ConnectorError, SdkError};
use aws_sdk_bedrockruntime::operation::converse_stream::{
    ConverseStreamError, ConverseStreamInput,
};
use aws_sdk_bedrockruntime::types::error::{
    AccessDeniedException, ConverseStreamOutputError, InternalServerException,
    ModelNotReadyException, ResourceNotFoundException, ServiceUnavailableException,
    ThrottlingException, ValidationException,
};
use aws_sdk_bedrockruntime::types::{
    CachePointBlock, CachePointType, CitationsDelta, ContentBlock, ContentBlockDelta,
    ContentBlockDeltaEvent, ContentBlockStart, ContentBlockStartEvent, ContentBlockStopEvent,
    ConversationRole, ConverseStreamMetadataEvent, ConverseStreamOutput as StreamEvent,
    DocumentBlock, DocumentFormat as BedrockDocumentFormat, DocumentSource, ImageBlock,
    ImageFormat, ImageSource, InferenceConfiguration, Message as BedrockMessage, MessageStartEvent,
    MessageStopEvent, ReasoningContentBlock, ReasoningContentBlockDelta, ReasoningTextBlock,
    StopReason as BedrockStop, SystemContentBlock, TokenUsage, ToolUseBlockDelta,
    ToolUseBlockStart,
};
use aws_smithy_types::{Blob, Document};
use futures_util::StreamExt;
use mezame::conversation::{
    base64_decode, base64_encode, check_limits, document_name, user_message_from_blocks, Block,
    BlockError, DocumentFormat, Message, Role, DOCUMENT_NAME_MAX_CHARS, MAX_DOCUMENT_BYTES,
    MAX_IMAGE_BYTES,
};
use mezame::prompt::SystemPrompt;
use mezame::provider::bedrock::{
    alternate, always_thinks, build_request, chain_text, classify_error, classify_service,
    map_stop_reason, normalised_stream, parse_claude_id, replays_reasoning, thinking_fields,
    thinking_rule, to_bedrock_messages, vendor, Classified, Normaliser, ServiceErrorKind,
    REDACTED_KEY,
};
use mezame::provider::{
    ProviderError, ProviderRequest, StopReason, ThinkingMode, TurnEvent, Usage,
};
use serde_json::{json, Value};

const MODEL: &str = "anthropic.claude-sonnet-5";

// ---------- builders ----------

fn start() -> StreamEvent {
    StreamEvent::MessageStart(
        MessageStartEvent::builder()
            .role(ConversationRole::Assistant)
            .build()
            .unwrap(),
    )
}

fn delta(index: i32, delta: ContentBlockDelta) -> StreamEvent {
    StreamEvent::ContentBlockDelta(
        ContentBlockDeltaEvent::builder()
            .content_block_index(index)
            .delta(delta)
            .build()
            .unwrap(),
    )
}

fn text(index: i32, text: &str) -> StreamEvent {
    delta(index, ContentBlockDelta::Text(text.to_string()))
}

fn reasoning(index: i32, reasoning: ReasoningContentBlockDelta) -> StreamEvent {
    delta(index, ContentBlockDelta::ReasoningContent(reasoning))
}

fn tool_start(index: i32, id: &str, name: &str) -> StreamEvent {
    StreamEvent::ContentBlockStart(
        ContentBlockStartEvent::builder()
            .content_block_index(index)
            .start(ContentBlockStart::ToolUse(
                ToolUseBlockStart::builder()
                    .tool_use_id(id)
                    .name(name)
                    .build()
                    .unwrap(),
            ))
            .build()
            .unwrap(),
    )
}

fn tool_delta(index: i32, fragment: &str) -> StreamEvent {
    delta(
        index,
        ContentBlockDelta::ToolUse(
            ToolUseBlockDelta::builder()
                .input(fragment)
                .build()
                .unwrap(),
        ),
    )
}

fn block_stop(index: i32) -> StreamEvent {
    StreamEvent::ContentBlockStop(
        ContentBlockStopEvent::builder()
            .content_block_index(index)
            .build()
            .unwrap(),
    )
}

fn message_stop(reason: BedrockStop) -> StreamEvent {
    StreamEvent::MessageStop(
        MessageStopEvent::builder()
            .stop_reason(reason)
            .build()
            .unwrap(),
    )
}

fn metadata(usage: Option<TokenUsage>) -> StreamEvent {
    StreamEvent::Metadata(
        ConverseStreamMetadataEvent::builder()
            .set_usage(usage)
            .build(),
    )
}

fn tokens(
    input: i32,
    output: i32,
    cache_read: Option<i32>,
    cache_write: Option<i32>,
) -> TokenUsage {
    TokenUsage::builder()
        .input_tokens(input)
        .output_tokens(output)
        .total_tokens(input + output)
        .set_cache_read_input_tokens(cache_read)
        .set_cache_write_input_tokens(cache_write)
        .build()
        .unwrap()
}

fn feed_all(events: Vec<StreamEvent>) -> Vec<TurnEvent> {
    let mut normaliser = Normaliser::new(MODEL);
    events
        .into_iter()
        .flat_map(|event| normaliser.feed(event))
        .collect()
}

fn cache_point() -> ContentBlock {
    ContentBlock::CachePoint(
        CachePointBlock::builder()
            .r#type(CachePointType::Default)
            .build()
            .unwrap(),
    )
}

fn user(blocks: Vec<Block>) -> Message {
    Message {
        role: Role::User,
        blocks,
    }
}

fn assistant(blocks: Vec<Block>) -> Message {
    Message {
        role: Role::Assistant,
        blocks,
    }
}

fn text_block(text: &str) -> Block {
    Block::Text {
        text: text.to_string(),
    }
}

fn thinking_block(text: &str, signature: Option<&str>) -> Block {
    Block::Thinking {
        text: text.to_string(),
        signature: signature.map(str::to_string),
        provider: "bedrock".to_string(),
        model: MODEL.to_string(),
    }
}

// ---------- the normaliser, one case per emitted variant ----------

#[test]
fn message_start_carries_the_requested_model() {
    assert_eq!(
        feed_all(vec![start()]),
        vec![TurnEvent::MessageStart {
            model: MODEL.to_string()
        }]
    );
}

#[test]
fn text_deltas_become_text_deltas_in_order() {
    assert_eq!(
        feed_all(vec![text(0, "Hel"), text(0, "lo"), block_stop(0)]),
        vec![
            TurnEvent::TextDelta("Hel".into()),
            TurnEvent::TextDelta("lo".into())
        ]
    );
}

#[test]
fn a_thinking_block_starts_on_its_first_text_delta_and_ends_with_its_signature() {
    let events = feed_all(vec![
        start(),
        reasoning(0, ReasoningContentBlockDelta::Text("Let".into())),
        reasoning(0, ReasoningContentBlockDelta::Text(" me".into())),
        reasoning(0, ReasoningContentBlockDelta::Signature("sig-0".into())),
        block_stop(0),
        text(1, "Hi"),
        block_stop(1),
        message_stop(BedrockStop::EndTurn),
        metadata(Some(tokens(17, 700, Some(1370), Some(0)))),
    ]);
    assert_eq!(
        events,
        vec![
            TurnEvent::MessageStart {
                model: MODEL.into()
            },
            TurnEvent::ThinkingStart { id: "0".into() },
            TurnEvent::ThinkingDelta {
                id: "0".into(),
                text: "Let".into()
            },
            TurnEvent::ThinkingDelta {
                id: "0".into(),
                text: " me".into()
            },
            TurnEvent::ThinkingEnd {
                id: "0".into(),
                signature: Some("sig-0".into())
            },
            TurnEvent::TextDelta("Hi".into()),
            TurnEvent::Stop(StopReason::EndTurn),
            TurnEvent::Usage(Usage {
                input: 17,
                output: 700,
                cache_read: 1370,
                cache_write: 0
            }),
        ]
    );
}

#[test]
fn a_signature_only_block_ends_without_a_start_or_a_delta() {
    let events = feed_all(vec![
        reasoning(0, ReasoningContentBlockDelta::Signature("sig".into())),
        block_stop(0),
    ]);
    assert_eq!(
        events,
        vec![TurnEvent::ThinkingEnd {
            id: "0".into(),
            signature: Some("sig".into())
        }]
    );
}

#[test]
fn a_thinking_block_that_stops_without_a_signature_ends_with_none() {
    let events = feed_all(vec![
        reasoning(2, ReasoningContentBlockDelta::Text("x".into())),
        block_stop(2),
    ]);
    assert_eq!(
        events,
        vec![
            TurnEvent::ThinkingStart { id: "2".into() },
            TurnEvent::ThinkingDelta {
                id: "2".into(),
                text: "x".into()
            },
            TurnEvent::ThinkingEnd {
                id: "2".into(),
                signature: None
            },
        ]
    );
}

#[test]
fn redacted_content_becomes_an_opaque_block_holding_base64_and_opens_nothing() {
    let events = feed_all(vec![
        reasoning(
            0,
            ReasoningContentBlockDelta::RedactedContent(Blob::new(vec![0xde, 0xad, 0xbe, 0xef])),
        ),
        block_stop(0),
    ]);
    assert_eq!(
        events,
        vec![TurnEvent::OpaqueBlock {
            raw: json!({ REDACTED_KEY: "3q2+7w==" })
        }]
    );
}

#[test]
fn two_tool_use_blocks_are_reassembled_by_index() {
    let events = feed_all(vec![
        tool_start(0, "tool-a", "read"),
        tool_start(1, "tool-b", "write"),
        tool_delta(0, "{\"path\":"),
        tool_delta(1, "{\"to\":"),
        tool_delta(0, "\"a\"}"),
        tool_delta(1, "\"b\"}"),
        block_stop(1),
        block_stop(0),
    ]);
    assert_eq!(
        events,
        vec![
            TurnEvent::ToolUseStart {
                id: "tool-a".into(),
                name: "read".into()
            },
            TurnEvent::ToolUseStart {
                id: "tool-b".into(),
                name: "write".into()
            },
            TurnEvent::ToolInputDelta {
                id: "tool-a".into(),
                json_fragment: "{\"path\":".into()
            },
            TurnEvent::ToolInputDelta {
                id: "tool-b".into(),
                json_fragment: "{\"to\":".into()
            },
            TurnEvent::ToolInputDelta {
                id: "tool-a".into(),
                json_fragment: "\"a\"}".into()
            },
            TurnEvent::ToolInputDelta {
                id: "tool-b".into(),
                json_fragment: "\"b\"}".into()
            },
            TurnEvent::ToolUseEnd {
                id: "tool-b".into()
            },
            TurnEvent::ToolUseEnd {
                id: "tool-a".into()
            },
        ]
    );
}

#[test]
fn a_tool_input_delta_for_an_index_that_never_opened_is_dropped() {
    assert!(feed_all(vec![tool_delta(4, "{}"), block_stop(4)]).is_empty());
}

#[test]
fn metadata_becomes_usage_with_absent_cache_counts_as_zero() {
    assert_eq!(
        feed_all(vec![metadata(Some(tokens(10, 20, None, None)))]),
        vec![TurnEvent::Usage(Usage {
            input: 10,
            output: 20,
            cache_read: 0,
            cache_write: 0
        })]
    );
    assert_eq!(
        feed_all(vec![metadata(Some(tokens(10, 20, Some(5), Some(6))))]),
        vec![TurnEvent::Usage(Usage {
            input: 10,
            output: 20,
            cache_read: 5,
            cache_write: 6
        })]
    );
}

#[test]
fn metadata_without_usage_and_a_citation_delta_emit_nothing() {
    assert!(feed_all(vec![metadata(None)]).is_empty());
    assert!(feed_all(vec![delta(
        0,
        ContentBlockDelta::Citation(CitationsDelta::builder().build())
    )])
    .is_empty());
}

#[test]
fn every_stop_reason_maps_and_an_unknown_one_is_carried_by_name() {
    let cases = [
        (BedrockStop::EndTurn, StopReason::EndTurn),
        (BedrockStop::ToolUse, StopReason::ToolUse),
        (BedrockStop::MaxTokens, StopReason::MaxTokens),
        (BedrockStop::StopSequence, StopReason::StopSequence),
        (
            BedrockStop::ModelContextWindowExceeded,
            StopReason::ContextWindowExceeded,
        ),
        (BedrockStop::ContentFiltered, StopReason::ContentFiltered),
        (
            BedrockStop::GuardrailIntervened,
            StopReason::ContentFiltered,
        ),
        (
            BedrockStop::MalformedToolUse,
            StopReason::Other("malformed_tool_use".into()),
        ),
        (
            BedrockStop::MalformedModelOutput,
            StopReason::Other("malformed_model_output".into()),
        ),
        (
            BedrockStop::from("brand_new"),
            StopReason::Other("brand_new".into()),
        ),
    ];
    for (bedrock, expected) in cases {
        assert_eq!(map_stop_reason(&bedrock), expected);
        assert_eq!(
            feed_all(vec![message_stop(bedrock.clone())]),
            vec![TurnEvent::Stop(expected)]
        );
    }
}

#[test]
fn a_text_only_turn_end_to_end() {
    let events = feed_all(vec![
        start(),
        text(0, "Hello"),
        text(0, ", world"),
        block_stop(0),
        message_stop(BedrockStop::EndTurn),
        metadata(Some(tokens(3, 4, Some(0), Some(0)))),
    ]);
    assert_eq!(events.len(), 5);
    assert_eq!(events[1], TurnEvent::TextDelta("Hello".into()));
    assert_eq!(events[2], TurnEvent::TextDelta(", world".into()));
    assert_eq!(events[3], TurnEvent::Stop(StopReason::EndTurn));
}

#[tokio::test]
async fn a_normalised_stream_ends_with_one_error_after_a_stream_failure() {
    let failure: SdkError<ConverseStreamOutputError, ()> = SdkError::service_error(
        ConverseStreamOutputError::ThrottlingException(
            ThrottlingException::builder().message("slow down").build(),
        ),
        (),
    );
    let events = normalised_stream(
        MODEL.to_string(),
        futures_util::stream::iter(vec![Ok(text(0, "a")), Err(failure), Ok(text(0, "never"))]),
    )
    .collect::<Vec<_>>()
    .await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], TurnEvent::TextDelta("a".into()));
    match &events[1] {
        TurnEvent::Error { retryable, message } => {
            assert!(retryable);
            assert!(message.contains("throttling"), "{message}");
        }
        other => panic!("expected an Error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_normalised_stream_without_a_failure_ends_after_its_last_event() {
    let events = normalised_stream(
        MODEL.to_string(),
        futures_util::stream::iter(vec![
            Ok::<_, SdkError<ConverseStreamOutputError, ()>>(text(0, "a")),
            Ok(message_stop(BedrockStop::EndTurn)),
        ]),
    )
    .collect::<Vec<_>>()
    .await;
    assert_eq!(
        events,
        vec![
            TurnEvent::TextDelta("a".into()),
            TurnEvent::Stop(StopReason::EndTurn)
        ]
    );
}

// ---------- the error classifier ----------

#[derive(Debug)]
struct TestError(&'static str);

impl fmt::Display for TestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl StdError for TestError {}

fn boxed(text: &'static str) -> Box<dyn StdError + Send + Sync> {
    Box::new(TestError(text))
}

#[test]
fn service_rows_are_keyed_on_the_variant_not_on_metadata() {
    let denied = classify_service(
        &ConverseStreamError::AccessDeniedException(AccessDeniedException::builder().build()),
        MODEL,
    );
    assert!(!denied.retryable && !denied.rejected);
    assert!(denied.text.contains("model access") && denied.text.contains(MODEL));

    let throughput = classify_service(
        &ConverseStreamError::ValidationException(
            ValidationException::builder()
                .message("Invocation of model ID x with on-demand throughput isn't supported.")
                .build(),
        ),
        MODEL,
    );
    assert!(!throughput.rejected);
    assert!(throughput.text.contains(&format!("global.{MODEL}")));

    let refused = classify_service(
        &ConverseStreamError::ValidationException(
            ValidationException::builder()
                .message("The text field in the ContentBlock object is blank.")
                .build(),
        ),
        MODEL,
    );
    assert_eq!(
        refused,
        Classified {
            retryable: false,
            rejected: true,
            stale_reasoning: false,
            text:
                "Bedrock rejected the request: The text field in the ContentBlock object is blank."
                    .into()
        }
    );

    // The service refusing the request as too long is a rejection: the
    // message that overflowed leaves later requests (the only recovery
    // when the new message is the cause; when the history is, the next
    // turn fails either way), and the browser gets the context-window
    // advice.
    let too_long = classify_service(
        &ConverseStreamError::ValidationException(
            ValidationException::builder()
                .message("Input is too long for requested model.")
                .build(),
        ),
        MODEL,
    );
    assert_eq!(
        too_long,
        Classified {
            retryable: false,
            rejected: true,
            stale_reasoning: false,
            text: mezame::provider::CONTEXT_WINDOW_ERROR.into()
        }
    );

    let throttled = classify_service(
        &ConverseStreamError::ThrottlingException(ThrottlingException::builder().build()),
        MODEL,
    );
    assert!(throttled.retryable && !throttled.rejected);
    assert!(throttled.text.contains("throttling"));

    for transient in [
        ConverseStreamError::ServiceUnavailableException(
            ServiceUnavailableException::builder()
                .message("busy")
                .build(),
        ),
        ConverseStreamError::InternalServerException(
            InternalServerException::builder().message("oops").build(),
        ),
        ConverseStreamError::ModelNotReadyException(
            ModelNotReadyException::builder().message("warming").build(),
        ),
    ] {
        let classified = classify_service(&transient, MODEL);
        assert!(
            classified.retryable && !classified.rejected,
            "{classified:?}"
        );
        assert_eq!(classified.text, transient.service_message().unwrap());
    }

    let missing = classify_service(
        &ConverseStreamError::ResourceNotFoundException(
            ResourceNotFoundException::builder()
                .message("no such model")
                .build(),
        ),
        MODEL,
    );
    assert_eq!(
        missing,
        Classified {
            retryable: false,
            rejected: false,
            stale_reasoning: false,
            text: "no such model".into()
        }
    );
}

#[test]
fn the_stream_receiver_s_own_errors_classify_the_same_way() {
    let rejected = classify_service(
        &ConverseStreamOutputError::ValidationException(
            ValidationException::builder().message("bad").build(),
        ),
        MODEL,
    );
    assert!(rejected.rejected);
    let transient = classify_service(
        &ConverseStreamOutputError::InternalServerException(
            InternalServerException::builder().message("oops").build(),
        ),
        MODEL,
    );
    assert!(transient.retryable);
}

#[test]
fn transport_rows_read_the_source_chain_not_the_kind_label() {
    let credentials: SdkError<ConverseStreamError, ()> = SdkError::dispatch_failure(
        ConnectorError::other(boxed("no providers in chain provided credentials"), None),
    );
    let classified = classify_error(&credentials, MODEL);
    assert!(!classified.retryable && !classified.rejected);
    assert!(
        classified.text.contains("aws configure"),
        "{}",
        classified.text
    );

    let region: SdkError<ConverseStreamError, ()> = SdkError::dispatch_failure(
        ConnectorError::other(boxed("Invalid Configuration: Missing Region"), None),
    );
    let classified = classify_error(&region, MODEL);
    assert!(
        classified.text.contains("AWS_REGION"),
        "{}",
        classified.text
    );

    let io: SdkError<ConverseStreamError, ()> =
        SdkError::dispatch_failure(ConnectorError::io(boxed("connection reset")));
    let classified = classify_error(&io, MODEL);
    assert!(classified.retryable);
    assert!(classified.text.starts_with("Could not reach Bedrock:"));
    assert!(
        classified.text.contains("connection reset"),
        "{}",
        classified.text
    );

    let timeout: SdkError<ConverseStreamError, ()> =
        SdkError::dispatch_failure(ConnectorError::timeout(boxed("connect timed out")));
    assert!(classify_error(&timeout, MODEL).retryable);

    let op_timeout: SdkError<ConverseStreamError, ()> = SdkError::timeout_error(boxed("timed out"));
    let classified = classify_error(&op_timeout, MODEL);
    assert!(classified.retryable);
    assert!(classified.text.contains("timed out"));

    let construction: SdkError<ConverseStreamError, ()> =
        SdkError::construction_failure(boxed("could not build"));
    let classified = classify_error(&construction, MODEL);
    assert!(!classified.retryable);
    assert!(
        classified.text.contains("could not build"),
        "{}",
        classified.text
    );
    assert!(!classified.text.contains("failed to construct request"));

    let service: SdkError<ConverseStreamError, ()> = SdkError::service_error(
        ConverseStreamError::ThrottlingException(ThrottlingException::builder().build()),
        (),
    );
    assert!(classify_error(&service, MODEL).retryable);
}

#[test]
fn chain_text_joins_the_sources_and_falls_back_to_the_error_itself() {
    let leaf = TestError("leaf");
    assert_eq!(chain_text(&leaf), "leaf");
    let wrapped: SdkError<ConverseStreamError, ()> =
        SdkError::dispatch_failure(ConnectorError::io(boxed("reset by peer")));
    let text = chain_text(&wrapped);
    assert!(text.ends_with("reset by peer"), "{text}");
    assert!(!text.starts_with("dispatch failure"), "{text}");
}

// ---------- the thinking rule ----------

#[test]
fn the_thinking_rule_over_the_documented_ids() {
    use ThinkingMode::{Adaptive, Enabled, Off};
    let cases = [
        ("anthropic.claude-sonnet-5", Adaptive),
        ("global.anthropic.claude-opus-5", Adaptive),
        ("anthropic.claude-fable-5-1", Adaptive),
        ("anthropic.claude-opus-4-8", Adaptive),
        ("us.anthropic.claude-sonnet-4-6", Adaptive),
        ("anthropic.claude-opus-4-6-v1", Adaptive),
        ("global.anthropic.claude-sonnet-4-5-20250929-v1:0", Enabled),
        ("anthropic.claude-haiku-4-5-20251001-v1:0", Enabled),
        ("anthropic.claude-opus-4-5-20251101-v1:0", Enabled),
        ("anthropic.claude-opus-4-1-20250805-v1:0", Enabled),
        ("anthropic.claude-opus-4-20250514-v1:0", Enabled),
        ("anthropic.claude-3-7-sonnet-20250219-v1:0", Enabled),
        ("anthropic.claude-3-5-haiku-20241022-v1:0", Off),
        ("amazon.nova-pro-v1:0", Off),
        (
            "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-sonnet-5",
            Adaptive,
        ),
        ("", Off),
    ];
    for (id, expected) in cases {
        assert_eq!(thinking_rule(id), expected, "{id}");
    }
}

#[test]
fn parse_claude_id_reads_family_and_version_and_skips_dates() {
    let id = parse_claude_id("global.anthropic.claude-sonnet-4-5-20250929-v1:0").unwrap();
    assert_eq!(id.family.as_deref(), Some("sonnet"));
    assert_eq!((id.major, id.minor), (4, Some(5)));
    let id = parse_claude_id("anthropic.claude-3-7-sonnet-20250219-v1:0").unwrap();
    assert_eq!(id.family.as_deref(), Some("sonnet"));
    assert_eq!((id.major, id.minor), (3, Some(7)));
    assert!(parse_claude_id("anthropic.titan-text").is_none());
    assert!(parse_claude_id("anthropic.claude-next").is_none());
    assert!(always_thinks("anthropic.claude-fable-5-1"));
    assert!(always_thinks("us.anthropic.claude-mythos-5"));
    assert!(!always_thinks("anthropic.claude-opus-5"));
}

fn object(pairs: &[(&str, Document)]) -> Document {
    Document::Object(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<HashMap<_, _>>(),
    )
}

#[test]
fn thinking_fields_for_the_five_rows_of_the_table() {
    let adaptive = object(&[(
        "thinking",
        object(&[
            ("type", Document::from("adaptive")),
            ("display", Document::from("summarized")),
        ]),
    )]);
    assert_eq!(
        thinking_fields(ThinkingMode::Adaptive, "anthropic.claude-sonnet-5", 4096),
        Some(adaptive)
    );
    let enabled = object(&[(
        "thinking",
        object(&[
            ("type", Document::from("enabled")),
            ("budget_tokens", Document::from(4096u64)),
        ]),
    )]);
    assert_eq!(
        thinking_fields(
            ThinkingMode::Enabled,
            "anthropic.claude-haiku-4-5-20251001-v1:0",
            4096
        ),
        Some(enabled)
    );
    let disabled = object(&[("thinking", object(&[("type", Document::from("disabled"))]))]);
    assert_eq!(
        thinking_fields(ThinkingMode::Off, "anthropic.claude-opus-5", 4096),
        Some(disabled)
    );
    assert_eq!(
        thinking_fields(ThinkingMode::Off, "anthropic.claude-fable-5-1", 4096),
        None
    );
    assert_eq!(
        thinking_fields(ThinkingMode::Off, "global.anthropic.claude-mythos-5", 4096),
        None
    );
    assert_eq!(
        thinking_fields(
            ThinkingMode::Off,
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            4096
        ),
        None
    );
    assert_eq!(
        thinking_fields(ThinkingMode::Off, "amazon.nova-pro-v1:0", 4096),
        None
    );
}

// ---------- the request builder ----------

fn system() -> SystemPrompt {
    SystemPrompt {
        static_text: "STATIC".to_string(),
        date_line: "Today's date is 2026-09-07.".to_string(),
    }
}

fn request(messages: Vec<Message>) -> ProviderRequest {
    ProviderRequest {
        model: MODEL.to_string(),
        system: system(),
        messages,
        thinking: ThinkingMode::Adaptive,
        thinking_budget: 4096,
        max_output_tokens: 16384,
    }
}

fn bedrock_message(role: ConversationRole, blocks: Vec<ContentBlock>) -> BedrockMessage {
    BedrockMessage::builder()
        .role(role)
        .set_content(Some(blocks))
        .build()
        .unwrap()
}

#[test]
fn build_request_is_deterministic_and_equals_a_hand_built_input() {
    let messages = vec![
        user(vec![text_block("hello")]),
        assistant(vec![thinking_block("hmm", Some("sig")), text_block("hi")]),
        user(vec![
            text_block("Attached file report.pdf (application/pdf)"),
            Block::Document {
                format: DocumentFormat::Pdf,
                name: "report".to_string(),
                data: b"%PDF".to_vec(),
            },
            Block::Image {
                media_type: "image/png".to_string(),
                data: vec![1, 2, 3],
            },
            text_block("look"),
        ]),
    ];
    let req = request(messages);
    let built = build_request(&req).build().unwrap();
    assert_eq!(built, build_request(&req).build().unwrap());

    let expected = ConverseStreamInput::builder()
        .model_id(MODEL)
        .system(SystemContentBlock::Text("STATIC".into()))
        .system(SystemContentBlock::CachePoint(
            CachePointBlock::builder()
                .r#type(CachePointType::Default)
                .build()
                .unwrap(),
        ))
        .system(SystemContentBlock::Text(
            "Today's date is 2026-09-07.".into(),
        ))
        .messages(bedrock_message(
            ConversationRole::User,
            vec![ContentBlock::Text("hello".into())],
        ))
        .messages(bedrock_message(
            ConversationRole::Assistant,
            vec![
                ContentBlock::ReasoningContent(ReasoningContentBlock::ReasoningText(
                    ReasoningTextBlock::builder()
                        .text("hmm")
                        .signature("sig")
                        .build()
                        .unwrap(),
                )),
                ContentBlock::Text("hi".into()),
            ],
        ))
        .messages(bedrock_message(
            ConversationRole::User,
            vec![
                ContentBlock::Text("Attached file report.pdf (application/pdf)".into()),
                ContentBlock::Document(
                    DocumentBlock::builder()
                        .format(BedrockDocumentFormat::Pdf)
                        .name("report")
                        .source(DocumentSource::Bytes(Blob::new(b"%PDF".to_vec())))
                        .build()
                        .unwrap(),
                ),
                ContentBlock::Image(
                    ImageBlock::builder()
                        .format(ImageFormat::Png)
                        .source(ImageSource::Bytes(Blob::new(vec![1, 2, 3])))
                        .build()
                        .unwrap(),
                ),
                ContentBlock::Text("look".into()),
                cache_point(),
            ],
        ))
        .inference_config(InferenceConfiguration::builder().max_tokens(16384).build())
        .additional_model_request_fields(object(&[(
            "thinking",
            object(&[
                ("type", Document::from("adaptive")),
                ("display", Document::from("summarized")),
            ]),
        )]))
        .build()
        .unwrap();
    assert_eq!(built, expected);
}

#[test]
fn the_system_is_static_then_cache_point_then_date() {
    let built = build_request(&request(vec![user(vec![text_block("x")])]))
        .build()
        .unwrap();
    let system = built.system();
    assert_eq!(system.len(), 3);
    assert!(matches!(&system[0], SystemContentBlock::Text(t) if t == "STATIC"));
    assert!(matches!(&system[1], SystemContentBlock::CachePoint(_)));
    assert!(
        matches!(&system[2], SystemContentBlock::Text(t) if t == "Today's date is 2026-09-07.")
    );
}

#[test]
fn an_off_mode_on_a_model_that_takes_no_field_sends_none() {
    let mut req = request(vec![user(vec![text_block("x")])]);
    req.model = "anthropic.claude-sonnet-4-5-20250929-v1:0".to_string();
    req.thinking = ThinkingMode::Off;
    let built = build_request(&req).build().unwrap();
    assert!(built.additional_model_request_fields().is_none());
    assert_eq!(built.inference_config().unwrap().max_tokens(), Some(16384));
}

#[test]
fn alternate_merges_adjacent_same_role_messages_in_order_without_copying() {
    let messages = [
        user(vec![text_block("a")]),
        user(vec![text_block("b")]),
        assistant(vec![text_block("c")]),
        assistant(vec![text_block("d")]),
        user(vec![text_block("e")]),
    ];
    let merged = alternate(&messages);
    let shape: Vec<(Role, Vec<&Block>)> = merged
        .iter()
        .map(|(role, blocks)| (*role, blocks.clone()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (
                Role::User,
                vec![&messages[0].blocks[0], &messages[1].blocks[0]]
            ),
            (
                Role::Assistant,
                vec![&messages[2].blocks[0], &messages[3].blocks[0]]
            ),
            (Role::User, vec![&messages[4].blocks[0]]),
        ]
    );
    // Borrowed, not copied: the block references point into the input.
    assert!(std::ptr::eq(merged[0].1[1], &messages[1].blocks[0]));
}

#[test]
fn the_cache_point_is_the_last_block_of_the_last_message_and_nowhere_else() {
    let messages = to_bedrock_messages(
        &[
            user(vec![text_block("a")]),
            assistant(vec![text_block("b")]),
            user(vec![text_block("c")]),
            user(vec![text_block("d")]),
        ],
        MODEL,
    );
    assert_eq!(messages.len(), 3);
    let points: Vec<(usize, usize)> = messages
        .iter()
        .enumerate()
        .flat_map(|(m, message)| {
            message
                .content()
                .iter()
                .enumerate()
                .filter(|(_, block)| matches!(block, ContentBlock::CachePoint(_)))
                .map(move |(b, _)| (m, b))
        })
        .collect();
    assert_eq!(points, vec![(2, 2)]);
    assert_eq!(messages[2].role(), &ConversationRole::User);
}

#[test]
fn reasoning_blocks_are_replayed_to_an_anthropic_model_only() {
    let messages = [
        user(vec![text_block("a")]),
        assistant(vec![
            thinking_block("t", Some("s")),
            Block::Opaque {
                provider: "bedrock".into(),
                model: MODEL.into(),
                raw: json!({ REDACTED_KEY: base64_encode(b"xyz") }),
            },
            text_block(""),
            text_block("b"),
        ]),
        user(vec![text_block("c")]),
    ];
    for anthropic_id in [
        "us.anthropic.claude-opus-5",
        "arn:aws:bedrock:us-east-1:123456789012:inference-profile/global.anthropic.claude-sonnet-5",
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abcd1234",
        "my-custom-model",
    ] {
        let built = to_bedrock_messages(&messages, anthropic_id);
        assert_eq!(built[1].content().len(), 3, "{anthropic_id}");
        assert!(matches!(
            &built[1].content()[1],
            ContentBlock::ReasoningContent(ReasoningContentBlock::RedactedContent(blob)) if blob.as_ref() == b"xyz"
        ));
    }
    for other_vendor in ["amazon.nova-pro-v1:0", "us.meta.llama3-70b-instruct-v1:0"] {
        let built = to_bedrock_messages(&messages, other_vendor);
        assert_eq!(built[1].content().len(), 1, "{other_vendor}");
        assert!(matches!(&built[1].content()[0], ContentBlock::Text(t) if t == "b"));
    }
    assert_eq!(vendor("us.anthropic.claude-opus-5"), Some("anthropic"));
    assert_eq!(vendor("amazon.nova-pro-v1:0"), Some("amazon"));
    assert_eq!(
        vendor("arn:aws:bedrock:us-east-1:1:application-inference-profile/abcd"),
        None
    );
    assert!(replays_reasoning(""));

    // A block another provider recorded is never replayed here, whatever
    // the target.
    let foreign = [
        user(vec![text_block("a")]),
        assistant(vec![
            Block::Thinking {
                text: "t".into(),
                signature: Some("s".into()),
                provider: "anthropic".into(),
                model: "claude-opus-5".into(),
            },
            text_block("b"),
        ]),
        user(vec![text_block("c")]),
    ];
    assert_eq!(to_bedrock_messages(&foreign, MODEL)[1].content().len(), 1);
}

#[test]
fn a_message_left_with_no_block_is_dropped_and_its_neighbours_merge() {
    let messages = [
        user(vec![text_block("a")]),
        assistant(vec![thinking_block("only thinking", Some("s"))]),
        user(vec![text_block("b")]),
    ];
    let built = to_bedrock_messages(&messages, "amazon.nova-pro-v1:0");
    assert_eq!(
        built.len(),
        1,
        "the empty assistant message is gone and the two user messages merged"
    );
    assert_eq!(built[0].role(), &ConversationRole::User);
    let texts: Vec<&str> = built[0]
        .content()
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["a", "b"]);
    assert!(matches!(
        built[0].content().last(),
        Some(ContentBlock::CachePoint(_))
    ));
}

// ---------- the block mapping ----------

#[test]
fn user_message_from_blocks_maps_each_accepted_block() {
    let blocks = vec![
        json!({ "type": "text", "text": "look at this" }),
        json!({ "type": "image", "mimeType": "image/png", "data": "AQID" }),
        json!({ "type": "resource", "resource": { "uri": "file:///notes.txt", "mimeType": "text/plain", "text": "line one" } }),
        json!({ "type": "resource", "resource": { "uri": "file:///readme", "text": "plain" } }),
        json!({ "type": "resource", "resource": { "uri": "file:///pic.webp", "mimeType": "image/webp", "blob": "AQID" } }),
        json!({ "type": "resource", "resource": { "uri": "file:///docs/report (final).pdf", "mimeType": "application/pdf", "blob": "JVBERg==" } }),
    ];
    let message = user_message_from_blocks(&blocks).unwrap();
    assert_eq!(message.role, Role::User);
    assert_eq!(
        message.blocks,
        vec![
            text_block("look at this"),
            Block::Image {
                media_type: "image/png".into(),
                data: vec![1, 2, 3]
            },
            text_block("Attached file file:///notes.txt (text/plain):\nline one"),
            text_block("Attached file file:///readme:\nplain"),
            Block::Image {
                media_type: "image/webp".into(),
                data: vec![1, 2, 3]
            },
            text_block("Attached file file:///docs/report (final).pdf (application/pdf)"),
            Block::Document {
                format: DocumentFormat::Pdf,
                name: "report (final)".into(),
                data: b"%PDF".to_vec()
            },
        ]
    );
    assert_eq!(message.text(), "look at this\nAttached file file:///notes.txt (text/plain):\nline one\nAttached file file:///readme:\nplain\nAttached file file:///docs/report (final).pdf (application/pdf)");
}

#[test]
fn blank_text_maps_to_nothing_and_a_prompt_left_with_nothing_is_refused() {
    let message = user_message_from_blocks(&[
        json!({ "type": "text", "text": "   " }),
        json!({ "type": "text", "text": "kept" }),
        json!({ "type": "text", "text": "" }),
    ])
    .unwrap();
    assert_eq!(message.blocks, vec![text_block("kept")]);
    assert_eq!(
        user_message_from_blocks(&[json!({ "type": "text", "text": " \n" })]),
        Err(BlockError::Nothing)
    );
    assert_eq!(user_message_from_blocks(&[]), Err(BlockError::Nothing));
    assert!(BlockError::Nothing.to_string().contains("nothing to send"));
}

#[test]
fn unsupported_blocks_are_refused_with_position_kind_and_the_accepted_types() {
    let unknown = user_message_from_blocks(&[
        json!({ "type": "text", "text": "ok" }),
        json!({ "type": "audio", "data": "AQID" }),
    ])
    .unwrap_err();
    let text = unknown.to_string();
    assert!(text.starts_with("Block 1 (`audio`)"), "{text}");
    assert!(text.contains("`text`, `image` and `resource`"), "{text}");

    let mime = user_message_from_blocks(&[
        json!({ "type": "image", "mimeType": "image/bmp", "data": "AQID" }),
    ])
    .unwrap_err()
    .to_string();
    assert!(mime.starts_with("Block 0 (`image`, `image/bmp`)"), "{mime}");
    assert!(mime.contains("image/webp"), "{mime}");

    let garbage = user_message_from_blocks(&[
        json!({ "type": "image", "mimeType": "image/png", "data": "not base64!" }),
    ])
    .unwrap_err()
    .to_string();
    assert!(garbage.contains("does not decode as base64"), "{garbage}");

    let zip = user_message_from_blocks(&[json!({ "type": "resource", "resource": { "uri": "file:///a.zip", "mimeType": "application/zip", "blob": "AQID" } })])
        .unwrap_err()
        .to_string();
    assert!(
        zip.starts_with("Block 0 (`resource`, `application/zip`)"),
        "{zip}"
    );
    assert!(zip.contains("application/pdf"), "{zip}");

    let neither = user_message_from_blocks(&[
        json!({ "type": "resource", "resource": { "uri": "file:///a" } }),
    ])
    .unwrap_err()
    .to_string();
    assert!(neither.contains("neither `text` nor `blob`"), "{neither}");
}

fn image(bytes: usize) -> Block {
    Block::Image {
        media_type: "image/png".into(),
        data: vec![0; bytes],
    }
}

fn document(bytes: usize) -> Block {
    Block::Document {
        format: DocumentFormat::Pdf,
        name: "doc".into(),
        data: vec![0; bytes],
    }
}

#[test]
fn check_limits_refuses_each_of_the_four_limits_and_accepts_the_boundary() {
    let ok = user(
        std::iter::repeat_with(|| image(1))
            .take(20)
            .chain(std::iter::repeat_with(|| document(1)).take(5))
            .chain(
                [image(MAX_IMAGE_BYTES), document(MAX_DOCUMENT_BYTES)]
                    .into_iter()
                    .take(0),
            )
            .collect(),
    );
    assert_eq!(check_limits([&ok]), Ok(()));
    assert_eq!(
        check_limits([&user(vec![
            image(MAX_IMAGE_BYTES),
            document(MAX_DOCUMENT_BYTES)
        ])]),
        Ok(())
    );

    let too_many_images = user(std::iter::repeat_with(|| image(1)).take(21).collect());
    match check_limits([&too_many_images]).unwrap_err() {
        BlockError::Limit { index, what, limit } => {
            assert_eq!((index, what), (20, "image"));
            assert!(limit.contains("20"), "{limit}");
        }
        other => panic!("{other:?}"),
    }
    match check_limits([&user(vec![text_block("x"), image(MAX_IMAGE_BYTES + 1)])]).unwrap_err() {
        BlockError::Limit { index, what, .. } => assert_eq!((index, what), (1, "image")),
        other => panic!("{other:?}"),
    }
    let too_many_documents = user(std::iter::repeat_with(|| document(1)).take(6).collect());
    match check_limits([&too_many_documents]).unwrap_err() {
        BlockError::Limit { index, what, .. } => assert_eq!((index, what), (5, "document")),
        other => panic!("{other:?}"),
    }
    match check_limits([&user(vec![document(MAX_DOCUMENT_BYTES + 1)])]).unwrap_err() {
        BlockError::Limit { index, what, limit } => {
            assert_eq!((index, what), (0, "document"));
            assert!(limit.contains("4500000"), "{limit}");
        }
        other => panic!("{other:?}"),
    }
    // A user message that got a reply is not measured again.
    let answered = [
        user(vec![image(MAX_IMAGE_BYTES + 1)]),
        assistant(vec![text_block("ok")]),
        user(vec![text_block("next")]),
    ];
    assert_eq!(check_limits(answered.iter()), Ok(()));
    // One that got none rides along with the new message, and the merged
    // message is what the provider measures: twelve and twelve is over.
    let unanswered = [
        user(std::iter::repeat_with(|| image(1)).take(12).collect()),
        user(std::iter::repeat_with(|| image(1)).take(12).collect()),
    ];
    match check_limits(unanswered.iter()).unwrap_err() {
        BlockError::Limit { index, what, .. } => assert_eq!((index, what), (20, "image")),
        other => panic!("{other:?}"),
    }
    assert_eq!(check_limits(std::iter::empty::<&Message>()), Ok(()));
}

#[test]
fn document_names_drop_the_extension_and_keep_to_the_allowed_characters() {
    assert_eq!(
        document_name("file:///docs/report (final).pdf"),
        "report (final)"
    );
    assert_eq!(document_name("notes.v2.md"), "notes-v2");
    assert_eq!(document_name("ç.pdf"), "-");
    assert_eq!(document_name("a   b\tc.txt"), "a b c");
    assert_eq!(document_name(""), "document");
    assert_eq!(document_name("/"), "document");
    assert_eq!(document_name(".hidden"), "-hidden");
    assert_eq!(document_name("https://example.com/x.pdf?y=1#z"), "x");
    assert_eq!(document_name("[draft] plan_v3.docx"), "[draft] plan-v3");
    let long = format!("{}.pdf", "a".repeat(300));
    assert_eq!(document_name(&long).len(), DOCUMENT_NAME_MAX_CHARS);
}

#[test]
fn base64_round_trips_and_rejects_garbage() {
    for bytes in [
        &b""[..],
        &b"f"[..],
        &b"fo"[..],
        &b"foo"[..],
        &b"foob"[..],
        &b"%PDF-1.7 \x00\xff"[..],
    ] {
        let encoded = base64_encode(bytes);
        assert_eq!(base64_decode(&encoded).as_deref(), Some(bytes), "{encoded}");
    }
    assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    assert_eq!(base64_decode("not base64!"), None);
    assert_eq!(base64_decode("Zg=x"), None, "data after padding");
    assert_eq!(base64_decode("Z"), None, "a lone sextet");
}

#[test]
fn blocks_serialise_to_the_fixed_shape_and_round_trip() {
    let cases: Vec<(Block, Value)> = vec![
        (text_block("hi"), json!({ "type": "text", "text": "hi" })),
        (
            Block::Image {
                media_type: "image/png".into(),
                data: vec![1, 2, 3],
            },
            json!({ "type": "image", "media_type": "image/png", "data": "AQID" }),
        ),
        (
            Block::Document {
                format: DocumentFormat::Pdf,
                name: "report".into(),
                data: b"%PDF".to_vec(),
            },
            json!({ "type": "document", "format": "pdf", "name": "report", "data": "JVBERg==" }),
        ),
        (
            thinking_block("t", Some("s")),
            json!({ "type": "thinking", "text": "t", "signature": "s", "provider": "bedrock", "model": MODEL }),
        ),
        (
            thinking_block("t", None),
            json!({ "type": "thinking", "text": "t", "provider": "bedrock", "model": MODEL }),
        ),
        (
            Block::Opaque {
                provider: "bedrock".into(),
                model: MODEL.into(),
                raw: json!({ REDACTED_KEY: "AQID" }),
            },
            json!({ "type": "opaque", "provider": "bedrock", "model": MODEL, "raw": { REDACTED_KEY: "AQID" } }),
        ),
    ];
    for (block, expected) in cases {
        let value = serde_json::to_value(&block).unwrap();
        assert_eq!(value, expected);
        let back: Block = serde_json::from_value(value).unwrap();
        assert_eq!(back, block);
    }
}

// ---------- the provider vocabulary ----------

#[test]
fn a_provider_error_displays_its_message_and_carries_the_rejection_flag() {
    let rejected = ProviderError::BeforeStream {
        retryable: false,
        rejected: true,
        stale_reasoning: false,
        message: "Bedrock rejected the request: blank text.".into(),
    };
    assert_eq!(
        rejected.message(),
        "Bedrock rejected the request: blank text."
    );
    assert_eq!(rejected.to_string(), rejected.message());
    assert!(rejected.is_rejected());
    let throttled = ProviderError::BeforeStream {
        retryable: true,
        rejected: false,
        stale_reasoning: false,
        message: "throttled".into(),
    };
    assert!(!throttled.is_rejected());
    assert!(!throttled.is_stale_reasoning());
    let stale = ProviderError::BeforeStream {
        retryable: false,
        rejected: false,
        stale_reasoning: true,
        message: "stale".into(),
    };
    assert!(stale.is_stale_reasoning() && !stale.is_rejected());
    assert!(std::error::Error::source(&throttled).is_none());
}

#[test]
fn thinking_modes_serialise_in_lowercase_and_refuse_anything_else() {
    for (mode, text) in [
        (ThinkingMode::Adaptive, "\"adaptive\""),
        (ThinkingMode::Enabled, "\"enabled\""),
        (ThinkingMode::Off, "\"off\""),
    ] {
        assert_eq!(serde_json::to_string(&mode).unwrap(), text);
        assert_eq!(serde_json::from_str::<ThinkingMode>(text).unwrap(), mode);
    }
    assert!(serde_json::from_str::<ThinkingMode>("\"Adaptive\"").is_err());
    assert!(serde_json::from_str::<ThinkingMode>("\"budget\"").is_err());
}

// ---------- review fixes, 2026-09-08 ----------

#[test]
fn a_refusal_stop_reason_is_mapped_by_name() {
    // The SDK enum has no `refusal` variant yet; the string is matched so
    // the loop's refusal rule reaches it the day Bedrock forwards it.
    assert_eq!(
        map_stop_reason(&BedrockStop::from("refusal")),
        StopReason::Refusal
    );
    assert_eq!(
        map_stop_reason(&BedrockStop::from("brand_new")),
        StopReason::Other("brand_new".into())
    );
}

#[test]
fn a_signature_failure_is_never_relayed_and_messages_keep_their_first_line() {
    use aws_sdk_bedrockruntime::error::ErrorMetadata;
    use mezame::provider::bedrock::{describe_error, CREDENTIALS_REFUSED_ERROR};
    let body = "The request signature we calculated does not match the signature you provided.\n\n\
                The Canonical String for this request should have been\n\
                'POST\n/model/x/converse-stream\n\nx-amz-security-token:SECRETTOKEN\n'";
    let err = ConverseStreamError::generic(
        ErrorMetadata::builder()
            .code("InvalidSignatureException")
            .message(body)
            .build(),
    );
    let classified = classify_service(&err, MODEL);
    assert_eq!(classified.text, CREDENTIALS_REFUSED_ERROR);
    assert!(!classified.retryable && !classified.rejected && !classified.stale_reasoning);
    // The stderr line carries the code and nothing of the body.
    let sdk: SdkError<ConverseStreamError, ()> = SdkError::service_error(err, ());
    let line = describe_error(&sdk);
    assert!(line.contains("InvalidSignatureException"), "{line}");
    assert!(!line.contains("SECRETTOKEN"), "{line}");

    // Any other multi-line message is cut at its first line, in the
    // browser text and in the log line alike.
    let validation = ConverseStreamError::ValidationException(
        ValidationException::builder()
            .message("The text field is blank.\n\nDiagnostics: x-amz-security-token:SECRETTOKEN")
            .build(),
    );
    let classified = classify_service(&validation, MODEL);
    assert_eq!(
        classified.text,
        "Bedrock rejected the request: The text field is blank."
    );
    let sdk: SdkError<ConverseStreamError, ()> = SdkError::service_error(validation, ());
    let line = describe_error(&sdk);
    assert_eq!(line, "ValidationException: The text field is blank.");
    for code in [
        "SignatureDoesNotMatch",
        "UnrecognizedClientException",
        "ExpiredTokenException",
        "InvalidClientTokenId",
    ] {
        let err = ConverseStreamError::generic(
            ErrorMetadata::builder()
                .code(code)
                .message("x-amz-security-token:SECRETTOKEN")
                .build(),
        );
        assert_eq!(
            classify_service(&err, MODEL).text,
            CREDENTIALS_REFUSED_ERROR,
            "{code}"
        );
    }
}

#[test]
fn stale_reasoning_is_classified_for_the_retry() {
    use mezame::provider::bedrock::STALE_REASONING_ERROR;
    let err = ConverseStreamError::ValidationException(
        ValidationException::builder()
            .message(
                "messages.5.content.0: Invalid `signature` in `thinking` block. The block is \
                 bound to a different conversation.",
            )
            .build(),
    );
    let classified = classify_service(&err, MODEL);
    assert!(classified.stale_reasoning);
    assert!(!classified.rejected && !classified.retryable);
    assert_eq!(classified.text, STALE_REASONING_ERROR);
    // Through the SDK error too.
    let sdk: SdkError<ConverseStreamError, ()> = SdkError::service_error(err, ());
    assert!(classify_error(&sdk, MODEL).stale_reasoning);
    // The reasoning wording of the Converse API's own message.
    let converse = ConverseStreamError::ValidationException(
        ValidationException::builder()
            .message("The reasoning content signature is invalid.")
            .build(),
    );
    assert!(classify_service(&converse, MODEL).stale_reasoning);
}

#[test]
fn reasoning_is_not_replayed_to_a_model_from_before_thinking() {
    let messages = [
        user(vec![text_block("a")]),
        assistant(vec![thinking_block("t", Some("s")), text_block("b")]),
        user(vec![text_block("c")]),
    ];
    for old in [
        "anthropic.claude-3-5-haiku-20241022-v1:0",
        "us.anthropic.claude-3-haiku-20240307-v1:0",
        "anthropic.claude-3-5-sonnet-20240620-v1:0",
    ] {
        assert!(!replays_reasoning(old), "{old}");
        let built = to_bedrock_messages(&messages, old);
        assert_eq!(built[1].content().len(), 1, "{old}");
        assert!(matches!(&built[1].content()[0], ContentBlock::Text(t) if t == "b"));
    }
    for thinking_model in [
        "anthropic.claude-3-7-sonnet-20250219-v1:0",
        "anthropic.claude-sonnet-4-5-20250929-v1:0",
        "global.anthropic.claude-sonnet-5",
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abcd1234",
    ] {
        assert!(replays_reasoning(thinking_model), "{thinking_model}");
    }
}

#[test]
fn the_output_ceiling_clamps_the_request_for_models_that_cap_below_the_default() {
    use mezame::provider::bedrock::max_output_ceiling;
    assert_eq!(
        max_output_ceiling("anthropic.claude-3-5-haiku-20241022-v1:0"),
        Some(8192)
    );
    assert_eq!(
        max_output_ceiling("anthropic.claude-3-5-sonnet-20241022-v2:0"),
        Some(8192)
    );
    assert_eq!(
        max_output_ceiling("anthropic.claude-3-5-sonnet-20240620-v1:0"),
        Some(4096)
    );
    assert_eq!(
        max_output_ceiling("anthropic.claude-3-haiku-20240307-v1:0"),
        Some(4096)
    );
    assert_eq!(
        max_output_ceiling("anthropic.claude-3-opus-20240229-v1:0"),
        Some(4096)
    );
    assert_eq!(
        max_output_ceiling("anthropic.claude-3-7-sonnet-20250219-v1:0"),
        None
    );
    assert_eq!(max_output_ceiling(MODEL), None);
    assert_eq!(max_output_ceiling("amazon.nova-pro-v1:0"), None);

    let mut haiku = request(vec![user(vec![text_block("a")])]);
    haiku.model = "anthropic.claude-3-5-haiku-20241022-v1:0".into();
    haiku.thinking = ThinkingMode::Off;
    let built = build_request(&haiku).build().unwrap();
    assert_eq!(built.inference_config().unwrap().max_tokens(), Some(8192));
    // A configured ceiling below the model's stands.
    haiku.max_output_tokens = 1000;
    let built = build_request(&haiku).build().unwrap();
    assert_eq!(built.inference_config().unwrap().max_tokens(), Some(1000));
    // A model with no known cap takes the configured value whole.
    let built = build_request(&request(vec![user(vec![text_block("a")])]))
        .build()
        .unwrap();
    assert_eq!(built.inference_config().unwrap().max_tokens(), Some(16384));
}
