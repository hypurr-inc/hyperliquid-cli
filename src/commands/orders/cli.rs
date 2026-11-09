use clap::Args;
use hypersdk::hypercore::Subscription;

use super::args::{
    BatchCreateArgs, BatchModifyArgs, BracketArgs, BuySellArgs, CancelAllArgs, CancelArgs,
    ChaseArgs, CreateArgs, ModifyArgs, OrderSide, OutcomeOrderArgs, ScaleArgs, ScheduleCancelArgs,
    StatusArgs, TpslArgs, TrailingStopArgs, TwapCancelArgs, TwapCreateArgs,
};
use super::bracket::{bracket, bracket_dry_run_plan, validate_bracket_args};
use super::chase::{chase, chase_dry_run_plan, chase_timeout};
use super::existing::{
    batch_modify_dry_run_plan, cancel_all_dry_run_plan, cancel_dry_run_plan, modify_dry_run_plan,
    validate_modify_args,
};
use super::planning::{
    OrderEcho, batch_create_dry_run_plan, create_dry_run_plan, prepare_batch_create_orders,
    prepare_create_order, prepare_scale_batch, prepare_schedule_cancel_plan,
    prepare_trailing_stop_plan, prepare_twap_cancel_plan, prepare_twap_create_plan, resolve_perp,
    scale_dry_run_plan, schedule_cancel_dry_run_plan, tpsl_dry_run_plan, twap_cancel_dry_run_plan,
    twap_create_dry_run_plan, validate_trailing_stop_args,
};
use super::queries::{history, open, status};
use super::run::OrderRun;
use super::submit::Gate;
use super::trailing_stop::trailing_stop;
use super::validation::{
    OrderSpec, ScaleLadder, TpslSpec, read_validated_batch_create_orders,
    read_validated_batch_modify_orders, validate_batch_create_args,
};
use super::{
    batch_modify, cancel, cancel_all, create, modify, place_batch, schedule_cancel, tpsl,
    twap_cancel, twap_create,
};
use crate::assets::DexArg;
use crate::cli_command::CliCommand;
use crate::clock;
use crate::command_spec::{
    ArgSpec, CommandSpec, GroupSpec, InputKind, NoArgs, OwsSupport, Risk, YES_FOR_AUTOMATION, arg,
};
use crate::commands::common::order_list::open_orders_output;
use crate::errors::CliError;
use crate::input_hardening::{FilePolicy, validate_file_path};
use crate::invocation::Invocation;
use crate::margin::MarginPlan;
use crate::stream_bounds::SnapshotWatchArgs;
use crate::stream_bounds::StreamBound;

pub const GROUP: GroupSpec = GroupSpec::new(&["orders"], "Manage orders").aliases(&["order"]);

macro_rules! prompts_unless_yes {
    ($orders:literal) => {
        concat!(
            "Mainnet ",
            $orders,
            " prompt for confirmation unless ",
            $crate::yes_flag!(),
            " is passed; --testnet ",
            $orders,
            " run without a prompt."
        )
    };
}

// What each order command says about the flags they share. A flag is described
// here once, so every command that takes it gets the same input hardening.

/// `--on-behalf-of`, hardened; a command adds [`ON_BEHALF_OF_DESCRIBED`] when
/// the schema should say more than the clap help.
const ON_BEHALF_OF: ArgSpec = arg("on_behalf_of")
    .kind(InputKind::ActingAccountSelector)
    .resource_id("on-behalf-of selector");
const ON_BEHALF_OF_DESCRIBED: ArgSpec = ON_BEHALF_OF.describe(
    "Acting-account selector for vaultAddress: subaccount/vault address, wallet name, or wallet id.",
);
const YES: ArgSpec = crate::command_spec::YES.describe(
    "Skip the mainnet confirmation prompt for deliberate automation. Testnet orders never prompt.",
);
const DEX: ArgSpec = arg("dex")
    .kind(InputKind::VenueSelector)
    .describe("HIP-3 DEX name.")
    .resource_id("dex");
const CLOID: ArgSpec = arg("cloid")
    .kind(InputKind::Identifier)
    .describe("0x-prefixed 16-byte client order id.")
    .resource_id("cloid");
const BUILDER: ArgSpec = arg("builder")
    .kind(InputKind::ProtocolObjectAddress)
    .describe("Builder address to receive the additional order fee. Requires --builder-fee-rate.")
    .resource_id("builder address");
const BUILDER_FEE_RATE: ArgSpec = arg("builder_fee_rate")
    .describe("Builder fee rate percent string, e.g. 0.001%. Requires --builder.")
    .resource_id("builder fee rate");
const SUB_BUILDER: ArgSpec = arg("sub_builder")
    .kind(InputKind::ProtocolObjectAddress)
    .describe("Sub-builder ADDRESS:SHARE, with SHARE a percent of the builder fee such as 25%. Up to 5; shares sum to at most 100%. Requires --builder.")
    .resource_id("sub-builder");
const PRIORITY_RATE: ArgSpec = arg("priority_rate");
const MAX_SLIPPAGE_BPS: ArgSpec = arg("max_slippage_bps").describe(
    "Maximum slippage bound in basis points for market orders. Default 500; allowed range 1-1000.",
);

macro_rules! margin_mode_help {
    ($orders:literal) => {
        concat!(
            "Margin mode intent for perpetual ",
            $orders,
            ". Defaults to the market's current margin mode; isolated-only markets always use isolated margin. Set the mode first with `positions update-leverage`."
        )
    };
}
const MARGIN_MODE: ArgSpec = arg("margin_mode").describe(margin_mode_help!("orders"));

macro_rules! priority_rate_help {
    ($rest:literal) => {
        concat!(
            "Order write priority rate in 1/100,000,000 of notional (80000 is 8 bps",
            $rest
        )
    };
}

