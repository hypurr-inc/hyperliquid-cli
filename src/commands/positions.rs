//! Position listing and position-management commands.

use std::fmt;

use clap::Args;
use hypersdk::Address;
use hypersdk::Decimal;
use hypersdk::hypercore::HttpClient;
use hypersdk::hypercore::Subscription;
use hypersdk::hypercore::api::UpdateLeverage;
use hypersdk::hypercore::types::api::UpdateIsolatedMargin;
use hypersdk::hypercore::types::{Action, AssetPosition, LeverageType, PositionData};
use serde::{Serialize, Serializer};

use crate::assets::AssetResolver;
use crate::command_spec::{
    CliCommand, CommandSpec, GroupSpec, InputKind, OwsSupport, Risk, StreamBound, arg,
};
use crate::commands::actions;
use crate::commands::support::{
    canonical_dex, dry_run_signed_details, dry_run_signing_addresses, print_dry_run,
};
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::invocation::Invocation;
use crate::output::{self, TableData};
use crate::quantity::{self, Scale, ScaleError};
use crate::stream::SnapshotWatchArgs;

/// Arguments for `positions update-leverage`.
#[derive(Args, Debug, Clone)]
pub struct UpdateLeverageArgs {
    /// Perpetual coin (for example: BTC, ETH, SOL)
    #[arg(long)]
    pub coin: String,

    /// New leverage value
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=100))]
    pub leverage: u32,

    /// Use isolated margin mode instead of cross margin
    #[arg(long)]
    pub isolated: bool,
}

/// Arguments for `positions update-margin`.
#[derive(Args, Debug, Clone)]
pub struct UpdateMarginArgs {
    /// Perpetual coin (for example: BTC, ETH, SOL)
    #[arg(long)]
    pub coin: String,

    /// Amount of isolated margin to add, in USDC
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,
}

/// A position's leverage mode and multiple, shown as `cross 5x`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Leverage {
    pub kind: LeverageType,
    pub value: u32,
}

impl fmt::Display for Leverage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}x", self.kind, self.value)
    }
}

impl Serialize for Leverage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The mark price a position implies: its value per unit of size.
pub(crate) fn mark_price(position: &PositionData) -> Decimal {
    let size = position.abs_size();
    if size.is_zero() {
        Decimal::ZERO
    } else {
        position.position_value / size
    }
}

/// A position as `positions list` and `vault positions` show it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PositionRow {
    pub coin: String,
    pub side: &'static str,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub entry: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str")]
    pub mark: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub unrealized_pnl: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub position_value: Decimal,
    pub leverage: Leverage,
    #[serde(with = "rust_decimal::serde::str")]
    pub margin_used: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub liquidation_price: Option<Decimal>,
}

impl From<AssetPosition> for PositionRow {
    fn from(value: AssetPosition) -> Self {
        let position = value.position;
        Self {
            mark: mark_price(&position),
            side: position.side(),
            leverage: Leverage {
                kind: position.leverage.leverage_type,
                value: position.leverage.value,
            },
            coin: position.coin,
            size: position.szi,
            entry: position.entry_px,
            unrealized_pnl: position.unrealized_pnl,
            position_value: position.position_value,
            margin_used: position.margin_used,
            liquidation_price: position.liquidation_px,
        }
    }
}

impl PositionRow {
    pub(crate) const HEADERS: [&'static str; 10] = [
        "Coin",
        "Side",
        "Size",
        "Entry",
        "Mark",
        "Unrealized PnL",
        "Position Value",
        "Leverage",
        "Margin Used",
        "Liquidation",
    ];
    const PNL_COLUMN: usize = 5;

    /// The rows of the positions with a nonzero size.
    pub(crate) fn open(positions: Vec<AssetPosition>) -> Vec<Self> {
        positions
            .into_iter()
            .map(Self::from)
            .filter(|row| !row.size.is_zero())
            .collect()
    }

