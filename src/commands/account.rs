use alloy::sol;
use chrono::DateTime;
use clap::{Args, ValueEnum};
use hypersdk::Address;
use hypersdk::Decimal;
use hypersdk::hypercore::{ClearinghouseState, Fill, SubAccount, UserVaultEquity};
use serde::Serialize;

use crate::command_spec::{ArgSpec, CliCommand, CommandSpec, GroupSpec, InputKind, Risk, arg};
use crate::commands::orders::OrderListOutput;
use crate::commands::positions::{self, pretty_pnl as format_pretty_pnl};
use crate::commands::spot_balances::{RawSpotBalance, user_spot_balances_raw};
use crate::commands::support::{canonical_dex, print_dry_run, require_master_signer};
use crate::commands::user_signed::{self, UserSigned};
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::mode::Mode;
use crate::output::{JsonValueOutput, TableData};
use crate::signing::{SelectedSigner, Signs};
use crate::wire::InfoRequest;

sol! {
    #[derive(Serialize)]
    struct UserSetAbstraction {
        string hyperliquidChain;
        address user;
        string abstraction;
        uint64 nonce;
    }
}

impl UserSigned for UserSetAbstraction {
    const ACTION_TYPE: &'static str = "userSetAbstraction";
}

/// The account selector every public read command takes first, as the schema
/// describes it. Reads that need an address mark it `.schema_required()`.
pub const USER_SELECTOR: ArgSpec = arg("address")
    .kind(InputKind::PublicUserSelector)
    .describe("Ethereum address, wallet name, or wallet id.")
    .resource_id("account selector");

/// An optional account selector: an address, wallet name or wallet id.
#[derive(Args, Debug, Clone)]
pub struct OptionalAddressArgs {
    /// Ethereum address, wallet name, or wallet id.
    /// Defaults to the selected/default signer when omitted.
    pub address: Option<String>,
}

impl OptionalAddressArgs {
    /// The address this selector names, or the selected signer's.
    pub fn resolve(&self, inv: &Invocation) -> Result<Address, anyhow::Error> {
        inv.resolve_user(self.address.as_deref())
    }
}

#[derive(Args, Debug, Clone)]
pub struct FillsArgs {
    #[command(flatten)]
    pub user: OptionalAddressArgs,
    /// Start time as RFC3339 or epoch milliseconds. Uses userFillsByTime when present.
    #[arg(long, value_parser = parse_time_millis)]
    pub start: Option<u64>,
    /// End time as RFC3339 or epoch milliseconds.
    #[arg(long, requires = "start", value_parser = parse_time_millis)]
    pub end: Option<u64>,
    /// Combine partial fills from the same crossing order where supported by the API.
    #[arg(long, requires = "start")]
    pub aggregate_by_time: bool,
}

#[derive(Args, Debug, Clone)]
pub struct TimeRangeArgs {
    #[command(flatten)]
    pub user: OptionalAddressArgs,
    /// Start time as RFC3339 or epoch milliseconds.
    #[arg(long, value_parser = parse_time_millis)]
    pub start: u64,
    /// End time as RFC3339 or epoch milliseconds.
    #[arg(long, value_parser = parse_time_millis)]
    pub end: Option<u64>,
}

#[derive(Args, Debug, Clone)]
pub struct TwapFillsArgs {
    #[command(flatten)]
    pub user: OptionalAddressArgs,
    /// Start time as RFC3339 or epoch milliseconds. Uses userTwapSliceFillsByTime when present.
    #[arg(long, value_parser = parse_time_millis)]
    pub start: Option<u64>,
    /// End time as RFC3339 or epoch milliseconds.
    #[arg(long, requires = "start", value_parser = parse_time_millis)]
    pub end: Option<u64>,
    /// Combine partial fills where supported by the API.
    #[arg(long, requires = "start")]
    pub aggregate_by_time: bool,
}

#[derive(Args, Debug, Clone)]
pub struct PortfolioArgs {
    #[command(flatten)]
    pub user: OptionalAddressArgs,
    /// Perp dex name (HIP-3). Defaults to the first dex.
    #[arg(long)]
    pub dex: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct SetAbstractionArgs {
    /// Abstraction mode to set
    #[arg(long, value_enum)]
    pub mode: AbstractionModeArg,

    /// Confirm live execution without prompting
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AbstractionModeArg {
    Disabled,
    UnifiedAccount,
    PortfolioMargin,
}

impl AbstractionModeArg {
    fn protocol_value(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::UnifiedAccount => "unifiedAccount",
            Self::PortfolioMargin => "portfolioMargin",
        }
    }

    fn cli_value(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::UnifiedAccount => "unified-account",
            Self::PortfolioMargin => "portfolio-margin",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AbstractionRow {
    pub user: String,
    pub raw_mode: String,
    pub normalized_mode: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbstractionOutput {
    pub row: AbstractionRow,
}

#[derive(Debug, Clone, Serialize)]
struct AbstractionSetRow {
    status: String,
    action: String,
    signer: String,
    user: String,
    mode: String,
    protocol_mode: String,
    network: String,
}

struct AbstractionSetOutput {
    row: AbstractionSetRow,
}

pub fn parse_time_millis(raw: &str) -> Result<u64, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("time value cannot be empty".to_string());
    }

    if trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return trimmed
            .parse::<u64>()
            .map_err(|err| format!("invalid epoch millisecond timestamp: {err}"));
    }

    let parsed = DateTime::parse_from_rfc3339(trimmed)
        .map_err(|err| format!("invalid RFC3339 or epoch millisecond timestamp: {err}"))?;
    let millis = parsed.timestamp_millis();
    if millis < 0 {
        return Err("time must be at or after the Unix epoch".to_string());
    }
    Ok(millis as u64)
}

impl TableData for AbstractionOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["User", "Mode", "Raw Mode"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.row.user.clone(),
            self.row.normalized_mode.clone(),
            self.row.raw_mode.clone(),
        ]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.row).unwrap_or_else(|_| serde_json::json!({}))
    }
}