/// The args `buy` and `sell` take: `orders create` without the trigger, TP/SL
/// and grouping flags, with `--side` preset by the command.
const BUY_SELL_ARGS: &[ArgSpec] = &[
    arg("coin")
        .kind(InputKind::MarketSymbol)
        .describe("Perpetual coin, spot pair, or outcome notation.")
        .asset_input("coin"),
    DEX,
    arg("price").kind(InputKind::Price),
    arg("size")
        .kind(InputKind::Amount)
        .describe("Base-asset size. Accepted for market and required for limit."),
    arg("amount").kind(InputKind::Amount),
    arg("order_type")
        .describe("Order type. Defaults to limit when --price is set, otherwise market."),
    arg("tif").describe("Time in force: gtc, ioc, or alo."),
    arg("reduce_only")
        .kind(InputKind::Boolean)
        .describe("Only reduce an existing perpetual position; spot markets are not supported."),
    MAX_SLIPPAGE_BPS,
    ON_BEHALF_OF_DESCRIBED,
    MARGIN_MODE,
    BUILDER,
    BUILDER_FEE_RATE,
    SUB_BUILDER,
    PRIORITY_RATE.describe(priority_rate_help!(
        "). Requires a limit order with --tif ioc, or --tif alo without --reduce-only."
    )),
    CLOID,
    YES,
];

/// The args `outcome-buy` and `outcome-sell` take: those of `buy` and `sell`
/// for outcome tokens, which take no order write priority.
const OUTCOME_ARGS: &[ArgSpec] = &[
    arg("coin").kind(InputKind::MarketSymbol).describe("Outcome notation such as #10 or +10.").asset_input("coin"),
    arg("price").kind(InputKind::Price).describe("Limit price. When omitted, attempts a market order."),
    arg("size").kind(InputKind::Amount).describe("Outcome token size."),
    DEX,
    arg("amount").kind(InputKind::Amount),
    arg("order_type").describe("Order type. Defaults to limit when --price is set, otherwise market."),
    arg("tif").describe("Time in force: gtc, ioc, or alo."),
    arg("reduce_only").kind(InputKind::Boolean).describe("Only reduce an existing perpetual position; spot markets are not supported."),
    MAX_SLIPPAGE_BPS,
    ON_BEHALF_OF_DESCRIBED,
    MARGIN_MODE,
    BUILDER,
    BUILDER_FEE_RATE,
    SUB_BUILDER.describe("Sub-builder ADDRESS:SHARE, with SHARE a percent of the builder fee such as 25%. Requires --builder."),
    PRIORITY_RATE.describe("Order write priority is not accepted on outcome markets; the flag is refused."),
    CLOID,
    YES,
];

#[derive(Args, Debug, Clone)]
pub struct OpenOrdersArgs {
    #[command(flatten)]
    pub watch: SnapshotWatchArgs,
    /// Perp dex name (HIP-3). Defaults to the first dex.
    #[arg(long)]
    pub dex: Option<DexArg>,
}

pub struct OrdersOpen;

impl CliCommand for OrdersOpen {
    type Args = OpenOrdersArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["orders", "open"],
        "account",
        "List open orders",
        "List open orders for the configured signer.",
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
    ]);

    async fn run(inv: &Invocation, args: &OpenOrdersArgs) -> Result<(), anyhow::Error> {
        // Validate once, before either path: a watch must not start on a dex that
        // cannot be read, and both paths send the canonical spelling.
        let dex = inv.dex_name(args.dex.as_ref()).await?;
        if args.watch.watch {
            watch_open_orders(inv, dex.as_deref(), args.watch.max_ticks).await
        } else {
            let resolver = inv.asset_resolver().await?;
            let user = inv.resolve_signer_account().await?;
            open(inv, &resolver, user, dex.as_deref()).await
        }
    }
}

pub struct OrdersStatus;

impl CliCommand for OrdersStatus {
    type Args = StatusArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["orders", "status"],
        "account",
        "Show an order's status by OID or CLOID",
        "Query public order status by OID or CLOID.",
    )
    .one_of_required(&[&["oid"], &["cloid"]])
    .args(&[
        arg("user")
            .kind(InputKind::PublicUserSelector)
            .describe("Ethereum address, wallet name, or wallet id.")
            .resource_id("account selector"),
        arg("oid")
            .kind(InputKind::Identifier)
            .describe("Exchange-assigned order id."),
        arg("cloid")
            .kind(InputKind::Identifier)
            .describe("Client order id.")
            .resource_id("cloid"),
    ]);

    async fn run(inv: &Invocation, args: &StatusArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_account(&args.user)?;
        status(inv, user, args).await
    }
}

pub struct OrdersHistory;

impl CliCommand for OrdersHistory {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["orders", "history"],
        "account",
        "Show order history",
        "List order history for the configured signer.",
    )
    .risk(Risk::AccountState)
    .ows(OwsSupport::ExperimentalFeatureGated)
    .auth();

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        let resolver = inv.asset_resolver().await?;
        let user = inv.resolve_signer_account().await?;
        history(inv, &resolver, user).await
    }
}

pub struct OrdersCreate;

