//! Mezame's own content blocks, the messages built from them, and the two
//! coupled stores a session keeps in memory: the Conversation the next
//! request is built from and the Transcript `GET /history` serves.
//!
//! The block set is provider-neutral. A prompt's wire blocks are mapped
//! into it here, before any request, so an unsupported attachment is
//! refused with a readable error and nothing is recorded; a provider
//! adapter maps the canonical blocks to its own request shape and never
//! sees the wire. The serde shape is fixed now so a later store can hold
//! it without a translation layer.
//!
//! The two stores evict together. Each exchange knows how many transcript
//! entries it contributed and how many payload bytes it holds beyond the
//! entries' text, and one budget covers both, so the model is never sent
//! an exchange the browser can no longer show and the reverse.

use std::collections::VecDeque;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::{
    entry_text_len, HistoryEntry, Transcript, TRANSCRIPT_BUDGET_BYTES, TRANSCRIPT_MAX_ENTRIES,
};

/// The image media types a prompt may carry.
pub const IMAGE_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
/// The most images one user message may hold.
pub const MAX_IMAGES_PER_MESSAGE: usize = 20;
/// The most documents one user message may hold.
pub const MAX_DOCUMENTS_PER_MESSAGE: usize = 5;
/// The most bytes one image may hold, the decimal reading of the
/// provider's 3.75 MB.
pub const MAX_IMAGE_BYTES: usize = 3_750_000;
/// The most bytes one document may hold, the decimal reading of the
/// provider's 4.5 MB.
pub const MAX_DOCUMENT_BYTES: usize = 4_500_000;
/// The longest document name sent.
pub const DOCUMENT_NAME_MAX_CHARS: usize = 200;

/// One block of a message. Serialises tagged on `type`, with bytes as
/// base64, which is the shape a later store keeps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// Plain text.
    Text { text: String },
    /// An inline image.
    Image {
        media_type: String,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    /// An inline document.
    Document {
        format: DocumentFormat,
        name: String,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    /// A reasoning block the model produced, replayed unchanged to the
    /// provider and model that produced it.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        provider: String,
        model: String,
    },
    /// A provider-opaque block, held verbatim.
    Opaque {
        provider: String,
        model: String,
        raw: Value,
    },
}

impl Block {
    /// The bytes this block holds that no transcript entry carries: the
    /// budget counts them beside the entries' text. Zero for text and for
    /// a thinking block's text, which the `thought` entry already counts.
    pub fn payload_len(&self) -> usize {
        match self {
            Block::Text { .. } => 0,
            Block::Image { data, .. } | Block::Document { data, .. } => data.len(),
            Block::Thinking { signature, .. } => signature.as_ref().map_or(0, String::len),
            Block::Opaque { raw, .. } => serde_json::to_vec(raw).map_or(0, |v| v.len()),
        }
    }
}

/// The document formats a prompt may carry, by the provider's names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocumentFormat {
    Csv,
    Doc,
    Docx,
    Html,
    Md,
    Pdf,
    Txt,
    Xls,
    Xlsx,
}

impl DocumentFormat {
    /// The format for a media type, or `None` for one no document takes.
    pub fn from_media_type(media_type: &str) -> Option<Self> {
        Some(match media_type {
            "application/pdf" => DocumentFormat::Pdf,
            "text/csv" => DocumentFormat::Csv,
            "application/msword" => DocumentFormat::Doc,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
                DocumentFormat::Docx
            }
            "application/vnd.ms-excel" => DocumentFormat::Xls,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => {
                DocumentFormat::Xlsx
            }
            "text/html" => DocumentFormat::Html,
            "text/plain" => DocumentFormat::Txt,
            "text/markdown" => DocumentFormat::Md,
            _ => return None,
        })
    }

    /// The provider's name for the format.
    pub fn as_str(self) -> &'static str {
        match self {
            DocumentFormat::Csv => "csv",
            DocumentFormat::Doc => "doc",
            DocumentFormat::Docx => "docx",
            DocumentFormat::Html => "html",
            DocumentFormat::Md => "md",
            DocumentFormat::Pdf => "pdf",
            DocumentFormat::Txt => "txt",
            DocumentFormat::Xls => "xls",
            DocumentFormat::Xlsx => "xlsx",
        }
    }
}

