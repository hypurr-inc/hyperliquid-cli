use clap::Args;
use hypersdk::Address;
use hypersdk::Decimal;
use hypersdk::hypercore::Subscription;
use hypersdk::hypercore::api::UpdateLeverage;
use hypersdk::hypercore::types::api::{TopUpIsolatedOnlyMargin, UpdateIsolatedMargin};
use hypersdk::hypercore::types::{Action, LeverageType};
use serde::Serialize;

use crate::assets::canonical_dex;
use crate::assets::{AssetResolver, PerpAsset};
use crate::command_spec::{
    ArgSpec, CliCommand, CommandSpec, GroupSpec, InputKind, OwsSupport, Risk, arg,
};
use crate::commands::common::position::{PositionRow, find_position};
use crate::commands::common::rows::{ListRow, Row, Rows};
use crate::commands::common::support::{
    print_dry_run_acting_for, resolve_optional_acting_account_target,
};
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::exchange::actions;
use crate::hints::update_leverage_command;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::margin::{MarginModeArg, isolated_only_market};
use crate::quantity;
use crate::stream_bounds::SnapshotWatchArgs;
use crate::stream_bounds::StreamBound;

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

/// Arguments for `positions top-up-margin`.
#[derive(Args, Debug, Clone)]
pub struct TopUpMarginArgs {
    /// Perpetual coin (for example: BTC, ETH, SOL)
    #[arg(long)]
    pub coin: String,

    /// Target leverage: isolated margin is added until the position reaches it
    #[arg(long, allow_hyphen_values = true)]
    pub leverage: Decimal,