impl CliCommand for OrdersCreate {
    type Args = CreateArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "create"],
        "trade",
        "Create a new order",
        concat!("Create a live limit, market, stop-loss, take-profit, stop-limit, take-limit, or verified outcome limit order. Supports optional per-order builder fee params. ", prompts_unless_yes!("orders")),
    )
    .long_about("Create a new order.\n\
\n\
Examples:\n  \
hyperliquid --dry-run orders create --coin BTC --side buy --price 60000 --size 0.001\n  \
hyperliquid orders create --coin BTC --side buy --price 60000 --size 0.001 --yes")
    .prompt()
    .testnet_unprompted()
    .args(&[
        arg("coin").kind(InputKind::MarketSymbol).describe("Perpetual coin such as BTC, supported spot pair, or outcome notation (#<encoding> or +<encoding>) for verified outcome limit orders.").asset_input("coin"),
        DEX,
        arg("side"),
        arg("price").kind(InputKind::Price).describe("Limit price. For stop-loss and take-profit market trigger orders, also accepted as a legacy trigger price."),
        arg("trigger_price").kind(InputKind::Price),
        arg("size").kind(InputKind::Amount).describe("Base-asset size. Required for limit and trigger-limit orders."),
        arg("amount").kind(InputKind::Amount).describe("Quote-sized amount in USDC. Required for market orders."),
        arg("order_type").describe("Order type: limit, market, market-trigger, or trigger-limit."),
        arg("tif").describe("Time in force: gtc, ioc, or alo."),
        arg("reduce_only").kind(InputKind::Boolean).describe("Only reduce an existing perpetual position; spot markets are not supported."),
        MAX_SLIPPAGE_BPS,
        arg("take_profit").kind(InputKind::Price).describe("Take-profit trigger price for a normal-tpsl child order."),
        arg("stop_loss").kind(InputKind::Price).describe("Stop-loss trigger price for a normal-tpsl child order."),
        arg("grouping").describe("Parent-attached TP/SL grouping. orders create requires normal-tpsl when TP/SL children are present."),
        ON_BEHALF_OF_DESCRIBED,
        MARGIN_MODE,
        BUILDER,
        BUILDER_FEE_RATE,
        SUB_BUILDER,
        PRIORITY_RATE.describe(priority_rate_help!("; max 100000000). Requires a limit order with --tif ioc, or --tif alo without --reduce-only; cannot combine with --grouping, --take-profit or --stop-loss.")),
        CLOID,
        YES,
    ]);

    async fn run(inv: &Invocation, args: &CreateArgs) -> Result<(), anyhow::Error> {
        let spec = OrderSpec::from_create(args)?;
        let run = OrderRun::new(inv, args.on_behalf_of.as_deref(), args.yes)?;
        let client = inv.remote()?.api();
        let resolver = inv.asset_resolver().await?;
        let submission = prepare_create_order(client, &resolver, inv.settings(), &spec).await?;
        if inv.dry_run() {
            let parent = submission.parent();
            let margin = run
                .previewed_margin_of(parent.perp(), parent.margin)
                .await?;
            let mut plan =
                create_dry_run_plan(&submission, &OrderEcho::from(args), margin.as_ref())?;
            if let Some(effects) = run.hip3_side_effects(&[parent]).await? {
                plan = plan.with_extra("side_effects", effects);
            }
            if let Some(approval) = run.builder_approval(parent.builder.as_ref()).await? {
                plan = plan.with_extra("builder_approval", approval);
            }
            return run.preview(plan);
        }
        let signer = inv.resolve_signer()?;
        create(run.context(&signer)?, submission).await
    }
}

pub struct OrdersScale;

impl CliCommand for OrdersScale {
    type Args = ScaleArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "scale"],
        "trade",
        "Create a deterministic batch of scaled limit orders",
        concat!(
            "Create a deterministic batch of evenly spaced limit orders. ",
            prompts_unless_yes!("orders")
        ),
    )
    .prompt()
    .testnet_unprompted()
    .args(&[
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Perpetual coin, for example BTC.")
            .asset_input("coin"),
        DEX,
        arg("side").describe("Order side for every generated leg."),
        arg("start_price"),
        arg("end_price"),
        arg("total_size")
            .kind(InputKind::Amount)
            .describe("Total base-asset size split across all legs."),
        arg("orders").describe("Number of limit orders to generate."),
        arg("tif").describe("Time in force for generated limit orders: gtc, ioc, or alo."),
        arg("reduce_only").kind(InputKind::Boolean),
        ON_BEHALF_OF_DESCRIBED,
        MARGIN_MODE,
        PRIORITY_RATE.describe(priority_rate_help!(
            "). Requires --tif ioc, or --tif alo without --reduce-only."
        )),
        YES,
    ]);

    async fn run(inv: &Invocation, args: &ScaleArgs) -> Result<(), anyhow::Error> {
        let ladder = ScaleLadder::from_args(args)?;
        let run = OrderRun::new(inv, args.on_behalf_of.as_deref(), args.yes)?;
        let client = inv.remote()?.api();
        let resolver = inv.asset_resolver().await?;
        let prepared =
            prepare_scale_batch(client, &resolver, inv.settings(), args, &ladder).await?;
        if inv.dry_run() {
            let (perp, margin) = prepared
                .margin_check()
                .map_or((None, None), |(perp, margin)| (Some(perp), Some(*margin)));
            let margin = run.previewed_margin_of(perp, margin).await?;
            return run.preview(scale_dry_run_plan(&prepared, margin.as_ref()));
        }
        let signer = inv.resolve_signer()?;
        place_batch(run.context(&signer)?, &prepared, Gate::scale).await
    }
}

pub struct OrdersBatchCreate;

impl CliCommand for OrdersBatchCreate {
    type Args = BatchCreateArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "batch-create"],
        "trade",
        "Create a batch of limit orders from a JSON file",
        concat!(
            "Create a batch of limit orders from a JSON order-leg file. ",
            prompts_unless_yes!("orders")
        ),
    )
    .prompt()
    .testnet_unprompted()
    .args(&[
        arg("orders_file").kind(InputKind::FilePath).describe(
            "JSON file containing an array of order legs or an object with an orders array.",
        ),
        ON_BEHALF_OF_DESCRIBED,
        PRIORITY_RATE.describe(priority_rate_help!(
            "). Every leg must be IOC, or every leg ALO and not reduce-only."
        )),
        YES,
    ]);

    fn validate(args: &BatchCreateArgs) -> Result<(), CliError> {
        validate_file_path(
            &args.orders_file.to_string_lossy(),
            FilePolicy::json_artifact("orders file"),
        )?;
        Ok(())
    }

    async fn run(inv: &Invocation, args: &BatchCreateArgs) -> Result<(), anyhow::Error> {
        validate_batch_create_args(args)?;
        let orders = read_validated_batch_create_orders(args)?;
        let run = OrderRun::new(inv, args.on_behalf_of.as_deref(), args.yes)?;
        let client = inv.remote()?.api();
        let resolver = inv.asset_resolver().await?;
        let prepared = prepare_batch_create_orders(
            client,
            &resolver,
            inv.settings(),
            orders,
            args.priority_rate,
        )
        .await?;
        if inv.dry_run() {
            return run.preview(batch_create_dry_run_plan(&prepared, args));
        }
        let signer = inv.resolve_signer()?;
        place_batch(run.context(&signer)?, &prepared, Gate::batch_create).await
    }
}

pub struct OrdersTpsl;

