use alloy::sol;
use chrono::DateTime;
use clap::{Args, ValueEnum};
use hypersdk::Address;
use hypersdk::Decimal;
use hypersdk::hypercore::types::Action;
use hypersdk::hypercore::{
    AbstractionMode, AssetPosition, ClearinghouseState, SubAccount, UserRole, UserVaultEquity,
};
use serde::{Deserialize, Serialize};

use crate::assets::{AssetResolver, canonical_dex};
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, Risk, arg};
use crate::commands::abstraction::{AccountMode, fetch_abstraction};
use crate::commands::actions;
use crate::commands::confirm;
use crate::commands::orders;
use crate::commands::paging::{self, Endpoint};
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
    /// Keep reading until the window is exhausted, instead of stopping at the API's 2000-fill page.
    #[arg(long, requires = "start")]
    pub all: bool,
}

#[derive(Args, Debug, Clone)]
pub struct OrdersArgs {
    #[command(flatten)]
    pub user: OptionalAddressArgs,
    /// Perp dex name (HIP-3). Defaults to the first dex.
    #[arg(long)]
    pub dex: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct AssetDataArgs {
    #[command(flatten)]
    pub user: OptionalAddressArgs,
    /// Perpetual coin, for example BTC or xyz:XYZ100.
    #[arg(long)]
    pub coin: String,
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
    /// Keep reading until the window is exhausted, instead of stopping at the API's page limit.
    #[arg(long)]
    pub all: bool,
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

    /// Subaccount of the signer to set the mode for: subaccount address, stored account alias, or stored account id. Defaults to the signer's own account
    #[arg(long)]
    pub user: Option<String>,

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

    fn sdk_mode(self) -> AbstractionMode {
        match self {
            Self::Disabled => AbstractionMode::Standard,
            Self::UnifiedAccount => AbstractionMode::UnifiedAccount,
            Self::PortfolioMargin => AbstractionMode::PortfolioMargin,
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

/// Whose abstraction a change targets, and how the signer signs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AbstractionPlan {
    user: Address,
    /// An API wallet cannot sign `userSetAbstraction`; it signs
    /// `agentSetAbstraction`, which only ever applies to its master account.
    agent_signed: bool,
}

fn abstraction_plan(
    signer: &SelectedSigner,
    requested: Option<Address>,
) -> Result<AbstractionPlan, CliError> {
    if signer.is_stored_agent() {
        let master = signer.query_address();
        if let Some(requested) = requested
            && requested != master
        {
            return Err(CliError::Unsupported(format!(
                "an API wallet can only set the abstraction of its own master account ({master}), not {requested}; use the master wallet for a subaccount"
            )));
        }
        return Ok(AbstractionPlan {
            user: master,
            agent_signed: true,
        });
    }
    Ok(AbstractionPlan {
        user: requested.unwrap_or_else(|| signer.address()),
        agent_signed: false,
    })
}

/// `userSetAbstraction` may name the signer or one of its subaccounts. Refuse
/// any other address before the exchange does.
async fn ensure_signer_owns(
    api: &crate::http_api::ApiClient,
    signer: Address,
    user: Address,
) -> Result<(), CliError> {
    if user == signer {
        return Ok(());
    }
    match api
        .info::<UserRole>(
            &InfoRequest::user("userRole", user),
            "loading the role of the target account",
        )
        .await?
    {
        UserRole::SubAccount { master } if master == signer => Ok(()),
        _ => Err(CliError::Unsupported(format!(
            "{user} is not a subaccount of the signer {signer}; --user must name the signer's own account or one of its subaccounts"
        ))),
    }
}

fn abstraction_set_dry_run_value(
    chain: hypersdk::hypercore::Chain,
    plan: Option<AbstractionPlan>,
    mode: AbstractionModeArg,
) -> serde_json::Value {
    let user = plan.map(|plan| plan.user.to_string());
    let agent_signed = plan.is_some_and(|plan| plan.agent_signed);
    let action = if agent_signed {
        serde_json::json!({
            "type": "agentSetAbstraction",
            "abstraction": mode.sdk_mode().agent_code(),
        })
    } else {
        serde_json::json!({
            "type": UserSetAbstraction::ACTION_TYPE,
            "hyperliquidChain": chain.to_string(),
            "signatureChainId": chain.arbitrum_id().to_string(),
            "user": user,
            "abstraction": mode.protocol_value(),
        })
    };
    let mut preview = serde_json::json!({
        "mode": mode.cli_value(),
        "protocol_mode": mode.protocol_value(),
        "network": chain.to_string(),
        "user": user,
        "action": action,
    });
    if agent_signed {
        preview["agent_signed"] = true.into();
    }
    preview
}

async fn set_abstraction(
    inv: &Invocation,
    signer: &SelectedSigner,
    plan: AbstractionPlan,
    args: &SetAbstractionArgs,
) -> Result<(), anyhow::Error> {
    let user = plan.user;
    let remote = inv.remote()?;
    if !plan.agent_signed {
        ensure_signer_owns(remote.api(), signer.address(), user).await?;
    }
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
    let protocol_mode = args.mode.protocol_value();
    if plan.agent_signed {
        actions::send_l1_action(
            remote.api(),
            signer,
            Action::AgentSetAbstraction {
                abstraction: args.mode.sdk_mode(),
            },
            actions::nonce_now(),
        )
        .await?;
    } else {
        user_signed::submit(remote.api(), signer, |hyperliquid_chain, nonce| {
            UserSetAbstraction {
                hyperliquidChain: hyperliquid_chain,
                user,
                abstraction: protocol_mode.to_string(),
                nonce,
            }
        })
        .await?;
    }

    inv.print(&One::new(AbstractionSetRow {
        status: "submitted".to_string(),
        action: "set-abstraction".to_string(),
        signer: signer.address().to_string(),
        user: user.to_string(),
        mode: args.mode.cli_value().to_string(),
        protocol_mode: protocol_mode.to_string(),
        network: remote.chain().to_string(),
    }));
    Ok(())
}

pub(crate) fn validate_optional_time_range(
    start: Option<u64>,
    end: Option<u64>,
) -> Result<(), CliError> {
    if let (Some(start), Some(end)) = (start, end)
        && end < start
    {
        return Err(CliError::Configuration(
            "--end must be greater than or equal to --start".to_string(),
        ));
    }
    Ok(())
}

/// A fills request over `start..end`, which the API aggregates by time on request.
fn fills_window(
    request: InfoRequest,
    start: u64,
    end: Option<u64>,
    aggregate_by_time: bool,
) -> InfoRequest {
    request
        .time_range(Some(start), end)
        .field_if("aggregateByTime", aggregate_by_time.then_some(true))
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

/// Print a time-windowed history as the raw rows the API sent (keys
/// snake_cased), saying on stderr when the page was cut off at the API's cap.
async fn print_window(
    inv: &Invocation,
    endpoint: Endpoint,
    args: &TimeRangeArgs,
    request: impl Fn(u64) -> InfoRequest,
    context: &'static str,
    empty_message: &'static str,
) -> Result<(), anyhow::Error> {
    let window = paging::read_window(
        inv.remote()?.api(),
        endpoint,
        args.start,
        args.all,
        request,
        context,
    )
    .await?;
    paging::announce_truncation(inv, endpoint, &window, args.all);
    inv.print(&JsonValueOutput::new(
        serde_json::Value::Array(window.rows),
        empty_message,
    ));
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
    pub hash: String,
    /// Only fills that paid a builder fee carry one.
    #[serde(
        with = "rust_decimal::serde::str_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub builder_fee: Option<Decimal>,
    /// Only liquidation fills carry one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liquidation: Option<FillLiquidation>,
    /// Only TWAP slice fills carry one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub twap_id: Option<u64>,
}

/// The liquidation a fill took part in, as the API reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all(deserialize = "camelCase", serialize = "snake_case"))]
pub struct FillLiquidation {
    pub liquidated_user: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub mark_px: Decimal,
    pub method: String,
}

/// A fill as `userFills` and `userFillsByTime` send it. `dir` stays a string:
/// the SDK's closed `FillDirection` would fail every fill listing the day the
/// exchange adds a direction.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFill {
    coin: String,
    #[serde(with = "rust_decimal::serde::str")]
    px: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    sz: Decimal,
    side: FillSide,
    time: u64,
    dir: String,
    #[serde(with = "rust_decimal::serde::str")]
    closed_pnl: Decimal,
    hash: String,
    oid: u64,
    crossed: bool,
    #[serde(with = "rust_decimal::serde::str")]
    fee: Decimal,
    tid: u64,
    #[serde(default)]
    cloid: Option<String>,
    fee_token: String,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    builder_fee: Option<Decimal>,
    #[serde(default)]
    liquidation: Option<FillLiquidation>,
    #[serde(default)]
    twap_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
enum FillSide {
    #[serde(rename = "B")]
    Bid,
    #[serde(rename = "A")]
    Ask,
}

impl From<RawFill> for FillRow {
    fn from(fill: RawFill) -> Self {
        Self {
            coin: fill.coin,
            side: match fill.side {
                FillSide::Bid => "Bid",
                FillSide::Ask => "Ask",
            }
            .to_string(),
            price: fill.px,
            size: fill.sz,
            direction: fill.dir,
            closed_pnl: fill.closed_pnl,
            fee: fill.fee,
            fee_token: fill.fee_token,
            oid: fill.oid,
            time: fill.time,
            liquidity: if fill.crossed { "taker" } else { "maker" }.to_string(),
            trade_id: fill.tid,
            cloid: fill.cloid,
            hash: fill.hash,
            builder_fee: fill.builder_fee,
            liquidation: fill.liquidation,
            twap_id: fill.twap_id,
        }
    }
}

/// Decode fill rows, naming spot pairs as the markets list them (`@107` is
/// `HYPE/USDC`). The metadata is read only when a spot coin needs it.
async fn fill_rows(
    inv: &Invocation,
    raw: Vec<serde_json::Value>,
) -> Result<Vec<FillRow>, anyhow::Error> {
    let mut rows = raw
        .into_iter()
        .map(|fill| {
            serde_json::from_value::<RawFill>(fill)
                .map(FillRow::from)
                .map_err(|err| {
                    CliError::Internal(anyhow::anyhow!("decode failed while reading fills: {err}"))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if rows.iter().any(|row| row.coin.starts_with('@')) {
        let resolver: AssetResolver = inv.asset_resolver().await?;
        for row in &mut rows {
            row.coin = resolver.display_coin(&row.coin);
        }
    }
    Ok(rows)
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
    /// Unrealized PnL over the margin put into the position, as a fraction.
    #[serde(with = "rust_decimal::serde::str")]
    pub return_on_equity: Decimal,
    /// The market's maximum leverage, not this position's.
    pub max_leverage: u32,
    pub cum_funding: CumFundingRow,
}

/// A position's cumulative funding over the three horizons the API reports.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CumFundingRow {
    #[serde(with = "rust_decimal::serde::str")]
    pub all_time: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub since_open: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub since_change: Decimal,
}

impl From<AssetPosition> for PositionRow {
    fn from(asset: AssetPosition) -> Self {
        let position = &asset.position;
        let (return_on_equity, max_leverage) = (position.return_on_equity, position.max_leverage);
        let cum_funding = CumFundingRow {
            all_time: position.cum_funding.all_time,
            since_open: position.cum_funding.since_open,
            since_change: position.cum_funding.since_change,
        };
        let row = positions::PositionRow::from(asset);
        Self {
            coin: row.coin,
            side: row.side.to_string(),
            size: row.size,
            entry_price: row.entry,
            position_value: row.position_value,
            unrealized_pnl: row.unrealized_pnl,
            liquidation_price: row.liquidation_price,
            leverage: format!("{}x {}", row.leverage.value, row.leverage.kind),
            return_on_equity,
            max_leverage,
            cum_funding,
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
        arg("all").kind(InputKind::Boolean).describe(
            "Page through the whole window (requires start). Without it one page is returned and a notice on stderr names the start that continues.",
        ),
    ]);

    async fn run(inv: &Invocation, args: &FillsArgs) -> Result<(), anyhow::Error> {
        let user = args.user.resolve(inv).await?;
        let api = inv.remote()?.api();
        validate_optional_time_range(args.start, args.end)?;
        let (endpoint, window) = match args.start {
            Some(start) => (
                paging::USER_FILLS_BY_TIME,
                paging::read_window(
                    api,
                    paging::USER_FILLS_BY_TIME,
                    start,
                    args.all,
                    |from| {
                        fills_window(
                            InfoRequest::user("userFillsByTime", user),
                            from,
                            args.end,
                            args.aggregate_by_time,
                        )
                    },
                    "loading user fills by time",
                )
                .await?,
            ),
            None => (
                paging::USER_FILLS_RECENT,
                paging::read_recent(
                    api,
                    paging::USER_FILLS_RECENT,
                    &InfoRequest::user("userFills", user),
                    "loading user fills",
                )
                .await?,
            ),
        };
        paging::announce_truncation(inv, endpoint, &window, args.all);
        inv.print(&FillsOutput::new(fill_rows(inv, window.rows).await?));
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
        let user = args.resolve(inv).await?;
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
        let user = args.resolve(inv).await?;
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
    type Args = OrdersArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "orders"],
        "account",
        "List open orders for an address or wallet",
        "Query public open orders for an address or wallet. Pass dex for a HIP-3 dex's orders; the default dex's orders do not include them.",
    )
    .args(&[
        USER_SELECTOR.schema_required(),
        arg("dex").describe("Perp dex name (HIP-3). Defaults to the first dex."),
    ]);

    async fn run(inv: &Invocation, args: &OrdersArgs) -> Result<(), anyhow::Error> {
        let user = args.user.resolve(inv).await?;
        let api = inv.remote()?.api();
        let dex = canonical_dex(api.sdk(), args.dex.as_deref()).await?;
        let resolver = inv.asset_resolver().await?;
        inv.print(&orders::open_orders_output(api, &resolver, user, dex.as_deref()).await?);
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
        let user = args.user.resolve(inv).await?;
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
        subaccounts(inv, args.resolve(inv).await?).await
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
        let user = args.resolve(inv).await?;
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
        arg("all").kind(InputKind::Boolean).describe(
            "Page through the whole window. Without it one page is returned and a notice on stderr names the start that continues.",
        ),
    ]);

    async fn run(inv: &Invocation, args: &TimeRangeArgs) -> Result<(), anyhow::Error> {
        let user = args.user.resolve(inv).await?;
        validate_optional_time_range(Some(args.start), args.end)?;
        print_window(
            inv,
            paging::USER_LEDGER,
            args,
            |from| {
                InfoRequest::user("userNonFundingLedgerUpdates", user)
                    .time_range(Some(from), args.end)
            },
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
        arg("all").kind(InputKind::Boolean).describe(
            "Page through the whole window. Without it one page is returned and a notice on stderr names the start that continues.",
        ),
    ]);

    async fn run(inv: &Invocation, args: &TimeRangeArgs) -> Result<(), anyhow::Error> {
        let user = args.user.resolve(inv).await?;
        validate_optional_time_range(Some(args.start), args.end)?;
        print_window(
            inv,
            paging::USER_FUNDING,
            args,
            |from| InfoRequest::user("userFunding", user).time_range(Some(from), args.end),
            "userFunding",
            "No funding history.",
        )
        .await
    }
}

pub struct AccountRole;

impl CliCommand for AccountRole {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "role"],
        "account",
        "Show an address's role: user, agent, vault, subaccount or missing",
        "Query an address's protocol role. A subaccount names its master and an agent its owner.",
    )
    .args(&[USER_SELECTOR.schema_required()]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        print_info_value(
            inv,
            InfoRequest::user("userRole", args.resolve(inv).await?),
            "userRole",
            "No role.",
        )
        .await
    }
}

pub struct AccountMultisigSigners;

impl CliCommand for AccountMultisigSigners {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "multisig-signers"],
        "account",
        "Show the authorized signers of a multi-sig account",
        "Query the authorized signers and signature threshold of a multi-sig account. The exchange answers nothing for an address that is not multi-sig.",
    )
    .args(&[USER_SELECTOR.schema_required()]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        print_info_value(
            inv,
            InfoRequest::user("userToMultiSigSigners", args.resolve(inv).await?),
            "userToMultiSigSigners",
            "Not a multi-sig account.",
        )
        .await
    }
}

pub struct AccountAssetData;

impl CliCommand for AccountAssetData {
    type Args = AssetDataArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "asset-data"],
        "account",
        "Show an account's leverage and trade capacity on one perpetual",
        "Query an account's active asset data for a perpetual: leverage, maximum trade sizes and size available to trade (buy side first, sell side second) and the mark price. Works with no open position.",
    )
    .args(&[
        USER_SELECTOR.schema_required(),
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Perpetual coin, for example BTC or xyz:XYZ100.")
            .resource_id("coin"),
    ]);

    async fn run(inv: &Invocation, args: &AssetDataArgs) -> Result<(), anyhow::Error> {
        let user = args.user.resolve(inv).await?;
        let resolver = inv.asset_resolver().await?;
        let perp = resolver
            .resolve_perp_or_fetch(inv.remote()?.api().sdk(), &args.coin)
            .await
            .map_err(CliError::from)?;
        print_info_value(
            inv,
            InfoRequest::user("activeAssetData", user).field("coin", perp.display_name()),
            "activeAssetData",
            "No asset data.",
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
        let user = args.resolve(inv).await?;
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
        let user = args.user.resolve(inv).await?;
        validate_optional_time_range(args.start, args.end)?;
        match args.start {
            Some(start) => {
                let request = fills_window(
                    InfoRequest::user("userTwapSliceFillsByTime", user),
                    start,
                    args.end,
                    args.aggregate_by_time,
                );
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
        let user = args.resolve(inv).await?;
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
        "Set abstraction mode for the selected signer or one of its subaccounts",
        "Set account abstraction for the signer or one of its subaccounts (userSetAbstraction); an API wallet sets its master account's mode with agentSetAbstraction. `disabled` is Standard: separate perp and spot balances. Cautions: builder-code addresses and HIP-3/HIP-4 deployers must stay in standard mode; unified-account and portfolio-margin accounts are limited to 50k actions per day. Use --dry-run to preview; confirm or pass -y for live submit.",
    )
    .long_about("Set account abstraction for the signer or one of its subaccounts (userSetAbstraction).\n\
\n\
`disabled` is Standard: separate perp and spot balances. `unified-account` keeps one balance per asset for spot and every perp dex; `portfolio-margin` margins eligible assets together.\n\
\n\
Cautions: builder-code addresses and HIP-3/HIP-4 deployers must stay in standard mode. Unified-account and portfolio-margin accounts are limited to 50k actions per day.\n\
\n\
--user names a subaccount of the signer. An API wallet signs agentSetAbstraction instead, which changes its master account's mode only; --user may not name another account.\n\
\n\
Use --dry-run to preview; confirm or pass -y for live submit.")
    .signs(Signs::user_signed::<UserSetAbstraction>())
    .risk(Risk::AccountState)
    .prompt()
    .args(&[
        arg("mode").describe("Target abstraction mode."),
        arg("user").kind(InputKind::ActingAccountSelector).describe("Subaccount of the signer to set the mode for: subaccount address, wallet name, or wallet id. Defaults to the signer's own account.").resource_id("subaccount selector"),
        arg("yes").kind(InputKind::Boolean).describe("Skip confirmation prompt for live execution."),
    ]);

    async fn run(inv: &Invocation, args: &SetAbstractionArgs) -> Result<(), anyhow::Error> {
        let requested = args
            .user
            .as_deref()
            .map(|selector| inv.resolve_account(selector))
            .transpose()?;
        let resolved = inv.resolve_signer();
        if inv.dry_run() {
            let plan = match resolved {
                Ok(signer) => Some(abstraction_plan(&signer, requested)?),
                Err(_) => requested.map(|user| AbstractionPlan {
                    user,
                    agent_signed: false,
                }),
            };
            return print_dry_run(
                inv,
                "set_abstraction",
                abstraction_set_dry_run_value(inv.remote()?.chain(), plan, args.mode),
            );
        }
        let signer = resolved?;
        let plan = abstraction_plan(&signer, requested)?;
        set_abstraction(inv, &signer, plan, args).await
    }
}

/// Arguments for `account evm-big-blocks`.
#[derive(Args, Debug, Clone)]
pub struct EvmBigBlocksArgs {
    /// Send the account's HyperEVM transactions to big blocks
    #[arg(long, conflicts_with = "disable", required_unless_present = "disable")]
    pub enable: bool,

    /// Send the account's HyperEVM transactions back to small blocks
    #[arg(long)]
    pub disable: bool,
}

#[derive(Debug, Clone, Serialize)]
struct EvmBigBlocksRow {
    status: String,
    action: String,
    signer: String,
    acting_as: String,
    using_big_blocks: bool,
    network: String,
}

impl Row for EvmBigBlocksRow {
    const HEADERS: &'static [&'static str] = &[
        "Status",
        "Action",
        "Signer",
        "Acting As",
        "Big Blocks",
        "Network",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.status.clone(),
            self.action.clone(),
            self.signer.clone(),
            self.acting_as.clone(),
            self.using_big_blocks.to_string(),
            self.network.clone(),
        ]
    }
}