    pub(crate) fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.side.to_string(),
            self.size.to_string(),
            optional_cell(self.entry),
            self.mark.to_string(),
            self.unrealized_pnl.to_string(),
            self.position_value.to_string(),
            self.leverage.to_string(),
            self.margin_used.to_string(),
            optional_cell(self.liquidation_price),
        ]
    }

    /// [`Self::cells`] with the PnL coloured by sign and missing values greyed.
    pub(crate) fn pretty_cells(&self) -> Vec<String> {
        let mut cells = self.cells();
        cells[Self::PNL_COLUMN] = pretty_pnl(self.unrealized_pnl);
        for cell in &mut cells {
            if cell == NOT_AVAILABLE {
                *cell = output::colors::gray(cell);
            }
        }
        cells
    }
}

const NOT_AVAILABLE: &str = "n/a";

fn optional_cell(value: Option<Decimal>) -> String {
    value.map_or_else(|| NOT_AVAILABLE.to_string(), |value| value.to_string())
}

/// A PnL value, green when positive and red when negative.
pub(crate) fn pretty_pnl(value: Decimal) -> String {
    let formatted = value.to_string();
    if value > Decimal::ZERO {
        output::colors::green(&formatted)
    } else if value < Decimal::ZERO {
        output::colors::red(&formatted)
    } else {
        formatted
    }
}

struct PositionsOutput {
    rows: Vec<PositionRow>,
}

impl TableData for PositionsOutput {
    fn headers(&self) -> Vec<&str> {
        PositionRow::HEADERS.to_vec()
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows.iter().map(PositionRow::cells).collect()
    }

    fn pretty_rows(&self) -> Vec<Vec<String>> {
        self.rows.iter().map(PositionRow::pretty_cells).collect()
    }