impl CliCommand for OrdersTpsl {
    type Args = TpslArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "tpsl"],
        "trade",
        "Create position-attached TP/SL orders",
        concat!("Create position-attached take-profit and/or stop-loss orders. Dry-run first. Retry: check_open_orders — a second tpsl can duplicate working triggers. ", prompts_unless_yes!("orders")),
    )
    .long_about("Create position-attached take-profit and/or stop-loss orders.\n\
\n\
Triggers accept an absolute price, a percent offset (`+10%`, `-5%`), or `entry`.\n\
\n\
Retry: check open orders first. A second tpsl can duplicate working triggers.\n\
\n\
Examples:\n  \
hyperliquid --format json --dry-run orders tpsl --coin ETH --take-profit +10% --stop-loss -5%\n  \
hyperliquid --format json --dry-run orders tpsl --coin ETH --stop-loss entry\n  \
hyperliquid --format json orders tpsl --coin ETH --take-profit +10% --stop-loss -5% --yes")
    .prompt()
    .testnet_unprompted()
    .args(&[
        arg("coin").kind(InputKind::MarketSymbol).describe("Perpetual coin, for example BTC.").asset_input("coin"),
        DEX,
        ON_BEHALF_OF,
        arg("take_profit").kind(InputKind::Price).describe("Take-profit trigger: absolute price, percent offset such as +10%, or entry."),
        arg("stop_loss").kind(InputKind::Price).describe("Stop-loss trigger: absolute price, percent offset such as -5%, or entry."),
        arg("grouping"),
        arg("side").describe("Fixed close side. Omit live to infer from the current position."),
        arg("size").kind(InputKind::Amount).describe("Fixed base-asset size. Omit live to use the current position size."),
        MARGIN_MODE.describe(margin_mode_help!("TP/SL orders")),
        CLOID,
        YES,
    ]);

    async fn run(inv: &Invocation, args: &TpslArgs) -> Result<(), anyhow::Error> {
        let spec = TpslSpec::from_flags(args)?;
        let run = OrderRun::new(inv, args.on_behalf_of.as_deref(), args.yes)?;
        let client = inv.remote()?.api();
        let resolver = inv.asset_resolver().await?;
        let perp = resolve_perp(client, &resolver, spec.dex.as_deref(), &spec.coin).await?;
        if inv.dry_run() {
            let margin = MarginPlan::for_market(&perp, spec.margin_mode)?;
            let margin = run.previewed_margin(&perp, margin).await?;
            return run.preview(tpsl_dry_run_plan(&perp, &spec, &margin)?);
        }
        let signer = inv.resolve_signer()?;
        tpsl(run.context(&signer)?, &perp, &spec).await
    }
}

pub struct OrdersTrailingStop;

impl CliCommand for OrdersTrailingStop {
    type Args = TrailingStopArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "trailing-stop"],
        "trade",
        "Place a native trailing stop on a perp market",
        concat!("Place a native trailing stop on a perp market. It tracks the mark price and fires a market order once the mark retraces from its best level since activation. Dry-run first. Retry: check_open_orders — a second trailing stop can duplicate a working one. ", prompts_unless_yes!("orders")),
    )
    .long_about("Place a native trailing stop on a perp market.\n\
\n\
The stop tracks the mark price and fires a market order once the mark retraces from its best level since activation by --retracement-pct or --retracement-px. A sell trails the highest mark (closes a long); a buy trails the lowest (closes a short). The trigger never moves back. --activation-price delays tracking until the mark reaches it.\n\
\n\
Retry: check open orders first. A second trailing stop can duplicate a working one.\n\
\n\
Examples:\n  \
hyperliquid --format json --dry-run orders trailing-stop --coin BTC --side sell --size 0.01 --retracement-pct 5 --reduce-only\n  \
hyperliquid --format json orders trailing-stop --coin ETH --side sell --size 0.5 --retracement-px 50 --activation-price 3500 --yes")
    .prompt()
    .testnet_unprompted()
    .one_of_required(&[&["retracement_pct"], &["retracement_px"]])
    .args(&[
        arg("coin").kind(InputKind::MarketSymbol).describe("Perpetual coin, for example BTC. Spot and outcome markets are not supported.").asset_input("coin"),
        DEX,
        arg("side"),
        arg("size").kind(InputKind::Amount),
        arg("retracement_pct").describe("Retracement from the best mark price as a percent, above 0 and below 100. Use this or --retracement-px."),
        arg("retracement_px").kind(InputKind::Price),
        arg("activation_price").kind(InputKind::Price),
        arg("reduce_only").kind(InputKind::Boolean),
        ON_BEHALF_OF,
        YES,
    ]);

    async fn run(inv: &Invocation, args: &TrailingStopArgs) -> Result<(), anyhow::Error> {
        validate_trailing_stop_args(args)?;
        let run = OrderRun::new(inv, args.on_behalf_of.as_deref(), args.yes)?;
        let client = inv.remote()?.api();
        let resolver = inv.asset_resolver().await?;
        let perp = resolve_perp(client, &resolver, args.dex.as_deref(), &args.coin).await?;
        let plan = prepare_trailing_stop_plan(&perp, args)?;
        if inv.dry_run() {
            return run.preview(plan.dry_run_plan(args));
        }
        let signer = inv.resolve_signer()?;
        trailing_stop(run.context(&signer)?, &plan, run.on_behalf_of()).await
    }
}

pub struct OrdersCancel;

impl CliCommand for OrdersCancel {
    type Args = CancelArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "cancel"],
        "trade",
        "Cancel an order by ID",
        "Cancel an order.",
    )
    .args(&[
        arg("order_id")
            .schema_string()
            .kind(InputKind::Identifier)
            .describe("Exchange-assigned order id."),
        arg("cloid")
            .kind(InputKind::Identifier)
            .describe("Client order id.")
            .resource_id("cloid"),
        ON_BEHALF_OF,
    ]);

    async fn run(inv: &Invocation, args: &CancelArgs) -> Result<(), anyhow::Error> {
        let run = OrderRun::unprompted(inv, args.on_behalf_of.as_deref())?;
        if inv.dry_run() {
            return run.preview(cancel_dry_run_plan(args)?);
        }
        let resolver = inv.asset_resolver().await?;
        let signer = inv.resolve_signer()?;
        cancel(run.context(&signer)?, &resolver, args).await
    }
}

