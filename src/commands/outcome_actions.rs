//! `outcomes split|merge|merge-question|negate`: the HIP-4 `userOutcome` exchange
//! action, an L1 action signed like an order.
//!
//! Wire format (docs exchange-endpoint "Split/Merge outcome", "Merge question",
//! "Negate outcome"; hypersdk `UserOutcomeAction`; nktkas `userOutcome`): one of
//! `splitOutcome {outcome, amount}`, `mergeOutcome {outcome, amount|null}`,
//! `mergeQuestion {question, amount|null}`, `negateOutcome {question, outcome,
//! amount}` under `{"type": "userOutcome"}`, amounts as normalized decimal
//! strings, `null` meaning the maximum.

use std::collections::BTreeSet;

use clap::Args;
use hypersdk::Decimal;
use hypersdk::hypercore::types::Action;
use hypersdk::hypercore::types::api::UserOutcomeAction;
use serde::Serialize;

use crate::assets::{OutcomeCatalog, load_outcome_listing};
use crate::command_spec::{CliCommand, CommandSpec, InputKind, arg};
use crate::commands::actions;
use crate::commands::confirm;
use crate::commands::rows::{ListRow, Row, Rows, or_missing};
use crate::commands::support::print_dry_run;
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::quantity;

/// Arguments for `outcomes split`.
#[derive(Args, Debug, Clone)]
pub struct OutcomeSplitArgs {
    /// Outcome id from `outcomes list`
    #[arg(long)]
    pub outcome: u32,

    /// Quote tokens to split into that many shares of each side
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,

    /// Confirm live execution without prompting
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Arguments for `outcomes merge`.
#[derive(Args, Debug, Clone)]
pub struct OutcomeMergeArgs {
    /// Outcome id from `outcomes list`
    #[arg(long)]
    pub outcome: u32,

    /// Shares of each side to merge back into quote tokens
    #[arg(
        long,
        allow_hyphen_values = true,
        conflicts_with = "max",
        required_unless_present = "max"
    )]
    pub amount: Option<Decimal>,

    /// Merge the maximum available
    #[arg(long)]
    pub max: bool,

    /// Confirm live execution without prompting
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Arguments for `outcomes merge-question`.
#[derive(Args, Debug, Clone)]
pub struct OutcomeMergeQuestionArgs {
    /// Question id from `outcomes questions`
    #[arg(long)]
    pub question: u32,

    /// Yes shares of every outcome of the question to merge into quote tokens
    #[arg(
        long,
        allow_hyphen_values = true,
        conflicts_with = "max",
        required_unless_present = "max"
    )]
    pub amount: Option<Decimal>,

    /// Merge the maximum available
    #[arg(long)]
    pub max: bool,

    /// Confirm live execution without prompting
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Arguments for `outcomes negate`.
#[derive(Args, Debug, Clone)]
pub struct OutcomeNegateArgs {
    /// Question id from `outcomes questions`
    #[arg(long)]
    pub question: u32,

    /// Outcome id of the question whose No shares are converted
    #[arg(long)]
    pub outcome: u32,

    /// No shares to convert
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,

    /// Confirm live execution without prompting
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// One `userOutcome` operation with its validated arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Conversion {
    Split {
        outcome: u32,
        amount: Decimal,
    },
    Merge {
        outcome: u32,
        amount: Option<Decimal>,
    },
    MergeQuestion {
        question: u32,
        amount: Option<Decimal>,
    },
    Negate {
        question: u32,
        outcome: u32,
        amount: Decimal,
    },
}

impl Conversion {
    fn split(args: &OutcomeSplitArgs) -> Result<Self, CliError> {
        quantity::require_positive(args.amount, "amount")?;
        Ok(Self::Split {
            outcome: args.outcome,
            amount: args.amount.normalize(),
        })
    }

    fn merge(args: &OutcomeMergeArgs) -> Result<Self, CliError> {
        Ok(Self::Merge {
            outcome: args.outcome,
            amount: merge_amount(args.amount)?,
        })
    }

    fn merge_question(args: &OutcomeMergeQuestionArgs) -> Result<Self, CliError> {
        Ok(Self::MergeQuestion {
            question: args.question,
            amount: merge_amount(args.amount)?,
        })
    }