impl TableData for AbstractionSetOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Status", "Action", "Signer", "User", "Mode", "Network"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.row.status.clone(),
            self.row.action.clone(),
            self.row.signer.clone(),
            self.row.user.clone(),
            self.row.mode.clone(),
            self.row.network.clone(),
        ]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.row).unwrap_or_else(|_| serde_json::json!({}))
    }
}

pub(crate) async fn subaccounts(inv: &Invocation, user: Address) -> Result<(), anyhow::Error> {
    // The exchange answers `null` for an account without subaccounts.
    let subaccounts = inv
        .remote()?
        .api()
        .info::<Option<Vec<SubAccount>>>(
            &InfoRequest::user("subAccounts", user),
            "loading subaccounts",
        )
        .await?
        .unwrap_or_default()
        .into_iter()
        .map(SubaccountRow::from)
        .collect();
    inv.print(&SubaccountsOutput::new(subaccounts));
    Ok(())
}

pub async fn fetch_abstraction(
    api: &ApiClient,
    user: Address,
) -> Result<AbstractionRow, anyhow::Error> {
    let raw_mode = api
        .info::<String>(
            &InfoRequest::user("userAbstraction", user),
            "loading user abstraction",
        )
        .await?;
    Ok(AbstractionRow {
        user: user.to_string(),
        normalized_mode: normalize_abstraction_mode(&raw_mode).to_string(),
        raw_mode,
    })
}

fn abstraction_set_dry_run_value(
    chain: hypersdk::hypercore::Chain,
    signer: Option<Address>,
    mode: AbstractionModeArg,
) -> serde_json::Value {
    let user = signer.map(|address| address.to_string());
    serde_json::json!({
        "mode": mode.cli_value(),
        "protocol_mode": mode.protocol_value(),
        "network": chain.to_string(),
        "user": user,
        "action": {
            "type": UserSetAbstraction::ACTION_TYPE,
            "hyperliquidChain": chain.to_string(),
            "signatureChainId": chain.arbitrum_id().to_string(),
            "user": user,
            "abstraction": mode.protocol_value(),
        }
    })
}

async fn set_abstraction(
    inv: &Invocation,
    signer: &SelectedSigner,
    args: &SetAbstractionArgs,
) -> Result<(), anyhow::Error> {
    let user = signer.address();
    if !args.yes {
        confirm_account_mode_change(inv.mode(), args.mode, user)?;
    }
    let remote = inv.remote()?;
    let protocol_mode = args.mode.protocol_value();
    user_signed::submit(remote.api(), signer, |hyperliquid_chain, nonce| {
        UserSetAbstraction {
            hyperliquidChain: hyperliquid_chain,
            user,
            abstraction: protocol_mode.to_string(),
            nonce,
        }
    })
    .await?;

    inv.print(&AbstractionSetOutput {
        row: AbstractionSetRow {
            status: "submitted".to_string(),
            action: "set-abstraction".to_string(),
            signer: user.to_string(),
            user: user.to_string(),
            mode: args.mode.cli_value().to_string(),
            protocol_mode: protocol_mode.to_string(),
            network: remote.chain().to_string(),
        },
    });
    Ok(())
}

fn normalize_abstraction_mode(raw: &str) -> &'static str {
    match raw {
        "unifiedAccount" => "unified-account",
        "portfolioMargin" => "portfolio-margin",
        "disabled" => "disabled",
        "default" => "default",
        "dexAbstraction" => "dex-abstraction",
        _ => "unknown",
    }
}

fn confirm_account_mode_change(
    prompt_mode: &Mode,
    mode: AbstractionModeArg,
    user: Address,
) -> Result<(), CliError> {
    let question = format!(
        "Set account abstraction for {user} to {}?",
        mode.cli_value()
    );
    crate::prompt::consent(
        prompt_mode,
        "--yes",
        "",
        &question,
        "account abstraction update aborted",
    )
}

fn validate_optional_time_range(start: Option<u64>, end: Option<u64>) -> Result<(), CliError> {
    if let (Some(start), Some(end)) = (start, end)
        && end < start
    {
        return Err(CliError::Configuration(
            "--end must be greater than or equal to --start".to_string(),
        ));
    }
    Ok(())
}