pub struct OrdersCancelAll;

impl CliCommand for OrdersCancelAll {
    type Args = CancelAllArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "cancel-all"],
        "trade",
        "Cancel all open orders",
        "Cancel all open orders, optionally filtered by coin.",
    )
    .prompt()
    .args(&[
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Optional coin filter.")
            .asset_input("coin"),
        DEX.describe("HIP-3 DEX name for the coin filter."),
        ON_BEHALF_OF,
        YES_FOR_AUTOMATION,
    ]);

    async fn run(inv: &Invocation, args: &CancelAllArgs) -> Result<(), anyhow::Error> {
        let run = OrderRun::new(inv, args.on_behalf_of.as_deref(), args.yes)?;
        let resolver = inv.asset_resolver().await?;
        if inv.dry_run() {
            let client = inv.remote()?.api();
            return run.preview(cancel_all_dry_run_plan(inv, client, &resolver, args).await?);
        }
        let signer = inv.resolve_signer()?;
        cancel_all(run.context(&signer)?, &resolver, args).await
    }
}

pub struct OrdersModify;

impl CliCommand for OrdersModify {
    type Args = ModifyArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "modify"],
        "trade",
        "Modify an existing order",
        "Modify an existing order.",
    )
    .one_of_required(&[&["order_id"], &["cloid"]])
    .args(&[
        arg("order_id")
            .kind(InputKind::Identifier)
            .describe("Exchange-assigned order id."),
        CLOID,
        arg("price")
            .kind(InputKind::Price)
            .describe("Replacement price."),
        arg("trigger_price")
            .kind(InputKind::Price)
            .describe("Replacement trigger price for trigger orders."),
        arg("size")
            .kind(InputKind::Amount)
            .describe("Replacement size."),
        arg("always_place").kind(InputKind::Boolean),
        ON_BEHALF_OF,
    ]);

    async fn run(inv: &Invocation, args: &ModifyArgs) -> Result<(), anyhow::Error> {
        let identifier = validate_modify_args(args)?;
        let run = OrderRun::unprompted(inv, args.on_behalf_of.as_deref())?;
        if inv.dry_run() {
            return run.preview(modify_dry_run_plan(args));
        }
        let resolver = inv.asset_resolver().await?;
        let signer = inv.resolve_signer()?;
        modify(run.context(&signer)?, &resolver, &identifier, args).await
    }
}

pub struct OrdersBatchModify;

impl CliCommand for OrdersBatchModify {
    type Args = BatchModifyArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "batch-modify"],
        "trade",
        "Modify several existing orders in one request from a JSON file",
        concat!("Modify several existing orders in one batchModify action from a JSON order file; each modification names an order by order_id or cloid and the price, trigger_price or size to replace. ", prompts_unless_yes!("modifications")),
    )
    .long_about("Modify several existing orders in one request.\n\
\n\
The orders file is a JSON array (or an object with an `orders` array) of modifications, each with `order_id` or `cloid` and at least one of `price`, `trigger_price`, `size`. Each order keeps its type, side, reduce-only flag and cloid, as with `orders modify`.\n\
\n\
Examples:\n  \
hyperliquid --format json --dry-run orders batch-modify --orders-file modifications.json\n  \
hyperliquid --format json orders batch-modify --orders-file modifications.json --yes")
    .prompt()
    .testnet_unprompted()
    .args(&[
        arg("orders_file").kind(InputKind::FilePath).describe("JSON file containing an array of modifications or an object with an orders array."),
        arg("always_place").kind(InputKind::Boolean).describe("Place each replacement even if its cancel fails. Required to modify trigger orders or place an IOC replacement."),
        ON_BEHALF_OF_DESCRIBED,
        YES.describe("Skip the mainnet confirmation prompt for deliberate automation. Testnet modifications never prompt."),
    ]);

    fn validate(args: &BatchModifyArgs) -> Result<(), CliError> {
        validate_file_path(
            &args.orders_file.to_string_lossy(),
            FilePolicy::json_artifact("orders file"),
        )?;
        Ok(())
    }

    async fn run(inv: &Invocation, args: &BatchModifyArgs) -> Result<(), anyhow::Error> {
        let orders = read_validated_batch_modify_orders(args)?;
        let run = OrderRun::new(inv, args.on_behalf_of.as_deref(), args.yes)?;
        if inv.dry_run() {
            return run.preview(batch_modify_dry_run_plan(&orders, args));
        }
        let resolver = inv.asset_resolver().await?;
        let signer = inv.resolve_signer()?;
        batch_modify(run.context(&signer)?, &resolver, &orders, args.always_place).await
    }
}

pub struct OrdersTwapCreate;

impl CliCommand for OrdersTwapCreate {
    type Args = TwapCreateArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "twap-create"],
        "trade",
        "Create a TWAP order",
        concat!("Create a TWAP order. ", prompts_unless_yes!("orders")),
    )
    .prompt()
    .testnet_unprompted()
    .args(&[
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Perpetual coin, for example BTC.")
            .asset_input("coin"),
        DEX,
        ON_BEHALF_OF,
        arg("side"),
        arg("size")
            .kind(InputKind::Amount)
            .describe("Total base-asset size."),
        arg("duration")
            .kind(InputKind::Duration)
            .describe("Whole-minute duration in seconds. Minimum 300."),
        MARGIN_MODE.describe(margin_mode_help!("TWAP orders")),
        arg("randomize")
            .kind(InputKind::Boolean)
            .describe("Randomize execution timing and suborder sizes up to ±20%."),
        YES,
    ]);

    async fn run(inv: &Invocation, args: &TwapCreateArgs) -> Result<(), anyhow::Error> {
        let run = OrderRun::new(inv, args.on_behalf_of.as_deref(), args.yes)?;
        let client = inv.remote()?.api();
        let resolver = inv.asset_resolver().await?;
        let perp = resolve_perp(client, &resolver, args.dex.as_deref(), &args.coin).await?;
        let plan = prepare_twap_create_plan(&perp, args)?;
        if inv.dry_run() {
            let margin = run.previewed_margin(&plan.perp, plan.margin).await?;
            return run.preview(twap_create_dry_run_plan(&plan, args, &margin));
        }
        let signer = inv.resolve_signer()?;
        twap_create(run.context(&signer)?, plan).await
    }
}

