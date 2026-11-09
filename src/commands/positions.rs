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

use crate::assets::canonical_dex;
use crate::assets::{AssetResolver, PerpAsset};
use crate::command_spec::{
    CliCommand, CommandSpec, GroupSpec, InputKind, OwsSupport, Risk, StreamBound, arg,
};
use crate::commands::actions;
use crate::commands::rows::{ListRow, MISSING, Row, Rows, or_missing, style_column};
use crate::commands::support::{print_dry_run_acting_for, resolve_optional_acting_account_target};
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::invocation::Invocation;
use crate::output;
use crate::quantity::{self, Scale};
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

    /// Use isolated margin mode instead of cross margin. Required on isolated-only markets
    #[arg(long)]
    pub isolated: bool,

    /// Acting-account selector for vaultAddress: subaccount/vault address, stored account alias, or stored account id
    #[arg(long)]
    pub on_behalf_of: Option<String>,
}

/// Arguments for `positions update-margin`.
#[derive(Args, Debug, Clone)]
pub struct UpdateMarginArgs {
    /// Perpetual coin (for example: BTC, ETH, SOL)
    #[arg(long)]
    pub coin: String,

    /// Isolated margin to add (positive) or remove (negative), in USDC
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,

    /// Acting-account selector for vaultAddress: subaccount/vault address, stored account alias, or stored account id
    #[arg(long)]
    pub on_behalf_of: Option<String>,
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
    /// The rows of the positions with a nonzero size.
    pub(crate) fn open(positions: Vec<AssetPosition>) -> Vec<Self> {
        positions
            .into_iter()
            .map(Self::from)
            .filter(|row| !row.size.is_zero())
            .collect()
    }
}

impl Row for PositionRow {
    const HEADERS: &'static [&'static str] = &[
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

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.side.to_string(),
            self.size.to_string(),
            or_missing(self.entry),
            self.mark.to_string(),
            self.unrealized_pnl.to_string(),
            self.position_value.to_string(),
            self.leverage.to_string(),
            self.margin_used.to_string(),
            or_missing(self.liquidation_price),
        ]
    }

    /// [`Self::cells`] with the PnL coloured by sign and missing values greyed.
    fn pretty_cells(&self) -> Vec<String> {
        let mut cells = self.cells();
        style_column::<Self>(&mut cells, "Unrealized PnL", |_| {
            pretty_pnl(self.unrealized_pnl)
        });
        for cell in &mut cells {
            if cell == MISSING {
                *cell = output::colors::gray(cell);
            }
        }
        cells
    }
}

impl ListRow for PositionRow {
    const EMPTY: &'static str = "No open positions.";
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

impl Row for PositionActionConfirmation {
    const HEADERS: &'static [&'static str] = &[
        "Coin",
        "Action",
        "Status",
        "Leverage",
        "Margin Mode",
        "Amount",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.action.clone(),
            self.status.clone(),
            self.leverage
                .map(|leverage| leverage.to_string())
                .unwrap_or_default(),
            self.margin_mode.clone().unwrap_or_default(),
            self.amount
                .map(|amount| amount.to_string())
                .unwrap_or_default(),
        ]
    }
}

impl ListRow for PositionActionConfirmation {
    const EMPTY: &'static str = "No results.";
}

/// Fetch authenticated user's open positions without printing them.
async fn list_snapshot(
    client: &HttpClient,
    user: Address,
    dex: Option<&str>,
) -> Result<Rows<PositionRow>, anyhow::Error> {
    let positions = client
        .clearinghouse_state(user, dex.map(str::to_string))
        .await
        .map_err(from_sdk)?
        .asset_positions;

    Ok(Rows::new(PositionRow::open(positions)))
}

/// The perp a `--coin` names. A HIP-3 market (`dex:COIN`) is looked up live
/// when the metadata snapshot lacks it, as orders do.
async fn resolve_perp_asset(
    resolver: &AssetResolver,
    client: &HttpClient,
    coin: &str,
) -> Result<PerpAsset, CliError> {
    Ok(resolver.resolve_perp_or_fetch(client, coin).await?)
}