/// Who said a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One message of the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<Block>,
}

impl Message {
    /// The message's text blocks joined by one newline: what the
    /// transcript's `user` entry holds for a user message.
    pub fn text(&self) -> String {
        self.blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Why a prompt could not become a message. Rendered for the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockError {
    /// A block the mapping does not accept.
    Unsupported {
        /// The block's position in the prompt.
        index: usize,
        /// The block's `type`.
        kind: String,
        /// Its media type, when it named one.
        media_type: Option<String>,
        /// What is wrong, as a clause that follows "Block N (...)".
        reason: String,
    },
    /// Every block mapped to nothing.
    Nothing,
    /// A message over one of the provider's count or size limits.
    Limit {
        /// The offending block's position in the message.
        index: usize,
        /// `image` or `document`.
        what: &'static str,
        /// The limit, as a clause.
        limit: String,
    },
}

impl fmt::Display for BlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockError::Unsupported {
                index,
                kind,
                media_type: Some(media_type),
                reason,
            } => write!(f, "Block {index} (`{kind}`, `{media_type}`) {reason}"),
            BlockError::Unsupported {
                index,
                kind,
                media_type: None,
                reason,
            } => write!(f, "Block {index} (`{kind}`) {reason}"),
            BlockError::Nothing => f.write_str(
                "The prompt held nothing to send: every block was empty, or an attachment of a \
                 kind that is not supported.",
            ),
            BlockError::Limit { index, what, limit } => {
                write!(f, "Block {index} ({what}) is over the limit: {limit}.")
            }
        }
    }
}

impl std::error::Error for BlockError {}

const ACCEPTED_TYPES: &str = "accepted types are `text`, `image` and `resource`";
const ACCEPTED_IMAGES: &str =
    "accepted image types are image/png, image/jpeg, image/gif and image/webp";
const ACCEPTED_DOCUMENTS: &str = "accepted document types are application/pdf, text/csv, \
     application/msword, the Word and Excel OpenXML types, application/vnd.ms-excel, \
     text/html, text/plain and text/markdown";

