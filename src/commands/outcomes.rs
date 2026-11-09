use clap::Args;

use crate::assets::asset_id::parse_outcome_notation;
use crate::assets::{OutcomeSideRow, load_outcome_sides};
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, arg};
use crate::commands::rows::{ListRow, One, Row, Rows};
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::stream::parse_positive_usize;

#[derive(Args, Debug, Clone)]
pub struct OutcomeListArgs {
    /// Maximum number of outcome side rows to display
    #[arg(long, default_value = "100", value_parser = parse_positive_usize)]
    pub limit: usize,
}

#[derive(Args, Debug, Clone)]
pub struct OutcomeGetArgs {
    /// Outcome notation, for example #10 or +10
    pub notation: String,
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
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.outcome.to_string(),
            self.side.to_string(),
            self.encoding.to_string(),
            self.coin.clone(),
            self.token.clone(),
            self.asset_id.to_string(),
            self.outcome_name.clone(),
            self.side_name.clone(),
        ]
    }
}

impl ListRow for OutcomeSideRow {
    const EMPTY: &'static str = "No results.";
}

pub async fn list(inv: &Invocation, args: &OutcomeListArgs) -> Result<(), anyhow::Error> {
    let mut rows = load_outcome_sides(inv.remote()?.api()).await?;
    rows.truncate(args.limit);
    inv.print(&Rows::new(rows));
    Ok(())
}

pub async fn get(inv: &Invocation, args: &OutcomeGetArgs) -> Result<(), anyhow::Error> {
    let parsed = parse_outcome_notation(&args.notation)?;
    let row = load_outcome_sides(inv.remote()?.api())
        .await?
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
    GroupSpec::new(&["outcomes"], "Browse outcome markets").aliases(&["outcome"]);

pub struct OutcomesList;

impl CliCommand for OutcomesList {
    type Args = OutcomeListArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["outcomes", "list"],
        "market",
        "List active outcome market sides",
        "List active outcome market sides from outcomeMeta. Use orders create with #<encoding> or +<encoding> notation for verified outcome limit orders.",
    )
    .cli_aliases(&["ls"])
    .args(&[
        arg("limit").describe("Maximum number of outcome side rows to return."),
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
        "Show outcome side metadata by #N coin notation or +N token notation.",
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
