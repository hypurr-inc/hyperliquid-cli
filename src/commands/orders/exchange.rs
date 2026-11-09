//! One reading of an exchange order/cancel/modify status list.
//!
//! Placement rows, chase, and bracket all ask [`Placement`] what a status
//! means. The wire decode stays here so a new waiting status is handled once.
//! So does the one error every command raises for a rejected status.

use std::fmt;

use hypersdk::hypercore::types::{
    Action, BatchOrder, OrderGrouping, OrderRequest, OrderResponseStatus,
};
use hypersdk::{Address, Decimal};
use serde::{Deserialize, Serialize, Serializer};

use super::OrderBuilderFee;
use crate::commands::actions;
use crate::errors::{CliError, Rejection, classify_rejection};
use crate::http_api::ApiClient;
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

/// The error for a rejected status. A rate limit or a refusal of the
/// credentials keeps its own exit code (11, 10); any other rejection reads as
/// `lead` followed by the exchange's text and exits 13.
pub(crate) fn rejection_error(lead: impl fmt::Display, rejected: &Rejected) -> CliError {
    rejected
        .classify()
        .into_error_with(|remote| CliError::unsupported_with_remote(lead, &remote))
}

/// [`rejection_error`] for text read straight from a response. `action`
/// completes "Couldn't ...", such as `cancel order`.
pub(crate) fn status_error(action: &str, text: &str) -> CliError {
    rejection_error(
        format!("Couldn't {action}. Exchange said: "),
        &Rejected::new(text),
    )
}