    fn empty_message(&self) -> Option<&str> {
        Some("No open positions.")
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

#[derive(Debug, Clone, Serialize)]
struct PositionActionConfirmation {
    coin: String,
    action: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    leverage: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    margin_mode: Option<String>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[serde(skip_serializing_if = "Option::is_none")]
    amount: Option<Decimal>,
}

struct PositionActionOutput {
    rows: Vec<PositionActionConfirmation>,
}

impl TableData for PositionActionOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Coin",
            "Action",
            "Status",
            "Leverage",
            "Margin Mode",
            "Amount",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    row.coin.clone(),
                    row.action.clone(),
                    row.status.clone(),
                    row.leverage
                        .map(|leverage| leverage.to_string())
                        .unwrap_or_default(),
                    row.margin_mode.clone().unwrap_or_default(),
                    row.amount
                        .map(|amount| amount.to_string())
                        .unwrap_or_default(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

/// Fetch authenticated user's open positions without printing them.
async fn list_snapshot(
    client: &HttpClient,
    user: Address,
    dex: Option<&str>,
) -> Result<PositionsOutput, anyhow::Error> {
    let positions = client
        .clearinghouse_state(user, dex.map(str::to_string))
        .await
        .map_err(from_sdk)?
        .asset_positions;

    Ok(PositionsOutput {
        rows: PositionRow::open(positions),
    })
}

/// The perp a `--coin` names: its display name and protocol asset id.
fn resolve_perp_asset(resolver: &AssetResolver, coin: &str) -> Result<(String, usize), CliError> {
    let perp = resolver.resolve_perp(coin)?;
    Ok((perp.display_name(), perp.index))
}

/// Margin to add, in USDC micro-units.
fn margin_micro_units(amount: Decimal) -> Result<i64, CliError> {
    quantity::positive(amount, Scale::USDC).map_err(|err| match err {
        ScaleError::TooLarge => {
            CliError::Configuration("amount is too large for Hyperliquid micro-units".to_string())
        }
        err => err.config("amount", Scale::USDC),
    })
}

pub const GROUP: GroupSpec =
    GroupSpec::new(&["positions"], "Manage positions").aliases(&["position"]);

#[derive(Args, Debug, Clone)]
pub struct PositionsListArgs {
    #[command(flatten)]
    pub watch: SnapshotWatchArgs,
    /// Perp dex name (HIP-3). Defaults to the first dex.
    #[arg(long)]
    pub dex: Option<String>,
}

pub struct PositionsList;

impl CliCommand for PositionsList {
    type Args = PositionsListArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["positions", "list"],
        "account",
        "List open positions",
        "List positions for the configured signer.",
    )
    .risk(Risk::AccountState)
    .ows(OwsSupport::ExperimentalFeatureGated)
    .auth(true)
    .stream(StreamBound::SnapshotWatch)
    .args(&[
        arg("watch")
            .kind(InputKind::Boolean)
            .describe("Watch live snapshots. Use max_ticks to bound automation calls."),
        arg("max_ticks").describe("Stop after this many watch snapshots."),
        arg("dex").describe("Perp dex name (HIP-3). Defaults to the first dex."),
    ])
    .cli_aliases(&["ls"]);

    async fn run(inv: &Invocation, args: &PositionsListArgs) -> Result<(), anyhow::Error> {
        // Validate once, before either path: a watch must not start on a dex that
        // cannot be read, and both paths send the canonical spelling.
        let client = inv.remote()?.api().sdk();
        let dex = canonical_dex(client, args.dex.as_deref()).await?;
        if args.watch.watch {
            watch_positions(inv, dex.as_deref(), args.watch.max_ticks).await
        } else {
            let user = inv.resolve_signer()?.query_address();
            inv.print(&list_snapshot(client, user, dex.as_deref()).await?);
            Ok(())
        }
    }
}

pub struct PositionsUpdateLeverage;

impl CliCommand for PositionsUpdateLeverage {
    type Args = UpdateLeverageArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["positions", "update-leverage"],
        "trade",
        "Update leverage for a position",
        "Update leverage for a position.",
    )
    .args(&[
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Perpetual coin.")
            .resource_id("coin"),
        arg("leverage").describe("Target leverage."),
        arg("isolated")
            .kind(InputKind::Boolean)
            .describe("Use isolated margin mode."),
    ]);

    async fn run(inv: &Invocation, args: &UpdateLeverageArgs) -> Result<(), anyhow::Error> {
        let resolver = inv.trading_resolver().await?;
        let (coin, asset) = resolve_perp_asset(&resolver, &args.coin)?;
        let is_cross = !args.isolated;
        let margin_mode = if is_cross { "cross" } else { "isolated" };
        if inv.dry_run() {
            let (signer, acting_as) = dry_run_signing_addresses(inv);
            return print_dry_run(
                inv,
                "positions update-leverage",
                dry_run_signed_details(
                    inv,
                    "update_position_leverage",
                    serde_json::json!({
                        "coin": args.coin,
                        "resolved_coin": coin,
                        "asset": asset,
                        "network": inv.remote()?.network().to_string(),
                        "leverage": args.leverage,
                        "margin_mode": margin_mode,
                        "is_cross": is_cross,
                    }),
                    signer,
                    acting_as,
                    None,
                ),
            );
        }
        let resolved = inv.resolve_signer()?;
        actions::send_l1_action(
            inv.remote()?.api(),
            &resolved,
            Action::UpdateLeverage(UpdateLeverage {
                asset,
                is_cross,
                leverage: args.leverage,
            }),
            actions::nonce_now(),
        )
        .await?;

        inv.print(&PositionActionOutput {
            rows: vec![PositionActionConfirmation {
                coin,
                action: "update-leverage".to_string(),
                status: "updated".to_string(),
                leverage: Some(args.leverage),
                margin_mode: Some(margin_mode.to_string()),
                amount: None,
            }],
        });
        Ok(())
    }
}

pub struct PositionsUpdateMargin;

impl CliCommand for PositionsUpdateMargin {
    type Args = UpdateMarginArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["positions", "update-margin"],
        "trade",
        "Update margin for a position",
        "Add isolated margin.",
    )
    .args(&[
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Perpetual coin.")
            .resource_id("coin"),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("Margin amount."),
    ]);