pub struct AccountEvmBigBlocks;

impl CliCommand for AccountEvmBigBlocks {
    type Args = EvmBigBlocksArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["account", "evm-big-blocks"],
        "account",
        "Direct the account's HyperEVM transactions to big or small blocks",
        "Set the HyperCore user flag that routes the account's HyperEVM transactions to big blocks (--enable) or small blocks (--disable) (evmUserModify). Big blocks are for large contract deployments; the flag stays set until it is unset again. The account must already exist on HyperCore.",
    )
    .risk(Risk::AccountState)
    .one_of_required(&[&["enable"], &["disable"]])
    .args(&[
        arg("enable")
            .kind(InputKind::Boolean)
            .describe("Route HyperEVM transactions to big blocks."),
        arg("disable")
            .kind(InputKind::Boolean)
            .describe("Route HyperEVM transactions back to small blocks."),
    ]);

    async fn run(inv: &Invocation, args: &EvmBigBlocksArgs) -> Result<(), anyhow::Error> {
        let using_big_blocks = args.enable;
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "set_evm_big_blocks",
                serde_json::json!({
                    "using_big_blocks": using_big_blocks,
                    "network": inv.remote()?.chain().to_string(),
                    "action": {"type": "evmUserModify", "usingBigBlocks": using_big_blocks},
                }),
            );
        }
        let signer = inv.resolve_signer()?;
        let remote = inv.remote()?;
        actions::send_l1_action(
            remote.api(),
            &signer,
            Action::EvmUserModify { using_big_blocks },
            actions::nonce_now(),
        )
        .await?;
        inv.print(&One::new(EvmBigBlocksRow {
            status: "submitted".to_string(),
            action: "evm-big-blocks".to_string(),
            signer: signer.address().to_string(),
            acting_as: signer.query_address().to_string(),
            using_big_blocks,
            network: remote.chain().to_string(),
        }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use hypersdk::hypercore::PrivateKeySigner;

    use super::*;

    #[test]
    fn an_api_wallet_sets_its_masters_mode_and_no_other_accounts() {
        let master = Address::repeat_byte(1);
        let agent =
            SelectedSigner::stored_account(PrivateKeySigner::random(), "bot".to_string(), master);

        assert_eq!(
            abstraction_plan(&agent, None).unwrap(),
            AbstractionPlan {
                user: master,
                agent_signed: true
            }
        );
        assert!(abstraction_plan(&agent, Some(master)).unwrap().agent_signed);
        let err = abstraction_plan(&agent, Some(Address::repeat_byte(2))).unwrap_err();
        assert_eq!(err.exit_code(), 13);
    }

    #[test]
    fn any_other_signer_sets_its_own_or_a_named_account_with_the_user_signed_action() {
        let key = PrivateKeySigner::random();
        let signer = SelectedSigner::private_key(key.clone());
        let subaccount = Address::repeat_byte(2);

        assert_eq!(
            abstraction_plan(&signer, None).unwrap(),
            AbstractionPlan {
                user: key.address(),
                agent_signed: false
            }
        );
        assert_eq!(
            abstraction_plan(&signer, Some(subaccount)).unwrap(),
            AbstractionPlan {
                user: subaccount,
                agent_signed: false
            }
        );
    }
}