async fn print_info_value(
    inv: &Invocation,
    request: InfoRequest,
    context: &'static str,
    empty_message: &'static str,
) -> Result<(), anyhow::Error> {
    let value = inv
        .remote()?
        .api()
        .info::<serde_json::Value>(&request, context)
        .await?;
    inv.print(&JsonValueOutput::new(value, empty_message));
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FillRow {
    pub coin: String,
    pub side: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
    pub direction: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub closed_pnl: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub fee: Decimal,
    pub fee_token: String,
    pub oid: u64,
    pub time: u64,
    pub liquidity: String,
    pub trade_id: u64,
    pub cloid: Option<String>,
}

impl From<Fill> for FillRow {
    fn from(fill: Fill) -> Self {
        Self {
            coin: fill.coin,
            side: fill.side.to_string(),
            price: fill.px,
            size: fill.sz,
            direction: fill.dir.to_string(),
            closed_pnl: fill.closed_pnl,
            fee: fill.fee,
            fee_token: fill.fee_token,
            oid: fill.oid,
            time: fill.time,
            liquidity: if fill.crossed { "taker" } else { "maker" }.to_string(),
            trade_id: fill.tid,
            cloid: fill.cloid.map(|cloid| cloid.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillsOutput {
    fills: Vec<FillRow>,
}

impl FillsOutput {
    #[must_use]
    pub fn new(fills: Vec<FillRow>) -> Self {
        Self { fills }
    }
}

impl TableData for FillsOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Coin",
            "Side",
            "Price",
            "Size",
            "Direction",
            "Closed PnL",
            "Fee",
            "Fee Token",
            "OID",
            "Time",
            "Liquidity",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.fills
            .iter()
            .map(|fill| {
                vec![
                    fill.coin.clone(),
                    fill.side.clone(),
                    fill.price.to_string(),
                    fill.size.to_string(),
                    fill.direction.clone(),
                    fill.closed_pnl.to_string(),
                    fill.fee.to_string(),
                    fill.fee_token.clone(),
                    fill.oid.to_string(),
                    fill.time.to_string(),
                    fill.liquidity.clone(),
                ]
            })
            .collect()
    }

    fn pretty_rows(&self) -> Vec<Vec<String>> {
        self.fills
            .iter()
            .map(|fill| {
                vec![
                    fill.coin.clone(),
                    fill.side.clone(),
                    fill.price.to_string(),
                    fill.size.to_string(),
                    fill.direction.clone(),
                    format_pretty_pnl(fill.closed_pnl),
                    fill.fee.to_string(),
                    fill.fee_token.clone(),
                    fill.oid.to_string(),
                    fill.time.to_string(),
                    fill.liquidity.clone(),
                ]
            })
            .collect()
    }

    fn empty_message(&self) -> Option<&str> {
        Some("No fills.")
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.fills).unwrap_or_else(|_| serde_json::json!([]))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PortfolioSummaryRow {
    #[serde(with = "rust_decimal::serde::str")]
    pub account_value: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub total_notional_position: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub total_raw_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub total_margin_used: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub cross_account_value: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub withdrawable: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub margin_utilization_pct: Decimal,
    pub positions_count: usize,
    pub spot_balances_count: usize,
    pub vault_equities_count: usize,
    pub time: u64,
}

impl From<&ClearinghouseState> for PortfolioSummaryRow {
    fn from(state: &ClearinghouseState) -> Self {
        Self {
            account_value: state.margin_summary.account_value,
            total_notional_position: state.margin_summary.total_ntl_pos,
            total_raw_usd: state.margin_summary.total_raw_usd,
            total_margin_used: state.margin_summary.total_margin_used,
            cross_account_value: state.cross_margin_summary.account_value,
            withdrawable: state.withdrawable,
            margin_utilization_pct: state.margin_summary.margin_utilization(),
            positions_count: state.asset_positions.len(),
            spot_balances_count: 0,
            vault_equities_count: 0,
            time: state.time,
        }
    }
}

/// A position in the portfolio view: the shared position fields under the
/// public `account portfolio` JSON keys.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PositionRow {
    pub coin: String,
    pub side: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub entry_price: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str")]
    pub position_value: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub unrealized_pnl: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub liquidation_price: Option<Decimal>,
    pub leverage: String,
}

impl From<positions::PositionRow> for PositionRow {
    fn from(row: positions::PositionRow) -> Self {
        Self {
            coin: row.coin,
            side: row.side.to_string(),
            size: row.size,
            entry_price: row.entry,
            position_value: row.position_value,
            unrealized_pnl: row.unrealized_pnl,
            liquidation_price: row.liquidation_price,
            leverage: format!("{}x {}", row.leverage.value, row.leverage.kind),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BalanceRow {
    pub coin: String,
    pub token: Option<usize>,
    #[serde(with = "rust_decimal::serde::str")]
    pub hold: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub total: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub entry_notional: Decimal,
}

impl From<RawSpotBalance> for BalanceRow {
    fn from(balance: RawSpotBalance) -> Self {
        Self {
            coin: balance.coin,
            token: balance.token,
            hold: balance.hold,
            total: balance.total,
            entry_notional: balance.entry_ntl,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VaultEquityRow {
    pub vault_address: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub equity: Decimal,
    pub locked_until_timestamp: Option<u64>,
}

impl From<UserVaultEquity> for VaultEquityRow {
    fn from(equity: UserVaultEquity) -> Self {
        Self {
            vault_address: equity.vault_address.to_string(),
            equity: equity.equity,
            locked_until_timestamp: equity.locked_until_timestamp,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortfolioOutput {
    summary: PortfolioSummaryRow,
    positions: Vec<PositionRow>,
    spot_balances: Vec<BalanceRow>,
    vault_equities: Vec<VaultEquityRow>,
}

impl PortfolioOutput {
    #[must_use]
    pub fn new(
        state: ClearinghouseState,
        spot_balances: Vec<BalanceRow>,
        vault_equities: Vec<UserVaultEquity>,
    ) -> Self {
        let mut summary = PortfolioSummaryRow::from(&state);
        let positions = state
            .asset_positions
            .into_iter()
            .map(positions::PositionRow::from)
            .map(PositionRow::from)
            .collect::<Vec<_>>();
        let vault_equities = vault_equities
            .into_iter()
            .map(VaultEquityRow::from)
            .collect::<Vec<_>>();
        summary.positions_count = positions.len();
        summary.spot_balances_count = spot_balances.len();
        summary.vault_equities_count = vault_equities.len();

        Self {
            summary,
            positions,
            spot_balances,
            vault_equities,
        }
    }
}

impl PortfolioOutput {
    /// The table rows; `pnl` formats a position's unrealized PnL.
    fn table_rows(&self, pnl: fn(Decimal) -> String) -> Vec<Vec<String>> {
        let mut rows = vec![
            vec![
                "Summary".to_string(),
                "Account Value".to_string(),
                self.summary.account_value.to_string(),
                "Total equity".to_string(),
            ],
            vec![
                "Summary".to_string(),
                "Withdrawable".to_string(),
                self.summary.withdrawable.to_string(),
                "Available to withdraw".to_string(),
            ],
            vec![
                "Summary".to_string(),
                "Margin Used".to_string(),
                self.summary.total_margin_used.to_string(),
                format!("{}% utilization", self.summary.margin_utilization_pct),
            ],
        ];

        if self.positions.is_empty()
            && self.spot_balances.is_empty()
            && self.vault_equities.is_empty()
        {
            rows.push(vec![
                "Portfolio".to_string(),
                "Holdings".to_string(),
                "No data.".to_string(),
                "No positions, spot balances, or vault equities.".to_string(),
            ]);
        }

        rows.extend(self.positions.iter().map(|position| {
            vec![
                "Position".to_string(),
                position.coin.clone(),
                position.size.to_string(),
                format!(
                    "{} pnl={} value={}",
                    position.side,
                    pnl(position.unrealized_pnl),
                    position.position_value
                ),
            ]
        }));
        rows.extend(self.spot_balances.iter().map(|balance| {
            vec![
                "Spot Balance".to_string(),
                balance.coin.clone(),
                balance.total.to_string(),
                format!(
                    "hold={} token={}",
                    balance.hold,
                    balance
                        .token
                        .map(|token| token.to_string())
                        .unwrap_or_else(|| "n/a".to_string())
                ),
            ]
        }));
        rows.extend(self.vault_equities.iter().map(|equity| {
            vec![
                "Vault Equity".to_string(),
                equity.vault_address.clone(),
                equity.equity.to_string(),
                equity
                    .locked_until_timestamp
                    .map(|ts| format!("locked_until={ts}"))
                    .unwrap_or_else(|| "unlocked".to_string()),
            ]
        }));

        rows
    }
}

impl TableData for PortfolioOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Section", "Name", "Value", "Details"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.table_rows(|pnl| pnl.to_string())
    }

    fn pretty_rows(&self) -> Vec<Vec<String>> {
        self.table_rows(format_pretty_pnl)
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "summary": self.summary,
            "positions": self.positions,
            "spot_balances": self.spot_balances,
            "vault_equities": self.vault_equities,
        })
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SubaccountRow {
    pub name: String,
    pub address: String,
    pub master: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub account_value: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub withdrawable: Decimal,
    pub positions_count: usize,
    pub spot_balances_count: usize,
}

impl From<SubAccount> for SubaccountRow {
    fn from(subaccount: SubAccount) -> Self {
        Self {
            name: subaccount.name,
            address: subaccount.sub_account_user.to_string(),
            master: subaccount.master.to_string(),
            account_value: subaccount.clearinghouse_state.margin_summary.account_value,
            withdrawable: subaccount.clearinghouse_state.withdrawable,
            positions_count: subaccount.clearinghouse_state.asset_positions.len(),
            spot_balances_count: subaccount.spot_state.balances.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubaccountsOutput {
    subaccounts: Vec<SubaccountRow>,
}

impl SubaccountsOutput {
    #[must_use]
    pub fn new(subaccounts: Vec<SubaccountRow>) -> Self {
        Self { subaccounts }
    }
}

impl TableData for SubaccountsOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Name",
            "Address",
            "Master",
            "Account Value",
            "Withdrawable",
            "Positions",
            "Spot Balances",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.subaccounts
            .iter()
            .map(|subaccount| {
                vec![
                    subaccount.name.clone(),
                    subaccount.address.clone(),
                    subaccount.master.clone(),
                    subaccount.account_value.to_string(),
                    subaccount.withdrawable.to_string(),
                    subaccount.positions_count.to_string(),
                    subaccount.spot_balances_count.to_string(),
                ]
            })
            .collect()
    }

    fn empty_message(&self) -> Option<&str> {
        Some("No subaccounts.")
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.subaccounts).unwrap_or_else(|_| serde_json::json!([]))
    }
}

pub const GROUP: GroupSpec = GroupSpec::new(&["account"], "Read account data and manage OWS wallets")
    .long_about("Read account data and manage OWS wallets.\n\
\n\
Public reads (`fills`, `orders`, `portfolio`, `subaccounts`) query any Ethereum address and do not require a signer. For these commands, wallets can also be selected by name, id, or address.\n\
\n\
OWS account commands (`add`, `ls`, `set-default`, `remove`) manage wallets used for authenticated actions.\n\
\n\
Use global --account <WALLET_NAME_OR_ID_OR_ADDRESS> to set the signer for the current command without changing the default wallet.")
    .aliases(&["accounts"]);

pub struct AccountFills;

impl CliCommand for AccountFills {
    type Args = FillsArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "fills"],
        "account",
        "Show fill history for an address or wallet",
        "Query public fill history for an address or wallet, optionally bounded by time.",
    )
    .args(&[
        USER_SELECTOR.schema_required(),
        arg("start").schema_string().describe(
            "RFC3339 or epoch-millisecond start time. Uses userFillsByTime when present.",
        ),
        arg("end")
            .schema_string()
            .describe("RFC3339 or epoch-millisecond end time."),
        arg("aggregate_by_time")
            .kind(InputKind::Boolean)
            .describe("Ask the API to aggregate fills by time where supported."),
    ]);

    async fn run(inv: &Invocation, args: &FillsArgs) -> Result<(), anyhow::Error> {
        let user = args.user.resolve(inv)?;
        let api = inv.remote()?.api();
        validate_optional_time_range(args.start, args.end)?;
        let fills = match args.start {
            Some(start) => {
                let request = InfoRequest::user("userFillsByTime", user)
                    .time_range(Some(start), args.end)
                    .field_if("aggregateByTime", args.aggregate_by_time.then_some(true));
                api.info::<Vec<Fill>>(&request, "loading user fills by time")
                    .await?
                    .into_iter()
                    .map(FillRow::from)
                    .collect()
            }
            None => api
                .sdk()
                .user_fills(user)
                .await
                .map_err(from_sdk)?
                .into_iter()
                .map(FillRow::from)
                .collect(),
        };
        inv.print(&FillsOutput::new(fills));
        Ok(())
    }
}

pub struct AccountFees;

impl CliCommand for AccountFees {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "fees"],
        "account",
        "Show the fee schedule and volume context for an address or wallet",
        "Query fee schedule and volume context for an address or wallet.",
    )
    .args(&[USER_SELECTOR.schema_required()]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        let user = args.resolve(inv)?;
        print_info_value(
            inv,
            InfoRequest::user("userFees", user),
            "userFees",
            "No fee data.",
        )
        .await
    }
}

pub struct AccountRateLimit;

impl CliCommand for AccountRateLimit {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "rate-limit"],
        "account",
        "Show rate-limit context for an address or wallet",
        "Query user rate-limit context for an address or wallet.",
    )
    .args(&[USER_SELECTOR.schema_required()]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        let user = args.resolve(inv)?;
        print_info_value(
            inv,
            InfoRequest::user("userRateLimit", user),
            "userRateLimit",
            "No rate-limit data.",
        )
        .await
    }
}

pub struct AccountOrders;

impl CliCommand for AccountOrders {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "orders"],
        "account",
        "List open orders for an address or wallet",
        "Query public open orders for an address or wallet.",
    )
    .args(&[USER_SELECTOR.schema_required()]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        let user = args.resolve(inv)?;
        let orders = inv
            .remote()?
            .api()
            .sdk()
            .open_orders(user, None)
            .await
            .map_err(from_sdk)?;
        inv.print(&OrderListOutput::new(orders, "No orders."));
        Ok(())
    }
}

pub struct AccountPortfolio;

impl CliCommand for AccountPortfolio {
    type Args = PortfolioArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "portfolio"],
        "account",
        "Show the portfolio summary for an address or wallet",
        "Query public portfolio summary for an address or wallet.",
    )
    .args(&[
        USER_SELECTOR.schema_required(),
        arg("dex").describe("Perp dex name (HIP-3). Defaults to the first dex."),
    ]);

    async fn run(inv: &Invocation, args: &PortfolioArgs) -> Result<(), anyhow::Error> {
        let user = args.user.resolve(inv)?;
        let api = inv.remote()?.api();
        let client = api.sdk();
        let dex = canonical_dex(client, args.dex.as_deref()).await?;
        let state = client
            .clearinghouse_state(user, dex.clone())
            .await
            .map_err(from_sdk)?;
        // Spot balances and vault equities are account-wide, not per-dex: report
        // them only for the default dex so a HIP-3 view is not double-counted.
        let (spot_balances, vault_equities) = if dex.is_some() {
            (Vec::new(), Vec::new())
        } else {
            let balances = user_spot_balances_raw(api, user)
                .await?
                .into_iter()
                .map(BalanceRow::from)
                .collect();
            (
                balances,
                client.user_vault_equities(user).await.map_err(from_sdk)?,
            )
        };
        inv.print(&PortfolioOutput::new(state, spot_balances, vault_equities));
        Ok(())
    }
}