    fn negate(args: &OutcomeNegateArgs) -> Result<Self, CliError> {
        quantity::require_positive(args.amount, "amount")?;
        Ok(Self::Negate {
            question: args.question,
            outcome: args.outcome,
            amount: args.amount.normalize(),
        })
    }

    fn action(&self) -> UserOutcomeAction {
        match *self {
            Self::Split { outcome, amount } => UserOutcomeAction::split(outcome, amount),
            Self::Merge { outcome, amount } => UserOutcomeAction::merge(outcome, amount),
            Self::MergeQuestion { question, amount } => {
                UserOutcomeAction::merge_question(question, amount)
            }
            Self::Negate {
                question,
                outcome,
                amount,
            } => UserOutcomeAction::negate(question, outcome, amount),
        }
    }

    /// The name a dry-run preview and the confirmation row give the action.
    fn name(&self) -> &'static str {
        match self {
            Self::Split { .. } => "split-outcome",
            Self::Merge { .. } => "merge-outcome",
            Self::MergeQuestion { .. } => "merge-question",
            Self::Negate { .. } => "negate-outcome",
        }
    }

    fn outcome(&self) -> Option<u32> {
        match *self {
            Self::Split { outcome, .. }
            | Self::Merge { outcome, .. }
            | Self::Negate { outcome, .. } => Some(outcome),
            Self::MergeQuestion { .. } => None,
        }
    }

    fn question(&self) -> Option<u32> {
        match *self {
            Self::MergeQuestion { question, .. } | Self::Negate { question, .. } => Some(question),
            Self::Split { .. } | Self::Merge { .. } => None,
        }
    }

    fn amount(&self) -> Option<Decimal> {
        match *self {
            Self::Split { amount, .. } | Self::Negate { amount, .. } => Some(amount),
            Self::Merge { amount, .. } | Self::MergeQuestion { amount, .. } => amount,
        }
    }

    fn amount_text(&self) -> String {
        self.amount()
            .map_or_else(|| "max".to_string(), |amount| amount.to_string())
    }

    /// What the exchange will do, for the confirmation prompt.
    fn describe(&self, quote: &str) -> String {
        let amount = self.amount().map_or_else(
            || "the maximum available".to_string(),
            |amount| amount.to_string(),
        );
        match self {
            Self::Split { outcome, .. } => format!(
                "Split {amount} {quote} into {amount} shares of each side of outcome {outcome}"
            ),
            Self::Merge { outcome, .. } => {
                format!("Merge {amount} shares of each side of outcome {outcome} into {quote}")
            }
            Self::MergeQuestion { question, .. } => format!(
                "Merge {amount} Yes shares of every outcome of question {question} into {quote}"
            ),
            Self::Negate {
                question, outcome, ..
            } => format!(
                "Convert {amount} No shares of outcome {outcome} into {amount} Yes shares of every other outcome of question {question}"
            ),
        }
    }

    /// Check the ids against `outcomeMeta` and name the quote tokens involved.
    /// A listed outcome or question is the only one the exchange can convert:
    /// settled outcomes pay out on their own.
    fn preflight(&self, catalog: &OutcomeCatalog) -> Result<BTreeSet<String>, CliError> {
        let listed = |outcome: u32| {
            catalog
                .sides
                .iter()
                .any(|row| row.outcome == u64::from(outcome))
        };
        let quotes_of = |outcomes: &mut dyn Iterator<Item = u64>| -> BTreeSet<String> {
            let wanted: BTreeSet<u64> = outcomes.collect();
            catalog
                .sides
                .iter()
                .filter(|row| wanted.contains(&row.outcome))
                .filter_map(|row| row.quote_token.clone())
                .collect()
        };
        if let Some(outcome) = self.outcome().filter(|outcome| !listed(*outcome)) {
            return Err(CliError::Unsupported(format!(
                "outcome {outcome} is not listed by outcomeMeta: it never existed, could not be read, or has settled (settled outcomes pay out automatically; see `outcomes settled {outcome}`)"
            )));
        }
        let Some(question) = self.question() else {
            return Ok(quotes_of(&mut self.outcome().map(u64::from).into_iter()));
        };
        let Some(listing) = catalog
            .questions
            .iter()
            .find(|candidate| candidate.question == u64::from(question))
        else {
            return Err(CliError::Unsupported(format!(
                "question {question} is not listed by outcomeMeta; see `outcomes questions`"
            )));
        };
        let members: Vec<u64> = listing
            .named_outcomes
            .iter()
            .chain(&listing.settled_named_outcomes)
            .chain(&listing.fallback_outcome)
            .copied()
            .collect();
        if let Some(outcome) = self
            .outcome()
            .filter(|outcome| !members.contains(&u64::from(*outcome)))
        {
            return Err(CliError::Unsupported(format!(
                "outcome {outcome} is not part of question {question}; its outcomes are named {:?}, fallback {:?}",
                listing.named_outcomes, listing.fallback_outcome
            )));
        }
        Ok(quotes_of(&mut members.into_iter()))
    }
}

