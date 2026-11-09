//! Read-only position risk watcher: the alert rules and one polling tick.
//!
//! Looping, bounds, signals and line output belong to [`crate::stream`].

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use clap::{Args, ValueEnum};
use hypersdk::hypercore::types::{
    AssetPosition, BasicOrder, ClearinghouseState, Fill, OrderType, Side,
};
use hypersdk::{Address, Decimal};
use serde::Serialize;
use serde_json::json;

use crate::account_mode::{
    AccountMode, DexCollateral, UnifiedRisk, fetch_abstraction, load_dex_collateral,
    unified_account_ratio,
};
use crate::clock::{self, DurationError};
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, arg};
use crate::commands::common::borrow_lend_state::fetch_user_state;
use crate::commands::common::position::mark_price;
use crate::commands::common::rows::{One, Row};
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::{Session, sink};
use crate::spot_balances::user_spot_balances_raw;
use crate::stream::{self, StreamEnd, Ticker, emit_json_line};
use crate::stream_bounds::StreamBound;
use crate::stream_bounds::{StreamBoundArgs, StreamBounds};
use crate::terminal::OutputFormat;
use crate::wire::{AddressRole, address};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum RiskRule {
    LiqProximity,
    MarginUsage,
    NoTpsl,
    StaleOrder,
    FundingBleed,
    PositionLifecycle,
}

impl RiskRule {
    pub const ALL: [RiskRule; 6] = [
        RiskRule::LiqProximity,
        RiskRule::MarginUsage,
        RiskRule::NoTpsl,
        RiskRule::StaleOrder,
        RiskRule::FundingBleed,
        RiskRule::PositionLifecycle,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RiskRule::LiqProximity => "liq_proximity",
            RiskRule::MarginUsage => "margin_usage",
            RiskRule::NoTpsl => "no_tpsl",
            RiskRule::StaleOrder => "stale_order",
            RiskRule::FundingBleed => "funding_bleed",
            RiskRule::PositionLifecycle => "position_lifecycle",
        }
    }
}

/// Margin used, in percent of equity, at or above which `margin_usage` fires.
const MARGIN_USED_LIMIT_PERCENT: i64 = 80;
/// Distance to liquidation, as a fraction of the mark price, at or below which
/// `liq_proximity` fires as critical and as a warning.
const LIQ_CRITICAL_DISTANCE: Decimal = Decimal::from_parts(5, 0, 0, false, 2);
const LIQ_WARNING_DISTANCE: Decimal = Decimal::from_parts(1, 0, 0, false, 1);
const NO_TPSL_AFTER_MINUTES: u64 = 15;
const NO_TPSL_AFTER: Duration = Duration::from_secs(NO_TPSL_AFTER_MINUTES * 60);
const FUNDING_BLEED_WINDOW: Duration = Duration::from_secs(60 * 60);
/// A resting order at least this old and this far from the mid (a fraction) is stale.
const STALE_ORDER_AGE: Duration = Duration::from_secs(12 * 60 * 60);
const STALE_ORDER_MID_DISTANCE: Decimal = Decimal::from_parts(3, 0, 0, false, 2);

#[derive(Args, Debug, Clone)]
#[command(
    mut_arg("max_events", |arg| arg.help(
        "Stop after this many alerts. Required in agent mode unless --idle-timeout-ms is set"
    )),
    mut_arg("idle_timeout_ms", |arg| arg
        .value_name("IDLE_TIMEOUT_MS")
        .help("Stop after this many idle milliseconds"))
)]
pub struct WatchRiskArgs {
    /// Protocol user address to watch. Defaults to the selected signer.
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long, value_enum, default_value = "info")]
    pub min_severity: AlertSeverity,
    #[arg(long = "disable", value_enum)]
    pub disable: Vec<RiskRule>,
    #[arg(long = "only", value_enum)]
    pub only: Vec<RiskRule>,
    #[arg(long, default_value = "15s", value_parser = parse_poll_interval)]
    pub interval: Duration,
    #[command(flatten)]
    pub stream: StreamBoundArgs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Serialize)]
#[clap(rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Info,
    Warning,
    Critical,
}