pub struct AccountSubaccounts;

impl CliCommand for AccountSubaccounts {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "subaccounts"],
        "account",
        "List subaccounts for an address or wallet",
        "Query public subaccounts for an address or wallet.",
    )
    .args(&[USER_SELECTOR.schema_required()]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        subaccounts(inv, args.resolve(inv)?).await
    }
}

pub struct AccountPortfolioHistory;

impl CliCommand for AccountPortfolioHistory {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "portfolio-history"],
        "account",
        "Show portfolio history for an address or wallet",
        "Query frontend portfolio graph/history data for an address or wallet.",
    )
    .args(&[USER_SELECTOR.schema_required()]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        let user = args.resolve(inv)?;
        print_info_value(
            inv,
            InfoRequest::user("portfolio", user),
            "portfolio",
            "No portfolio history.",
        )
        .await
    }
}

pub struct AccountLedger;

impl CliCommand for AccountLedger {
    type Args = TimeRangeArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "ledger"],
        "account",
        "Show non-funding ledger updates: deposits, withdrawals, and transfers",
        "Query non-funding ledger updates, including deposits and withdrawals.",
    )
    .args(&[
        USER_SELECTOR.schema_required(),
        arg("start")
            .schema_string()
            .describe("RFC3339 or epoch-millisecond start time."),
        arg("end")
            .schema_string()
            .describe("RFC3339 or epoch-millisecond end time."),
    ]);

    async fn run(inv: &Invocation, args: &TimeRangeArgs) -> Result<(), anyhow::Error> {
        let user = args.user.resolve(inv)?;
        validate_optional_time_range(Some(args.start), args.end)?;
        print_info_value(
            inv,
            InfoRequest::user("userNonFundingLedgerUpdates", user)
                .time_range(Some(args.start), args.end),
            "userNonFundingLedgerUpdates",
            "No ledger updates.",
        )
        .await
    }
}