    async fn run(inv: &Invocation, args: &UpdateMarginArgs) -> Result<(), anyhow::Error> {
        let ntli = margin_micro_units(args.amount)?;
        let resolver = inv.trading_resolver().await?;
        let (coin, asset) = resolve_perp_asset(&resolver, &args.coin)?;
        if inv.dry_run() {
            let (signer, acting_as) = dry_run_signing_addresses(inv);
            return print_dry_run(
                inv,
                "positions update-margin",
                dry_run_signed_details(
                    inv,
                    "update_isolated_margin",
                    serde_json::json!({
                        "coin": args.coin,
                        "resolved_coin": coin,
                        "asset": asset,
                        "network": inv.remote()?.network().to_string(),
                        "amount": args.amount.to_string(),
                        "ntli": ntli,
                        "margin_mode": "isolated",
                    }),
                    signer,
                    acting_as,
                    None,
                ),
            );
        }
        let resolved = inv.resolve_signer()?;
        actions::send_l1_action(
            inv.remote()?.api(),
            &resolved,
            Action::UpdateIsolatedMargin(UpdateIsolatedMargin {
                asset,
                is_buy: true,
                ntli,
            }),
            actions::nonce_now(),
        )
        .await?;

        inv.print(&PositionActionOutput {
            rows: vec![PositionActionConfirmation {
                coin,
                action: "update-margin".to_string(),
                status: "updated".to_string(),
                leverage: None,
                margin_mode: None,
                amount: Some(args.amount),
            }],
        });
        Ok(())
    }
}

async fn watch_positions(
    inv: &Invocation,
    dex: Option<&str>,
    max_ticks: Option<usize>,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let client = remote.api().sdk();
    let resolved = inv.resolve_signer()?;
    let user = resolved.query_address();
    let dex = dex.map(str::to_string);

    crate::watch::run_snapshot_watch(
        inv.session(),
        "Live positions",
        remote.chain(),
        vec![Subscription::WebData3 { user }],
        max_ticks,
        move || {
            let dex = dex.clone();
            async move { list_snapshot(client, user, dex.as_deref()).await }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(szi: &str, pnl: &str) -> AssetPosition {
        serde_json::from_value::<AssetPosition>(serde_json::json!({
            "type": "oneWay",
            "position": {
                "coin": "BTC",
                "szi": szi,
                "leverage": {"type": "cross", "value": 5},
                "entryPx": "50000",
                "positionValue": "5100",
                "unrealizedPnl": pnl,
                "returnOnEquity": "0.1",
                "liquidationPx": null,
                "marginUsed": "1000",
                "maxLeverage": 50,
                "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
            }
        }))
        .unwrap()
    }

    #[test]
    fn position_rows_compute_mark_from_value_and_abs_size() {
        let row = PositionRow::from(position("0.1", "100"));

        assert_eq!(row.mark, Decimal::from(51_000));
        assert_eq!(row.side, "long");
        assert_eq!(row.leverage.to_string(), "cross 5x");
    }

    #[test]
    fn closed_positions_are_not_listed() {
        let rows = PositionRow::open(vec![position("0", "0"), position("0.1", "100")]);

        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn position_json_keeps_decimal_strings_and_null_liquidation() {
        let json = serde_json::to_value(PositionRow::from(position("0.1", "100"))).unwrap();

        assert_eq!(json["entry"], "50000");
        assert_eq!(json["mark"], "51000");
        assert_eq!(json["leverage"], "cross 5x");
        assert!(json["liquidation_price"].is_null());
    }

    #[test]
    fn pretty_cells_colour_only_the_pnl_by_sign() {
        let gain = PositionRow::from(position("0.1", "100")).pretty_cells();
        let loss = PositionRow::from(position("0.1", "-5")).pretty_cells();

        assert_eq!(gain[PositionRow::PNL_COLUMN], output::colors::green("100"));
        assert_eq!(loss[PositionRow::PNL_COLUMN], output::colors::red("-5"));
        assert_eq!(gain[PositionRow::PNL_COLUMN - 1], "51000");
        assert_eq!(gain[9], output::colors::gray("n/a"));
    }

    #[test]
    fn update_margin_amount_must_be_positive_and_micro_unit_exact() {
        assert!(margin_micro_units(Decimal::new(100_123456, 6)).is_ok());
        assert!(margin_micro_units(Decimal::new(1, 7)).is_err());
        assert!(margin_micro_units(Decimal::ZERO).is_err());
        assert!(margin_micro_units(Decimal::MAX).is_err());
    }
}
