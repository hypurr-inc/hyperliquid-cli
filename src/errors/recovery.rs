use super::CliError;
use crate::settings::env;

/// A recovery command in the `next` list of an error envelope. `when` states
/// the condition for running it, such as human approval.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NextCommand {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<&'static str>,
}

impl NextCommand {
    #[must_use]
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            when: None,
        }
    }
}

/// Recovery guidance rendered under an error: an instruction ending in `:`,
/// the commands indented four spaces, then an optional closing note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    pub instruction: String,
    pub commands: Vec<NextCommand>,
    pub note: Option<String>,
}

impl CliError {
    /// Recovery commands for this error, when one is known.
    #[must_use]
    pub fn recovery(&self) -> Option<Recovery> {
        match self {
            Self::AuthRequired => Some(Recovery {
                instruction: "Set up a wallet:".into(),
                commands: vec![
                    NextCommand::new("hyperliquid setup"),
                    NextCommand::new("hyperliquid wallet login"),
                ],
                note: Some(format!(
                    "Or set {}, or {} with --bankr, for this run.",
                    env::PRIVATE_KEY,
                    env::BANKR_API_KEY
                )),
            }),
            Self::UnknownOutcome(_) => Some(Recovery {
                instruction: "Check open orders and recent fills:".into(),
                commands: vec![
                    NextCommand::new(crate::hints::CHECK_LIVE_COMMAND),
                    NextCommand::new("hyperliquid account fills"),
                ],
                note: None,
            }),
            Self::ConfirmationRequired {
                preview, approve, ..
            } => {
                let approve = approve.as_ref().map(|command| NextCommand {
                    command: command.clone(),
                    when: Some("Only after a human approves"),
                });
                match preview {
                    Some(preview) => Some(Recovery {
                        instruction: "Preview it:".into(),
                        commands: vec![NextCommand::new(preview.clone())]
                            .into_iter()
                            .chain(approve)
                            .collect(),
                        note: None,
                    }),
                    None => approve.map(|approve| Recovery {
                        instruction: "After a human approves, rerun with --yes:".into(),
                        commands: vec![approve],
                        note: None,
                    }),
                }
            }
            _ => None,
        }
    }
}
