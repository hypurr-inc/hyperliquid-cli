//! Notices: what the CLI says next to the answer, such as an empty result, a
//! truncated read, a lost connection or an automatic retry.
//!
//! Each kind owns its words and its machine shape, so a sentence is written
//! once wherever it is printed: [`Notice::text`] is what a person reads,
//! [`Notice::json`] is the object a program reads when the kind has one, and
//! [`Notice::eprint`] writes the sentence to stderr through [`super::sink`].
//!
//! A warning is marked two ways: [`Notice::Warning`] puts a `!` before the
//! message (`Warning:` when stderr is not rich), and [`caution`] paints a whole
//! line, for the lines inside a prompt or a consent preview.

use serde_json::{Value, json};

use super::{colors, format, sink};
use crate::mode::DisplayBlocker;

/// Marks the first line of a notice that needs attention, however it is built.
const GLYPH: &str = "!";

/// What an automatic retry or reconnect says it is doing.
const RETRYING: &str = "Retrying automatically…";

const STREAM_GAP: &str = "Connection was lost and restored. Events may have been missed.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A warning for a person, on any format.
    Warning(String),
    /// A `! ` headline and the indented lines under it that say what to do
    /// about it, printed as written.
    Advice {
        headline: String,
        lines: Vec<String>,
    },
    /// `text` painted as a whole line, for a prompt or a consent preview.
    Caution(String),
    /// A read stopped at the API's per-request cap. `message` says how to
    /// continue; the JSON form also carries the counts and the cursor.
    Truncated {
        message: String,
        returned: usize,
        cap: usize,
        next_start: Option<u64>,
    },
    /// The result has no rows; the sentence names what was empty.
    Empty(String),
    /// `--max-results` clipped a table.
    Clipped {
        shown: usize,
        total: usize,
    },
    ConnectionLost,
    Retrying,
    /// A human format on a pipe or an unattended run prints JSON lines.
    JsonLines(DisplayBlocker),
}

impl Notice {
    /// An advice with only a headline.
    #[must_use]
    pub fn advice(headline: impl Into<String>) -> Self {
        Self::Advice {
            headline: headline.into(),
            lines: Vec::new(),
        }
    }

    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Warning(message) | Self::Truncated { message, .. } => warning_line(message),
            Self::Advice { headline, lines } => {
                let mut text = format!("{GLYPH} {headline}");
                for line in lines {
                    text.push('\n');
                    text.push_str(line);
                }
                text
            }
            Self::Caution(text) => caution(text),
            Self::Empty(sentence) => sentence.clone(),
            Self::Clipped { shown, total } => format!(
                "Showing {} of {}. Remove --max-results to see all.",
                format::thousands(*shown as u64),
                format::thousands(*total as u64)
            ),
            Self::ConnectionLost => format!("{GLYPH} Lost connection. {RETRYING}"),
            Self::Retrying => RETRYING.to_string(),
            Self::JsonLines(DisplayBlocker::StdoutNotTerminal | DisplayBlocker::NoTerminal) => {
                "Streaming JSON lines because stdout isn't a terminal.".to_string()
            }
            Self::JsonLines(DisplayBlocker::StdinNotTerminal) => {
                "Streaming JSON lines because stdin isn't a terminal.".to_string()
            }
            Self::JsonLines(DisplayBlocker::Driven) => {
                "Streaming JSON lines because an agent, CI or --non-interactive run is driving."
                    .to_string()
            }
        }
    }

    /// The object a program reads, for the kinds that have one.
    #[must_use]
    pub fn json(&self) -> Option<Value> {
        match self {
            Self::Truncated {
                message,
                returned,
                cap,
                next_start,
            } => Some(json!({
                "warning": {
                    "code": "truncated",
                    "message": message,
                    "returned": returned,
                    "cap": cap,
                    "next_start": next_start,
                }
            })),
            Self::Warning(_)
            | Self::Advice { .. }
            | Self::Caution(_)
            | Self::Empty(_)
            | Self::Clipped { .. }
            | Self::ConnectionLost
            | Self::Retrying
            | Self::JsonLines(_) => None,
        }
    }

    /// Print the sentence to stderr, best effort.
    pub fn eprint(&self) {
        sink::err_line(&self.text());
    }

    /// Print to stderr: the JSON object when the run's output is JSON and the
    /// kind has one, else the sentence.
    pub fn eprint_as(&self, machine_format: bool) {
        match self.json().filter(|_| machine_format) {
            Some(value) => sink::err_line(&value.to_string()),
            None => self.eprint(),
        }
    }
}

