//! The transcript entries a stored message row stands for.
//!
//! One mapping, used twice: [`crate::conversation::Conversation::restore`]
//! records these entries as it rebuilds a session from its rows, and
//! `GET /history` serves what that rebuild recorded. A user row is one
//! `user` entry holding the text the loop recorded when the row was
//! written, which is the typed text alone; the blocks are not consulted,
//! because an attachment's text is a text block in them and was never in
//! the entry. An assistant row is a `thought` entry per thinking block
//! with text, then an `agent` entry for the joined text when there is
//! any, carrying the row's usage counts. The `rejected` flag changes
//! nothing here: a refused reply's entries were shown live and stay in
//! the transcript, as `reject_open` recorded them.

use crate::backend::{EntryBody, HistoryEntry};
use crate::conversation::Block;
use crate::store::{MessageRole, MessageRow};

/// The `user` entry a user row stands for: the row's recorded text.
pub fn user_entry(row: &MessageRow) -> HistoryEntry {
    HistoryEntry {
        body: EntryBody::User {
            text: row.text.clone().unwrap_or_default(),
        },
        timestamp: row.created,
        usage: None,
    }
}

/// The entries `row` contributes to the transcript, in order.
pub fn entries_from_row(row: &MessageRow) -> Vec<HistoryEntry> {
    match row.role {
        MessageRole::User => vec![user_entry(row)],
        MessageRole::Assistant => {
            let mut entries = Vec::new();
            for block in &row.blocks {
                if let Block::Thinking { text, .. } = block {
                    if !text.is_empty() {
                        entries.push(HistoryEntry {
                            body: EntryBody::Thought { text: text.clone() },
                            timestamp: row.created,
                            usage: None,
                        });
                    }
                }
            }
            let text = agent_text(&row.blocks);
            if !text.is_empty() {
                entries.push(HistoryEntry {
                    body: EntryBody::Agent { text },
                    timestamp: row.created,
                    usage: row.usage,
                });
            }
            entries
        }
    }
}

/// The entries of every row, oldest first.
pub fn entries_from_rows(rows: &[MessageRow]) -> Vec<HistoryEntry> {
    rows.iter().flat_map(entries_from_row).collect()
}

/// The text of an assistant message's `agent` entry: its text blocks in
/// order, joined by one newline. A reply streamed live accumulates into
/// one text block, so the join only matters for a row written with more.
pub fn agent_text(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
