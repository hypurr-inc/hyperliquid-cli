use alloy::sol;
use chrono::DateTime;
use clap::{Args, ValueEnum};
use hypersdk::Address;
use hypersdk::Decimal;
use hypersdk::hypercore::{ClearinghouseState, Fill, SubAccount, UserVaultEquity};
use serde::Serialize;

use crate::assets::canonical_dex;
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, Risk, arg};
use crate::commands::abstraction::{AccountMode, fetch_abstraction};
use crate::commands::confirm;
use crate::commands::orders::OrderListOutput;
use crate::commands::positions::{self, pretty_pnl as format_pretty_pnl};
use crate::commands::rows::{ListRow, One, Row, Rows, or_missing, style_column};
use crate::commands::selector::{OptionalAddressArgs, USER_SELECTOR};
use crate::commands::spot_balances::{RawSpotBalance, user_spot_balances_raw};
use crate::commands::support::print_dry_run;
use crate::commands::user_signed::{self, UserSigned};
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::invocation::Invocation;
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

impl Row for AbstractionSetRow {
    const HEADERS: &'static [&'static str] =
        &["Status", "Action", "Signer", "User", "Mode", "Network"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.status.clone(),
            self.action.clone(),
            self.signer.clone(),
            self.user.clone(),
            self.mode.clone(),
            self.network.clone(),
        ]
    }
}

pub(crate) async fn subaccounts(inv: &Invocation, user: Address) -> Result<(), anyhow::Error> {
    // The exchange answers `null` for an account without subaccounts.
    let subaccounts: Vec<SubaccountRow> = inv
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
    inv.print(&Rows::new(subaccounts));
    Ok(())
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
    let question = format!(
        "Set account abstraction for {user} to {}?",
        args.mode.cli_value()
    );
    confirm::unless_yes(
        args.yes,
        inv.mode(),
        "",
        &question,
        "account abstraction update aborted",
    )?;
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

    inv.print(&One::new(AbstractionSetRow {
        status: "submitted".to_string(),
        action: "set-abstraction".to_string(),
        signer: user.to_string(),
        user: user.to_string(),
        mode: args.mode.cli_value().to_string(),
        protocol_mode: protocol_mode.to_string(),
        network: remote.chain().to_string(),
    }));
    Ok(())
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

pub type FillsOutput = Rows<FillRow>;

impl Row for FillRow {
    const HEADERS: &'static [&'static str] = &[
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
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.side.clone(),
            self.price.to_string(),
            self.size.to_string(),
            self.direction.clone(),
            self.closed_pnl.to_string(),
            self.fee.to_string(),
            self.fee_token.clone(),
            self.oid.to_string(),
            self.time.to_string(),
            self.liquidity.clone(),
        ]
    }

    fn pretty_cells(&self) -> Vec<String> {
        let mut cells = self.cells();
        style_column::<Self>(&mut cells, "Closed PnL", |_| {
            format_pretty_pnl(self.closed_pnl)
        });
        cells
    }
}

impl ListRow for FillRow {
    const EMPTY: &'static str = "No fills.";
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
    pub account_mode: AccountMode,
    /// Under a pooled mode the perp figures above are not the account's real
    /// balances: they live in the spot balance.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub perp_state_is_partial: bool,
}

impl PortfolioSummaryRow {
    fn new(
        state: &ClearinghouseState,
        spot_balances_count: usize,
        vault_equities_count: usize,
        account_mode: AccountMode,
    ) -> Self {
        Self {
            account_value: state.margin_summary.account_value,
            total_notional_position: state.margin_summary.total_ntl_pos,
            total_raw_usd: state.margin_summary.total_raw_usd,
            total_margin_used: state.margin_summary.total_margin_used,
            cross_account_value: state.cross_margin_summary.account_value,
            withdrawable: state.withdrawable,
            margin_utilization_pct: state.margin_summary.margin_utilization(),
            positions_count: state.asset_positions.len(),
            spot_balances_count,
            vault_equities_count,
            time: state.time,
            account_mode,
            perp_state_is_partial: account_mode.pools_collateral(),
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
        account_mode: AccountMode,
    ) -> Self {
        let summary = PortfolioSummaryRow::new(
            &state,
            spot_balances.len(),
            vault_equities.len(),
            account_mode,
        );
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
                format!("hold={} token={}", balance.hold, or_missing(balance.token)),
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
        let mut rows = self.table_rows(format_pretty_pnl);
        if self.summary.perp_state_is_partial {
            rows.insert(
                3,
                vec![
                    "Note".to_string(),
                    "Account Mode".to_string(),
                    self.summary.account_mode.as_str().to_string(),
                    "Balances live in the spot balance under this mode; the perp figures above are partial."
                        .to_string(),
                ],
            );
        }
        rows
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

impl Row for SubaccountRow {
    const HEADERS: &'static [&'static str] = &[
        "Name",
        "Address",
        "Master",
        "Account Value",
        "Withdrawable",
        "Positions",
        "Spot Balances",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.address.clone(),
            self.master.clone(),
            self.account_value.to_string(),
            self.withdrawable.to_string(),
            self.positions_count.to_string(),
            self.spot_balances_count.to_string(),
        ]
    }
}

impl ListRow for SubaccountRow {
    const EMPTY: &'static str = "No subaccounts.";
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
        let account_mode = fetch_abstraction(api, user).await?.normalized_mode;
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
        inv.print(&PortfolioOutput::new(
            state,
            spot_balances,
            vault_equities,
            account_mode,
        ));
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
        inv.print(&One::new(row));
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
        "Set account abstraction for the signer (userSetAbstraction). `disabled` is Standard: separate perp and spot balances. Cautions: builder-code addresses and HIP-3/HIP-4 deployers must stay in standard mode; unified-account and portfolio-margin accounts are limited to 50k actions per day. Use --dry-run to preview; confirm or pass -y for live submit.",
    )
    .long_about("Set account abstraction for the signer (userSetAbstraction).\n\
\n\
`disabled` is Standard: separate perp and spot balances. `unified-account` keeps one balance per asset for spot and every perp dex; `portfolio-margin` margins eligible assets together.\n\
\n\
Cautions: builder-code addresses and HIP-3/HIP-4 deployers must stay in standard mode. Unified-account and portfolio-margin accounts are limited to 50k actions per day.\n\
\n\
Use --dry-run to preview; confirm or pass -y for live submit.")
    .signs(Signs::user_signed::<UserSetAbstraction>())
    .master_only("userSetAbstraction")
    .risk(Risk::AccountState)
    .prompt()
    .args(&[
        arg("mode").describe("Target abstraction mode."),
        arg("yes").kind(InputKind::Boolean).describe("Skip confirmation prompt for live execution."),
    ]);

    async fn run(inv: &Invocation, args: &SetAbstractionArgs) -> Result<(), anyhow::Error> {
        let resolved = inv.resolve_signer();
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
