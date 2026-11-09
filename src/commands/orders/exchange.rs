//! What the exchange said about orders: one reading of an order, cancel or
//! modify status list, and the one wire shape of a builder fee.
//!
//! Placement rows, chase, and bracket all ask [`Placement`] what a status
//! means. The wire decode stays here so a new waiting status is handled once.
//! So does the one error every command raises for a rejected status.

use std::fmt;

use hypersdk::hypercore::types::{
    Action, BatchOrder, OrderGrouping, OrderRequest, OrderResponseStatus,
};
use hypersdk::{Address, Decimal};
use serde::{Serialize, Serializer};

use crate::commands::actions;
use crate::errors::{CliError, Rejection, classify_rejection};
use crate::http_api::ApiClient;
use crate::quantity::Scale;
use crate::response_sanitization::labelled_untrusted_text;
use crate::signing::SelectedSigner;

/// The exchange's own words for refusing an order or a cancel. Remote text, so
/// it is only ever shown sanitized and labelled: in a result row's `error` and
/// in the [`CliError`] [`rejection_error`] builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rejected(String);

impl Rejected {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    fn classify(&self) -> Rejection {
        classify_rejection(&self.0)
    }

    /// The exchange's text as received. For building an error, never for display.
    pub(crate) fn raw(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&labelled_untrusted_text(&self.0))
    }
}

