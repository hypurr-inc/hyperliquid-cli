//! The consent step of a command that asks before it runs.

use crate::command_spec::YES_ARG;
use crate::errors::CliError;
use crate::mode::Mode;
use crate::prompt;

/// Ask for consent unless `--yes` was passed. `preview` is printed to stderr
/// before the `question`; a decline is `abort` as a configuration error.
pub(crate) fn unless_yes(
    yes: bool,
    mode: &Mode,
    preview: &str,
    question: &str,
    abort: &str,
) -> Result<(), CliError> {
    if yes {
        return Ok(());
    }
    prompt::consent(mode, &format!("--{YES_ARG}"), preview, question, abort)
}
