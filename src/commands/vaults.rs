//! Vault public queries and authenticated vault transfers.

use clap::{Args, ValueEnum};
use hypersdk::hypercore::Chain;
use hypersdk::hypercore::api::VaultTransfer;
use hypersdk::hypercore::types::Action;
use hypersdk::{Address, Decimal};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, arg};
use crate::commands::actions;
use crate::commands::positions::PositionRow;
use crate::commands::support::print_dry_run;
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::TableData;
use crate::quantity::{self, Scale};
use crate::wire::{InfoRequest, config_address};

const TRANSFER_REVERSIBILITY: ActionReversibility = ActionReversibility::PartiallyReversible;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VaultTransferActionKind {
    Deposit,
    Withdraw,
}

impl VaultTransferActionKind {
    fn is_deposit(self) -> bool {
        matches!(self, Self::Deposit)
    }

    fn action_name(self) -> &'static str {
        if self.is_deposit() {
            "deposit"
        } else {
            "withdraw"
        }
    }

    fn would_execute(self) -> &'static str {
        if self.is_deposit() {
            "deposit_usdc_to_vault"
        } else {
            "withdraw_usdc_from_vault"
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct VaultTransferArgs {
    /// Vault address
    #[arg(long)]
    pub vault: String,

    /// Amount of USDC to deposit or withdraw
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,
}

/// Arguments for `vault list`.
#[derive(Args, Debug, Clone)]
pub struct VaultListArgs {
    /// Filter by vault kind
    #[arg(long, value_enum, default_value = "all")]
    pub kind: VaultKindArg,

    /// Optional user address for user-specific deposit context
    #[arg(long)]
    pub user: Option<String>,

    /// Maximum number of vaults to return
    #[arg(long, default_value_t = 25, value_parser = clap::value_parser!(usize))]
    pub limit: usize,

    /// Sort field
    #[arg(long, value_enum, default_value = "tvl")]
    pub sort: VaultSortArg,
}

/// Arguments for `vault search`.
#[derive(Args, Debug, Clone)]
pub struct VaultSearchArgs {
    /// Search query matched against vault name, leader, or address
    pub query: String,

    /// Optional user address for user-specific deposit context
    #[arg(long)]
    pub user: Option<String>,

    /// Maximum number of vaults to return
    #[arg(long, default_value_t = 25, value_parser = clap::value_parser!(usize))]
    pub limit: usize,

    /// Sort field
    #[arg(long, value_enum, default_value = "tvl")]
    pub sort: VaultSortArg,
}

/// Vault relationship filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum VaultKindArg {
    All,
    Normal,
    Child,
    Parent,
    Protocol,
    User,
}

impl VaultKindArg {
    fn matches(self, kind: &str) -> bool {
        match self {
            Self::All => true,
            Self::Protocol => kind.eq_ignore_ascii_case("normal"),
            Self::User => kind.eq_ignore_ascii_case("child") || kind.eq_ignore_ascii_case("parent"),
            Self::Normal | Self::Child | Self::Parent => self
                .to_possible_value()
                .is_some_and(|value| value.get_name().eq_ignore_ascii_case(kind)),
        }
    }
}

pub struct VaultPositionsOutput {
    vault_address: String,
    rows: Vec<PositionRow>,
}

impl TableData for VaultPositionsOutput {
    fn headers(&self) -> Vec<&str> {
        std::iter::once("Vault")
            .chain(PositionRow::HEADERS)
            .collect()
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| self.with_vault(row.cells()))
            .collect()
    }

    fn pretty_rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| self.with_vault(row.pretty_cells()))
            .collect()
    }

    fn empty_message(&self) -> Option<&str> {
        Some("No open positions.")
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "vault_address": self.vault_address,
            "positions": self.rows,
        })
    }
}

impl VaultPositionsOutput {
    fn with_vault(&self, mut cells: Vec<String>) -> Vec<String> {
        cells.insert(0, self.vault_address.clone());
        cells
    }
}

/// Vault list sort field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum VaultSortArg {
    Tvl,
    Apr,
    Age,
    Name,
}