fn merge_amount(amount: Option<Decimal>) -> Result<Option<Decimal>, CliError> {
    amount
        .map(|amount| {
            quantity::require_positive(amount, "amount")?;
            Ok(amount.normalize())
        })
        .transpose()
}

#[derive(Debug, Clone, Serialize)]
struct OutcomeActionConfirmation {
    action: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    question: Option<u32>,
    /// A decimal, or `max` when the whole available balance was converted.
    amount: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    quote_token: Option<String>,
}

impl Row for OutcomeActionConfirmation {
    const HEADERS: &'static [&'static str] =
        &["Action", "Status", "Outcome", "Question", "Amount", "Quote"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.action.clone(),
            self.status.clone(),
            or_missing(self.outcome),
            or_missing(self.question),
            self.amount.clone(),
            or_missing(self.quote_token.as_ref()),
        ]
    }
}

impl ListRow for OutcomeActionConfirmation {
    const EMPTY: &'static str = "No results.";
}

/// The shared run: check the ids, preview or sign and send, confirm on mainnet.
async fn convert(inv: &Invocation, conversion: Conversion, yes: bool) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let catalog = load_outcome_listing(remote.api()).await?;
    let quotes = conversion.preflight(&catalog)?;
    let quote = quotes.iter().cloned().collect::<Vec<_>>().join("/");
    let quote_token = (!quote.is_empty()).then(|| quote.clone());
    let action = Action::UserOutcome(conversion.action());

    if inv.dry_run() {
        return print_dry_run(
            inv,
            &conversion.name().replace('-', "_"),
            serde_json::json!({
                "outcome": conversion.outcome(),
                "question": conversion.question(),
                "amount": conversion.amount_text(),
                "quote_token": quote_token,
                "action": serde_json::to_value(&action)?,
            }),
        );
    }

    let signer = inv.resolve_signer()?;
    if inv.command().prompts_on(remote.network()) {
        let label = quote_token.as_deref().unwrap_or("quote tokens");
        let description = conversion.describe(label);
        confirm::unless_yes(
            yes,
            inv.mode(),
            &format!(
                "{description}\n  network: {}\n  signer: {}",
                remote.network(),
                signer.address()
            ),
            &format!("{description}?"),
            "outcome conversion aborted",
        )?;
    }
    actions::send_l1_action_raw(
        remote.api(),
        &signer,
        action,
        actions::nonce_now(),
        None,
        "exchange action",
    )
    .await?;

    inv.print(&Rows::new(vec![OutcomeActionConfirmation {
        action: conversion.name().to_string(),
        status: "confirmed".to_string(),
        outcome: conversion.outcome(),
        question: conversion.question(),
        amount: conversion.amount_text(),
        quote_token,
    }]));
    Ok(())
}

pub struct OutcomesSplit;

impl CliCommand for OutcomesSplit {
    type Args = OutcomeSplitArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["outcomes", "split"],
        "trade",
        "Split quote tokens into outcome shares",
        "Split X quote tokens into X shares of each side of an outcome (HIP-4 userOutcome splitOutcome). Dry-run first. Mainnet runs prompt for confirmation unless --yes is passed; --testnet runs without a prompt.",
    )
    .prompt()
    .testnet_unprompted()
    .args(&[
        arg("outcome")
            .kind(InputKind::Identifier)
            .describe("Outcome id from `outcomes list`."),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("Quote tokens to split (the outcome's quote token, see `outcomes get`)."),
        arg("yes")
            .kind(InputKind::Boolean)
            .describe("Skip the mainnet confirmation prompt for deliberate automation. Testnet runs never prompt."),
    ]);

    fn validate(args: &OutcomeSplitArgs) -> Result<(), CliError> {
        Conversion::split(args).map(drop)
    }

    async fn run(inv: &Invocation, args: &OutcomeSplitArgs) -> Result<(), anyhow::Error> {
        convert(inv, Conversion::split(args)?, args.yes).await
    }
}