impl AlertSeverity {
    fn as_str(self) -> &'static str {
        match self {
            AlertSeverity::Info => "info",
            AlertSeverity::Warning => "warning",
            AlertSeverity::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RiskAlert {
    pub kind: &'static str,
    pub time: u64,
    pub address: String,
    pub rule: String,
    pub coin: Option<String>,
    pub severity: AlertSeverity,
    pub message: String,
    pub payload: serde_json::Value,
}

impl RiskAlert {
    fn for_rule(
        user: Address,
        rule: RiskRule,
        coin: Option<&str>,
        severity: AlertSeverity,
        message: String,
        payload: serde_json::Value,
    ) -> Self {
        Self::build(
            "alert",
            user,
            rule.as_str(),
            coin,
            severity,
            message,
            payload,
        )
    }

    fn heartbeat(user: Address) -> Self {
        Self::build(
            "heartbeat",
            user,
            "heartbeat",
            None,
            AlertSeverity::Info,
            "no open positions".to_string(),
            json!({ "positions": 0 }),
        )
    }

    fn build(
        kind: &'static str,
        user: Address,
        rule: &str,
        coin: Option<&str>,
        severity: AlertSeverity,
        message: String,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            kind,
            time: clock::now_ms(),
            address: user.to_string(),
            rule: rule.to_string(),
            coin: coin.map(str::to_string),
            severity,
            message,
            payload,
        }
    }
}

pub async fn watch_risk(
    inv: &Invocation,
    user: Address,
    args: &WatchRiskArgs,
) -> Result<(), anyhow::Error> {
    let api = inv.remote()?.api();
    let session = inv.session();
    let bounds = StreamBounds::watch_risk(&args.stream);

    stream::run_stream(async {
        let mut watcher = RiskWatcher::new(api, user, args).await?;
        stream::poll_stream(&mut watcher, args.interval, bounds, |alert| {
            emit_alert(alert, session)
        })
        .await?;
        Ok(StreamEnd::Finished)
    })
    .await
}

fn emit_alert(alert: RiskAlert, session: &Session) -> Result<(), anyhow::Error> {
    match session.format() {
        OutputFormat::Json => emit_json_line(
            &mut std::io::stdout().lock(),
            &alert,
            session.json_options(),
        ),
        _ => sink::stream_line(&session.render(&One::new(alert))),
    }
}

struct RiskWatcher<'a> {
    api: &'a ApiClient,
    user: Address,
    enabled: Vec<RiskRule>,
    min_severity: AlertSeverity,
    state: RiskEvalState,
    flat_announced: bool,
    /// Each perp dex's collateral token, read when a unified account first
    /// needs it. A dex deployed after that is not part of the ratio.
    dex_collateral: Option<Vec<DexCollateral>>,
}

impl<'a> RiskWatcher<'a> {
    async fn new(
        api: &'a ApiClient,
        user: Address,
        args: &WatchRiskArgs,
    ) -> Result<Self, anyhow::Error> {
        let fills = api.sdk().user_fills(user).await?;
        Ok(Self {
            api,
            user,
            enabled: enabled_rules(args),
            min_severity: args.min_severity,
            state: RiskEvalState::new(position_open_times(&fills)),
            flat_announced: false,
            dex_collateral: None,
        })
    }

