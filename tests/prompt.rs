//! The system prompt assembly: a function of its inputs, date last.

use mezame::prompt::{assemble, civil_from_days, preamble, today_utc, Date, Part, PART_SEPARATOR};

fn part(name: &'static str, text: &str) -> Part {
    Part {
        name,
        text: text.to_string(),
    }
}

#[test]
fn two_assemblies_from_identical_inputs_are_byte_identical() {
    let parts = [preamble(), part("steering", "Be brief.")];
    let date = Date {
        year: 2026,
        month: 9,
        day: 7,
    };
    assert_eq!(assemble(&parts, date), assemble(&parts, date));
}

#[test]
fn the_date_is_its_own_line_and_appears_nowhere_in_the_static_text() {
    let date = Date {
        year: 2026,
        month: 9,
        day: 7,
    };
    let prompt = assemble(&[preamble()], date);
    assert_eq!(prompt.date_line, "Today's date is 2026-09-07.");
    assert!(!prompt.static_text.contains("2026-09-07"));
    assert!(!prompt.static_text.contains("Today's date"));
}

#[test]
fn parts_are_joined_by_one_blank_line_with_trailing_whitespace_trimmed() {
    let prompt = assemble(
        &[
            part("a", "first\n\n"),
            part("b", "second  "),
            part("c", "third"),
        ],
        Date {
            year: 2026,
            month: 1,
            day: 2,
        },
    );
    assert_eq!(PART_SEPARATOR, "\n\n");
    assert_eq!(prompt.static_text, "first\n\nsecond\n\nthird");
}

#[test]
fn no_parts_assemble_to_an_empty_static_text() {
    let prompt = assemble(
        &[],
        Date {
            year: 2026,
            month: 1,
            day: 2,
        },
    );
    assert_eq!(prompt.static_text, "");
    assert_eq!(prompt.date_line, "Today's date is 2026-01-02.");
}

#[test]
fn civil_from_days_pins_the_epoch_two_leap_days_and_a_2026_date() {
    let date = |year, month, day| Date { year, month, day };
    assert_eq!(civil_from_days(0), date(1970, 1, 1));
    assert_eq!(civil_from_days(-1), date(1969, 12, 31));
    assert_eq!(civil_from_days(11016), date(2000, 2, 29));
    assert_eq!(civil_from_days(19782), date(2024, 2, 29));
    assert_eq!(civil_from_days(19783), date(2024, 3, 1));
    assert_eq!(civil_from_days(20703), date(2026, 9, 7));
}

#[test]
fn today_utc_is_a_plausible_date() {
    let today = today_utc();
    assert!(today.year >= 2026, "{today}");
    assert!((1..=12).contains(&today.month), "{today}");
    assert!((1..=31).contains(&today.day), "{today}");
}

#[test]
fn a_date_displays_as_iso_with_zero_padding() {
    let date = Date {
        year: 7,
        month: 3,
        day: 4,
    };
    assert_eq!(date.to_string(), "0007-03-04");
}

#[test]
fn the_preamble_is_the_fixed_text_with_the_os_and_nothing_per_session() {
    let part = preamble();
    assert_eq!(part.name, "preamble");
    assert_eq!(
        part.text,
        format!(
            "You are Mezame, an agent harness that connects people to language models through a \
             browser. The host operating system is {}.",
            std::env::consts::OS
        )
    );
    for forbidden in ["session", "anthropic", "claude", "attach"] {
        assert!(
            !part.text.to_ascii_lowercase().contains(forbidden),
            "the preamble names {forbidden}, a per-session or per-model value"
        );
    }
}