pub struct OutcomesMerge;

impl CliCommand for OutcomesMerge {
    type Args = OutcomeMergeArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["outcomes", "merge"],
        "trade",
        "Merge both sides of an outcome into quote tokens",
        "Merge X shares of each side of an outcome back into X quote tokens (HIP-4 userOutcome mergeOutcome); --max merges the maximum available. Dry-run first. Mainnet runs prompt for confirmation unless --yes is passed; --testnet runs without a prompt.",
    )
    .prompt()
    .testnet_unprompted()
    .one_of_required(&[&["amount"], &["max"]])
    .args(&[
        arg("outcome")
            .kind(InputKind::Identifier)
            .describe("Outcome id from `outcomes list`."),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("Shares of each side to merge."),
        arg("max")
            .kind(InputKind::Boolean)
            .describe("Merge the maximum available instead of an amount."),
        arg("yes")
            .kind(InputKind::Boolean)
            .describe("Skip the mainnet confirmation prompt for deliberate automation. Testnet runs never prompt."),
    ]);

    fn validate(args: &OutcomeMergeArgs) -> Result<(), CliError> {
        Conversion::merge(args).map(drop)
    }

    async fn run(inv: &Invocation, args: &OutcomeMergeArgs) -> Result<(), anyhow::Error> {
        convert(inv, Conversion::merge(args)?, args.yes).await
    }
}

pub struct OutcomesMergeQuestion;

impl CliCommand for OutcomesMergeQuestion {
    type Args = OutcomeMergeQuestionArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["outcomes", "merge-question"],
        "trade",
        "Merge a question's Yes shares into quote tokens",
        "Merge X Yes shares from each outcome of a question into X quote tokens (HIP-4 userOutcome mergeQuestion); --max merges the maximum available. Dry-run first. Mainnet runs prompt for confirmation unless --yes is passed; --testnet runs without a prompt.",
    )
    .prompt()
    .testnet_unprompted()
    .one_of_required(&[&["amount"], &["max"]])
    .args(&[
        arg("question")
            .kind(InputKind::Identifier)
            .describe("Question id from `outcomes questions`."),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("Yes shares of each outcome of the question to merge."),
        arg("max")
            .kind(InputKind::Boolean)
            .describe("Merge the maximum available instead of an amount."),
        arg("yes")
            .kind(InputKind::Boolean)
            .describe("Skip the mainnet confirmation prompt for deliberate automation. Testnet runs never prompt."),
    ]);

    fn validate(args: &OutcomeMergeQuestionArgs) -> Result<(), CliError> {
        Conversion::merge_question(args).map(drop)
    }

    async fn run(inv: &Invocation, args: &OutcomeMergeQuestionArgs) -> Result<(), anyhow::Error> {
        convert(inv, Conversion::merge_question(args)?, args.yes).await
    }
}

pub struct OutcomesNegate;

