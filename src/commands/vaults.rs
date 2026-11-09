use clap::{Args, ValueEnum};
use hypersdk::hypercore::Chain;
use hypersdk::hypercore::api::VaultTransfer;
use hypersdk::hypercore::types::Action;
use hypersdk::{Address, Decimal};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::clock;
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, arg};
use crate::commands::actions;
use crate::commands::positions::PositionRow;
use crate::commands::rows::{One, Row, or_missing};
use crate::commands::support::print_dry_run;
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::{self, TableData};
use crate::quantity;
use crate::wire::{AddressRole, InfoRequest, address};
use crate::wire_decimal;

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

    /// Show this user's deposit in each vault they hold
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

    /// Show this user's deposit in each vault they hold
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
    fn matches(self, kind: &VaultKind) -> bool {
        match self {
            Self::All => true,
            Self::Protocol | Self::Normal => *kind == VaultKind::Normal,
            Self::User => matches!(kind, VaultKind::Child | VaultKind::Parent),
            Self::Child => *kind == VaultKind::Child,
            Self::Parent => *kind == VaultKind::Parent,
        }
    }
}

/// How a vault relates to others, as `vault list` reports it: the vault's
/// `relationship.type`, or `closed`/`normal` for a vault that states none.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VaultKind {
    Normal,
    Child,
    Parent,
    Closed,
    /// A relationship type this CLI does not know, as the exchange spells it.
    Other(String),
}

impl VaultKind {
    fn parse(raw: &str) -> Self {
        match raw.to_ascii_lowercase().as_str() {
            "normal" => Self::Normal,
            "child" => Self::Child,
            "parent" => Self::Parent,
            "closed" => Self::Closed,
            _ => Self::Other(raw.to_string()),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Normal => "normal",
            Self::Child => "child",
            Self::Parent => "parent",
            Self::Closed => "closed",
            Self::Other(raw) => raw,
        }
    }
}

impl Serialize for VaultKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

pub struct VaultPositionsOutput {
    vault_address: String,
    rows: Vec<PositionRow>,
}

impl TableData for VaultPositionsOutput {
    fn headers(&self) -> Vec<&str> {
        std::iter::once("Vault")
            .chain(PositionRow::HEADERS.iter().copied())
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
    /// How many followers the response listed. The API lists at most 100, so
    /// for a larger vault this is not its follower count.
    followers_returned: usize,
    /// The list reached the API's cap: the vault may have more followers.
    followers_truncated: bool,
    /// The requested `--user`'s stake: `null` when they hold none, absent
    /// without `--user`.
    #[serde(skip_serializing_if = "Option::is_none")]
    follower_state: Option<Option<FollowerStateRow>>,
    allow_deposits: bool,
    is_closed: bool,
    #[serde(with = "rust_decimal::serde::str")]
    max_distributable: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    max_withdrawable: Decimal,
    portfolio_periods: Vec<String>,
}

/// `vaultDetails` lists at most this many followers (observed on the live
/// exchange for a vault with thousands).
const FOLLOWERS_LIST_CAP: usize = 100;

/// One user's stake in a vault.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all(deserialize = "camelCase", serialize = "snake_case"))]
struct FollowerStateRow {
    #[serde(with = "rust_decimal::serde::str")]
    vault_equity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pnl: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    all_time_pnl: Decimal,
    days_following: u64,
    vault_entry_time: u64,
    #[serde(default)]
    lockup_until: Option<u64>,
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
    kind: VaultKind,
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

impl Row for VaultDetailsRow {
    const HEADERS: &'static [&'static str] = &[
        "Name",
        "Vault",
        "TVL",
        "APR",
        "Strategy",
        "Leader",
        "Followers Returned",
        "Allow Deposits",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.vault_address.clone(),
            self.tvl.to_string(),
            self.apr.to_string(),
            self.description.clone(),
            self.leader.clone(),
            if self.followers_truncated {
                format!("{}+", self.followers_returned)
            } else {
                self.followers_returned.to_string()
            },
            self.allow_deposits.to_string(),
        ]
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
                    or_missing(row.age_days),
                    row.kind.as_str().to_string(),
                ];
                if show_user_deposit {
                    columns.push(or_missing(row.user_deposit));
                }
                columns
            })
            .collect()
    }

    fn empty_message(&self) -> Option<&str> {
        Some("No vaults.")
    }

    fn to_json_value(&self) -> serde_json::Value {
        output::serialize_row(&self.rows)
    }
}