#[derive(Debug, Clone, Serialize)]
struct VaultDetailsRow {
    name: String,
    vault_address: String,
    leader: String,
    description: String,
    #[serde(with = "rust_decimal::serde::str")]
    tvl: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    apr: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    leader_fraction: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    leader_commission: Decimal,
    followers: usize,
    allow_deposits: bool,
    is_closed: bool,
    #[serde(with = "rust_decimal::serde::str")]
    max_distributable: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    max_withdrawable: Decimal,
    portfolio_periods: Vec<String>,
}

pub struct VaultDetailsOutput {
    row: VaultDetailsRow,
}

#[derive(Debug, Clone, Serialize)]
struct VaultSummaryRow {
    vault_address: String,
    name: String,
    leader: String,
    #[serde(with = "rust_decimal::serde::str")]
    apr: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    tvl: Decimal,
    age_days: Option<i64>,
    kind: String,
    #[serde(with = "rust_decimal::serde::str_option")]
    user_deposit: Option<Decimal>,
}

pub struct VaultSummaryOutput {
    rows: Vec<VaultSummaryRow>,
}

#[derive(Debug, Clone, Serialize)]
struct VaultTransferConfirmation {
    action: String,
    status: String,
    network: String,
    signer: String,
    acting_as: String,
    reversibility: String,
    vault_address: String,
    #[serde(with = "rust_decimal::serde::str")]
    amount: Decimal,
    asset: String,
}

struct VaultTransferOutput {
    row: VaultTransferConfirmation,
}

impl TableData for VaultDetailsOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Name",
            "Vault",
            "TVL",
            "APR",
            "Strategy",
            "Leader",
            "Followers",
            "Allow Deposits",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.row.name.clone(),
            self.row.vault_address.clone(),
            self.row.tvl.to_string(),
            self.row.apr.to_string(),
            self.row.description.clone(),
            self.row.leader.clone(),
            self.row.followers.to_string(),
            self.row.allow_deposits.to_string(),
        ]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.row).unwrap_or_else(|_| serde_json::json!({}))
    }
}

impl TableData for VaultSummaryOutput {
    fn headers(&self) -> Vec<&str> {
        let mut headers = vec!["Vault", "Name", "Leader", "APR", "TVL", "Age Days", "Kind"];
        if self.rows.iter().any(|row| row.user_deposit.is_some()) {
            headers.push("User Deposit");
        }
        headers
    }

    fn rows(&self) -> Vec<Vec<String>> {
        let show_user_deposit = self.rows.iter().any(|row| row.user_deposit.is_some());
        self.rows
            .iter()
            .map(|row| {
                let mut columns = vec![
                    row.vault_address.clone(),
                    row.name.clone(),
                    row.leader.clone(),
                    row.apr.to_string(),
                    row.tvl.to_string(),
                    row.age_days.map(|age| age.to_string()).unwrap_or_default(),
                    row.kind.clone(),
                ];
                if show_user_deposit {
                    columns.push(
                        row.user_deposit
                            .map(|deposit| deposit.to_string())
                            .unwrap_or_default(),
                    );
                }
                columns
            })
            .collect()
    }

    fn empty_message(&self) -> Option<&str> {
        Some("No vaults.")
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

impl TableData for VaultTransferOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Action",
            "Status",
            "Network",
            "Signer",
            "Acting As",
            "Reversibility",
            "Vault",
            "Amount",
            "Asset",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.row.action.clone(),
            self.row.status.clone(),
            self.row.network.clone(),
            self.row.signer.clone(),
            self.row.acting_as.clone(),
            self.row.reversibility.clone(),
            self.row.vault_address.clone(),
            self.row.amount.to_string(),
            self.row.asset.clone(),
        ]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.row).unwrap_or_else(|_| serde_json::json!({}))
    }
}

/// Get vault details.
async fn get(inv: &Invocation, address: &str) -> Result<(), anyhow::Error> {
    let vault = parse_address(address)?;
    let row = raw_vault_details_row(inv.remote()?.api(), vault).await?;
    inv.print(&VaultDetailsOutput { row });
    Ok(())
}

