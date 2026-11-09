use std::collections::HashSet;
use std::time::Duration;

use clap::Args;
use futures::StreamExt;
use hypersdk::Address;
use hypersdk::hypercore::{Incoming, Subscription, ws::Event};

use crate::errors::CliError;
use crate::http_api::ApiClient;

/// Documented per-IP WebSocket limit: subscriptions across all connections.
const MAX_SUBSCRIPTIONS: usize = 1000;
/// Documented per-IP WebSocket limit: unique users across user-specific subscriptions.
const MAX_SUBSCRIBED_USERS: usize = 10;

/// Refuse more subscriptions than the exchange allows per IP.
pub fn ensure_subscription_count(count: usize) -> Result<(), CliError> {
    if count > MAX_SUBSCRIPTIONS {
        return Err(CliError::Unsupported(format!(
            "{count} subscriptions requested, but Hyperliquid allows at most {MAX_SUBSCRIPTIONS} WebSocket subscriptions per IP. Subscribe to fewer dexes."
        )));
    }
    Ok(())
}

/// The account a subscription is about. The match is exhaustive, so a
/// subscription the SDK adds must say whether it counts toward the
/// unique-user limit.
fn subscribed_user(subscription: &Subscription) -> Option<Address> {
    match subscription {
        Subscription::OrderUpdates { user }
        | Subscription::UserFills { user }
        | Subscription::UserEvents { user }
        | Subscription::UserTwapSliceFills { user }
        | Subscription::UserTwapHistory { user }
        | Subscription::ActiveAssetData { user, .. }
        | Subscription::UserHistoricalOrders { user }
        | Subscription::ClearinghouseState { user, .. }
        | Subscription::AllDexsClearinghouseState { user }
        | Subscription::OpenOrders { user, .. }
        | Subscription::SpotState { user, .. }
        | Subscription::Notification { user }
        | Subscription::WebData3 { user }
        | Subscription::TwapStates { user, .. }
        | Subscription::UserFundings { user }
        | Subscription::UserNonFundingLedgerUpdates { user } => Some(*user),
        Subscription::Bbo { .. }
        | Subscription::Trades { .. }
        | Subscription::L2Book { .. }
        | Subscription::Candle { .. }
        | Subscription::AllMids { .. }
        | Subscription::ActiveAssetCtx { .. }
        | Subscription::AssetCtxs { .. }
        | Subscription::SpotAssetCtxs
        | Subscription::AllDexsAssetCtxs
        | Subscription::FastAssetCtxs
        | Subscription::OutcomeMetaUpdates => None,
    }
}

/// Refuse a subscription set the exchange would reject, before any socket opens.
///
/// The exchange limits the subscriptions and the unique users across
/// user-specific subscriptions (rate-limits page). One `subscribe` command
/// holds one connection, so the connection limits cannot be reached from here.
pub fn ensure_within_limits(subscriptions: &[Subscription]) -> Result<(), CliError> {
    ensure_subscription_count(subscriptions.len())?;
    let users: HashSet<Address> = subscriptions.iter().filter_map(subscribed_user).collect();
    if users.len() > MAX_SUBSCRIBED_USERS {
        return Err(CliError::Unsupported(format!(
            "{} distinct users requested, but Hyperliquid allows at most {MAX_SUBSCRIBED_USERS} unique users across user-specific WebSocket subscriptions per IP.",
            users.len()
        )));
    }
    Ok(())
}

fn parse_n_sig_figs(raw: &str) -> Result<u8, String> {
    match raw.parse::<u8>() {
        Ok(figs @ 2..=5) => Ok(figs),
        _ => Err(format!(
            "invalid n-sig-figs '{raw}'. Valid values: 2, 3, 4, 5"
        )),
    }
}

fn parse_mantissa(raw: &str) -> Result<u8, String> {
    match raw.parse::<u8>() {
        Ok(mantissa @ (1 | 2 | 5)) => Ok(mantissa),
        _ => Err(format!("invalid mantissa '{raw}'. Valid values: 1, 2, 5")),
    }
}

/// `--n-sig-figs` / `--mantissa`: the `l2Book` price aggregation the exchange offers.
#[derive(Args, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BookAggregationArgs {
    /// Aggregate price levels to this many significant figures (2-5). Omit for full precision.
    #[arg(long, value_parser = parse_n_sig_figs)]
    pub n_sig_figs: Option<u8>,
    /// Further aggregation (1, 2 or 5); only valid with --n-sig-figs 5.
    #[arg(long, value_parser = parse_mantissa)]
    pub mantissa: Option<u8>,
}

impl BookAggregationArgs {
    /// The exchange accepts a mantissa only together with `nSigFigs` 5.
    pub fn validate(&self) -> Result<(), CliError> {
        if self.mantissa.is_some() && self.n_sig_figs != Some(5) {
            return Err(CliError::Configuration(
                "--mantissa is only valid together with --n-sig-figs 5".to_string(),
            ));
        }
        Ok(())
    }

    /// The `l2Book` subscription for `coin` with this aggregation.
    #[must_use]
    pub fn subscription(&self, coin: impl Into<String>) -> Subscription {
        Subscription::L2Book {
            coin: coin.into(),
            n_sig_figs: self.n_sig_figs,
            mantissa: self.mantissa,
            fast: false,
        }
    }
}