impl Row for VaultTransferConfirmation {
    const HEADERS: &'static [&'static str] = &[
        "Action",
        "Status",
        "Network",
        "Signer",
        "Acting As",
        "Reversibility",
        "Vault",
        "Amount",
        "Asset",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.action.clone(),
            self.status.clone(),
            self.network.clone(),
            self.signer.clone(),
            self.acting_as.clone(),
            self.reversibility.clone(),
            self.vault_address.clone(),
            self.amount.to_string(),
            self.asset.clone(),
        ]
    }
}

async fn get(inv: &Invocation, args: &VaultGetArgs) -> Result<(), anyhow::Error> {
    let vault = parse_address(&args.address)?;
    let user = match args.user.as_deref() {
        Some(selector) => Some(inv.resolve_user(Some(selector)).await?),
        None => None,
    };
    let row = raw_vault_details_row(inv.remote()?.api(), vault, user).await?;
    inv.print(&One::new(row));
    Ok(())
}

async fn list(inv: &Invocation, args: &VaultListArgs) -> Result<(), anyhow::Error> {
    let user = validate_list_args(args)?;
    let mut rows = vault_summary_rows(inv.remote()?.api(), user).await?;
    rows.retain(|row| args.kind.matches(&row.kind));
    sort_vault_rows(&mut rows, args.sort);
    rows.truncate(args.limit);
    inv.print(&VaultSummaryOutput { rows });
    Ok(())
}

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
    let usd: u64 = quantity::usdc_units(args.amount)?;
    if inv.dry_run() {
        return print_dry_run(
            inv,
            action_kind.would_execute(),
            transfer_dry_run_value(inv.remote()?.chain(), args, action_kind, vault, usd),
        );
    }
    let signer = inv.resolve_signer()?;
    let remote = inv.remote()?;
    actions::send_default_l1_action(
        remote.api(),
        &signer,
        Action::VaultTransfer(VaultTransfer {
            vault_address: vault,
            is_deposit: action_kind.is_deposit(),
            usd,
        }),
    )
    .await?;

    inv.print(&One::new(VaultTransferConfirmation {
        action: action_kind.action_name().to_string(),
        status: "submitted".to_string(),
        network: remote.chain().to_string(),
        signer: signer.address().to_string(),
        acting_as: signer.query_address().to_string(),
        reversibility: TRANSFER_REVERSIBILITY.as_str().to_string(),
        vault_address: args.vault.clone(),
        amount: args.amount,
        asset: "USDC".to_string(),
    }));
    Ok(())
}

async fn raw_vault_details_row(
    api: &ApiClient,
    vault: Address,
    user: Option<Address>,
) -> Result<VaultDetailsRow, CliError> {
    // The exchange answers `null` for an address that is not a vault.
    let details = api
        .info::<Option<RawVaultDetails>>(
            &InfoRequest::new("vaultDetails")
                .address("vaultAddress", vault)
                .address_if("user", user),
            "fetching vault details",
        )
        .await?
        .ok_or_else(|| CliError::Unsupported(format!("vault details not found for {vault}")))?;
    details.into_row(vault, user.is_some())
}

/// Every vault the exchange lists, from its public stats dataset.
///
/// `/info` `vaultSummaries` answers `[]` on both networks, so the list comes
/// from the public stats dataset the exchange's own vault page reads. With a
/// `user`, each vault row carries that user's deposit, read from their vault
/// equities.
async fn vault_summary_rows(
    api: &ApiClient,
    user: Option<Address>,
) -> Result<Vec<VaultSummaryRow>, CliError> {
    let vaults: Vec<StatsVault> = api.stats("vaults", "loading the vault list").await?;
    let deposits = match user {
        Some(user) => user_vault_deposits(api, user).await?,
        None => Vec::new(),
    };
    vaults
        .into_iter()
        .map(|vault| {
            let deposit = deposits
                .iter()
                .find(|(address, _)| address.eq_ignore_ascii_case(&vault.summary.vault_address))
                .map(|(_, equity)| *equity);
            vault.into_row(deposit)
        })
        .collect()
}