pub struct AccountFunding;

impl CliCommand for AccountFunding {
    type Args = TimeRangeArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "funding"],
        "account",
        "Show funding payment history",
        "Query user funding payment history.",
    )
    .args(&[
        USER_SELECTOR.schema_required(),
        arg("start")
            .schema_string()
            .describe("RFC3339 or epoch-millisecond start time."),
        arg("end")
            .schema_string()
            .describe("RFC3339 or epoch-millisecond end time."),
    ]);

    async fn run(inv: &Invocation, args: &TimeRangeArgs) -> Result<(), anyhow::Error> {
        let user = args.user.resolve(inv)?;
        validate_optional_time_range(Some(args.start), args.end)?;
        print_info_value(
            inv,
            InfoRequest::user("userFunding", user).time_range(Some(args.start), args.end),
            "userFunding",
            "No funding history.",
        )
        .await
    }
}

pub struct AccountTwapHistory;

impl CliCommand for AccountTwapHistory {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "twap-history"],
        "account",
        "Show TWAP order history",
        "Query user TWAP order history.",
    )
    .args(&[USER_SELECTOR.schema_required()]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        let user = args.resolve(inv)?;
        print_info_value(
            inv,
            InfoRequest::user("twapHistory", user),
            "twapHistory",
            "No TWAP history.",
        )
        .await
    }
}