    /// A pooled account's perp margin summary is not its real equity, so a unified
    /// account is measured by the docs' unified account ratio and a portfolio-margin one
    /// by the protocol's own health verdict.
    async fn margin_basis(
        &mut self,
        mode: AccountMode,
        state: &ClearinghouseState,
    ) -> Result<MarginBasis, anyhow::Error> {
        match mode {
            AccountMode::Unified => {
                if self.dex_collateral.is_none() {
                    self.dex_collateral = Some(load_dex_collateral(self.api).await?);
                }
                let dexes = self.dex_collateral.as_deref().unwrap_or_default();
                let mut others = Vec::new();
                for dex in dexes {
                    if let Some(name) = &dex.dex {
                        let other = self
                            .api
                            .sdk()
                            .clearinghouse_state(self.user, Some(name.clone()))
                            .await?;
                        others.push((dex.token, other));
                    }
                }
                let default_token = dexes
                    .iter()
                    .find(|dex| dex.dex.is_none())
                    .map_or(0, |dex| dex.token);
                let mut states = vec![(default_token, state)];
                states.extend(others.iter().map(|(token, other)| (*token, other)));
                let balances = user_spot_balances_raw(self.api, self.user).await?;
                Ok(MarginBasis::UnifiedRatio(unified_account_ratio(
                    &states, &balances,
                )))
            }
            AccountMode::PortfolioMargin => {
                let borrow_lend = fetch_user_state(self.api, self.user).await?;
                Ok(MarginBasis::PortfolioHealth {
                    health_factor: borrow_lend.health_factor(),
                    health: borrow_lend.health,
                })
            }
            AccountMode::Standard { .. } | AccountMode::DexAbstraction | AccountMode::Unknown => {
                Ok(MarginBasis::PerpSummary {
                    equity: state.margin_summary.account_value,
                    margin_used: state.margin_summary.total_margin_used,
                })
            }
        }
    }
}

impl Ticker for RiskWatcher<'_> {
    type Item = RiskAlert;

    async fn tick(&mut self) -> Result<Vec<RiskAlert>, anyhow::Error> {
        let client = self.api.sdk();
        let mode = fetch_abstraction(self.api, self.user)
            .await?
            .normalized_mode;
        let state = client.clearinghouse_state(self.user, None).await?;
        let open_orders = client.open_orders(self.user, None).await?;
        let mids = client.all_mids(None).await?;
        let margin = self.margin_basis(mode, &state).await?;

        let snapshot = RiskSnapshot {
            positions: &state.asset_positions,
            open_orders: &open_orders,
            mids: &mids,
            margin,
        };
        let mut alerts = evaluate_state(self.user, &self.enabled, snapshot, &mut self.state);

        let no_positions = state
            .asset_positions
            .iter()
            .all(|position| position.position.szi.is_zero());
        if no_positions {
            if !self.flat_announced {
                alerts.push(RiskAlert::heartbeat(self.user));
                self.flat_announced = true;
            }
        } else {
            self.flat_announced = false;
        }

        alerts.retain(|alert| alert.severity >= self.min_severity);
        Ok(alerts)
    }
}

struct RiskSnapshot<'a> {
    positions: &'a [AssetPosition],
    open_orders: &'a [BasicOrder],
    mids: &'a HashMap<String, Decimal>,
    margin: MarginBasis,
}

enum MarginBasis {
    /// The perp margin summary: a standard or dex-abstraction account.
    PerpSummary {
        equity: Decimal,
        margin_used: Decimal,
    },
    /// The unified account risk; `None` when no collateral token carries
    /// cross maintenance margin.
    UnifiedRatio(Option<UnifiedRisk>),
    PortfolioHealth {
        health: String,
        health_factor: Option<Decimal>,
    },
}

impl MarginBasis {
    fn alert(&self, user: Address) -> Option<RiskAlert> {
        let limit = Decimal::from(MARGIN_USED_LIMIT_PERCENT);
        let (message, payload) = match self {
            Self::PerpSummary {
                equity,
                margin_used,
            } => {
                if *equity <= Decimal::ZERO {
                    return None;
                }
                let pct = *margin_used * Decimal::from(100) / *equity;
                if pct < limit {
                    return None;
                }
                (
                    format!("margin used {pct}% of equity"),
                    json!({ "margin_used_pct": pct.to_string() }),
                )
            }
            Self::UnifiedRatio(risk) => match (*risk)? {
                UnifiedRisk::Ratio(ratio) => {
                    let pct = (ratio.ratio * Decimal::from(100)).round_dp(2).normalize();
                    if pct < limit {
                        return None;
                    }
                    (
                        format!(
                            "unified account ratio {pct}% (maintenance margin over collateral)"
                        ),
                        json!({
                            "margin_used_pct": pct.to_string(),
                            "basis": "unified_account_ratio",
                            "collateral_token": ratio.collateral_token,
                        }),
                    )
                }
                UnifiedRisk::Uncovered { collateral_token } => (
                    format!(
                        "unified account has maintenance margin on collateral token {collateral_token} and no collateral left to cover it"
                    ),
                    json!({
                        "basis": "unified_account_ratio",
                        "collateral_token": collateral_token,
                        "uncovered": true,
                    }),
                ),
            },
            Self::PortfolioHealth {
                health,
                health_factor,
            } => {
                if health == "healthy" {
                    return None;
                }
                (
                    format!("portfolio margin health is {health}"),
                    json!({
                        "basis": "portfolio_margin_health",
                        "health": health,
                        "health_factor": health_factor.map(|factor| factor.to_string()),
                    }),
                )
            }
        };
        Some(RiskAlert::for_rule(
            user,
            RiskRule::MarginUsage,
            None,
            AlertSeverity::Warning,
            message,
            payload,
        ))
    }
}

