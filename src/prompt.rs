//! Shared prompts. Questions go to stderr; answers come from stdin.
//!
//! Every prompt checks [`Mode::can_prompt`] first and fails with
//! [`CliError::MissingInput`] naming the option that supplies the answer, so a
//! non-interactive run never blocks or guesses. End of input is also missing
//! input: an empty stdin never accepts a default.

use std::io::{self, BufRead};

use crate::errors::CliError;
use crate::mode::Mode;
use crate::output::sink;

/// Read a yes/no answer: `y` or `yes` is yes, `n` or `no` is no, any case,
/// and an empty answer is `blank`. `None` is anything else, which a
/// confirmation reads as no and a setup question rejects.
#[must_use]
pub fn parse_yes_no(answer: &str, blank: bool) -> Option<bool> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "" => Some(blank),
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

/// Ask a yes/no question. Only `y` or `yes` (any case) confirm; an empty or
/// unrecognized answer declines.
///
/// `bypass` names the option that answers the question in non-interactive
/// runs, such as `--yes`.
pub fn confirm(mode: &Mode, question: &str, bypass: &str) -> Result<bool, CliError> {
    let answer = read_line(mode, &format!("{question} [y/N]: "), bypass)?;
    Ok(parse_yes_no(&answer, false) == Some(true))
}

/// Ask for one line of text and return it without its line ending.
///
/// `input` names the option or argument that supplies the value without a
/// prompt.
pub fn read_line(mode: &Mode, label: &str, input: &str) -> Result<String, CliError> {
    ensure_can_prompt(mode, input)?;
    write_label(label)?;
    let Some(line) = read_stdin_line()? else {
        sink::err_line("");
        return Err(stdin_ended(input, "an answer"));
    };
    if !mode.terminal.stdin {
        // Piped answers aren't echoed, so end the prompt line ourselves.
        sink::err_line("");
    }
    Ok(line)
}

pub fn ensure_can_prompt(mode: &Mode, input: &str) -> Result<(), CliError> {
    match mode.prompt_blocker() {
        None => Ok(()),
        Some(reason) => Err(CliError::MissingInput(format!(
            "Can't prompt for {input} because of {reason}. Pass {input} instead."
        ))),
    }
}

/// Ask the user to type `expected` exactly. Anything else, including an empty
/// line, declines. Use for destructive actions that can't be undone.
pub fn confirm_typed(
    mode: &Mode,
    label: &str,
    expected: &str,
    bypass: &str,
) -> Result<bool, CliError> {
    let answer = read_line(mode, label, bypass)?;
    Ok(answer.trim() == expected)
}

/// On a terminal the value is typed at a masked prompt, which needs a run that
/// can prompt. Piped stdin supplies the secret directly: the first line, without
/// its line ending. End of input is missing input.
pub fn read_secret(mode: &Mode, label: &str, input: &str) -> Result<String, CliError> {
    let value = if mode.terminal.stdin {
        ensure_can_prompt(mode, input)?;
        write_label(label)?;
        let value = rpassword::read_password().map_err(io_error)?;
        sink::err_line("");
        value
    } else {
        if mode.can_prompt() {
            write_label(label)?;
        }
        match read_stdin_line()? {
            Some(line) => line,
            None => {
                if mode.can_prompt() {
                    sink::err_line("");
                }
                return Err(stdin_ended(input, "a value"));
            }
        }
    };
    if value.trim().is_empty() {
        return Err(CliError::MissingInput(format!(
            "Can't continue without {input}. The value was empty."
        )));
    }
    Ok(value)
}

/// A wallet's private key, read as [`read_secret`] reads a secret.
pub fn read_private_key(mode: &Mode) -> Result<String, CliError> {
    read_secret(mode, "Private key: ", "a private key on stdin")
}

fn write_label(label: &str) -> Result<(), CliError> {
    sink::try_err(label).map_err(io_error)
}

fn io_error(err: io::Error) -> CliError {
    CliError::Internal(anyhow::anyhow!(
        "couldn't use the terminal for a prompt: {err}"
    ))
}

/// One line from stdin without its line ending, or `None` at end of input.
fn read_stdin_line() -> Result<Option<String>, CliError> {
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line).map_err(io_error)?;
    Ok((read > 0).then(|| line.trim_end_matches(['\r', '\n']).to_string()))
}

/// End of input is missing input: an empty stdin never supplies `input`.
fn stdin_ended(input: &str, given: &str) -> CliError {
    CliError::MissingInput(format!(
        "Can't continue without {input}. Stdin ended before {given} was given."
    ))
}

#[cfg(test)]
mod tests {
    use super::parse_yes_no;

    #[test]
    fn yes_and_no_read_in_any_case_with_surrounding_space() {
        for yes in ["y", "Y", " YES \n", "yes"] {
            assert_eq!(parse_yes_no(yes, false), Some(true), "{yes:?}");
        }
        for no in ["n", "N", " No\n", "no"] {
            assert_eq!(parse_yes_no(no, true), Some(false), "{no:?}");
        }
    }

    #[test]
    fn an_empty_answer_is_the_prompts_default_and_anything_else_is_unrecognized() {
        assert_eq!(parse_yes_no("", true), Some(true));
        assert_eq!(parse_yes_no("  \n", false), Some(false));
        for other in ["yep", "nope", "1", "maybe"] {
            assert_eq!(parse_yes_no(other, true), None, "{other:?}");
        }
    }
}