pub struct AccountTwapFills;

impl CliCommand for AccountTwapFills {
    type Args = TwapFillsArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "twap-fills"],
        "account",
        "Show TWAP slice fills",
        "Query user TWAP slice fills, optionally bounded by time.",
    )
    .args(&[
        USER_SELECTOR.schema_required(),
        arg("start").schema_string().describe(
            "RFC3339 or epoch-millisecond start time. Uses userTwapSliceFillsByTime when present.",
        ),
        arg("end")
            .schema_string()
            .describe("RFC3339 or epoch-millisecond end time."),
        arg("aggregate_by_time")
            .kind(InputKind::Boolean)
            .describe("Ask the API to aggregate TWAP fills by time where supported."),
    ]);

    async fn run(inv: &Invocation, args: &TwapFillsArgs) -> Result<(), anyhow::Error> {
        let user = args.user.resolve(inv)?;
        validate_optional_time_range(args.start, args.end)?;
        match args.start {
            Some(start) => {
                let request = InfoRequest::user("userTwapSliceFillsByTime", user)
                    .time_range(Some(start), args.end)
                    .field_if("aggregateByTime", args.aggregate_by_time.then_some(true));
                print_info_value(
                    inv,
                    request,
                    "loading TWAP slice fills by time",
                    "No TWAP slice fills.",
                )
                .await
            }
            None => {
                print_info_value(
                    inv,
                    InfoRequest::user("userTwapSliceFills", user),
                    "loading TWAP slice fills",
                    "No TWAP slice fills.",
                )
                .await
            }
        }
    }
}