/// List vault summaries.
async fn list(inv: &Invocation, args: &VaultListArgs) -> Result<(), anyhow::Error> {
    let user = validate_list_args(args)?;
    let mut rows = vault_summary_rows(inv.remote()?.api(), user).await?;
    rows.retain(|row| args.kind.matches(&row.kind));
    sort_vault_rows(&mut rows, args.sort);
    rows.truncate(args.limit);
    inv.print(&VaultSummaryOutput { rows });
    Ok(())
}

/// Search vault summaries by name, leader, or address.
async fn search(inv: &Invocation, args: &VaultSearchArgs) -> Result<(), anyhow::Error> {
    let user = validate_search_args(args)?;
    let query = args.query.to_ascii_lowercase();
    let mut rows = vault_summary_rows(inv.remote()?.api(), user)
        .await?
        .into_iter()
        .filter(|row| {
            row.name.to_ascii_lowercase().contains(&query)
                || row.leader.to_ascii_lowercase().contains(&query)
                || row.vault_address.to_ascii_lowercase().contains(&query)
        })
        .collect::<Vec<_>>();
    sort_vault_rows(&mut rows, args.sort);
    rows.truncate(args.limit);
    inv.print(&VaultSummaryOutput { rows });
    Ok(())
}

/// List a vault's open positions.
async fn positions(inv: &Invocation, address: &str) -> Result<(), anyhow::Error> {
    let vault = parse_address(address)?;
    let positions = inv
        .remote()?
        .api()
        .sdk()
        .clearinghouse_state(vault, None)
        .await
        .map_err(from_sdk)?
        .asset_positions;

    inv.print(&VaultPositionsOutput {
        vault_address: address.to_string(),
        rows: PositionRow::open(positions),
    });
    Ok(())
}

fn transfer_dry_run_value(
    chain: Chain,
    args: &VaultTransferArgs,
    action_kind: VaultTransferActionKind,
    vault: Address,
    usd: u64,
) -> serde_json::Value {
    serde_json::json!({
        "action": action_kind.action_name(),
        "network": chain.to_string(),
        "vault": vault.to_string(),
        "amount": args.amount.to_string(),
        "asset": "USDC",
        "is_deposit": action_kind.is_deposit(),
        "usd": usd,
        "reversibility": TRANSFER_REVERSIBILITY,
        "verified_shape": {
            "type": "vaultTransfer",
            "vaultAddress": vault.to_string(),
            "isDeposit": action_kind.is_deposit(),
            "usd": usd
        }
    })
}

fn validate_list_args(args: &VaultListArgs) -> Result<Option<Address>, CliError> {
    if args.limit == 0 {
        return Err(CliError::Configuration(
            "vault list --limit must be greater than zero".to_string(),
        ));
    }
    parse_optional_user(args.user.as_deref())
}

fn validate_search_args(args: &VaultSearchArgs) -> Result<Option<Address>, CliError> {
    if args.query.trim().is_empty() {
        return Err(CliError::Configuration(
            "vault search query cannot be empty".to_string(),
        ));
    }
    if args.limit == 0 {
        return Err(CliError::Configuration(
            "vault search --limit must be greater than zero".to_string(),
        ));
    }
    parse_optional_user(args.user.as_deref())
}

/// Deposit USDC to a vault or withdraw it, previewing instead under `--dry-run`.
async fn run_transfer(
    inv: &Invocation,
    args: &VaultTransferArgs,
    action_kind: VaultTransferActionKind,
) -> Result<(), anyhow::Error> {
    let vault = parse_address(&args.vault)?;
    let usd = validate_usdc_amount(args.amount)?;
    if inv.dry_run() {
        return print_dry_run(
            inv,
            action_kind.would_execute(),
            transfer_dry_run_value(inv.remote()?.chain(), args, action_kind, vault, usd),
        );
    }
    let resolved = inv.resolve_signer()?;
    let signer = resolved.selected_signer();
    let remote = inv.remote()?;
    actions::send_l1_action(
        remote.api(),
        &signer,
        Action::VaultTransfer(VaultTransfer {
            vault_address: vault,
            is_deposit: action_kind.is_deposit(),
            usd,
        }),
        actions::nonce_now(),
    )
    .await?;

    inv.print(&VaultTransferOutput {
        row: VaultTransferConfirmation {
            action: action_kind.action_name().to_string(),
            status: "submitted".to_string(),
            network: remote.chain().to_string(),
            signer: signer.address().to_string(),
            acting_as: signer.query_address().to_string(),
            reversibility: TRANSFER_REVERSIBILITY.as_str().to_string(),
            vault_address: args.vault.clone(),
            amount: args.amount,
            asset: "USDC".to_string(),
        },
    });
    Ok(())
}