pub struct OrdersTwapCancel;

impl CliCommand for OrdersTwapCancel {
    type Args = TwapCancelArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "twap-cancel"],
        "trade",
        "Cancel a TWAP order",
        "Cancel a TWAP order.",
    )
    .args(&[
        arg("twap_id")
            .kind(InputKind::Identifier)
            .describe("TWAP id."),
        arg("coin")
            .kind(InputKind::MarketSymbol)
            .describe("Perpetual coin.")
            .asset_input("coin"),
        DEX,
        ON_BEHALF_OF,
    ]);

    async fn run(inv: &Invocation, args: &TwapCancelArgs) -> Result<(), anyhow::Error> {
        let run = OrderRun::unprompted(inv, args.on_behalf_of.as_deref())?;
        let client = inv.remote()?.api();
        let resolver = inv.asset_resolver().await?;
        let perp = resolve_perp(client, &resolver, args.dex.as_deref(), &args.coin).await?;
        let plan = prepare_twap_cancel_plan(&perp, args);
        if inv.dry_run() {
            return run.preview(twap_cancel_dry_run_plan(&plan, args));
        }
        let signer = inv.resolve_signer()?;
        twap_cancel(run.context(&signer)?, plan).await
    }
}

pub struct OrdersScheduleCancel;

impl CliCommand for OrdersScheduleCancel {
    type Args = ScheduleCancelArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "schedule-cancel"],
        "trade",
        "Schedule a cancel-all (dead man's switch)",
        concat!(
            "Configure a dead man's switch. ",
            prompts_unless_yes!("orders")
        ),
    )
    .prompt()
    .testnet_unprompted()
    .args(&[
        arg("in_duration")
            .kind(InputKind::Duration)
            .describe("Relative duration, for example 30s or 5m."),
        arg("clear").kind(InputKind::Boolean).schema_string(),
        ON_BEHALF_OF,
        YES,
    ]);

    async fn run(inv: &Invocation, args: &ScheduleCancelArgs) -> Result<(), anyhow::Error> {
        let plan = prepare_schedule_cancel_plan(args)?;
        let run = OrderRun::new(inv, args.on_behalf_of.as_deref(), args.yes)?;
        if inv.dry_run() {
            return run.preview(schedule_cancel_dry_run_plan(&plan, clock::now_utc()));
        }
        let signer = inv.resolve_signer()?;
        schedule_cancel(run.context(&signer)?, plan).await
    }
}

pub struct OrdersChase;

impl CliCommand for OrdersChase {
    type Args = ChaseArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "chase"],
        "trade",
        "Chase mid with ALO requotes",
        concat!("Bounded ALO requote loop that chases mid by a basis-point offset until fill, timeout, or max chase distance. Dry-run first. Retry: check_open_orders — not idempotent. Agent mode requires --timeout. ", prompts_unless_yes!("orders")),
    )
    .long_about("Bounded ALO requote loop that chases mid until fill, timeout, or max chase distance.\n\
\n\
Agent/non-TTY mode requires --timeout. Retry: check open orders first. Not idempotent.\n\
\n\
Examples:\n  \
hyperliquid --format json --dry-run orders chase --coin ETH --side buy --size 0.5 --timeout 60s\n  \
hyperliquid --format json orders chase --coin ETH --side buy --size 0.5 --timeout 60s --yes")
    .prompt()
    .testnet_unprompted()
    .args(&[
        arg("coin").kind(InputKind::MarketSymbol).describe("Perpetual coin, for example ETH.").asset_input("coin"),
        DEX,
        arg("side"),
        arg("size").kind(InputKind::Amount),
        arg("offset").describe("Offset from mid in basis points."),
        arg("timeout").kind(InputKind::Duration).describe("Max chase duration, for example 60s. Required in agent mode."),
        arg("interval").kind(InputKind::Duration),
        arg("max_chase").describe("Cancel and stop if mid travels this many basis points from the first mid."),
        arg("reduce_only").kind(InputKind::Boolean).describe("Only reduce an existing perpetual position; spot markets are not supported."),
        ON_BEHALF_OF_DESCRIBED,
        YES,
    ]);

    async fn run(inv: &Invocation, args: &ChaseArgs) -> Result<(), anyhow::Error> {
        let timeout = chase_timeout(inv, args)?;
        let run = OrderRun::new(inv, args.on_behalf_of.as_deref(), args.yes)?;
        let client = inv.remote()?.api();
        let resolver = inv.asset_resolver().await?;
        if inv.dry_run() {
            let plan = chase_dry_run_plan(client, &resolver, inv.settings(), args, timeout).await?;
            return run.preview(plan);
        }
        let signer = inv.resolve_signer()?;
        chase(run.context(&signer)?, &resolver, args, timeout).await
    }
}

pub struct OrdersBracket;