/// `(vault address, equity)` of each vault `user` has money in.
async fn user_vault_deposits(
    api: &ApiClient,
    user: Address,
) -> Result<Vec<(String, Decimal)>, CliError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Equity {
        vault_address: String,
        equity: Value,
    }

    api.info::<Vec<Equity>>(
        &InfoRequest::user("userVaultEquities", user),
        "loading the user's vault equities",
    )
    .await?
    .into_iter()
    .map(|entry| Ok((entry.vault_address, decimal_field(&entry.equity, "equity")?)))
    .collect()
}

fn parse_optional_user(raw: Option<&str>) -> Result<Option<Address>, CliError> {
    raw.map(parse_address).transpose()
}

fn parse_address(input: &str) -> Result<Address, CliError> {
    address(input, AddressRole::Explicit(None))
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
    #[serde(default)]
    follower_state: Option<FollowerStateRow>,
}

/// One entry of the stats dataset: the summary and, beside it, the APR.
#[derive(Debug, Deserialize)]
struct StatsVault {
    #[serde(default)]
    apr: Value,
    summary: RawVaultSummary,
}

impl StatsVault {
    fn into_row(mut self, user_deposit: Option<Decimal>) -> Result<VaultSummaryRow, CliError> {
        if self.summary.apr.is_null() {
            self.summary.apr = self.apr;
        }
        self.summary.into_row(user_deposit)
    }
}

impl RawVaultSummary {
    fn into_row(self, user_deposit: Option<Decimal>) -> Result<VaultSummaryRow, CliError> {
        let kind = self
            .relationship
            .get("type")
            .and_then(Value::as_str)
            .map_or_else(
                || {
                    if self.is_closed {
                        VaultKind::Closed
                    } else {
                        VaultKind::Normal
                    }
                },
                VaultKind::parse,
            );
        Ok(VaultSummaryRow {
            vault_address: self.vault_address,
            name: self.name,
            leader: self.leader,
            apr: optional_decimal_field(&self.apr, "apr")?.unwrap_or(Decimal::ZERO),
            tvl: decimal_field(&self.tvl, "tvl")?,
            age_days: self.create_time_millis.map(age_days_from_millis),
            kind,
            user_deposit,
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

/// A decimal that may be absent: `null` and the empty string are absent. Any
/// other value must decode, or the whole response fails.
fn optional_decimal_field(value: &Value, field: &'static str) -> Result<Option<Decimal>, CliError> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) if text.is_empty() => Ok(None),
        other => decimal_field(other, field).map(Some),
    }
}

fn age_days_from_millis(create_time_millis: i64) -> i64 {
    let now = i64::try_from(clock::now_ms()).unwrap_or(i64::MAX);
    ((now - create_time_millis).max(0)) / 86_400_000
}

impl RawVaultDetails {
    fn into_row(
        self,
        requested_vault: Address,
        user_requested: bool,
    ) -> Result<VaultDetailsRow, CliError> {
        let max_distributable = decimal_field(&self.max_distributable, "maxDistributable")?;
        let (portfolio_periods, tvl) = raw_portfolio_summary(&self.portfolio)?;
        let followers_returned = self.followers.as_array().map_or(0, Vec::len);
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
            followers_returned,
            followers_truncated: followers_returned >= FOLLOWERS_LIST_CAP,
            follower_state: user_requested.then_some(self.follower_state),
            allow_deposits: self.allow_deposits,
            is_closed: self.is_closed,
            max_distributable,
            max_withdrawable: decimal_field(&self.max_withdrawable, "maxWithdrawable")?,
            portfolio_periods,
        })
    }
}

fn decimal_field(value: &Value, field: &'static str) -> Result<Decimal, CliError> {
    let failed = |reason: &dyn std::fmt::Display| {
        CliError::Internal(anyhow::anyhow!(
            "decode failed while reading {field}: {reason}"
        ))
    };
    if !matches!(value, Value::String(_) | Value::Number(_)) {
        return Err(failed(&"expected string or number"));
    }
    wire_decimal::from_value(value).map_err(|err| failed(&err))
}

fn raw_portfolio_summary(portfolio: &Value) -> Result<(Vec<String>, Option<Decimal>), CliError> {
    let mut periods = Vec::new();
    let mut latest_tvl = None;
    let Some(entries) = portfolio.as_array() else {
        return Ok((periods, latest_tvl));
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
        latest_tvl = Some(decimal_field(value, "accountValueHistory")?);
    }

    Ok((periods, latest_tvl))
}

pub const GROUP: GroupSpec = GroupSpec::new(&["vault"], "Query and fund vaults")
    .aliases(&["vaults"])
    .schema_aliases();

#[derive(Args, Debug, Clone)]
pub struct VaultAddressArgs {
    /// Vault address
    pub address: String,
}

#[derive(Args, Debug, Clone)]
pub struct VaultGetArgs {
    /// Vault address
    pub address: String,