impl CliCommand for OutcomesNegate {
    type Args = OutcomeNegateArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["outcomes", "negate"],
        "trade",
        "Convert No shares of a question outcome into Yes shares",
        "Convert X No shares of an outcome of a question into X Yes shares of every other outcome of the question (HIP-4 userOutcome negateOutcome). Dry-run first. Mainnet runs prompt for confirmation unless --yes is passed; --testnet runs without a prompt.",
    )
    .prompt()
    .testnet_unprompted()
    .args(&[
        arg("question")
            .kind(InputKind::Identifier)
            .describe("Question id from `outcomes questions`."),
        arg("outcome")
            .kind(InputKind::Identifier)
            .describe("Outcome id of the question whose No shares are converted."),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("No shares to convert."),
        arg("yes")
            .kind(InputKind::Boolean)
            .describe("Skip the mainnet confirmation prompt for deliberate automation. Testnet runs never prompt."),
    ]);

    fn validate(args: &OutcomeNegateArgs) -> Result<(), CliError> {
        Conversion::negate(args).map(drop)
    }

    async fn run(inv: &Invocation, args: &OutcomeNegateArgs) -> Result<(), anyhow::Error> {
        convert(inv, Conversion::negate(args)?, args.yes).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> OutcomeCatalog {
        let meta = serde_json::json!({
            "outcomes": [
                {"outcome": 10, "name": "fallback", "description": "", "quoteToken": "USDC",
                 "sideSpecs": [{"name": "Yes"}, {"name": "No"}]},
                {"outcome": 11, "name": "A", "description": "", "quoteToken": "USDC",
                 "sideSpecs": [{"name": "Yes"}, {"name": "No"}]},
                {"outcome": 12, "name": "B", "description": "", "quoteToken": "USDH",
                 "sideSpecs": [{"name": "Yes"}, {"name": "No"}]},
                {"outcome": 99, "name": "alone", "description": "", "quoteToken": "USDC",
                 "sideSpecs": [{"name": "Yes"}, {"name": "No"}]}
            ],
            "questions": [{"question": 7, "name": "Q", "description": "",
                "fallbackOutcome": 10, "namedOutcomes": [11, 12], "settledNamedOutcomes": []}]
        });
        crate::assets::catalog_for_tests(meta)
    }

    fn dec(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    #[test]
    fn wire_actions_match_the_documented_shapes() {
        let cases = [
            (
                Conversion::Split {
                    outcome: 20,
                    amount: dec("10"),
                },
                r#"{"type":"userOutcome","splitOutcome":{"outcome":20,"amount":"10"}}"#,
            ),
            (
                Conversion::Merge {
                    outcome: 20,
                    amount: None,
                },
                r#"{"type":"userOutcome","mergeOutcome":{"outcome":20,"amount":null}}"#,
            ),
            (
                Conversion::MergeQuestion {
                    question: 3,
                    amount: Some(dec("2.5")),
                },
                r#"{"type":"userOutcome","mergeQuestion":{"question":3,"amount":"2.5"}}"#,
            ),
            (
                Conversion::Negate {
                    question: 3,
                    outcome: 20,
                    amount: dec("1"),
                },
                r#"{"type":"userOutcome","negateOutcome":{"question":3,"outcome":20,"amount":"1"}}"#,
            ),
        ];
        for (conversion, wire) in cases {
            assert_eq!(
                serde_json::to_string(&Action::UserOutcome(conversion.action())).unwrap(),
                wire
            );
        }
    }

    #[test]
    fn amounts_are_normalized_and_must_be_positive() {
        let args = OutcomeSplitArgs {
            outcome: 1,
            amount: dec("10.500"),
            yes: false,
        };
        assert_eq!(Conversion::split(&args).unwrap().amount_text(), "10.5");
        for bad in ["0", "-1"] {
            let args = OutcomeSplitArgs {
                outcome: 1,
                amount: dec(bad),
                yes: false,
            };
            assert!(matches!(
                Conversion::split(&args),
                Err(CliError::Configuration(_))
            ));
        }
    }

    #[test]
    fn preflight_names_quote_tokens_and_refuses_unknown_ids() {
        let catalog = catalog();
        let split = Conversion::Split {
            outcome: 12,
            amount: dec("1"),
        };
        assert_eq!(
            split
                .preflight(&catalog)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            ["USDH"]
        );

        let missing = Conversion::Split {
            outcome: 5,
            amount: dec("1"),
        };
        assert!(matches!(
            missing.preflight(&catalog),
            Err(CliError::Unsupported(_))
        ));

        let merge_question = Conversion::MergeQuestion {
            question: 7,
            amount: None,
        };
        assert_eq!(
            merge_question
                .preflight(&catalog)
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>(),
            ["USDC", "USDH"]
        );
        let no_question = Conversion::MergeQuestion {
            question: 8,
            amount: None,
        };
        assert!(no_question.preflight(&catalog).is_err());
    }

    #[test]
    fn negate_needs_an_outcome_of_that_question_fallback_included() {
        let catalog = catalog();
        let negate = |outcome| Conversion::Negate {
            question: 7,
            outcome,
            amount: dec("1"),
        };
        assert!(negate(11).preflight(&catalog).is_ok());
        assert!(negate(10).preflight(&catalog).is_ok());
        let err = negate(99).preflight(&catalog).unwrap_err();
        assert!(err.to_string().contains("not part of question 7"), "{err}");
    }
}