impl CliCommand for OrdersBracket {
    type Args = BracketArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["orders", "bracket"],
        "trade",
        "Create an entry order with linked TP/SL",
        concat!("Place an entry order, then arm position TP/SL. Limit entries wait for fill up to --entry-timeout. Dry-run first. Retry: check_open_orders — not idempotent. Emits one result object. ", prompts_unless_yes!("orders")),
    )
    .long_about("Place an entry order, then arm position TP/SL. Limit entries wait for fill up to --entry-timeout.\n\
\n\
Retry: check open orders first. Not idempotent. On limit-entry timeout the remaining entry is cancelled and any filled portion is protected.\n\
\n\
Examples:\n  \
hyperliquid --format json --dry-run orders bracket --coin ETH --side buy --size 0.1 --take-profit +10% --stop-loss -5%\n  \
hyperliquid --format json --dry-run orders bracket --coin ETH --side buy --entry limit --price 3000 --size 0.1 --take-profit +10% --stop-loss -5%\n  \
hyperliquid --format json orders bracket --coin ETH --side buy --size 0.1 --take-profit +10% --stop-loss -5% --yes")
    .prompt()
    .testnet_unprompted()
    .args(&[
        arg("coin").kind(InputKind::MarketSymbol).describe("Perpetual coin, for example ETH.").asset_input("coin"),
        arg("side"),
        arg("size").kind(InputKind::Amount).describe("Base-asset size."),
        arg("amount").kind(InputKind::Amount).describe("Quote-sized amount for market entry."),
        arg("entry").describe("Entry order type."),
        arg("price").kind(InputKind::Price).describe("Limit entry price."),
        arg("take_profit").kind(InputKind::Price).describe("Take-profit trigger: absolute price, +10%, or entry."),
        arg("stop_loss").kind(InputKind::Price).describe("Stop-loss trigger: absolute price, -5%, or entry."),
        arg("entry_timeout").kind(InputKind::Duration).describe("How long to wait for a limit entry fill before the remaining entry is cancelled and any filled portion is protected."),
        DEX,
        MAX_SLIPPAGE_BPS,
        ON_BEHALF_OF_DESCRIBED,
        YES,
    ]);

    async fn run(inv: &Invocation, args: &BracketArgs) -> Result<(), anyhow::Error> {
        validate_bracket_args(args)?;
        let run = OrderRun::new(inv, args.on_behalf_of.as_deref(), args.yes)?;
        let client = inv.remote()?.api();
        let resolver = inv.asset_resolver().await?;
        if inv.dry_run() {
            let plan = bracket_dry_run_plan(client, &resolver, inv.settings(), args).await?;
            return run.preview(plan);
        }
        let signer = inv.resolve_signer()?;
        bracket(run.context(&signer)?, &resolver, args).await
    }
}

pub struct Buy;

impl CliCommand for Buy {
    type Args = BuySellArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["buy"],
        "trade",
        "Buy at market (shortcut for orders create --side buy)",
        concat!("Market buy alias for orders create --side buy. Limit if --price is set. Accepts --size or --amount. Dry-run first. Retry: check_open_orders — not idempotent. ", prompts_unless_yes!("orders")),
    )
    .long_about("Buy at market. Shortcut for orders create --side buy. Places a limit order if --price is set.\n\
\n\
Retry: check open orders first. Not idempotent. Inspect `orders open` and `positions list` before rerunning.\n\
\n\
Examples:\n  \
hyperliquid --format json --dry-run buy --coin BTC --size 0.001\n  \
hyperliquid --format json --dry-run buy --coin BTC --amount 20\n  \
hyperliquid --format json --dry-run buy --coin ETH --price 3000 --size 0.1\n  \
hyperliquid --format json buy --coin BTC --size 0.001 --yes")
    .prompt()
    .testnet_unprompted()
    .args(BUY_SELL_ARGS);

    async fn run(inv: &Invocation, args: &BuySellArgs) -> Result<(), anyhow::Error> {
        let create_args = args.clone().into_create(OrderSide::Buy);
        OrdersCreate::run(inv, &create_args).await
    }
}

pub struct Sell;

impl CliCommand for Sell {
    type Args = BuySellArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["sell"],
        "trade",
        "Sell at market (shortcut for orders create --side sell)",
        concat!("Market sell alias for orders create --side sell. Limit if --price is set. Accepts --size or --amount. Dry-run first. Retry: check_open_orders — not idempotent. ", prompts_unless_yes!("orders")),
    )
    .long_about("Sell at market. Shortcut for orders create --side sell. Places a limit order if --price is set.\n\
\n\
Retry: check open orders first. Not idempotent. Inspect `orders open` and `positions list` before rerunning.\n\
\n\
Examples:\n  \
hyperliquid --format json --dry-run sell --coin BTC --size 0.001\n  \
hyperliquid --format json --dry-run sell --coin ETH --price 3000 --size 0.1\n  \
hyperliquid --format json sell --coin BTC --size 0.001 --yes")
    .prompt()
    .testnet_unprompted()
    .args(BUY_SELL_ARGS);

    async fn run(inv: &Invocation, args: &BuySellArgs) -> Result<(), anyhow::Error> {
        let create_args = args.clone().into_create(OrderSide::Sell);
        OrdersCreate::run(inv, &create_args).await
    }
}

pub struct OutcomeBuy;

impl CliCommand for OutcomeBuy {
    type Args = OutcomeOrderArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["outcome-buy"],
        "trade",
        "Buy a HIP-4 outcome token",
        concat!("Buy a HIP-4 outcome token. Coin must be #N or +N notation. Dry-run first. Retry: check_open_orders — not idempotent. ", prompts_unless_yes!("orders")),
    )
    .long_about("Buy a HIP-4 outcome token. --coin must be #N or +N notation.\n\
\n\
Retry: check open orders first. Not idempotent.\n\
\n\
Examples:\n  \
hyperliquid --format json --dry-run outcome-buy --coin #10 --size 1\n  \
hyperliquid --format json outcome-buy --coin #10 --size 1 --yes")
    .prompt()
    .testnet_unprompted()
    .args(OUTCOME_ARGS);

    async fn run(inv: &Invocation, args: &OutcomeOrderArgs) -> Result<(), anyhow::Error> {
        ensure_outcome_coin(&args.inner.coin)?;
        let create_args = args.inner.clone().into_create(OrderSide::Buy);
        OrdersCreate::run(inv, &create_args).await
    }
}

pub struct OutcomeSell;

impl CliCommand for OutcomeSell {
    type Args = OutcomeOrderArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["outcome-sell"],
        "trade",
        "Sell a HIP-4 outcome token",
        concat!("Sell a HIP-4 outcome token. Coin must be #N or +N notation. Dry-run first. Retry: check_open_orders — not idempotent. ", prompts_unless_yes!("orders")),
    )
    .long_about("Sell a HIP-4 outcome token. --coin must be #N or +N notation.\n\
\n\
Retry: check open orders first. Not idempotent.\n\
\n\
Examples:\n  \
hyperliquid --format json --dry-run outcome-sell --coin #10 --size 1\n  \
hyperliquid --format json outcome-sell --coin #10 --size 1 --yes")
    .prompt()
    .testnet_unprompted()
    .args(OUTCOME_ARGS);

    async fn run(inv: &Invocation, args: &OutcomeOrderArgs) -> Result<(), anyhow::Error> {
        ensure_outcome_coin(&args.inner.coin)?;
        let create_args = args.inner.clone().into_create(OrderSide::Sell);
        OrdersCreate::run(inv, &create_args).await
    }
}

