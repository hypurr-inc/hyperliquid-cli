//! One consent policy. [`Invocation::requires_approval`] decides whether a run
//! must be approved; this module says what the approval block shows
//! ([`ConsentBlock`]), asks the question, and turns a decline or a run that
//! cannot ask into its error. The pipeline refuses runs that cannot ask before
//! a command starts (`cli_runtime`); the functions here are what a command
//! calls once it has its facts.
//!
//! The error a decline fails with stays the command's own text, and its class
//! stays with the surface that produced it:
//!
//! | surface                                   | error                          |
//! |-------------------------------------------|--------------------------------|
//! | pipeline, run cannot ask, no `--yes`      | `ConfirmationRequired`         |
//! | a command asks and the run cannot ask     | `MissingInput` ([`ensure_can_ask`]) |
//! | the user declines the prompt              | `Configuration` ([`declined`]) |
//! | unattended command needs approval         | `Configuration` ([`unattended_refusal`]) |
//! | wallet prompts: the user declines         | a "cancelled" receipt, exit 0  |

use std::borrow::Cow;
use std::fmt::Display;

use hypersdk::Address;

use crate::command_spec::YES_FLAG;
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::mode::Mode;
use crate::output::{notice, sink};
use crate::prompt;

/// The lines a person reads before they approve, in the order they are shown.
/// A command supplies the facts and keeps its own wording: [`Self::field`]
/// takes the label as the command spells it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConsentBlock {
    lines: Vec<String>,
}

impl ConsentBlock {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The two lines every approval of a signed action opens with. `network`
    /// is spelled as the command spells it.
    #[must_use]
    pub fn signed(network: impl Display, signer: Address) -> Self {
        Self::new()
            .field("Network", network)
            .field("Signer", signer)
    }

    /// One line as written, or an empty line.
    #[must_use]
    pub fn text(mut self, line: impl Into<String>) -> Self {
        self.lines.push(line.into());
        self
    }

    #[must_use]
    pub fn texts(mut self, lines: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.lines.extend(lines.into_iter().map(Into::into));
        self
    }

    /// `label: value`.
    #[must_use]
    pub fn field(self, label: &str, value: impl Display) -> Self {
        self.text(format!("{label}: {value}"))
    }

    /// The lines joined by newlines, without a trailing one.
    #[must_use]
    pub fn render(&self) -> String {
        self.lines.join("\n")
    }

    fn print(&self) -> Result<(), CliError> {
        for line in &self.lines {
            sink::try_err_line(line).map_err(|err| CliError::Internal(anyhow::anyhow!(err)))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Styled as a warning on a terminal.
    Warning,
    /// As written.
    Plain,
}

/// A run that cannot prompt fails with `--yes` as the way to answer.
pub fn ensure_can_ask(mode: &Mode) -> Result<(), CliError> {
    prompt::ensure_can_prompt(mode, YES_FLAG)
}

/// Print `block` on stderr. A run that cannot ask fails first, so nothing is
/// shown that cannot be answered.
pub fn show(mode: &Mode, block: &ConsentBlock) -> Result<(), CliError> {
    ensure_can_ask(mode)?;
    block.print()
}

/// [`show`] `block`, then ask `question`. `false` is a decline.
pub fn ask(
    mode: &Mode,
    block: &ConsentBlock,
    question: &str,
    tone: Tone,
) -> Result<bool, CliError> {
    show(mode, block)?;
    let question = match tone {
        Tone::Warning => Cow::Owned(notice::caution(question)),
        Tone::Plain => Cow::Borrowed(question),
    };
    prompt::confirm(mode, &question, YES_FLAG)
}

/// Ask before a run acts, unless it needs no approval
/// ([`Invocation::requires_approval`]). A decline fails with `abort`.
pub fn approve(
    inv: &Invocation,
    yes: bool,
    block: &ConsentBlock,
    question: &str,
    abort: &str,
) -> Result<(), CliError> {
    if !inv.requires_approval(yes)? {
        return Ok(());
    }
    if ask(inv.mode(), block, question, Tone::Warning)? {
        Ok(())
    } else {
        Err(declined(abort))
    }
}

/// The user said no. `abort` is the command's own text.
#[must_use]
pub fn declined(abort: impl Into<String>) -> CliError {
    CliError::Configuration(abort.into())
}

/// A command that cannot ask (it runs unattended for minutes) still needs
/// approval. `action` completes "Can't {action} on mainnet without --yes";
/// `command` is the invocation to rerun, which gets `--yes` appended.
#[must_use]
pub fn unattended_refusal(action: &str, command: &str) -> CliError {
    CliError::Configuration(format!(
        "Can't {action} on mainnet without {YES_FLAG}. Rerun with {YES_FLAG} once you approve it:\n    {command} {YES_FLAG}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_renders_its_lines_in_order() {
        let signer: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let block = ConsentBlock::signed("mainnet", signer)
            .field("Acting as", "0xabc (vault or subaccount)")
            .text("")
            .texts(["Orders: 2 batch orders"]);
        assert_eq!(
            block.render(),
            "Network: mainnet\n\
             Signer: 0x0000000000000000000000000000000000000001\n\
             Acting as: 0xabc (vault or subaccount)\n\
             \n\
             Orders: 2 batch orders"
        );
        assert_eq!(ConsentBlock::new().render(), "");
    }

    #[test]
    fn the_refusal_names_the_action_and_the_command_to_rerun_with_yes() {
        let err = unattended_refusal("chase", "hyperliquid --format json orders chase --coin ETH");
        assert_eq!(err.exit_code(), 2);
        assert_eq!(
            err.to_string(),
            "Configuration error: Can't chase on mainnet without --yes. Rerun with --yes once you approve it:\n    hyperliquid --format json orders chase --coin ETH --yes"
        );
    }

    #[test]
    fn a_decline_is_the_commands_own_text_as_a_configuration_error() {
        let err = declined("transfer aborted");
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.to_string(), "Configuration error: transfer aborted");
    }

    #[test]
    fn a_run_that_cannot_ask_names_the_flag_that_answers() {
        let mode = Mode::default().running(crate::terminal::OutputFormat::Json, false, false);
        let err = ensure_can_ask(&mode).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Can't prompt for --yes because of JSON output. Pass --yes instead."
        );
        assert!(show(&mode, &ConsentBlock::new().text("never shown")).is_err());
    }

    #[test]
    fn the_flag_is_the_arg_id_with_dashes() {
        assert_eq!(YES_FLAG, format!("--{}", crate::command_spec::YES_ARG));
    }
}
