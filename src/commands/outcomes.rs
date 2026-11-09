use clap::Args;

use crate::assets::asset_id::parse_outcome_notation;
use crate::assets::{
    Names, OutcomeCatalog, OutcomeQuestionRow, OutcomeSideRow, OutcomeTemplate, QuestionState,
    SettledOutcomeRow, load_outcome_templates, load_settled_outcome,
};
use crate::cli_command::CliCommand;
use crate::command_spec::{CommandSpec, GroupSpec, InputKind, NoArgs, arg};
use crate::commands::common::rows::{ListRow, One, Row, Rows, or_missing};
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::stream_bounds::parse_positive_usize;

#[derive(Args, Debug, Clone)]
pub struct OutcomeListArgs {
    /// Maximum number of outcome side rows to display
    #[arg(long, default_value = "100", value_parser = parse_positive_usize)]
    pub limit: usize,

    /// List the lowest outcome ids first instead of the newest
    #[arg(long)]
    pub oldest_first: bool,
}

#[derive(Args, Debug, Clone)]
pub struct OutcomeQuestionsArgs {
    /// Maximum number of questions to display
    #[arg(long, default_value = "100", value_parser = parse_positive_usize)]
    pub limit: usize,

    /// List the lowest question ids first instead of the newest
    #[arg(long)]
    pub oldest_first: bool,
}

#[derive(Args, Debug, Clone)]
pub struct OutcomeGetArgs {
    /// Outcome notation, for example #10 or +10
    pub notation: String,
}

#[derive(Args, Debug, Clone)]
pub struct OutcomeSettledArgs {
    /// Outcome id from `outcomes list`
    pub outcome: u64,
}

fn warnings_cell(warnings: &[String]) -> String {
    warnings.join("; ")
}

fn ids_cell(ids: &[u64]) -> String {
    ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",")
}

impl Row for OutcomeSideRow {
    const HEADERS: &'static [&'static str] = &[
        "Outcome",
        "Side",
        "Encoding",
        "Coin",
        "Token",
        "Asset ID",
        "Outcome Name",
        "Side Name",
        "Quote",
        "Venue",
        "Question",
        "Role",
        "Warnings",
    ];

    fn cells(&self) -> Vec<String> {
        let role = self.question_role.map(|role| {
            let role = role.as_str();
            if self.settled {
                format!("{role} (settled)")
            } else {
                role.to_string()
            }
        });
        vec![
            self.outcome.to_string(),
            self.side.to_string(),
            self.encoding.to_string(),
            self.coin.clone(),
            self.token.clone(),
            self.asset_id.to_string(),
            self.outcome_name.clone(),
            self.side_name.clone(),
            or_missing(self.quote_token.as_ref()),
            or_missing(self.venue.as_ref()),
            or_missing(self.question),
            or_missing(role),
            warnings_cell(&self.warnings),
        ]
    }
}

impl ListRow for OutcomeSideRow {
    const EMPTY: &'static str = "No results.";
}

impl Row for OutcomeQuestionRow {
    const HEADERS: &'static [&'static str] = &[
        "Question",
        "Name",
        "Fallback",
        "Named Outcomes",
        "Settled Named",
        "Warnings",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.question.to_string(),
            self.name.clone(),
            or_missing(self.fallback_outcome),
            ids_cell(&self.named_outcomes),
            ids_cell(&self.settled_named_outcomes),
            warnings_cell(&self.warnings),
        ]
    }
}

impl ListRow for OutcomeQuestionRow {
    const EMPTY: &'static str = "No results.";
}

impl Row for SettledOutcomeRow {
    const HEADERS: &'static [&'static str] = &[
        "Outcome",
        "Name",
        "Quote",
        "Settle Fraction",
        "Side Payouts",
        "Details",
        "Question",
        "Question State",
        "Warnings",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.outcome.to_string(),
            self.name.clone(),
            or_missing(self.quote_token.as_ref()),
            self.settle_fraction.to_string(),
            self.side_payouts
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
            self.details.clone(),
            or_missing(self.question),
            or_missing(self.question_state.map(QuestionState::as_str)),
            warnings_cell(&self.warnings),
        ]
    }
}

impl Row for OutcomeTemplate {
    const HEADERS: &'static [&'static str] =
        &["Id", "Role", "Parent", "Name", "Keywords", "Warnings"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.id.clone(),
            self.role.as_str().to_string(),
            or_missing(self.parent.as_ref()),
            self.name.clone(),
            self.keywords
                .iter()
                .map(|keyword| format!("{}:{}", keyword.keyword, keyword.kind))
                .collect::<Vec<_>>()
                .join(","),
            warnings_cell(&self.warnings),
        ]
    }
}

impl ListRow for OutcomeTemplate {
    const EMPTY: &'static str = "No results.";
}

pub async fn list(inv: &Invocation, args: &OutcomeListArgs) -> Result<(), anyhow::Error> {
    let mut rows = OutcomeCatalog::load(inv.remote()?.api(), Names::Resolved)
        .await?
        .sides;
    if !args.oldest_first {
        rows.sort_by_key(|row| (std::cmp::Reverse(row.outcome), row.side));
    }
    rows.truncate(args.limit);
    inv.print(&Rows::new(rows));
    Ok(())
}