async fn watch_open_orders(
    inv: &Invocation,
    dex: Option<&str>,
    max_ticks: Option<usize>,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let api = remote.api();
    let resolver = std::sync::Arc::new(inv.asset_resolver().await?);
    let user = inv.resolve_signer_account().await?;
    let dex = dex.map(str::to_string);

    crate::watch::run_snapshot_watch(
        inv,
        "Live open orders",
        vec![Subscription::OrderUpdates { user }],
        max_ticks,
        move || {
            let resolver = std::sync::Arc::clone(&resolver);
            let dex = dex.clone();
            async move { open_orders_output(api, &resolver, user, dex.as_deref()).await }
        },
    )
    .await
}

fn ensure_outcome_coin(coin: &str) -> Result<(), CliError> {
    crate::assets::parse_outcome_notation(coin)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_spec::Check;

    fn order_specs() -> Vec<CommandSpec> {
        vec![
            OrdersCreate::SPEC,
            OrdersScale::SPEC,
            OrdersBatchCreate::SPEC,
            OrdersTpsl::SPEC,
            OrdersTrailingStop::SPEC,
            OrdersCancel::SPEC,
            OrdersCancelAll::SPEC,
            OrdersModify::SPEC,
            OrdersBatchModify::SPEC,
            OrdersTwapCreate::SPEC,
            OrdersTwapCancel::SPEC,
            OrdersScheduleCancel::SPEC,
            OrdersChase::SPEC,
            OrdersBracket::SPEC,
            Buy::SPEC,
            Sell::SPEC,
            OutcomeBuy::SPEC,
            OutcomeSell::SPEC,
        ]
    }

    #[test]
    fn every_order_command_hardens_the_flags_it_shares_with_orders_create() {
        let hardened = |spec: &CommandSpec, id: &str| spec.arg(id).map(|arg| arg.check.is_some());
        for spec in [
            OrdersCreate::SPEC,
            Buy::SPEC,
            Sell::SPEC,
            OutcomeBuy::SPEC,
            OutcomeSell::SPEC,
        ] {
            for id in [
                "cloid",
                "builder",
                "builder_fee_rate",
                "sub_builder",
                "on_behalf_of",
                "dex",
            ] {
                assert_eq!(hardened(&spec, id), Some(true), "{:?} {id}", spec.path);
            }
        }
        for spec in order_specs() {
            for id in ["on_behalf_of", "dex", "cloid"] {
                assert_ne!(hardened(&spec, id), Some(false), "{:?} {id}", spec.path);
            }
        }
        assert!(matches!(
            Buy::SPEC.arg("coin").and_then(|arg| arg.check),
            Some(Check::AssetInput("coin"))
        ));
    }

    /// The command words of each `impl CliCommand` block in the production half
    /// of this file (before its tests, which quote what they scan for) whose
    /// source contains `wanted`, read from the block's `SPEC` path.
    fn commands_whose_source_contains(wanted: &str) -> Vec<Vec<String>> {
        let source = include_str!("cli.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("source before the tests");
        source
            .split("impl CliCommand for ")
            .skip(1)
            .filter(|block| block.contains(wanted))
            .map(|block| {
                let after_spec = &block[block.find("const SPEC").expect("a SPEC")..];
                let path = &after_spec[after_spec.find("&[").expect("a path") + 2..];
                path[..path.find(']').expect("a closed path")]
                    .split('"')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_string)
                    .collect()
            })
            .collect()
    }

    /// `OrderRun::unprompted` hands a command a run that never asks, so the
    /// command must not prompt: its spec has no `.prompt()`. Without this a
    /// command that gained one would run on mainnet unconfirmed.
    #[test]
    fn commands_run_unprompted_only_when_their_spec_never_prompts() {
        let unprompted = commands_whose_source_contains("OrderRun::unprompted(");
        assert_eq!(
            unprompted,
            [
                vec!["orders", "cancel"],
                vec!["orders", "modify"],
                vec!["orders", "twap-cancel"]
            ],
            "the scan finds the commands that never ask"
        );
        for path in unprompted {
            let spec = order_specs()
                .into_iter()
                .find(|spec| spec.path == path.as_slice())
                .unwrap_or_else(|| panic!("no spec for {path:?}"));
            assert_ne!(
                spec.confirmation,
                crate::command_spec::ConfirmationPolicy::Prompt,
                "{path:?} prompts, so its run needs `--yes` wired through `OrderRun::new`"
            );
        }
    }

    fn flag_ids<A: Args>() -> Vec<String> {
        A::augment_args(clap::Command::new("probe"))
            .get_arguments()
            .map(|arg| arg.get_id().to_string())
            .collect()
    }

    /// A flag added to `CreateArgs` reaches `buy`, `sell` and the outcome
    /// commands through `BuySellArgs`, and their spec lists (`BUY_SELL_ARGS`,
    /// `OUTCOME_ARGS`) must name it too, or it goes unhardened and undescribed.
    /// Four places hold a create flag; this fails when one is missed.
    #[test]
    fn buy_sell_and_outcome_commands_carry_every_create_flag_they_take() {
        const CREATE_ONLY: [&str; 5] = [
            "side",
            "trigger_price",
            "take_profit",
            "stop_loss",
            "grouping",
        ];
        let create = flag_ids::<CreateArgs>();
        let shortcut = flag_ids::<BuySellArgs>();
        let expected: Vec<&String> = create
            .iter()
            .filter(|id| !CREATE_ONLY.contains(&id.as_str()))
            .collect();
        for id in &expected {
            assert!(shortcut.contains(id), "buy/sell lacks `{id}`");
        }
        for id in &shortcut {
            assert!(
                expected.contains(&id),
                "BuySellArgs has `{id}`, create lacks it"
            );
        }
        for spec in [Buy::SPEC, Sell::SPEC, OutcomeBuy::SPEC, OutcomeSell::SPEC] {
            for id in &shortcut {
                assert!(
                    spec.arg(id).is_some(),
                    "{:?} does not list `{id}`",
                    spec.path
                );
            }
        }
    }
}