/// A signed margin change in USDC micro-units: positive adds isolated margin,
/// negative removes it. Zero changes nothing and is refused.
fn margin_micro_units(amount: Decimal) -> Result<i64, CliError> {
    if amount.is_zero() {
        return Err(CliError::Configuration(
            "amount must not be zero; use a positive amount to add isolated margin or a negative amount to remove it"
                .to_string(),
        ));
    }
    let magnitude: i64 = quantity::positive(amount.abs(), Scale::USDC)
        .map_err(|err| err.config("amount", Scale::USDC))?;
    Ok(if amount.is_sign_negative() {
        -magnitude
    } else {
        magnitude
    })
}

/// Which way a margin change moves isolated margin, from its signed `ntli`.
fn margin_direction(ntli: i64) -> &'static str {
    if ntli < 0 { "remove" } else { "add" }
}

/// The ` --on-behalf-of <selector>` a suggested command needs to act on the
/// same account as this run.
fn on_behalf_of_flag(on_behalf_of: Option<&str>) -> String {
    on_behalf_of
        .map(|selector| format!(" --on-behalf-of {selector}"))
        .unwrap_or_default()
}

/// The command that puts `coin` into isolated margin mode at `leverage`.
fn isolate_command(coin: &str, leverage: impl fmt::Display, on_behalf_of: Option<&str>) -> String {
    format!(
        "positions update-leverage --coin {coin} --leverage {leverage} --isolated{}",
        on_behalf_of_flag(on_behalf_of)
    )
}