pub async fn get(inv: &Invocation, args: &OutcomeGetArgs) -> Result<(), anyhow::Error> {
    let parsed = parse_outcome_notation(&args.notation)?;
    let row = OutcomeCatalog::load(inv.remote()?.api(), Names::Resolved)
        .await?
        .sides
        .into_iter()
        .find(|row| row.encoding == parsed.encoding)
        .ok_or_else(|| {
            CliError::Unsupported(format!(
                "outcome notation '{}' was not found in outcomeMeta",
                args.notation
            ))
        })?;
    inv.print(&One::new(row));
    Ok(())
}

pub const GROUP: GroupSpec =
    GroupSpec::new(&["outcomes"], "Browse and convert outcome markets").aliases(&["outcome"]);

pub struct OutcomesList;

impl CliCommand for OutcomesList {
    type Args = OutcomeListArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["outcomes", "list"],
        "market",
        "List active outcome market sides",
        "List active outcome market sides from outcomeMeta, newest outcome first (--oldest-first reverses). Template names are resolved; each row carries its quote token, venue, deployer fee scale, question (with its role: named or fallback) and any warnings. Use orders create with #<encoding> or +<encoding> notation for verified outcome limit orders.",
    )
    .cli_aliases(&["ls"])
    .args(&[
        arg("limit").describe("Maximum number of outcome side rows to return."),
        arg("oldest_first")
            .kind(InputKind::Boolean)
            .describe("List the lowest outcome ids first instead of the newest."),
    ]);

    async fn run(inv: &Invocation, args: &OutcomeListArgs) -> Result<(), anyhow::Error> {
        list(inv, args).await
    }
}

pub struct OutcomesGet;

impl CliCommand for OutcomesGet {
    type Args = OutcomeGetArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["outcomes", "get"],
        "market",
        "Get outcome side metadata by #N or +N notation",
        "Show outcome side metadata by #N coin notation or +N token notation, including its quote token, question and warnings.",
    )
    .args(&[arg("notation")]);

    fn validate(args: &OutcomeGetArgs) -> Result<(), CliError> {
        parse_outcome_notation(&args.notation)?;
        Ok(())
    }

    async fn run(inv: &Invocation, args: &OutcomeGetArgs) -> Result<(), anyhow::Error> {
        get(inv, args).await
    }
}

pub struct OutcomesQuestions;

impl CliCommand for OutcomesQuestions {
    type Args = OutcomeQuestionsArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["outcomes", "questions"],
        "market",
        "List outcome questions",
        "List questions from outcomeMeta, newest first. A question is a set of outcomes of which exactly one settles to Yes: named_outcomes, the protocol-created fallback_outcome, and settled_named_outcomes. Merge or negate them with `outcomes merge-question` and `outcomes negate`.",
    )
    .args(&[
        arg("limit").describe("Maximum number of questions to return."),
        arg("oldest_first")
            .kind(InputKind::Boolean)
            .describe("List the lowest question ids first instead of the newest."),
    ]);

    async fn run(inv: &Invocation, args: &OutcomeQuestionsArgs) -> Result<(), anyhow::Error> {
        let mut rows = OutcomeCatalog::load(inv.remote()?.api(), Names::Resolved)
            .await?
            .questions;
        if !args.oldest_first {
            rows.sort_by_key(|row| std::cmp::Reverse(row.question));
        }
        rows.truncate(args.limit);
        inv.print(&Rows::new(rows));
        Ok(())
    }
}

pub struct OutcomesSettled;

impl CliCommand for OutcomesSettled {
    type Args = OutcomeSettledArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["outcomes", "settled"],
        "market",
        "Show how an outcome settled",
        "Show how a settled outcome resolved: settle_fraction, what each side pays per share (settlement converts shares to quote tokens automatically, there is nothing to claim), the deployer's details and the question it belonged to. Fails for an outcome that has not settled or does not exist.",
    )
    .args(&[arg("outcome")
        .kind(InputKind::Identifier)
        .describe("Outcome id.")]);

    async fn run(inv: &Invocation, args: &OutcomeSettledArgs) -> Result<(), anyhow::Error> {
        let row = load_settled_outcome(inv.remote()?.api(), args.outcome)
            .await?
            .ok_or_else(|| {
                CliError::Unsupported(format!(
                    "outcome {} has not settled or does not exist: settledOutcome answered null; active outcomes are listed by `outcomes list`",
                    args.outcome
                ))
            })?;
        inv.print(&One::new(row));
        Ok(())
    }
}

pub struct OutcomesTemplates;

impl CliCommand for OutcomesTemplates {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["outcomes", "templates"],
        "market",
        "List outcome templates",
        "List the validator-approved outcome templates from outcomeTemplates: id, role (standalone_outcome, question, question_outcome), parent question template, side names, the {keyword} name and description text, and each keyword's type. Outcomes named template:<id> take their text from these.",
    );

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        let list = load_outcome_templates(inv.remote()?.api()).await?;
        inv.print(&Rows::new(list.templates));
        Ok(())
    }
}