async fn raw_vault_details_row(
    api: &ApiClient,
    vault: Address,
) -> Result<VaultDetailsRow, CliError> {
    // The exchange answers `null` for an address that is not a vault.
    let details = api
        .info::<Option<RawVaultDetails>>(
            &InfoRequest::new("vaultDetails").address("vaultAddress", vault),
            "fetching vault details",
        )
        .await?
        .ok_or_else(|| CliError::Unsupported(format!("vault details not found for {vault}")))?;
    details.into_row(vault)
}

async fn vault_summary_rows(
    api: &ApiClient,
    user: Option<Address>,
) -> Result<Vec<VaultSummaryRow>, CliError> {
    let summaries: Vec<RawVaultSummary> = api
        .info(
            &InfoRequest::new("vaultSummaries").address_if("user", user),
            "fetching vault summaries",
        )
        .await?;
    summaries
        .into_iter()
        .map(RawVaultSummary::into_row)
        .collect()
}

fn parse_optional_user(raw: Option<&str>) -> Result<Option<Address>, CliError> {
    raw.map(parse_address).transpose()
}

fn parse_address(input: &str) -> Result<Address, CliError> {
    config_address(input, None, true)
}

fn validate_usdc_amount(amount: Decimal) -> Result<u64, CliError> {
    quantity::positive(amount, Scale::USDC).map_err(|err| err.config("amount", Scale::USDC))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawVaultSummary {
    name: String,
    vault_address: String,
    leader: String,
    tvl: Value,
    #[serde(default)]
    apr: Value,
    is_closed: bool,
    #[serde(default)]
    relationship: Value,
    #[serde(default)]
    create_time_millis: Option<i64>,
    #[serde(default, alias = "userDeposit", alias = "yourDeposit")]
    user_deposit: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawVaultDetails {
    name: String,
    #[serde(default)]
    vault_address: Option<String>,
    leader: String,
    description: String,
    #[serde(default)]
    portfolio: Value,
    apr: Value,
    leader_fraction: Value,
    leader_commission: Value,
    #[serde(default)]
    followers: Value,
    #[serde(default)]
    allow_deposits: bool,
    #[serde(default)]
    is_closed: bool,
    max_distributable: Value,
    max_withdrawable: Value,
}

impl RawVaultSummary {
    fn into_row(self) -> Result<VaultSummaryRow, CliError> {
        let kind = self
            .relationship
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(if self.is_closed { "closed" } else { "normal" })
            .to_string();
        Ok(VaultSummaryRow {
            vault_address: self.vault_address,
            name: self.name,
            leader: self.leader,
            apr: optional_decimal_field(&self.apr).unwrap_or(Decimal::ZERO),
            tvl: decimal_field(&self.tvl, "tvl")?,
            age_days: self.create_time_millis.map(age_days_from_millis),
            kind,
            user_deposit: self.user_deposit.as_ref().and_then(optional_decimal_field),
        })
    }
}

fn sort_vault_rows(rows: &mut [VaultSummaryRow], sort: VaultSortArg) {
    match sort {
        VaultSortArg::Tvl => rows.sort_by(|left, right| {
            right
                .tvl
                .cmp(&left.tvl)
                .then_with(|| left.name.cmp(&right.name))
        }),
        VaultSortArg::Apr => rows.sort_by(|left, right| {
            right
                .apr
                .cmp(&left.apr)
                .then_with(|| left.name.cmp(&right.name))
        }),
        VaultSortArg::Age => rows.sort_by(|left, right| {
            right
                .age_days
                .cmp(&left.age_days)
                .then_with(|| left.name.cmp(&right.name))
        }),
        VaultSortArg::Name => rows.sort_by(|left, right| left.name.cmp(&right.name)),
    }
}

fn optional_decimal_field(value: &Value) -> Option<Decimal> {
    match value {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => parse_decimal_text(text).ok(),
        Value::Number(number) => parse_decimal_text(&number.to_string()).ok(),
        _ => None,
    }
}

fn age_days_from_millis(create_time_millis: i64) -> i64 {
    let now = chrono::Utc::now().timestamp_millis();
    ((now - create_time_millis).max(0)) / 86_400_000
}

impl RawVaultDetails {
    fn into_row(self, requested_vault: Address) -> Result<VaultDetailsRow, CliError> {
        let max_distributable = decimal_field(&self.max_distributable, "maxDistributable")?;
        let (portfolio_periods, tvl) = raw_portfolio_summary(&self.portfolio);
        Ok(VaultDetailsRow {
            name: self.name,
            vault_address: self
                .vault_address
                .unwrap_or_else(|| requested_vault.to_string()),
            leader: self.leader,
            description: self.description,
            tvl: tvl.unwrap_or(max_distributable),
            apr: decimal_field(&self.apr, "apr")?,
            leader_fraction: decimal_field(&self.leader_fraction, "leaderFraction")?,
            leader_commission: decimal_field(&self.leader_commission, "leaderCommission")?,
            followers: self.followers.as_array().map_or(0, Vec::len),
            allow_deposits: self.allow_deposits,
            is_closed: self.is_closed,
            max_distributable,
            max_withdrawable: decimal_field(&self.max_withdrawable, "maxWithdrawable")?,
            portfolio_periods,
        })
    }
}

fn decimal_field(value: &Value, field: &'static str) -> Result<Decimal, CliError> {
    let raw = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        _ => {
            return Err(CliError::Internal(anyhow::anyhow!(
                "decode failed while reading {field}: expected string or number"
            )));
        }
    };
    parse_decimal_text(&raw).map_err(|err| {
        CliError::Internal(anyhow::anyhow!(
            "decode failed while reading {field}: {err}"
        ))
    })
}

fn parse_decimal_text(raw: &str) -> Result<Decimal, rust_decimal::Error> {
    raw.parse::<Decimal>()
        .or_else(|_| Decimal::from_scientific(raw))
}

fn raw_portfolio_summary(portfolio: &Value) -> (Vec<String>, Option<Decimal>) {
    let mut periods = Vec::new();
    let mut latest_tvl = None;
    let Some(entries) = portfolio.as_array() else {
        return (periods, latest_tvl);
    };

    for entry in entries {
        let Some(pair) = entry.as_array() else {
            continue;
        };
        let Some(period) = pair.first().and_then(Value::as_str) else {
            continue;
        };
        periods.push(period.to_string());

        let Some(history) = pair
            .get(1)
            .and_then(|details| details.get("accountValueHistory"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let Some(value) = history
            .last()
            .and_then(Value::as_array)
            .and_then(|point| point.get(1))
        else {
            continue;
        };
        if let Ok(decimal) = decimal_field(value, "accountValueHistory") {
            latest_tvl = Some(decimal);
        }
    }

    (periods, latest_tvl)
}

pub const GROUP: GroupSpec = GroupSpec::new(&["vault"], "Query and fund vaults")
    .aliases(&["vaults"])
    .schema_aliases();

#[derive(Args, Debug, Clone)]
pub struct VaultAddressArgs {
    /// Vault address
    pub address: String,
}

pub struct VaultList;

impl CliCommand for VaultList {
    type Args = VaultListArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["vault", "list"],
        "vault",
        "List vault summaries",
        "List vault summaries with filtering, sorting, and limits.",
    )
    .cli_aliases(&["ls"])
    .args(&[
        arg("kind").describe("Vault relationship kind filter. protocol maps to normal protocol vaults; user maps to child/parent user vaults."),
        arg("user").describe("Optional user address for user-specific deposit context when available."),
        arg("limit"),
        arg("sort"),
    ]);

    async fn run(inv: &Invocation, args: &VaultListArgs) -> Result<(), anyhow::Error> {
        list(inv, args).await
    }
}

pub struct VaultSearch;

impl CliCommand for VaultSearch {
    type Args = VaultSearchArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["vault", "search"],
        "vault",
        "Search vault summaries by name, leader, or address",
        "Search vault summaries by name, leader, or address.",
    )
    .args(&[
        arg("query").describe("Search query."),
        arg("user")
            .describe("Optional user address for user-specific deposit context when available."),
        arg("limit"),
        arg("sort"),
    ]);

    async fn run(inv: &Invocation, args: &VaultSearchArgs) -> Result<(), anyhow::Error> {
        search(inv, args).await
    }
}

pub struct VaultGet;

impl CliCommand for VaultGet {
    type Args = VaultAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["vault", "get"],
        "vault",
        "Get vault details",
        "Show vault details.",
    )
    .args(&[arg("address")
        .kind(InputKind::ProtocolObjectAddress)
        .resource_id("vault address")]);

    async fn run(inv: &Invocation, args: &VaultAddressArgs) -> Result<(), anyhow::Error> {
        get(inv, &args.address).await
    }
}