/// Map a prompt's wire blocks to one user message, before any request.
///
/// A `text` block maps to text, or to nothing when it is empty or
/// whitespace only. An `image` block maps to an image when its type is one
/// of [`IMAGE_MEDIA_TYPES`] and its data decodes as base64. A `resource`
/// with `text` maps to one text block naming the file on its first line;
/// a `resource` with `blob` maps to an image for an image type, and to a
/// text line naming the file followed by a document for a document type,
/// because the provider refuses a document with no text beside it.
/// Anything else is refused with the block's position, kind and the
/// accepted types, and a prompt left with no block is refused too.
pub fn user_message_from_blocks(blocks: &[Value]) -> Result<Message, BlockError> {
    let mut out = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                if !text.trim().is_empty() {
                    out.push(Block::Text {
                        text: text.to_string(),
                    });
                }
            }
            "image" => {
                let media_type = block.get("mimeType").and_then(Value::as_str).unwrap_or("");
                let data = block.get("data").and_then(Value::as_str).unwrap_or("");
                out.push(image_block(index, "image", media_type, data)?);
            }
            "resource" => {
                let resource = block.get("resource").and_then(Value::as_object);
                let uri = resource
                    .and_then(|r| r.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("attachment");
                let media_type = resource
                    .and_then(|r| r.get("mimeType"))
                    .and_then(Value::as_str);
                let text = resource.and_then(|r| r.get("text")).and_then(Value::as_str);
                let blob = resource.and_then(|r| r.get("blob")).and_then(Value::as_str);
                match (text, blob) {
                    (Some(text), _) => {
                        let header = match media_type {
                            Some(media_type) => format!("Attached file {uri} ({media_type}):"),
                            None => format!("Attached file {uri}:"),
                        };
                        out.push(Block::Text {
                            text: format!("{header}\n{text}"),
                        });
                    }
                    (None, Some(blob)) => {
                        let media_type = media_type.unwrap_or("");
                        if IMAGE_MEDIA_TYPES.contains(&media_type) {
                            out.push(image_block(index, "resource", media_type, blob)?);
                        } else if let Some(format) = DocumentFormat::from_media_type(media_type) {
                            let data = base64_decode(blob).ok_or_else(|| {
                                unsupported(
                                    index,
                                    "resource",
                                    media_type,
                                    "does not decode as base64",
                                )
                            })?;
                            if data.is_empty() {
                                return Err(unsupported(
                                    index,
                                    "resource",
                                    media_type,
                                    "holds no data",
                                ));
                            }
                            out.push(Block::Text {
                                text: format!("Attached file {uri} ({media_type})"),
                            });
                            out.push(Block::Document {
                                format,
                                name: document_name(uri),
                                data,
                            });
                        } else {
                            return Err(unsupported(
                                index,
                                "resource",
                                media_type,
                                &format!("is not a supported attachment: {ACCEPTED_IMAGES}; {ACCEPTED_DOCUMENTS}"),
                            ));
                        }
                    }
                    (None, None) => {
                        return Err(BlockError::Unsupported {
                            index,
                            kind: kind.to_string(),
                            media_type: media_type.map(str::to_string),
                            reason: "holds neither `text` nor `blob`".to_string(),
                        });
                    }
                }
            }
            _ => {
                return Err(BlockError::Unsupported {
                    index,
                    kind: kind.to_string(),
                    media_type: None,
                    reason: format!("is not supported: {ACCEPTED_TYPES}"),
                });
            }
        }
    }
    if out.is_empty() {
        return Err(BlockError::Nothing);
    }
    Ok(Message {
        role: Role::User,
        blocks: out,
    })
}

fn image_block(
    index: usize,
    kind: &str,
    media_type: &str,
    data: &str,
) -> Result<Block, BlockError> {
    if !IMAGE_MEDIA_TYPES.contains(&media_type) {
        return Err(unsupported(
            index,
            kind,
            media_type,
            &format!("is not a supported image: {ACCEPTED_IMAGES}"),
        ));
    }
    let data = base64_decode(data)
        .ok_or_else(|| unsupported(index, kind, media_type, "does not decode as base64"))?;
    if data.is_empty() {
        return Err(unsupported(index, kind, media_type, "holds no data"));
    }
    Ok(Block::Image {
        media_type: media_type.to_string(),
        data,
    })
}

fn unsupported(index: usize, kind: &str, media_type: &str, reason: &str) -> BlockError {
    BlockError::Unsupported {
        index,
        kind: kind.to_string(),
        media_type: (!media_type.is_empty()).then(|| media_type.to_string()),
        reason: reason.to_string(),
    }
}