    /// Also show this user's stake in the vault
    #[arg(long)]
    pub user: Option<String>,
}

pub struct VaultList;

impl CliCommand for VaultList {
    type Args = VaultListArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["vault", "list"],
        "vault",
        "List vault summaries",
        "List vaults from the exchange's public stats dataset (stats-data.hyperliquid.xyz; /info vaultSummaries answers nothing on either network), with filtering, sorting, and limits.",
    )
    .cli_aliases(&["ls"])
    .args(&[
        arg("kind").describe("Vault relationship kind filter. protocol maps to normal protocol vaults; user maps to child/parent user vaults."),
        arg("user").describe("Optional user address; adds that user's deposit to each vault they hold."),
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
        "Search vaults by name, leader, or address in the exchange's public stats dataset (stats-data.hyperliquid.xyz).",
    )
    .args(&[
        arg("query").describe("Search query."),
        arg("user")
            .describe("Optional user address; adds that user's deposit to each vault they hold."),
        arg("limit"),
        arg("sort"),
    ]);

    async fn run(inv: &Invocation, args: &VaultSearchArgs) -> Result<(), anyhow::Error> {
        search(inv, args).await
    }
}

pub struct VaultGet;

impl CliCommand for VaultGet {
    type Args = VaultGetArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["vault", "get"],
        "vault",
        "Get vault details",
        "Show vault details. followers_returned counts the followers the API listed (at most 100; followers_truncated is true when the API returned that limit, so the list may be incomplete); user adds that user's stake as follower_state.",
    )
    .args(&[
        arg("address")
            .kind(InputKind::ProtocolObjectAddress)
            .resource_id("vault address"),
        arg("user")
            .kind(InputKind::PublicUserSelector)
            .describe("Optional user address; adds follower_state with that user's stake."),
    ]);

    async fn run(inv: &Invocation, args: &VaultGetArgs) -> Result<(), anyhow::Error> {
        get(inv, args).await
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
        let usd = |amount: &str| quantity::usdc_units::<u64>(amount.parse().unwrap());
        assert!(usd("0").is_err());
        assert!(usd("-1").is_err());
        assert!(usd("0.0000001").is_err());
        assert_eq!(usd("1.5").unwrap(), 1_500_000);
    }

    #[test]
    fn a_wire_decimal_reads_strings_numbers_and_scientific_notation_and_names_the_field() {
        let read =
            |value: serde_json::Value| decimal_field(&value, "apr").map_err(|e| e.to_string());
        assert_eq!(read(serde_json::json!("1.5")), Ok(Decimal::new(15, 1)));
        assert_eq!(read(serde_json::json!(2)), Ok(Decimal::from(2)));
        assert_eq!(read(serde_json::json!("1e-7")), Ok(Decimal::new(1, 7)));
        assert!(
            read(serde_json::json!(null))
                .unwrap_err()
                .ends_with("decode failed while reading apr: expected string or number")
        );
        assert!(
            read(serde_json::json!("abc"))
                .unwrap_err()
                .contains("decode failed while reading apr: ")
        );
    }
}