/// What a one-shot WebSocket read is waiting for, as phrased in its errors.
pub struct Awaited {
    /// Completes "WebSocket closed before receiving ...".
    pub closed: &'static str,
    /// Completes "Timed out waiting for Hyperliquid ...".
    pub timed_out: &'static str,
}

/// The socket ended before it delivered `what` (for example "subscription events").
pub fn closed_before(what: &str) -> CliError {
    CliError::Unavailable(format!("WebSocket closed before receiving {what}."))
}

fn timed_out(awaited: &Awaited) -> CliError {
    CliError::Timeout(format!(
        "Timed out waiting for Hyperliquid {}. Check your network connection.",
        awaited.timed_out
    ))
}

/// Subscribe on a fresh socket and return the first message `pick` accepts.
pub async fn first_matching_message<T>(
    api: &ApiClient,
    subscription: Subscription,
    timeout: Duration,
    awaited: &Awaited,
    mut pick: impl FnMut(Incoming) -> Option<T>,
) -> Result<T, CliError> {
    let mut ws = api.websocket();
    ws.subscribe(subscription);

    tokio::time::timeout(timeout, async {
        while let Some(event) = ws.next().await {
            if let Event::Message(message) = event
                && let Some(found) = pick(message)
            {
                return Ok(found);
            }
        }

        Err(closed_before(awaited.closed))
    })
    .await
    .map_err(|_| timed_out(awaited))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypersdk::Address;

    fn user(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn fills(n: u8) -> Subscription {
        Subscription::UserFills { user: user(n) }
    }

    #[test]
    fn subscription_count_is_limited_to_the_documented_maximum() {
        let at_limit: Vec<_> = (0..MAX_SUBSCRIPTIONS)
            .map(|dex| Subscription::OpenOrders {
                user: user(1),
                dex: Some(dex.to_string()),
            })
            .collect();
        assert!(ensure_within_limits(&at_limit).is_ok());

        let mut over = at_limit;
        over.push(Subscription::AllMids { dex: None });
        let error = ensure_within_limits(&over).unwrap_err();
        assert!(matches!(error, CliError::Unsupported(_)), "{error:?}");
        assert!(error.to_string().contains("1001 subscriptions requested"));
    }

    #[test]
    fn unique_users_are_limited_but_repeats_of_one_user_are_not() {
        let ten: Vec<_> = (1..=10).map(fills).collect();
        assert!(ensure_within_limits(&ten).is_ok());

        let mut eleven = ten.clone();
        eleven.push(fills(11));
        let error = ensure_within_limits(&eleven).unwrap_err();
        assert!(matches!(error, CliError::Unsupported(_)), "{error:?}");
        assert!(error.to_string().contains("11 distinct users"));

        // The same user across many subscriptions, and market subscriptions
        // (no user), do not count against the user limit.
        let mut repeated: Vec<_> = (0..ten.len()).map(|_| fills(1)).collect();
        repeated.extend((2..=10).map(fills));
        repeated.push(Subscription::Trades { coin: "BTC".into() });
        assert!(ensure_within_limits(&repeated).is_ok());
    }

    #[test]
    fn book_aggregation_accepts_only_the_documented_values() {
        for figs in ["2", "3", "4", "5"] {
            assert!(parse_n_sig_figs(figs).is_ok(), "{figs}");
        }
        for figs in ["0", "1", "6", "-2", "x", ""] {
            assert!(parse_n_sig_figs(figs).is_err(), "{figs}");
        }
        for mantissa in ["1", "2", "5"] {
            assert!(parse_mantissa(mantissa).is_ok(), "{mantissa}");
        }
        for mantissa in ["0", "3", "4", "10", "x"] {
            assert!(parse_mantissa(mantissa).is_err(), "{mantissa}");
        }
    }

    #[test]
    fn a_mantissa_needs_five_significant_figures() {
        let aggregation = |n_sig_figs, mantissa| BookAggregationArgs {
            n_sig_figs,
            mantissa,
        };
        assert!(aggregation(None, None).validate().is_ok());
        assert!(aggregation(Some(3), None).validate().is_ok());
        assert!(aggregation(Some(5), Some(2)).validate().is_ok());
        for bad in [aggregation(None, Some(2)), aggregation(Some(4), Some(5))] {
            let error = bad.validate().unwrap_err();
            assert!(matches!(error, CliError::Configuration(_)), "{error:?}");
        }
    }

    #[test]
    fn book_subscription_carries_the_aggregation_on_the_wire() {
        let subscription = BookAggregationArgs {
            n_sig_figs: Some(5),
            mantissa: Some(5),
        }
        .subscription("BTC");
        assert_eq!(
            serde_json::to_value(subscription).unwrap(),
            serde_json::json!({"type": "l2Book", "coin": "BTC", "nSigFigs": 5, "mantissa": 5})
        );
        assert_eq!(
            serde_json::to_value(BookAggregationArgs::default().subscription("BTC")).unwrap(),
            serde_json::json!({"type": "l2Book", "coin": "BTC"})
        );
    }

    #[test]
    fn waiting_out_the_socket_is_a_timeout_like_a_silent_subscription() {
        let awaited = Awaited {
            closed: "a snapshot",
            timed_out: "snapshot",
        };
        let error = timed_out(&awaited);
        assert_eq!(error.reason(), "timeout");
        assert_eq!(error.exit_code(), 12);
        assert_eq!(
            error.to_string(),
            "Timed out waiting for Hyperliquid snapshot. Check your network connection."
        );
        assert_eq!(closed_before("a snapshot").reason(), "network_error");
    }
}