/// Refuse a request whose last user message would be over the provider's
/// per-message limits: at most [`MAX_IMAGES_PER_MESSAGE`] images of
/// [`MAX_IMAGE_BYTES`] each and [`MAX_DOCUMENTS_PER_MESSAGE`] documents of
/// [`MAX_DOCUMENT_BYTES`] each.
///
/// The message measured is the one the provider sees: the trailing run of
/// consecutive user messages, which the request builder merges into one.
/// An earlier user message that got no reply, because its request failed
/// before any output, rides along with the new one, so measuring the new
/// one alone would pass a merged message the provider refuses. The index
/// reported is the block's position in that merged message.
pub fn check_limits<'a>(messages: impl IntoIterator<Item = &'a Message>) -> Result<(), BlockError> {
    let messages: Vec<&Message> = messages.into_iter().collect();
    let trailing_users = messages
        .iter()
        .rev()
        .take_while(|m| m.role == Role::User)
        .count();
    let merged = messages[messages.len() - trailing_users..]
        .iter()
        .flat_map(|m| m.blocks.iter());
    let (mut images, mut documents) = (0usize, 0usize);
    for (index, block) in merged.enumerate() {
        match block {
            Block::Image { data, .. } => {
                images += 1;
                if images > MAX_IMAGES_PER_MESSAGE {
                    return Err(BlockError::Limit {
                        index,
                        what: "image",
                        limit: format!("at most {MAX_IMAGES_PER_MESSAGE} images per message"),
                    });
                }
                if data.len() > MAX_IMAGE_BYTES {
                    return Err(BlockError::Limit {
                        index,
                        what: "image",
                        limit: format!(
                            "an image may hold at most {MAX_IMAGE_BYTES} bytes, this one holds {}",
                            data.len()
                        ),
                    });
                }
            }
            Block::Document { data, .. } => {
                documents += 1;
                if documents > MAX_DOCUMENTS_PER_MESSAGE {
                    return Err(BlockError::Limit {
                        index,
                        what: "document",
                        limit: format!("at most {MAX_DOCUMENTS_PER_MESSAGE} documents per message"),
                    });
                }
                if data.len() > MAX_DOCUMENT_BYTES {
                    return Err(BlockError::Limit {
                        index,
                        what: "document",
                        limit: format!(
                            "a document may hold at most {MAX_DOCUMENT_BYTES} bytes, this one holds {}",
                            data.len()
                        ),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The document name sent for a resource: the uri's last path segment
/// with its final extension removed, every character outside ASCII
/// letters, digits, space, hyphen, parentheses and square brackets
/// replaced by a hyphen, runs of whitespace collapsed to one space, cut to
/// [`DOCUMENT_NAME_MAX_CHARS`], and `document` when nothing remains.
pub fn document_name(uri: &str) -> String {
    let trimmed = uri.trim_end_matches('/');
    let segment = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let segment = segment.split(['?', '#']).next().unwrap_or(segment);
    let stem = match segment.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => segment,
    };
    let mut out = String::new();
    let mut pending_space = false;
    for c in stem.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '(' | ')' | '[' | ']') {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    let out: String = out.chars().take(DOCUMENT_NAME_MAX_CHARS).collect();
    let out = out.trim().to_string();
    if out.is_empty() {
        "document".to_string()
    } else {
        out
    }
}

// ---------- base64 ----------

/// Standard base64, padded, as `aws-smithy-types` implements it: one
/// codec in the binary, the one the SDK itself uses. `None` for anything
/// that is not canonical base64; the browser emits the canonical form.
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    aws_smithy_types::base64::decode(input).ok()
}

/// Standard base64 with padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    aws_smithy_types::base64::encode(bytes)
}

mod base64_bytes {
    use serde::{de, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::base64_encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        super::base64_decode(&text).ok_or_else(|| de::Error::custom("invalid base64"))
    }
}

// ---------- the coupled stores ----------

/// Where an exchange stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeStatus {
    /// Begun; the reply is not in.
    Open,
    /// The reply, or the lack of one, has been recorded.
    Closed,
    /// The service refused the request for its content: the user's text
    /// stays in the transcript and the message is never sent again.
    Rejected,
}

/// One exchange: the user's message and, once the reply is in, the
/// assistant's.
#[derive(Debug, Clone, PartialEq)]
pub struct Exchange {
    pub user: Message,
    pub assistant: Option<Message>,
    pub status: ExchangeStatus,
    /// Transcript entries this exchange contributed.
    entries: usize,
    /// Bytes this exchange holds that its entries do not count.
    payload: usize,
}

/// The conversation the next request is built from and the transcript
/// `GET /history` serves, evicted together under one budget.
#[derive(Debug)]
pub struct Conversation {
    exchanges: VecDeque<Exchange>,
    payload: usize,
    transcript: Transcript,
}

impl Default for Conversation {
    fn default() -> Self {
        Self::new()
    }
}

impl Conversation {
    /// An empty conversation under the shipped ceilings,
    /// [`TRANSCRIPT_BUDGET_BYTES`] and [`TRANSCRIPT_MAX_ENTRIES`].
    pub fn new() -> Self {
        Self::with_budget(TRANSCRIPT_BUDGET_BYTES, TRANSCRIPT_MAX_ENTRIES)
    }

    /// An empty conversation under `budget_bytes` of text plus payload
    /// and `max_entries` transcript entries.
    #[doc(hidden)]
    pub fn with_budget_for_test(budget_bytes: usize, max_entries: usize) -> Self {
        Self::with_budget(budget_bytes, max_entries)
    }

    fn with_budget(budget_bytes: usize, max_entries: usize) -> Self {
        Self {
            exchanges: VecDeque::new(),
            payload: 0,
            transcript: Transcript::new(budget_bytes, max_entries),
        }
    }

    /// Open an exchange with the user's message and its transcript entry,
    /// then hold the budget.
    ///
    /// The entry holds the user's typed text, the same derivation the
    /// hub's echo uses; text an attachment contributed sits in the message
    /// and not in the entry, so its bytes are counted here as payload.
    pub fn begin(&mut self, user: Message, entry: HistoryEntry) {
        let text_in_blocks: usize = user
            .blocks
            .iter()
            .map(|block| match block {
                Block::Text { text } => text.len(),
                _ => 0,
            })
            .sum();
        let entry_text = entry_text_len(&entry);
        let payload: usize = user.blocks.iter().map(Block::payload_len).sum::<usize>()
            + text_in_blocks.saturating_sub(entry_text);
        self.payload += payload;
        self.transcript.record([entry]);
        self.exchanges.push_back(Exchange {
            user,
            assistant: None,
            status: ExchangeStatus::Open,
            entries: 1,
            payload,
        });
        self.enforce_budget();
    }

    /// Close the open exchange with the assistant's message, if any, and
    /// its transcript entries, then hold the budget. A `None` closes the
    /// exchange too: a reply that never came is recorded as such, and a
    /// later `complete` or `reject_open` finds nothing open. Returns
    /// `false` and changes nothing when no exchange is open: the deque is
    /// empty because `clear` ran, or the back exchange is closed already.
    pub fn complete(&mut self, assistant: Option<Message>, entries: Vec<HistoryEntry>) -> bool {
        let Some(back) = self.exchanges.back_mut() else {
            return false;
        };
        if back.status != ExchangeStatus::Open {
            return false;
        }
        let payload: usize = assistant
            .as_ref()
            .map_or(0, |m| m.blocks.iter().map(Block::payload_len).sum());
        back.assistant = assistant;
        back.status = ExchangeStatus::Closed;
        back.entries += entries.len();
        back.payload += payload;
        self.payload += payload;
        self.transcript.record(entries);
        self.enforce_budget();
        true
    }

    /// Mark the open exchange as rejected: refused by the service before
    /// any event, or refused by the model at the end of its reply. Its
    /// user entry stays in the transcript, `entries` (what the browser was
    /// already shown) are recorded after it, and the exchange leaves every
    /// later request. Returns `false` when no exchange is open.
    ///
    /// What the service saw was not the newest message alone: the request
    /// builder merges every unanswered user message immediately before it
    /// into one, so those exchanges (closed with no reply: a failed or
    /// cancelled request, a reply with no text) were part of the refused
    /// content and are rejected with it. Leaving any of them would resend
    /// the refused content on every later turn.
    pub fn reject_open(&mut self, entries: Vec<HistoryEntry>) -> bool {
        let Some(back) = self.exchanges.back_mut() else {
            return false;
        };
        if back.status != ExchangeStatus::Open {
            return false;
        }
        back.status = ExchangeStatus::Rejected;
        back.entries += entries.len();
        let last = self.exchanges.len() - 1;
        for exchange in self.exchanges.iter_mut().take(last).rev() {
            if exchange.status == ExchangeStatus::Closed && exchange.assistant.is_none() {
                exchange.status = ExchangeStatus::Rejected;
            } else if exchange.status == ExchangeStatus::Closed {
                break;
            }
        }
        self.transcript.record(entries);
        self.enforce_budget();
        true
    }

    /// Drop every reasoning block (`thinking` and `opaque`) from the
    /// assistant messages held, so the next request replays none. Returns
    /// how many blocks were dropped.
    ///
    /// A reasoning block's signature is bound to the conversation prefix
    /// that produced it. Once that prefix has changed, the model refuses
    /// the block, and the only recovery a Converse client has is to send
    /// the history without its reasoning. The text of each reply stays,
    /// so does the transcript: the thought pane keeps showing what the
    /// browser was already shown.
    pub fn strip_reasoning(&mut self) -> usize {
        let mut dropped = 0;
        for exchange in self.exchanges.iter_mut() {
            let Some(assistant) = exchange.assistant.as_mut() else {
                continue;
            };
            let before = assistant.blocks.len();
            let mut freed = 0;
            assistant.blocks.retain(|block| match block {
                Block::Thinking { .. } | Block::Opaque { .. } => {
                    freed += block.payload_len();
                    false
                }
                _ => true,
            });
            let removed = before - assistant.blocks.len();
            if removed > 0 {
                dropped += removed;
                exchange.payload -= freed;
                self.payload -= freed;
            }
        }
        dropped
    }

    /// The messages the next request is built from, in order, with the
    /// rejected exchanges left out. A closed exchange with no reply
    /// contributes its user message alone; the request builder merges it
    /// into the next user message.
    pub fn messages(&self) -> Vec<&Message> {
        let mut out = Vec::with_capacity(self.exchanges.len() * 2);
        for exchange in &self.exchanges {
            if exchange.status == ExchangeStatus::Rejected {
                continue;
            }
            out.push(&exchange.user);
            if let Some(assistant) = &exchange.assistant {
                out.push(assistant);
            }
        }
        out
    }

    /// The transcript, in recorded order.
    pub fn history(&self) -> Vec<HistoryEntry> {
        self.transcript.entries().cloned().collect()
    }

    /// The timestamp of the last recorded entry, for clamping the next.
    pub fn last_timestamp(&self) -> Option<i64> {
        self.transcript.last_timestamp()
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.exchanges.clear();
        self.payload = 0;
        self.transcript.clear();
    }

    /// How many exchanges are held.
    pub fn exchange_count(&self) -> usize {
        self.exchanges.len()
    }

    /// Whether the back exchange awaits its reply.
    pub fn has_open_exchange(&self) -> bool {
        self.exchanges
            .back()
            .is_some_and(|back| back.status == ExchangeStatus::Open)
    }

    /// The exchanges held, oldest first.
    pub fn exchanges(&self) -> impl Iterator<Item = &Exchange> {
        self.exchanges.iter()
    }

    /// Transcript text plus payload, the figure the budget is held on.
    pub fn bytes(&self) -> usize {
        self.transcript.bytes() + self.payload
    }

    /// Pop whole exchanges from the front while the budget or the entry
    /// cap is exceeded, never the back exchange.
    ///
    /// Removing an earlier turn while keeping later ones changes the prefix
    /// every retained reasoning block is bound to, so an eviction also
    /// drops the reasoning of what remains; the request after it carries
    /// the text of each reply and nothing the model would refuse.
    fn enforce_budget(&mut self) {
        let mut evicted_any = false;
        while self.exchanges.len() > 1 && self.transcript.over_budget(self.payload) {
            if let Some(evicted) = self.exchanges.pop_front() {
                self.transcript.evict_front(evicted.entries);
                self.payload -= evicted.payload;
                evicted_any = true;
            }
        }
        if evicted_any {
            self.strip_reasoning();
        }
    }
}