    /// Acting-account selector for vaultAddress: subaccount/vault address, stored account alias, or stored account id
    #[arg(long)]
    pub on_behalf_of: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PositionActionConfirmation {
    coin: String,
    action: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    leverage: Option<u32>,
    #[serde(with = "rust_decimal::serde::str_option")]
    #[serde(skip_serializing_if = "Option::is_none")]
    target_leverage: Option<Decimal>,
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
                .or_else(|| self.target_leverage.map(|leverage| leverage.to_string()))
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

async fn list_snapshot(
    client: &ApiClient,
    user: Address,
    dex: Option<&str>,
) -> Result<Rows<PositionRow>, anyhow::Error> {
    let positions = client
        .sdk()
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
    client: &ApiClient,
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
    let magnitude: i64 = quantity::usdc_units(amount.abs())?;
    Ok(if amount.is_sign_negative() {
        -magnitude
    } else {
        magnitude
    })
}

fn margin_direction(ntli: i64) -> &'static str {
    if ntli < 0 { "remove" } else { "add" }
}

/// Refuse a margin change on a position that has no isolated margin to change.
/// The exchange would reject it with a terse error.
async fn ensure_isolated_position(
    inv: &Invocation,
    account: Address,
    perp: &PerpAsset,
    on_behalf_of: Option<&str>,
) -> Result<(), CliError> {
    let coin = perp.display_name();
    match find_position(inv.remote()?.api(), account, perp, false).await? {
        None => Err(CliError::Unsupported(format!(
            "{account} has no open {coin} position, and isolated margin belongs to an open isolated position; open one first (set isolated mode with `{}`)",
            update_leverage_command(&coin, "<N>", true, on_behalf_of)
        ))),
        Some(position) if position.position.leverage.leverage_type != LeverageType::Isolated => {
            Err(CliError::Unsupported(format!(
                "the {coin} position of {account} uses cross margin, which has no isolated margin to change; switch it first with `{}`",
                update_leverage_command(
                    &coin,
                    position.position.leverage.value,
                    true,
                    on_behalf_of
                )
            )))
        }
        Some(_) => Ok(()),
    }
}

const ON_BEHALF_OF: ArgSpec = arg("on_behalf_of")
    .kind(InputKind::ActingAccountSelector)
    .describe(
        "Acting-account selector for vaultAddress: subaccount/vault address, wallet name, or wallet id.",
    )
    .resource_id("on-behalf-of selector");

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
        let client = inv.remote()?.api();
        let dex = canonical_dex(client, args.dex.as_deref()).await?;
        if args.watch.watch {
            watch_positions(inv, dex.as_deref(), args.watch.max_ticks).await
        } else {
            let user = inv.resolve_signer_account().await?;
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
        ON_BEHALF_OF,
    ]);

    async fn run(inv: &Invocation, args: &UpdateLeverageArgs) -> Result<(), anyhow::Error> {
        let resolver = inv.asset_resolver().await?;
        let perp = resolve_perp_asset(&resolver, inv.remote()?.api(), &args.coin).await?;
        let (coin, asset) = (perp.display_name(), perp.index);
        let mode = if args.isolated {
            MarginModeArg::Isolated
        } else {
            MarginModeArg::Cross
        };
        if !mode.is_available_on(&perp) {
            return Err(CliError::Unsupported(format!(
                "{}, so cross margin is not available; run `{}`",
                isolated_only_market(&perp),
                update_leverage_command(&coin, args.leverage, true, args.on_behalf_of.as_deref())
            ))
            .into());
        }
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
                    "margin_mode": mode.as_str(),
                    "is_cross": mode.is_cross(),
                    "on_behalf_of": args.on_behalf_of,
                }),
                vault_address.map(|address| address.to_string()),
            );
        }
        let resolved = inv.resolve_signer()?;
        actions::send_l1_action(
            inv.remote()?.api(),
            &resolved,
            Action::UpdateLeverage(UpdateLeverage {
                asset,
                is_cross: mode.is_cross(),
                leverage: args.leverage,
            }),
            vault_address,
            actions::EXCHANGE_ACTION,
        )
        .await?;

        inv.print(&Rows::new(vec![PositionActionConfirmation {
            coin,
            action: "update-leverage".to_string(),
            status: "updated".to_string(),
            leverage: Some(args.leverage),
            target_leverage: None,
            margin_mode: Some(mode.to_string()),
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
        ON_BEHALF_OF,
    ]);

    async fn run(inv: &Invocation, args: &UpdateMarginArgs) -> Result<(), anyhow::Error> {
        let ntli = margin_micro_units(args.amount)?;
        let resolver = inv.asset_resolver().await?;
        let perp = resolve_perp_asset(&resolver, inv.remote()?.api(), &args.coin).await?;
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
        ensure_isolated_position(inv, account, &perp, args.on_behalf_of.as_deref()).await?;
        actions::send_l1_action(
            inv.remote()?.api(),
            &resolved,
            Action::UpdateIsolatedMargin(UpdateIsolatedMargin {
                asset,
                is_buy: true,
                ntli,
            }),
            vault_address,
            actions::EXCHANGE_ACTION,
        )
        .await?;

        inv.print(&Rows::new(vec![PositionActionConfirmation {
            coin,
            action: "update-margin".to_string(),
            status: "updated".to_string(),
            leverage: None,
            target_leverage: None,
            margin_mode: None,
            amount: Some(args.amount),
        }]));
        Ok(())
    }
}

pub struct PositionsTopUpMargin;

impl CliCommand for PositionsTopUpMargin {
    type Args = TopUpMarginArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["positions", "top-up-margin"],
        "trade",
        "Top up isolated margin to a target leverage",
        "Add isolated margin to an open isolated position until it reaches a target leverage (topUpIsolatedOnlyMargin), instead of a USDC amount as `positions update-margin` takes.",
    )
    .args(&[
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Perpetual coin.")
            .resource_id("coin"),
        arg("leverage")
            .kind(InputKind::Amount)
            .describe("Target leverage, a positive number such as 5 or 12.5."),
        ON_BEHALF_OF,
    ]);

    async fn run(inv: &Invocation, args: &TopUpMarginArgs) -> Result<(), anyhow::Error> {
        quantity::require_positive(args.leverage, "leverage")?;
        let leverage = args.leverage.normalize();
        let resolver = inv.asset_resolver().await?;
        let perp = resolve_perp_asset(&resolver, inv.remote()?.api(), &args.coin).await?;
        let (coin, asset) = (perp.display_name(), perp.index);
        let vault_address =
            resolve_optional_acting_account_target(inv, args.on_behalf_of.as_deref())?;
        if inv.dry_run() {
            return print_dry_run_acting_for(
                inv,
                "top_up_isolated_margin",
                serde_json::json!({
                    "coin": args.coin,
                    "resolved_coin": coin,
                    "asset": asset,
                    "network": inv.remote()?.network().to_string(),
                    "target_leverage": leverage.to_string(),
                    "margin_mode": "isolated",
                    "on_behalf_of": args.on_behalf_of,
                }),
                vault_address.map(|address| address.to_string()),
            );
        }
        let resolved = inv.resolve_signer()?;
        let account = vault_address.unwrap_or_else(|| resolved.query_address());
        ensure_isolated_position(inv, account, &perp, args.on_behalf_of.as_deref()).await?;
        let asset_id = u32::try_from(asset).map_err(|_| {
            CliError::Internal(anyhow::anyhow!("asset id {asset} exceeds the wire range"))
        })?;
        actions::send_l1_action(
            inv.remote()?.api(),
            &resolved,
            Action::TopUpIsolatedOnlyMargin(TopUpIsolatedOnlyMargin {
                asset: asset_id,
                leverage,
            }),
            vault_address,
            actions::EXCHANGE_ACTION,
        )
        .await?;

        inv.print(&Rows::new(vec![PositionActionConfirmation {
            coin,
            action: "top-up-margin".to_string(),
            status: "updated".to_string(),
            leverage: None,
            target_leverage: Some(leverage),
            margin_mode: None,
            amount: None,
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
    let client = remote.api();
    let user = inv.resolve_signer_account().await?;
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