struct RiskEvalState {
    last_sizes: HashMap<String, Decimal>,
    first_seen: HashMap<String, Instant>,
    funding_seen: HashMap<String, Instant>,
    funding_baseline: HashMap<String, Decimal>,
    funding_alerted: HashSet<String>,
    open_times: HashMap<String, u64>,
}

impl RiskEvalState {
    fn new(open_times: HashMap<String, u64>) -> Self {
        Self {
            last_sizes: HashMap::new(),
            first_seen: HashMap::new(),
            funding_seen: HashMap::new(),
            funding_baseline: HashMap::new(),
            funding_alerted: HashSet::new(),
            open_times,
        }
    }

    fn forget_clocks(&mut self, coin: &str) {
        self.first_seen.remove(coin);
        self.funding_seen.remove(coin);
        self.funding_baseline.remove(coin);
        self.funding_alerted.remove(coin);
    }
}

fn position_open_times(fills: &[Fill]) -> HashMap<String, u64> {
    let mut open_times: HashMap<String, u64> = HashMap::new();
    let mut oldest: HashMap<String, u64> = HashMap::new();
    for fill in fills {
        oldest
            .entry(fill.coin.clone())
            .and_modify(|time| *time = (*time).min(fill.time))
            .or_insert(fill.time);
        let signed_sz = if fill.side == Side::Bid {
            fill.sz
        } else {
            -fill.sz
        };
        let after = fill.start_position + signed_sz;
        let opened_here = fill.start_position.is_zero()
            || fill.start_position.is_sign_positive() != after.is_sign_positive();
        if opened_here {
            open_times
                .entry(fill.coin.clone())
                .and_modify(|time| *time = (*time).max(fill.time))
                .or_insert(fill.time);
        }
    }
    for (coin, time) in oldest {
        open_times.entry(coin).or_insert(time);
    }
    open_times
}

fn open_instant(open_times: &HashMap<String, u64>, coin: &str) -> Option<Instant> {
    let ms = *open_times.get(coin)?;
    let now_ms = clock::now_ms();
    if ms >= now_ms {
        return None;
    }
    Instant::now().checked_sub(Duration::from_millis(now_ms - ms))
}