impl Serialize for Rejected {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The error for a rejected status. Every class [`Rejection::into_error`]
/// names keeps its own error and exit code (a rate limit 11, rejected
/// credentials 10, a recognised refusal 13 with its own reason); any other
/// rejection reads as `lead` followed by the exchange's text and exits 13.
pub(crate) fn rejection_error(lead: impl fmt::Display, rejected: &Rejected) -> CliError {
    rejected.classify().into_error_after(lead)
}

/// [`rejection_error`] for text read straight from a response. `action`
/// completes "Couldn't ...", such as `cancel order`.
pub(crate) fn status_error(action: &str, text: &str) -> CliError {
    rejection_error(
        format!("Couldn't {action}. Exchange said: "),
        &Rejected::new(text),
    )
}

/// The error for a batch where the exchange rejected every leg. Legs with the
/// same text are one entry (a pre-validation failure rejects the whole batch
/// with one message, repeated per leg), so one distinct text reads like a
/// single rejection. A rate limit or a refusal of the credentials on any leg
/// decides the exit code; otherwise the legs are listed with their numbers,
/// and the error carries a refusal's reason when every entry is that same
/// refusal.
pub(crate) fn rejected_all(action: &str, failures: &[Rejected]) -> CliError {
    let mut distinct: Vec<(&Rejected, Vec<usize>)> = Vec::new();
    for (index, failure) in failures.iter().enumerate() {
        match distinct.iter_mut().find(|(seen, _)| *seen == failure) {
            Some((_, legs)) => legs.push(index + 1),
            None => distinct.push((failure, vec![index + 1])),
        }
    }
    if let [(only, _)] = distinct.as_slice() {
        return status_error(action, only.raw());
    }
    let decisive = distinct
        .iter()
        .find_map(|(failure, _)| failure.classify().decisive());
    decisive.unwrap_or_else(|| {
        let lines = distinct
            .iter()
            .map(|(failure, legs)| {
                let legs = legs
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("  {legs}. {failure}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("Couldn't {action}. Exchange said:\n{lines}");
        let mut refusals = distinct
            .iter()
            .map(|(failure, _)| match failure.classify() {
                Rejection::Refused(refusal, _) => Some(refusal),
                _ => None,
            });
        match refusals.next().flatten() {
            Some(first) if refusals.all(|refusal| refusal == Some(first)) => {
                CliError::ExchangeRefused {
                    refusal: first,
                    text,
                }
            }
            _ => CliError::Unsupported(text),
        }
    })
}

pub(crate) enum Placement {
    /// Accepted, waiting, or resting. `oid` is set only when the exchange
    /// assigned one (`resting`).
    Open {
        label: &'static str,
        oid: Option<u64>,
    },
    Filled {
        oid: u64,
        total_sz: Decimal,
        avg_px: Decimal,
    },
    Rejected(Rejected),
}

impl Placement {
    pub(crate) fn classify(status: OrderResponseStatus) -> Self {
        match status {
            OrderResponseStatus::Success => Self::Open {
                label: "accepted",
                oid: None,
            },
            OrderResponseStatus::WaitingForTrigger => Self::Open {
                label: "waiting_for_trigger",
                oid: None,
            },
            OrderResponseStatus::WaitingForFill => Self::Open {
                label: "waiting_for_fill",
                oid: None,
            },
            OrderResponseStatus::Resting { oid, .. } => Self::Open {
                label: "resting",
                oid: Some(oid),
            },
            OrderResponseStatus::Filled {
                oid,
                total_sz,
                avg_px,
            } => Self::Filled {
                oid,
                total_sz,
                avg_px,
            },
            OrderResponseStatus::Error(err) => Self::Rejected(Rejected::new(err)),
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Open { label, .. } => label,
            Self::Filled { .. } => "filled",
            Self::Rejected(_) => "rejected",
        }
    }

    pub(crate) fn oid(&self) -> Option<u64> {
        match self {
            Self::Open { oid, .. } => *oid,
            Self::Filled { oid, .. } => Some(*oid),
            Self::Rejected(_) => None,
        }
    }
}

/// The statuses of an order, cancel or modify response, in request order.
/// `what` names the action in the error when the exchange answers something
/// else.
pub(crate) fn decode_statuses(
    response: serde_json::Value,
    what: &str,
) -> Result<Vec<OrderResponseStatus>, CliError> {
    let body = || labelled_untrusted_text(&response.to_string());
    if !matches!(
        response.get("type").and_then(serde_json::Value::as_str),
        Some("order" | "cancel")
    ) {
        return Err(CliError::Internal(anyhow::anyhow!(
            "{what} response had unexpected type: {}",
            body()
        )));
    }
    let statuses = response
        .get("data")
        .and_then(|data| data.get("statuses"))
        .cloned()
        .ok_or_else(|| {
            CliError::Internal(anyhow::anyhow!(
                "{what} response missing statuses: {}",
                body()
            ))
        })?;
    serde_json::from_value(statuses).map_err(|err| {
        CliError::Internal(anyhow::anyhow!(
            "{what} response decode failed: {err}; body={}",
            body()
        ))
    })
}

/// Fail on the first rejected status, with the exchange text sanitized and
/// labelled. `action` completes "Couldn't ...", such as `cancel order`.
pub(crate) fn ensure_action_statuses_ok(
    statuses: &[OrderResponseStatus],
    action: &str,
) -> Result<(), CliError> {
    match statuses.iter().find_map(OrderResponseStatus::error) {
        Some(err) => Err(status_error(action, err)),
        None => Ok(()),
    }
}

pub(crate) fn modification_result_oid(statuses: &[OrderResponseStatus]) -> Option<u64> {
    statuses.iter().find_map(OrderResponseStatus::oid)
}

/// What Hyperliquid wants in a builder fee `{ b, f, s }`: the builder's address,
/// the fee in tenths of a basis point (0.001%), and the optional sub-builders
/// that split the fee. This is the one place the unit is named; the percent
/// form comes from [`OrderBuilderFee::percent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct OrderBuilderFee {
    pub(super) b: String,
    pub(super) f: u64,
    /// Left off the wire when empty, so a plain builder fee keeps its `{ b, f }` shape.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) s: Vec<SubBuilder>,
}

/// A sub-builder `{ b, s }`: an existing user that receives `s` of the
/// builder's fee, in units of 0.001% (100,000 is all of it). The builder keeps
/// the remainder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct SubBuilder {
    pub(super) b: String,
    pub(super) s: u64,
}

impl OrderBuilderFee {
    /// The most a perpetual order may pay a builder: 0.1%.
    pub(super) const PERP_MAX_TENTHS_BPS: u64 = 100;
    /// At most this many sub-builders share one builder fee.
    pub(super) const MAX_SUB_BUILDERS: usize = 5;
    /// The sub-builder shares sum to at most this: 100%.
    pub(super) const MAX_SUB_BUILDER_SHARES: u64 = 100_000;

    /// The fee as a percent, such as `0.01`.
    pub(super) fn percent(&self) -> Decimal {
        Scale::TENTHS_BPS_PER_PERCENT.amount(self.f)
    }
}

#[derive(Debug, Serialize)]
struct BuilderOrderAction {
    #[serde(rename = "type")]
    action_type: &'static str,
    orders: Vec<OrderRequest>,
    grouping: OrderGrouping,
    builder: OrderBuilderFee,
}

/// Send `batch` and return one status per order. A builder fee goes in the
/// raw action `{ type: "order", orders, grouping, builder: { b, f, s } }`. The
/// SDK's `BatchOrder.builder` carries only `{ b, f }`, which cannot express
/// sub-builders, so every builder order is sent this one way; without a
/// builder the SDK action is sent.
pub(super) async fn place_orders(
    api: &ApiClient,
    signer: &SelectedSigner,
    batch: BatchOrder,
    builder: Option<&OrderBuilderFee>,
    vault_address: Option<Address>,
) -> Result<Vec<OrderResponseStatus>, CliError> {
    let response = match builder {
        Some(builder) => {
            let action = BuilderOrderAction {
                action_type: "order",
                orders: batch.orders,
                grouping: batch.grouping,
                builder: builder.clone(),
            };
            actions::send_raw_l1_json_action(
                api,
                signer,
                &action,
                actions::RawL1ActionMetadata::new(actions::nonce_now())
                    .with_vault_address(vault_address),
                "order placement failed",
            )
            .await?
        }
        None => {
            actions::send_l1_action_raw(
                api,
                signer,
                Action::Order(batch),
                actions::nonce_now(),
                vault_address,
                "order placement failed",
            )
            .await?
        }
    };
    decode_statuses(response, "order placement")
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(super) enum TwapExchangeAction {
    TwapOrder { twap: TwapOrderAction },
    TwapCancel { a: usize, t: u64 },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ScheduleCancelRawAction {
    #[serde(rename = "type")]
    pub(super) action_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) time: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct TwapOrderAction {
    pub(super) a: usize,
    pub(super) b: bool,
    pub(super) s: String,
    pub(super) r: bool,
    pub(super) m: u64,
    pub(super) t: bool,
}

pub(super) fn parse_twap_create_response(response: serde_json::Value) -> Result<u64, CliError> {
    if let Some(error) = response
        .pointer("/data/status/error")
        .and_then(serde_json::Value::as_str)
    {
        return Err(status_error("create TWAP", error));
    }

    response
        .pointer("/data/status/running/twapId")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            CliError::Internal(anyhow::anyhow!(
                "twap-create returned unexpected response: {}",
                response
            ))
        })
}

pub(super) fn parse_twap_cancel_response(response: serde_json::Value) -> Result<(), CliError> {
    let status = response.pointer("/data/status").ok_or_else(|| {
        CliError::Internal(anyhow::anyhow!(
            "twap-cancel returned unexpected response: {}",
            response
        ))
    })?;

    if status.as_str() == Some("success") {
        return Ok(());
    }

    if let Some(error) = status.get("error").and_then(serde_json::Value::as_str) {
        return Err(status_error("cancel TWAP", error));
    }

    Err(CliError::Internal(anyhow::anyhow!(
        "twap-cancel returned unexpected status: {status}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placement_labels_match_the_row_vocabulary() {
        let cases = [
            (OrderResponseStatus::Success, "accepted", None),
            (
                OrderResponseStatus::WaitingForTrigger,
                "waiting_for_trigger",
                None,
            ),
            (
                OrderResponseStatus::WaitingForFill,
                "waiting_for_fill",
                None,
            ),
            (
                OrderResponseStatus::Resting {
                    oid: 9,
                    cloid: None,
                },
                "resting",
                Some(9),
            ),
            (
                OrderResponseStatus::Filled {
                    oid: 4,
                    total_sz: Decimal::ONE,
                    avg_px: Decimal::from(2),
                },
                "filled",
                Some(4),
            ),
            (
                OrderResponseStatus::Error("no".to_string()),
                "rejected",
                None,
            ),
        ];
        for (status, label, oid) in cases {
            let placed = Placement::classify(status);
            assert_eq!(placed.label(), label);
            assert_eq!(placed.oid(), oid);
        }
    }

    #[test]
    fn the_decoder_accepts_resting_and_waiting_without_a_second_status_type() {
        let response = serde_json::json!({
            "type": "order",
            "data": {
                "statuses": [
                    {"resting": {"oid": 12345}},
                    "waitingForFill",
                    "waitingForTrigger"
                ]
            }
        });
        let statuses = decode_statuses(response, "order placement").unwrap();
        assert!(matches!(
            statuses.as_slice(),
            [
                OrderResponseStatus::Resting { oid: 12345, .. },
                OrderResponseStatus::WaitingForFill,
                OrderResponseStatus::WaitingForTrigger
            ]
        ));
        ensure_action_statuses_ok(&statuses, "place order").unwrap();
        assert_eq!(modification_result_oid(&statuses), Some(12345));

        let waiting_only = decode_statuses(
            serde_json::json!({
                "type": "order",
                "data": { "statuses": ["waitingForTrigger", "waitingForFill"] }
            }),
            "order placement",
        )
        .unwrap();
        ensure_action_statuses_ok(&waiting_only, "place order").unwrap();
        assert_eq!(modification_result_oid(&waiting_only), None);
    }

    #[test]
    fn the_decoder_reads_cancel_responses_and_names_the_action_when_the_shape_is_wrong() {
        let cancelled = decode_statuses(
            serde_json::json!({"type": "cancel", "data": {"statuses": ["success"]}}),
            "cancel",
        )
        .unwrap();
        assert!(matches!(
            cancelled.as_slice(),
            [OrderResponseStatus::Success]
        ));

        for (response, what, fragment) in [
            (
                serde_json::json!({"type": "default"}),
                "cancel",
                "cancel response had unexpected type",
            ),
            (
                serde_json::json!({"type": "order"}),
                "modify",
                "modify response missing statuses",
            ),
            (
                serde_json::json!({"type": "order", "data": {"statuses": [42]}}),
                "cancel-all",
                "cancel-all response decode failed",
            ),
        ] {
            let err = decode_statuses(response, what).unwrap_err();
            assert!(err.to_string().contains(fragment), "{err}");
        }
    }

    #[test]
    fn a_rejected_status_fails_the_action_with_the_exchange_text() {
        let statuses = [
            OrderResponseStatus::Success,
            OrderResponseStatus::Error("Order has invalid size.".to_string()),
        ];
        let err = ensure_action_statuses_ok(&statuses, "cancel order").unwrap_err();
        assert!(err.to_string().contains("Couldn't cancel order"), "{err}");
        assert!(err.to_string().contains("Order has invalid size."), "{err}");
    }

    #[test]
    fn a_builder_fee_is_tenths_of_a_basis_point_and_reads_as_a_percent() {
        let fee = |f| OrderBuilderFee {
            b: "0xabc".to_string(),
            f,
            s: Vec::new(),
        };
        assert_eq!(fee(10).percent(), Decimal::new(1, 2));
        assert_eq!(
            fee(OrderBuilderFee::PERP_MAX_TENTHS_BPS).percent(),
            Decimal::new(1, 1)
        );
        assert_eq!(fee(0).percent(), Decimal::ZERO);
    }

    #[test]
    fn a_batch_rejected_on_every_leg_lists_them_unless_one_names_a_rate_limit() {
        let plain = [Rejected::new("first"), Rejected::new("second")];
        let err = rejected_all("place orders", &plain);
        assert_eq!(err.exit_code(), 13);
        assert!(err.to_string().contains("1. [untrusted remote data] first"));
        let limited = [Rejected::new("first"), Rejected::new("rate limit exceeded")];
        assert_eq!(rejected_all("place orders", &limited).exit_code(), 11);
    }

    #[test]
    fn a_whole_batch_rejection_repeated_per_leg_reads_as_one_rejection() {
        let text = "Order must have minimum value of $10.";
        let legs = [
            Rejected::new(text),
            Rejected::new(text),
            Rejected::new(text),
        ];
        let err = rejected_all("place orders", &legs);

        assert_eq!(
            err.to_string(),
            "Unsupported input: Couldn't place orders. Exchange said: [untrusted remote data] Order must have minimum value of $10."
        );
        assert_eq!(err.reason(), "order_below_minimum_value");
        assert_eq!(err.exit_code(), 13);
    }

    #[test]
    fn legs_that_differ_keep_their_numbers_and_equal_legs_share_a_line() {
        let legs = [
            Rejected::new("Order has invalid size."),
            Rejected::new("Order has zero size."),
            Rejected::new("Order has invalid size."),
        ];
        let err = rejected_all("place orders", &legs);

        assert_eq!(
            err.to_string(),
            "Unsupported input: Couldn't place orders. Exchange said:\n  1, 3. [untrusted remote data] Order has invalid size.\n  2. [untrusted remote data] Order has zero size."
        );
        assert_eq!(err.reason(), "unsupported_input");
    }

    #[test]
    fn legs_refused_for_the_same_reason_with_different_text_keep_that_reason() {
        let legs = [
            Rejected::new("Post only order would have immediately matched, bbo was 50000@50001."),
            Rejected::new("Post only order would have immediately matched, bbo was 3000@3001."),
        ];
        let err = rejected_all("place orders", &legs);

        assert_eq!(err.reason(), "post_only_would_cross");
        assert_eq!(err.exit_code(), 13);
        assert!(err.hint().is_some());
        let text = err.to_string();
        assert!(
            text.contains("\n  1. [untrusted remote data] Post only"),
            "{text}"
        );
        assert!(
            text.contains("\n  2. [untrusted remote data] Post only"),
            "{text}"
        );
    }

    #[test]
    fn legs_refused_for_different_reasons_are_listed_as_plain_unsupported_input() {
        let legs = [
            Rejected::new("Order has zero size."),
            Rejected::new("Invalid TP/SL price."),
        ];
        let err = rejected_all("place orders", &legs);
        assert_eq!(err.reason(), "unsupported_input");
        assert!(err.hint().is_none());
    }

    #[test]
    fn a_batch_credential_or_address_limit_on_any_leg_decides_the_error() {
        let signer = "L1 error: User or API Wallet 0x1111111111111111111111111111111111111111 does not exist.";
        let legs = [Rejected::new("first"), Rejected::new(signer)];
        let err = rejected_all("place orders", &legs);
        assert_eq!(err.exit_code(), 10);
        assert_eq!(err.reason(), "signer_not_registered");

        let legs = [
            Rejected::new("first"),
            Rejected::new("Too many cumulative requests sent (2 > 1)"),
        ];
        let err = rejected_all("place orders", &legs);
        assert_eq!(err.exit_code(), 11);
        assert!(err.hint().unwrap().contains("reserveRequestWeight"));
    }
}