/// The error for a batch where the exchange rejected every leg. A rate limit or
/// a refusal of the credentials on any leg decides the exit code.
pub(crate) fn rejected_all(action: &str, failures: &[Rejected]) -> CliError {
    if let [one] = failures {
        return status_error(action, &one.0);
    }
    let decisive = failures
        .iter()
        .find_map(|failure| match failure.classify() {
            Rejection::RateLimited => Some(CliError::rate_limited()),
            Rejection::AuthRejected(remote) => Some(CliError::AuthRejected(remote)),
            Rejection::UserState(_) | Rejection::Other(_) => None,
        });
    decisive.unwrap_or_else(|| {
        let lines = failures
            .iter()
            .enumerate()
            .map(|(index, failure)| format!("  {}. {failure}", index + 1))
            .collect::<Vec<_>>()
            .join("\n");
        CliError::Unsupported(format!("Couldn't {action}. Exchange said:\n{lines}"))
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

pub(crate) fn parse_order_statuses(
    response: serde_json::Value,
) -> Result<Vec<OrderResponseStatus>, CliError> {
    let body = || labelled_untrusted_text(&response.to_string());
    if response.get("type").and_then(serde_json::Value::as_str) != Some("order") {
        return Err(CliError::Internal(anyhow::anyhow!(
            "order placement response had unexpected type: {}",
            body()
        )));
    }
    let statuses = response
        .get("data")
        .and_then(|data| data.get("statuses"))
        .cloned()
        .ok_or_else(|| {
            CliError::Internal(anyhow::anyhow!(
                "order placement response missing statuses: {}",
                body()
            ))
        })?;
    serde_json::from_value(statuses).map_err(|err| {
        CliError::Internal(anyhow::anyhow!(
            "order placement response decode failed: {err}; body={}",
            body()
        ))
    })
}

pub(crate) fn order_statuses_from_response(
    response: serde_json::Value,
    action: &'static str,
) -> Result<Vec<OrderResponseStatus>, CliError> {
    #[derive(Deserialize)]
    struct StatusesResponse {
        statuses: Vec<OrderResponseStatus>,
    }

    #[derive(Deserialize)]
    #[serde(tag = "type", content = "data", rename_all = "camelCase")]
    enum TaggedResponse {
        Cancel(StatusesResponse),
        Order(StatusesResponse),
    }

    if let Ok(parsed) = serde_json::from_value::<StatusesResponse>(response.clone()) {
        return Ok(parsed.statuses);
    }

    serde_json::from_value::<TaggedResponse>(response)
        .map(|parsed| match parsed {
            TaggedResponse::Cancel(data) | TaggedResponse::Order(data) => data.statuses,
        })
        .map_err(|err| {
            CliError::Internal(anyhow::anyhow!(
                "{action}: unexpected response shape: {err}"
            ))
        })
}

/// Fail on the first rejected status, with the exchange text sanitized and
/// labelled. `action` completes "Couldn't ...", such as `cancel order`.
pub(crate) fn ensure_action_statuses_ok(
    statuses: &[OrderResponseStatus],
    action: &str,
) -> Result<usize, CliError> {
    for status in statuses {
        if let Some(err) = status.error() {
            return Err(status_error(action, err));
        }
    }
    Ok(statuses.len())
}

pub(crate) fn modification_result_oid(statuses: &[OrderResponseStatus]) -> Option<u64> {
    statuses.iter().find_map(OrderResponseStatus::oid)
}

pub(super) async fn place_order_batch_raw(
    api: &ApiClient,
    signer: &SelectedSigner,
    batch: BatchOrder,
    vault_address: Option<Address>,
) -> Result<Vec<OrderResponseStatus>, CliError> {
    let response = actions::send_l1_action_raw(
        api,
        signer,
        Action::Order(batch),
        actions::nonce_now(),
        vault_address,
        "order placement failed",
    )
    .await?;
    parse_order_statuses(response)
}

#[derive(Debug, Serialize)]
struct BuilderOrderAction {
    #[serde(rename = "type")]
    action_type: &'static str,
    orders: Vec<OrderRequest>,
    grouping: OrderGrouping,
    builder: OrderBuilderFee,
}

pub(super) async fn place_builder_order_batch_raw(
    api: &ApiClient,
    signer: &SelectedSigner,
    batch: BatchOrder,
    builder: OrderBuilderFee,
    vault_address: Option<Address>,
) -> Result<Vec<OrderResponseStatus>, CliError> {
    // Builder fee order shape is documented by Hyperliquid's exchange endpoint:
    // action `{ type: "order", orders, grouping, builder: { b, f } }`, where
    // `f` is measured in tenths of a basis point.
    let action = BuilderOrderAction {
        action_type: "order",
        orders: batch.orders,
        grouping: batch.grouping,
        builder,
    };
    let response = actions::send_raw_l1_json_action(
        api,
        signer,
        &action,
        actions::RawL1ActionMetadata::new(actions::nonce_now()).with_vault_address(vault_address),
        "order placement failed",
    )
    .await?;
    parse_order_statuses(response)
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
    fn order_parser_accepts_resting_and_waiting_without_a_second_status_type() {
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
        let statuses = parse_order_statuses(response).unwrap();
        assert!(matches!(
            statuses.as_slice(),
            [
                OrderResponseStatus::Resting { oid: 12345, .. },
                OrderResponseStatus::WaitingForFill,
                OrderResponseStatus::WaitingForTrigger
            ]
        ));
        assert_eq!(
            ensure_action_statuses_ok(&statuses, "place order").unwrap(),
            3
        );
        assert_eq!(modification_result_oid(&statuses), Some(12345));
    }

    #[test]
    fn rejection_text_picks_the_exit_code_and_other_text_keeps_its_wording() {
        let limited = status_error("place order", "Too many requests: rate limit exceeded");
        assert_eq!(limited.exit_code(), 11);
        let credentials = status_error("place order", "Unauthorized");
        assert_eq!(credentials.exit_code(), 10);
        let other = status_error("place order", "Order has invalid size.");
        assert_eq!(other.exit_code(), 13);
        assert_eq!(
            other.to_string(),
            "Unsupported input: Couldn't place order. Exchange said: [untrusted remote data] Order has invalid size."
        );
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
}