fn evaluate_state(
    user: Address,
    enabled: &[RiskRule],
    snapshot: RiskSnapshot<'_>,
    state: &mut RiskEvalState,
) -> Vec<RiskAlert> {
    let mut alerts = Vec::new();
    if enabled.contains(&RiskRule::MarginUsage)
        && let Some(alert) = snapshot.margin.alert(user)
    {
        alerts.push(alert);
    }

    if enabled.contains(&RiskRule::PositionLifecycle) {
        let present: HashSet<String> = snapshot
            .positions
            .iter()
            .map(|position| position.position.coin.clone())
            .collect();
        let vanished: Vec<String> = state
            .last_sizes
            .iter()
            .filter(|(coin, size)| !size.is_zero() && !present.contains(*coin))
            .map(|(coin, _)| coin.clone())
            .collect();
        for coin in vanished {
            alerts.push(RiskAlert::for_rule(
                user,
                RiskRule::PositionLifecycle,
                Some(&coin),
                AlertSeverity::Info,
                format!("{coin} closed"),
                json!({ "size": "0" }),
            ));
            state.last_sizes.insert(coin.clone(), Decimal::ZERO);
            state.forget_clocks(&coin);
        }
    }

    for position in snapshot.positions {
        let coin = position.position.coin.clone();
        let size = position.position.szi;
        if enabled.contains(&RiskRule::PositionLifecycle) {
            let lifecycle = |message: String, payload: serde_json::Value| {
                RiskAlert::for_rule(
                    user,
                    RiskRule::PositionLifecycle,
                    Some(&coin),
                    AlertSeverity::Info,
                    message,
                    payload,
                )
            };
            match state.last_sizes.get(&coin) {
                None if !size.is_zero() => alerts.push(lifecycle(
                    format!("{coin} opened"),
                    json!({ "size": size.to_string() }),
                )),
                Some(prev) if prev.is_zero() && !size.is_zero() => alerts.push(lifecycle(
                    format!("{coin} opened"),
                    json!({ "size": size.to_string() }),
                )),
                Some(prev) if !prev.is_zero() && size.is_zero() => alerts.push(lifecycle(
                    format!("{coin} closed"),
                    json!({ "size": size.to_string() }),
                )),
                Some(prev) if *prev != size => alerts.push(lifecycle(
                    format!("{coin} resized"),
                    json!({ "from": prev.to_string(), "to": size.to_string() }),
                )),
                _ => {}
            }
        }
        state.last_sizes.insert(coin.clone(), size);
        if size.is_zero() {
            state.forget_clocks(&coin);
            continue;
        }
        let seeded = open_instant(&state.open_times, &coin);
        state
            .first_seen
            .entry(coin.clone())
            .or_insert_with(|| seeded.unwrap_or_else(Instant::now));

        if enabled.contains(&RiskRule::LiqProximity)
            && let Some(liq) = position.position.liquidation_px
        {
            let mark = mark_price(&position.position);
            if mark > Decimal::ZERO {
                let distance = if size > Decimal::ZERO {
                    (mark - liq) / mark
                } else {
                    (liq - mark) / mark
                };
                let severity = if distance <= LIQ_CRITICAL_DISTANCE {
                    Some(AlertSeverity::Critical)
                } else if distance <= LIQ_WARNING_DISTANCE {
                    Some(AlertSeverity::Warning)
                } else {
                    None
                };
                if let Some(severity) = severity {
                    alerts.push(RiskAlert::for_rule(
                        user,
                        RiskRule::LiqProximity,
                        Some(&coin),
                        severity,
                        format!("{coin} is {distance} from liquidation"),
                        json!({ "distance": distance.to_string() }),
                    ));
                }
            }
        }

        if enabled.contains(&RiskRule::NoTpsl) {
            let open_for = NO_TPSL_AFTER;
            let seen = state
                .first_seen
                .get(&coin)
                .copied()
                .unwrap_or_else(Instant::now);
            let has_trigger = snapshot.open_orders.iter().any(|order| {
                order.coin.eq_ignore_ascii_case(&coin)
                    && matches!(order.order_type, OrderType::Trigger)
            });
            if !has_trigger && seen.elapsed() >= open_for {
                alerts.push(RiskAlert::for_rule(
                    user,
                    RiskRule::NoTpsl,
                    Some(&coin),
                    AlertSeverity::Warning,
                    format!("{coin} has been open {NO_TPSL_AFTER_MINUTES}m without TP/SL"),
                    json!({ "open_for_secs": seen.elapsed().as_secs() }),
                ));
            }
        }

        if enabled.contains(&RiskRule::FundingBleed) {
            let window = FUNDING_BLEED_WINDOW;
            // Bleed = funding actually paid while watching: the delta of
            // cum_funding.since_open since the position was first observed.
            let paid = position.position.cum_funding.since_open;
            let baseline = *state.funding_baseline.entry(coin.clone()).or_insert(paid);
            state
                .funding_seen
                .entry(coin.clone())
                .or_insert_with(|| seeded.unwrap_or_else(Instant::now));
            if let Some(seen) = state.funding_seen.get(&coin)
                && seen.elapsed() >= window
                && paid - baseline > Decimal::ZERO
                && state.funding_alerted.insert(coin.clone())
            {
                alerts.push(RiskAlert::for_rule(
                    user,
                    RiskRule::FundingBleed,
                    Some(&coin),
                    AlertSeverity::Warning,
                    format!(
                        "{coin} paid {} funding since watch started",
                        paid - baseline
                    ),
                    json!({
                        "open_for_secs": seen.elapsed().as_secs(),
                        "funding_paid": (paid - baseline).to_string(),
                    }),
                ));
            }
        }
    }

    if enabled.contains(&RiskRule::StaleOrder) {
        let now_ms = clock::now_ms();
        for order in snapshot.open_orders {
            let age_ms = now_ms.saturating_sub(order.timestamp);
            if Duration::from_millis(age_ms) < STALE_ORDER_AGE {
                continue;
            }
            if let Some(mid) = snapshot.mids.get(&order.coin)
                && *mid > Decimal::ZERO
            {
                let distance = (order.limit_px - *mid).abs() / *mid;
                if distance >= STALE_ORDER_MID_DISTANCE {
                    alerts.push(RiskAlert::for_rule(
                        user,
                        RiskRule::StaleOrder,
                        Some(&order.coin),
                        AlertSeverity::Warning,
                        format!("{} limit is stale and {distance} from mid", order.coin),
                        json!({
                            "oid": order.oid,
                            "distance": distance.to_string(),
                        }),
                    ));
                }
            }
        }
    }

    alerts
}