/// The open `coin` position of `account`, if any. A closed one is not open.
async fn open_position(
    client: &HttpClient,
    account: Address,
    perp: &PerpAsset,
) -> Result<Option<AssetPosition>, CliError> {
    let coin = perp.display_name();
    Ok(client
        .clearinghouse_state(account, perp.dex.clone())
        .await
        .map_err(from_sdk)?
        .asset_positions
        .into_iter()
        .find(|position| {
            position.position.coin.eq_ignore_ascii_case(&coin) && !position.position.szi.is_zero()
        }))
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
    .auth()
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
        "Set the leverage and margin mode (cross, or isolated with --isolated) the account uses for a perpetual market. Markets that trade isolated only refuse cross.",
    )
    .args(&[
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Perpetual coin.")
            .resource_id("coin"),
        arg("leverage").describe("Target leverage."),
        arg("isolated")
            .kind(InputKind::Boolean)
            .describe("Use isolated margin mode. Required on isolated-only markets."),
        arg("on_behalf_of").kind(InputKind::ActingAccountSelector).describe("Acting-account selector for vaultAddress: subaccount/vault address, wallet name, or wallet id.").resource_id("on-behalf-of selector"),
    ]);

    async fn run(inv: &Invocation, args: &UpdateLeverageArgs) -> Result<(), anyhow::Error> {
        let resolver = inv.trading_resolver().await?;
        let perp = resolve_perp_asset(&resolver, inv.remote()?.api().sdk(), &args.coin).await?;
        let (coin, asset) = (perp.display_name(), perp.index);
        let is_cross = !args.isolated;
        if is_cross && !perp.margin_rule.allows_cross() {
            return Err(CliError::Unsupported(format!(
                "{coin} is an isolated-only market ({}), so cross margin is not available; run `{}`",
                perp.margin_rule.as_str(),
                isolate_command(&coin, args.leverage, args.on_behalf_of.as_deref())
            ))
            .into());
        }
        let margin_mode = if is_cross { "cross" } else { "isolated" };
        let vault_address =
            resolve_optional_acting_account_target(inv, args.on_behalf_of.as_deref())?;
        if inv.dry_run() {
            return print_dry_run_acting_for(
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
                    "on_behalf_of": args.on_behalf_of,
                }),
                vault_address.map(|address| address.to_string()),
            );
        }
        let resolved = inv.resolve_signer()?;
        actions::send_l1_action_raw(
            inv.remote()?.api(),
            &resolved,
            Action::UpdateLeverage(UpdateLeverage {
                asset,
                is_cross,
                leverage: args.leverage,
            }),
            actions::nonce_now(),
            vault_address,
            "exchange action",
        )
        .await?;

        inv.print(&Rows::new(vec![PositionActionConfirmation {
            coin,
            action: "update-leverage".to_string(),
            status: "updated".to_string(),
            leverage: Some(args.leverage),
            margin_mode: Some(margin_mode.to_string()),
            amount: None,
        }]));
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
        "Add or remove isolated margin on an open isolated position: a positive amount adds, a negative amount removes. Removal is refused on strict-isolated markets.",
    )
    .args(&[
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Perpetual coin.")
            .resource_id("coin"),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("Margin amount in USDC: positive adds isolated margin, negative removes it."),
        arg("on_behalf_of").kind(InputKind::ActingAccountSelector).describe("Acting-account selector for vaultAddress: subaccount/vault address, wallet name, or wallet id.").resource_id("on-behalf-of selector"),
    ]);

    async fn run(inv: &Invocation, args: &UpdateMarginArgs) -> Result<(), anyhow::Error> {
        let ntli = margin_micro_units(args.amount)?;
        let resolver = inv.trading_resolver().await?;
        let perp = resolve_perp_asset(&resolver, inv.remote()?.api().sdk(), &args.coin).await?;
        let (coin, asset) = (perp.display_name(), perp.index);
        if ntli < 0 && perp.margin_rule.refuses_margin_removal() {
            return Err(CliError::Unsupported(format!(
                "{coin} is a strict-isolated market: isolated margin cannot be removed, it is released proportionally as the position closes"
            ))
            .into());
        }
        let vault_address =
            resolve_optional_acting_account_target(inv, args.on_behalf_of.as_deref())?;
        if inv.dry_run() {
            return print_dry_run_acting_for(
                inv,
                "update_isolated_margin",
                serde_json::json!({
                    "coin": args.coin,
                    "resolved_coin": coin,
                    "asset": asset,
                    "network": inv.remote()?.network().to_string(),
                    "amount": args.amount.to_string(),
                    "ntli": ntli,
                    "direction": margin_direction(ntli),
                    "margin_mode": "isolated",
                    "on_behalf_of": args.on_behalf_of,
                }),
                vault_address.map(|address| address.to_string()),
            );
        }
        let resolved = inv.resolve_signer()?;
        let account = vault_address.unwrap_or_else(|| resolved.query_address());
        let on_behalf_of = args.on_behalf_of.as_deref();
        // Isolated margin belongs to an open isolated position; the exchange
        // would reject anything else with a terse error.
        match open_position(inv.remote()?.api().sdk(), account, &perp).await? {
            None => {
                return Err(CliError::Unsupported(format!(
                    "{account} has no open {coin} position, and isolated margin belongs to an open isolated position; open one first (set isolated mode with `{}`)",
                    isolate_command(&coin, "<N>", on_behalf_of)
                ))
                .into());
            }
            Some(position)
                if position.position.leverage.leverage_type != LeverageType::Isolated =>
            {
                return Err(CliError::Unsupported(format!(
                    "the {coin} position of {account} uses cross margin, which has no isolated margin to change; switch it first with `{}`",
                    isolate_command(&coin, position.position.leverage.value, on_behalf_of)
                ))
                .into());
            }
            Some(_) => {}
        }
        actions::send_l1_action_raw(
            inv.remote()?.api(),
            &resolved,
            Action::UpdateIsolatedMargin(UpdateIsolatedMargin {
                asset,
                is_buy: true,
                ntli,
            }),
            actions::nonce_now(),
            vault_address,
            "exchange action",
        )
        .await?;

        inv.print(&Rows::new(vec![PositionActionConfirmation {
            coin,
            action: "update-margin".to_string(),
            status: "updated".to_string(),
            leverage: None,
            margin_mode: None,
            amount: Some(args.amount),
        }]));
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
        inv,
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

        let short = PositionRow::from(position("-0.1", "-20"));
        assert_eq!(short.mark, Decimal::from(51_000));
        assert_eq!(short.side, "short");
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
    fn update_margin_amount_is_signed_nonzero_and_micro_unit_exact() {
        assert_eq!(
            margin_micro_units(Decimal::new(100_123456, 6)).unwrap(),
            100_123_456
        );
        assert_eq!(margin_micro_units(Decimal::from(-5)).unwrap(), -5_000_000);
        assert_eq!(margin_micro_units(Decimal::new(-1, 6)).unwrap(), -1);
        assert!(margin_micro_units(Decimal::new(1, 7)).is_err());
        assert!(margin_micro_units(Decimal::new(-1, 7)).is_err());
        assert!(margin_micro_units(Decimal::ZERO).is_err());
        assert!(margin_micro_units(Decimal::MAX).is_err());
        assert!(margin_micro_units(Decimal::MIN).is_err());
    }

    #[test]
    fn margin_direction_follows_the_sign() {
        assert_eq!(margin_direction(1), "add");
        assert_eq!(margin_direction(-1), "remove");
    }
}
