//! The system prompt: fixed parts in a fixed order, with today's date alone
//! on the last line.
//!
//! Every turn re-sends the system prompt, and a provider's prompt cache is
//! a prefix match over it. Everything ahead of the date is meant to be
//! byte-identical from one turn to the next, so the parts are joined in a
//! fixed order with a fixed separator, and the date, the one thing that
//! changes between days, is kept apart in its own field: the Bedrock
//! adapter puts its cache point between the two. This phase assembles two
//! parts, the embedded preamble and the date. Later phases push more parts
//! between them and change nothing here.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// One named part of the static text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Where the part came from, for a log line or a test.
    pub name: &'static str,
    /// The text, joined to its neighbours by [`PART_SEPARATOR`].
    pub text: String,
}

/// The assembled prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPrompt {
    /// Every part but the date, joined. Identical across days for
    /// identical parts.
    pub static_text: String,
    /// `Today's date is YYYY-MM-DD.`
    pub date_line: String,
}

/// What separates two parts: exactly one blank line.
pub const PART_SEPARATOR: &str = "\n\n";

/// Join `parts` in order and render `today` as the date line.
///
/// A pure function of its inputs: two calls with equal arguments return
/// equal prompts. Trailing whitespace on a part is trimmed so a part that
/// ends in a newline does not widen the separator.
pub fn assemble(parts: &[Part], today: Date) -> SystemPrompt {
    let static_text = parts
        .iter()
        .map(|part| part.text.trim_end())
        .collect::<Vec<_>>()
        .join(PART_SEPARATOR);
    SystemPrompt {
        static_text,
        date_line: format!("Today's date is {today}."),
    }
}

/// The embedded preamble: identity and the host operating system, and
/// nothing that varies per session, per model or per turn, because every
/// byte of it sits in the cached prefix.
pub fn preamble() -> Part {
    Part {
        name: "preamble",
        text: format!(
            "You are Mezame, an agent harness that connects people to language models \
             through a browser. The host operating system is {}.",
            std::env::consts::OS
        ),
    }
}

/// A calendar date in the proleptic Gregorian calendar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Date {
    pub year: i64,
    pub month: u32,
    pub day: u32,
}

impl fmt::Display for Date {
    /// `YYYY-MM-DD`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// The civil date `days` days after 1970-01-01, for any `days`.
///
/// Howard Hinnant's days-to-civil algorithm, which the crate carries in
/// fifteen lines rather than in a dependency: the dependency budget for
/// this phase admits the AWS crates and nothing else.
pub fn civil_from_days(days: i64) -> Date {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year_of_era = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    Date {
        year: if month <= 2 {
            year_of_era + 1
        } else {
            year_of_era
        },
        month: month as u32,
        day: day as u32,
    }
}

/// Today's date in UTC, from the system clock.
pub fn today_utc() -> Date {
    let secs = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs() as i64,
        Err(before) => -(before.duration().as_secs() as i64),
    };
    civil_from_days(secs.div_euclid(86_400))
}