fn enabled_rules(args: &WatchRiskArgs) -> Vec<RiskRule> {
    RiskRule::ALL
        .into_iter()
        .filter(|rule| {
            if !args.only.is_empty() {
                args.only.contains(rule)
            } else {
                !args.disable.contains(rule)
            }
        })
        .collect()
}

fn parse_poll_interval(raw: &str) -> Result<Duration, String> {
    clock::parse_duration(raw, &clock::POLL_INTERVAL).map_err(|fault| match fault {
        DurationError::Zero => format!("duration must be greater than zero: {raw}"),
        _ => format!("invalid duration {raw}"),
    })
}

impl Row for RiskAlert {
    const HEADERS: &'static [&'static str] = &["Time", "Rule", "Coin", "Severity", "Message"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.time.to_string(),
            self.rule.clone(),
            self.coin.clone().unwrap_or_default(),
            self.severity.as_str().to_string(),
            self.message.clone(),
        ]
    }
}

pub const GROUP: GroupSpec = GroupSpec::new(&["watch"], "Watch accounts for risk alerts");

pub struct WatchRisk;

impl CliCommand for WatchRisk {
    type Args = WatchRiskArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["watch", "risk"],
        "watch",
        "Watch an address for liquidation and risk alerts",
        "Read-only liquidation and risk watcher. Emits JSON-line alerts with kind alert or heartbeat and never places orders. Retry: safe. Agent mode requires --max-events or --idle-timeout-ms.",
    )
    .long_about("Read-only liquidation and risk watcher. Emits JSON-line alerts and never places orders.\n\
\n\
Retry: safe. Agent/non-TTY mode requires --max-events or --idle-timeout-ms.\n\
\n\
Examples:\n  \
hyperliquid --format json watch risk --user 0x0000000000000000000000000000000000000001 --max-events 20\n  \
hyperliquid --format json watch risk --user 0x0000000000000000000000000000000000000001 --idle-timeout-ms 8000")
    .stream(StreamBound::Polled)
    .args(&[
        arg("user").kind(InputKind::Identifier).resource_id("user"),
        arg("min_severity").describe("Minimum alert severity to emit."),
        arg("disable").describe("Disable a risk rule.").schema_single(),
        arg("only").describe("Only evaluate these risk rules.").schema_single(),
        arg("interval").kind(InputKind::Duration).describe("Polling interval between risk checks, for example 15s or 500ms. Default 15s."),
        arg("max_events"),
        arg("idle_timeout_ms"),
    ]);

    async fn run(inv: &Invocation, args: &WatchRiskArgs) -> Result<(), anyhow::Error> {
        let user = if let Some(raw) = args.user.as_deref() {
            address(raw, AddressRole::Watched)?
        } else {
            inv.resolve_signer_account().await?
        };
        watch_risk(inv, user, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> WatchRiskArgs {
        WatchRiskArgs {
            user: None,
            min_severity: AlertSeverity::Info,
            disable: vec![],
            only: vec![],
            interval: Duration::from_secs(15),
            stream: StreamBoundArgs {
                max_events: Some(1),
                idle_timeout_ms: None,
            },
        }
    }

    fn user() -> Address {
        address(
            "0x0000000000000000000000000000000000000001",
            AddressRole::Watched,
        )
        .unwrap()
    }

    #[test]
    fn watched_user_takes_a_valid_address_with_or_without_the_0x_prefix() {
        assert!(address("not-an-address", AddressRole::Watched).is_err());
        assert!(
            address(
                "0000000000000000000000000000000000000001",
                AddressRole::Watched
            )
            .is_ok()
        );
        assert!(
            address(
                "0x0000000000000000000000000000000000000001",
                AddressRole::Watched
            )
            .is_ok()
        );
    }

    #[test]
    fn poll_interval_must_be_positive() {
        assert!(parse_poll_interval("0").is_err());
        assert!(parse_poll_interval("0ms").is_err());
        assert_eq!(
            parse_poll_interval("500ms").unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(parse_poll_interval("15s").unwrap(), Duration::from_secs(15));
    }

    #[test]
    fn disable_and_only_filter_rules() {
        let mut args = args();
        args.disable = vec![RiskRule::LiqProximity];
        assert!(!enabled_rules(&args).contains(&RiskRule::LiqProximity));
        args.only = vec![RiskRule::MarginUsage];
        assert_eq!(enabled_rules(&args), vec![RiskRule::MarginUsage]);
    }

    #[test]
    fn rule_names_match_command_line_values() {
        for rule in RiskRule::ALL {
            let value = rule.to_possible_value().unwrap();
            assert_eq!(value.get_name(), rule.as_str());
        }
    }

    #[test]
    fn heartbeat_and_rule_alerts_set_kind() {
        let heartbeat = RiskAlert::heartbeat(user());
        let liq = RiskAlert::for_rule(
            user(),
            RiskRule::LiqProximity,
            Some("ETH"),
            AlertSeverity::Critical,
            "near liq".to_string(),
            json!({}),
        );
        assert_eq!(heartbeat.kind, "heartbeat");
        assert_eq!(heartbeat.rule, "heartbeat");
        assert_eq!(liq.kind, "alert");
        assert_eq!(liq.rule, "liq_proximity");
        assert_eq!(
            serde_json::to_value(&heartbeat).unwrap()["kind"],
            "heartbeat"
        );
        assert_eq!(serde_json::to_value(&liq).unwrap()["kind"], "alert");
    }

    #[test]
    fn severity_names_match_serialized_names() {
        for severity in [
            AlertSeverity::Info,
            AlertSeverity::Warning,
            AlertSeverity::Critical,
        ] {
            assert_eq!(
                serde_json::to_value(severity).unwrap(),
                json!(severity.as_str())
            );
        }
    }

    #[test]
    fn margin_usage_fires_at_the_default_threshold() {
        let mids = HashMap::new();
        let evaluate = |margin_used: i64| {
            let mut state = RiskEvalState::new(HashMap::new());
            evaluate_state(
                user(),
                &[RiskRule::MarginUsage],
                RiskSnapshot {
                    positions: &[],
                    open_orders: &[],
                    mids: &mids,
                    margin: MarginBasis::PerpSummary {
                        equity: Decimal::from(100),
                        margin_used: Decimal::from(margin_used),
                    },
                },
                &mut state,
            )
        };

        assert!(evaluate(79).is_empty());
        let alerts = evaluate(80);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].rule, "margin_usage");
        assert_eq!(alerts[0].severity, AlertSeverity::Warning);
    }

    #[test]
    fn disabled_rules_do_not_fire() {
        let mids = HashMap::new();
        let mut state = RiskEvalState::new(HashMap::new());
        let alerts = evaluate_state(
            user(),
            &[RiskRule::NoTpsl],
            RiskSnapshot {
                positions: &[],
                open_orders: &[],
                mids: &mids,
                margin: MarginBasis::PerpSummary {
                    equity: Decimal::from(100),
                    margin_used: Decimal::from(100),
                },
            },
            &mut state,
        );
        assert!(alerts.is_empty());
    }
}