/// The NDJSON control line a stream writes when its connection came back:
/// events in between may be missing. It travels in the stream on stdout, never
/// on stderr.
#[must_use]
pub fn stream_gap(subscription: &str) -> Value {
    json!({
        "type": "warning",
        "reason": "stream_gap",
        "subscription": subscription,
        "message": STREAM_GAP,
    })
}

/// A warning line for stderr: `! message` with a yellow `!` in rich output,
/// `Warning: message` otherwise.
fn warning_line(message: &str) -> String {
    if colors::stderr_enabled() {
        format!("{} {message}", colors::stderr::yellow(GLYPH))
    } else {
        format!("Warning: {message}")
    }
}

/// `text` painted yellow as a whole when stderr is rich, as written otherwise.
#[must_use]
pub fn caution(text: &str) -> String {
    colors::stderr::yellow(text)
}

/// Print a warning to stderr (all formats).
pub fn warn(message: &str) {
    Notice::Warning(message.to_string()).eprint();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_use_the_glyph_only_when_stderr_is_rich() {
        let notice = Notice::Warning("careful".into());
        assert_eq!(notice.text(), "Warning: careful");
        colors::with_streams_for_test(false, true, || {
            assert_eq!(
                notice.text(),
                format!("{} careful", colors::paint(colors::YELLOW, "!"))
            );
        });
    }

    #[test]
    fn a_truncation_is_a_warning_for_people_and_an_object_for_programs() {
        let notice = Notice::Truncated {
            message: "Returned 2000 fills.".into(),
            returned: 2000,
            cap: 2000,
            next_start: Some(7),
        };
        assert_eq!(notice.text(), "Warning: Returned 2000 fills.");
        assert_eq!(
            notice.json().unwrap(),
            json!({"warning": {
                "code": "truncated",
                "message": "Returned 2000 fills.",
                "returned": 2000,
                "cap": 2000,
                "next_start": 7,
            }})
        );
        let unbounded = Notice::Truncated {
            message: "m".into(),
            returned: 1,
            cap: 1,
            next_start: None,
        };
        assert_eq!(
            unbounded.json().unwrap()["warning"]["next_start"],
            Value::Null
        );
    }

    #[test]
    fn the_retry_words_are_written_once() {
        assert_eq!(Notice::Retrying.text(), "Retrying automatically…");
        assert_eq!(
            Notice::ConnectionLost.text(),
            "! Lost connection. Retrying automatically…"
        );
    }

    #[test]
    fn a_stream_gap_is_an_ndjson_warning_line() {
        assert_eq!(
            stream_gap("trades(BTC)"),
            json!({
                "type": "warning",
                "reason": "stream_gap",
                "subscription": "trades(BTC)",
                "message": "Connection was lost and restored. Events may have been missed.",
            })
        );
    }

    #[test]
    fn a_caution_follows_stderr_color_only() {
        colors::with_streams_for_test(true, false, || assert_eq!(caution("q"), "q"));
        colors::with_streams_for_test(false, true, || {
            assert_eq!(caution("q"), colors::paint(colors::YELLOW, "q"));
            assert_eq!(Notice::Caution("q".into()).text(), caution("q"));
        });
    }

    #[test]
    fn advice_prints_its_lines_as_written_with_no_warning_prefix() {
        let notice = Notice::Advice {
            headline: "Config file /c is readable by other users (mode 0644).".into(),
            lines: vec!["  Restrict it:".into(), "    chmod 600 /c".into()],
        };
        assert_eq!(
            notice.text(),
            "! Config file /c is readable by other users (mode 0644).\n  Restrict it:\n    chmod 600 /c"
        );
    }

    #[test]
    fn row_notices_read_as_plain_sentences() {
        assert_eq!(Notice::Empty("No orders.".into()).text(), "No orders.");
        assert_eq!(
            Notice::Clipped {
                shown: 1,
                total: 1234
            }
            .text(),
            "Showing 1 of 1,234. Remove --max-results to see all."
        );
        for notice in [
            Notice::Empty("x".into()),
            Notice::Clipped { shown: 1, total: 2 },
            Notice::ConnectionLost,
            Notice::Retrying,
            Notice::Warning("x".into()),
            Notice::advice("x"),
            Notice::Caution("x".into()),
            Notice::JsonLines(DisplayBlocker::Driven),
        ] {
            assert!(notice.json().is_none(), "{notice:?}");
        }
    }
}