pub struct VaultPositions;

impl CliCommand for VaultPositions {
    type Args = VaultAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["vault", "positions"],
        "vault",
        "List vault positions",
        "List vault positions.",
    )
    .args(&[arg("address")
        .kind(InputKind::ProtocolObjectAddress)
        .resource_id("vault address")]);

    async fn run(inv: &Invocation, args: &VaultAddressArgs) -> Result<(), anyhow::Error> {
        positions(inv, &args.address).await
    }
}

pub struct VaultDeposit;

impl CliCommand for VaultDeposit {
    type Args = VaultTransferArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["vault", "deposit"],
        "vault",
        "Deposit to a vault",
        "Deposit USDC to a vault.",
    )
    .args(&[
        arg("vault")
            .kind(InputKind::ProtocolObjectAddress)
            .resource_id("vault address"),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("USDC amount."),
    ]);

    async fn run(inv: &Invocation, args: &VaultTransferArgs) -> Result<(), anyhow::Error> {
        run_transfer(inv, args, VaultTransferActionKind::Deposit).await
    }
}

pub struct VaultWithdraw;

impl CliCommand for VaultWithdraw {
    type Args = VaultTransferArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["vault", "withdraw"],
        "vault",
        "Withdraw from a vault",
        "Withdraw USDC from a vault.",
    )
    .args(&[
        arg("vault")
            .kind(InputKind::ProtocolObjectAddress)
            .resource_id("vault address"),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("USDC amount."),
    ]);

    async fn run(inv: &Invocation, args: &VaultTransferArgs) -> Result<(), anyhow::Error> {
        run_transfer(inv, args, VaultTransferActionKind::Withdraw).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usdc_amount_validation_rejects_zero_negative_and_over_precise_amounts() {
        assert!(validate_usdc_amount(Decimal::ZERO).is_err());
        assert!(validate_usdc_amount("-1".parse::<Decimal>().unwrap()).is_err());
        assert!(validate_usdc_amount("0.0000001".parse::<Decimal>().unwrap()).is_err());
        assert_eq!(
            validate_usdc_amount("1.5".parse::<Decimal>().unwrap()).unwrap(),
            1_500_000
        );
    }

    #[test]
    fn parse_address_rejects_bad_and_zero_addresses() {
        assert_eq!(parse_address("INVALID").unwrap_err().exit_code(), 2);
        assert_eq!(
            parse_address("0x0000000000000000000000000000000000000000")
                .unwrap_err()
                .exit_code(),
            2
        );
    }

    #[test]
    fn kind_filter_maps_protocol_and_user_to_relationship_kinds() {
        assert!(VaultKindArg::All.matches("anything"));
        assert!(VaultKindArg::Protocol.matches("normal"));
        assert!(VaultKindArg::User.matches("child"));
        assert!(VaultKindArg::User.matches("parent"));
        assert!(VaultKindArg::Child.matches("CHILD"));
        assert!(!VaultKindArg::Parent.matches("child"));
    }
}