pub struct AccountAbstraction;

impl CliCommand for AccountAbstraction {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "abstraction"],
        "account",
        "Show or set the account abstraction mode",
        "Read account abstraction mode via userAbstraction. When ADDRESS is omitted, uses the configured signer address.",
    )
    .args(&[USER_SELECTOR]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        let user = args.resolve(inv)?;
        let row = fetch_abstraction(inv.remote()?.api(), user).await?;
        inv.print(&AbstractionOutput { row });
        Ok(())
    }
}

pub struct AccountAbstractionSet;

impl CliCommand for AccountAbstractionSet {
    type Args = SetAbstractionArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["account", "abstraction", "set"],
        "account",
        "Set abstraction mode for the selected signer",
        "Set account abstraction for the signer (userSetAbstraction). Use --dry-run to preview; confirm or pass -y for live submit.",
    )
    .signs(Signs::user_signed::<UserSetAbstraction>())
    .risk(Risk::AccountState)
    .prompt()
    .args(&[
        arg("mode").describe("Target abstraction mode."),
        arg("yes").kind(InputKind::Boolean).describe("Skip confirmation prompt for live execution."),
    ]);

    async fn run(inv: &Invocation, args: &SetAbstractionArgs) -> Result<(), anyhow::Error> {
        let resolved = inv.resolve_signer();
        if let Ok(signer) = &resolved {
            require_master_signer(signer, "userSetAbstraction")?;
        }
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "set_abstraction",
                abstraction_set_dry_run_value(
                    inv.remote()?.chain(),
                    resolved.ok().map(|signer| signer.address()),
                    args.mode,
                ),
            );
        }
        set_abstraction(inv, &resolved?, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::support::parse_address;
    use crate::output::{OutputFormat, Session};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn dec(input: &str) -> Decimal {
        Decimal::from_str(input).unwrap()
    }

    #[test]
    fn empty_fills_show_no_data_message_but_json_is_empty_array() {
        let output = FillsOutput::new(vec![]);

        assert!(output.rows().is_empty());
        assert_eq!(output.empty_message(), Some("No fills."));
        assert!(output.to_json_value().as_array().unwrap().is_empty());
    }

    #[test]
    fn empty_subaccounts_show_no_data_message_but_json_is_empty_array() {
        let output = SubaccountsOutput::new(vec![]);

        assert!(output.rows().is_empty());
        assert_eq!(output.empty_message(), Some("No subaccounts."));
        assert!(output.to_json_value().as_array().unwrap().is_empty());
    }

    #[test]
    fn fill_json_has_public_account_fields() {
        let output = FillsOutput::new(vec![FillRow {
            coin: "BTC".to_string(),
            side: "Bid".to_string(),
            price: dec("100"),
            size: dec("0.5"),
            direction: "Open Long".to_string(),
            closed_pnl: Decimal::ZERO,
            fee: dec("0.01"),
            fee_token: "USDC".to_string(),
            oid: 42,
            time: 123,
            liquidity: "taker".to_string(),
            trade_id: 7,
            cloid: None,
        }]);
        let json = output.to_json_value();

        assert_eq!(json[0]["coin"], "BTC");
        assert_eq!(json[0]["price"], "100");
        assert_eq!(json[0]["fee_token"], "USDC");
    }

    #[test]
    fn fills_pretty_colors_positive_and_negative_closed_pnl_cells() {
        let output = FillsOutput::new(vec![
            FillRow {
                coin: "BTC".to_string(),
                side: "Bid".to_string(),
                price: dec("100"),
                size: dec("0.5"),
                direction: "Close Long".to_string(),
                closed_pnl: dec("12.34"),
                fee: dec("0.01"),
                fee_token: "USDC".to_string(),
                oid: 42,
                time: 123,
                liquidity: "taker".to_string(),
                trade_id: 7,
                cloid: None,
            },
            FillRow {
                coin: "ETH".to_string(),
                side: "Ask".to_string(),
                price: dec("90"),
                size: dec("1"),
                direction: "Close Short".to_string(),
                closed_pnl: dec("-5.67"),
                fee: dec("0.02"),
                fee_token: "USDC".to_string(),
                oid: 43,
                time: 124,
                liquidity: "maker".to_string(),
                trade_id: 8,
                cloid: None,
            },
        ]);

        let pretty = Session::for_test(OutputFormat::Pretty).render(&output);

        assert!(pretty.contains(&crate::output::colors::green("12.34")));
        assert!(pretty.contains(&crate::output::colors::red("-5.67")));
        assert!(pretty.contains(&crate::output::colors::cyan("Closed PnL")));
    }

    #[test]
    fn fills_table_and_json_keep_closed_pnl_uncolored() {
        let output = FillsOutput::new(vec![FillRow {
            coin: "BTC".to_string(),
            side: "Bid".to_string(),
            price: dec("100"),
            size: dec("0.5"),
            direction: "Close Long".to_string(),
            closed_pnl: dec("12.34"),
            fee: dec("0.01"),
            fee_token: "USDC".to_string(),
            oid: 42,
            time: 123,
            liquidity: "taker".to_string(),
            trade_id: 7,
            cloid: None,
        }]);

        let table = Session::for_test(OutputFormat::Table).render(&output);
        let json = Session::for_test(OutputFormat::Json).render(&output);

        assert!(table.contains("12.34"));
        assert!(!table.contains(crate::output::colors::GREEN));
        assert!(!table.contains(crate::output::colors::RED));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["closed_pnl"], "12.34");
        assert!(!json.contains(crate::output::colors::GREEN));
        assert!(!json.contains(crate::output::colors::RED));
    }

    #[test]
    fn zero_closed_pnl_is_not_forced_red_or_green() {
        let output = FillsOutput::new(vec![FillRow {
            coin: "BTC".to_string(),
            side: "Bid".to_string(),
            price: dec("100"),
            size: dec("0.5"),
            direction: "Close Long".to_string(),
            closed_pnl: Decimal::ZERO,
            fee: dec("0.01"),
            fee_token: "USDC".to_string(),
            oid: 42,
            time: 123,
            liquidity: "taker".to_string(),
            trade_id: 7,
            cloid: None,
        }]);

        let pretty = Session::for_test(OutputFormat::Pretty).render(&output);

        assert!(pretty.contains("\t0\t") || pretty.contains(" 0 "));
        assert!(!pretty.contains(&format!("{}0", crate::output::colors::GREEN)));
        assert!(!pretty.contains(&format!("{}0", crate::output::colors::RED)));
    }

    #[test]
    fn portfolio_pretty_colors_position_unrealized_pnl_only() {
        let output = PortfolioOutput {
            summary: PortfolioSummaryRow {
                account_value: dec("1000"),
                total_notional_position: dec("100"),
                total_raw_usd: dec("1000"),
                total_margin_used: dec("10"),
                cross_account_value: dec("1000"),
                withdrawable: dec("990"),
                margin_utilization_pct: dec("1"),
                positions_count: 2,
                spot_balances_count: 0,
                vault_equities_count: 0,
                time: 123,
            },
            positions: vec![
                PositionRow {
                    coin: "BTC".to_string(),
                    side: "long".to_string(),
                    size: dec("0.1"),
                    entry_price: Some(dec("100")),
                    position_value: dec("110"),
                    unrealized_pnl: dec("10"),
                    liquidation_price: None,
                    leverage: "1x cross".to_string(),
                },
                PositionRow {
                    coin: "ETH".to_string(),
                    side: "short".to_string(),
                    size: dec("-1"),
                    entry_price: Some(dec("100")),
                    position_value: dec("95"),
                    unrealized_pnl: dec("-5"),
                    liquidation_price: None,
                    leverage: "1x cross".to_string(),
                },
            ],
            spot_balances: vec![BalanceRow {
                coin: "USDC".to_string(),
                token: Some(0),
                hold: Decimal::ZERO,
                total: dec("100"),
                entry_notional: Decimal::ZERO,
            }],
            vault_equities: vec![],
        };

        let pretty = Session::for_test(OutputFormat::Pretty).render(&output);
        let table = Session::for_test(OutputFormat::Table).render(&output);

        assert!(pretty.contains(&format!("pnl={}", crate::output::colors::green("10"))));
        assert!(pretty.contains(&format!("pnl={}", crate::output::colors::red("-5"))));
        assert!(!pretty.contains(&format!("{}100", crate::output::colors::GREEN)));
        assert!(table.contains("pnl=10"));
        assert!(table.contains("pnl=-5"));
        assert!(!table.contains(crate::output::colors::GREEN));
        assert!(!table.contains(crate::output::colors::RED));
    }

    #[test]
    fn parse_address_rejects_invalid_addresses() {
        let err = parse_address("not-an-address").unwrap_err();

        assert_eq!(err.exit_code(), 13);
        assert!(err.to_string().contains("Invalid address"));
    }

    #[test]
    fn parse_address_accepts_zero_prefixed_ethereum_addresses() {
        let address = parse_address("0x0000000000000000000000000000000000000001").unwrap();

        assert_eq!(
            address.to_string(),
            "0x0000000000000000000000000000000000000001"
        );
    }
}
