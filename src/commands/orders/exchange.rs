//! What the exchange said about orders: how each send is worded, one reading
//! of an answer (order, cancel, modify, TWAP and trailing-stop statuses), and
//! the one action the SDK cannot express.
//!
//! Placement rows, chase, and bracket all ask [`Placement`] what a status
//! means. The wire decode stays here so a new waiting status is handled once.
//! So does the one error every command raises for a rejected status.

use std::fmt;

use hypersdk::Decimal;
use hypersdk::hypercore::types::api::{TwapCancelStatus, TwapOrderStatus};
use hypersdk::hypercore::types::{OkResponse, OrderResponseStatus};
use serde::{Serialize, Serializer};

use super::status::Status;
use crate::errors::{CliError, Rejection, classify_rejection};
use crate::exchange_reply;
use crate::hints;
use crate::response_sanitization::labelled_untrusted_text;

/// The exchange's own words for refusing an order or a cancel. Remote text, so
/// it is only ever shown sanitized and labelled: in a result row's `error` and
/// in the [`CliError`] [`rejection_error`] builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Rejected(String);

impl Rejected {
    pub(super) fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    fn classify(&self) -> Rejection {
        classify_rejection(&self.0)
    }

    /// The exchange's text as received. For building an error, never for display.
    pub(super) fn raw(&self) -> &str {
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
/// names keeps its own error (a rate limit, rejected credentials, a recognised
/// refusal with its own reason); any other rejection reads as `lead` followed
/// by the exchange's text.
pub(super) fn rejection_error(lead: impl fmt::Display, rejected: &Rejected) -> CliError {
    rejected.classify().into_error_after(lead)
}

/// `action` completes "Couldn't ...", such as `cancel order`.
pub(super) fn status_error(action: &str, text: &str) -> CliError {
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
pub(super) fn rejected_all(action: &str, failures: &[Rejected]) -> CliError {
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

pub(super) enum Placement {
    /// Accepted, waiting, or resting. `oid` is set only when the exchange
    /// assigned one (`resting`).
    Open {
        status: Status,
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
    pub(super) fn classify(status: OrderResponseStatus) -> Self {
        match status {
            OrderResponseStatus::Success => Self::Open {
                status: Status::Accepted,
                oid: None,
            },
            OrderResponseStatus::WaitingForTrigger => Self::Open {
                status: Status::WaitingForTrigger,
                oid: None,
            },
            OrderResponseStatus::WaitingForFill => Self::Open {
                status: Status::WaitingForFill,
                oid: None,
            },
            OrderResponseStatus::Resting { oid, .. } => Self::Open {
                status: Status::Resting,
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

    pub(super) fn status(&self) -> Status {
        match self {
            Self::Open { status, .. } => *status,
            Self::Filled { .. } => Status::Filled,
            Self::Rejected(_) => Status::Rejected,
        }
    }

    pub(super) fn oid(&self) -> Option<u64> {
        match self {
            Self::Open { oid, .. } => *oid,
            Self::Filled { oid, .. } => Some(*oid),
            Self::Rejected(_) => None,
        }
    }
}

/// How a send reads in errors: `rejected` names the action when the exchange
/// refuses it ("cancel failed"), `reply` names it when the exchange answers
/// something this CLI cannot read ("cancel response had unexpected type").
/// Every send in `orders` is worded here, once.
#[derive(Debug, Clone, Copy)]
pub(super) struct Wording {
    pub(super) rejected: &'static str,
    reply: &'static str,
}

impl Wording {
    const fn new(rejected: &'static str, reply: &'static str) -> Self {
        Self { rejected, reply }
    }

    /// For an action whose answer nobody reads, so `reply` is never shown.
    const fn unread(rejected: &'static str) -> Self {
        Self::new(rejected, rejected)
    }

    pub(super) const PLACEMENT: Self = Self::new("order placement failed", "order placement");
    pub(super) const CHASE_QUOTE: Self = Self::new("chase quote failed", "order placement");
    pub(super) const CHASE_CANCEL: Self = Self::unread("chase cancel failed");
    pub(super) const BRACKET_ENTRY_CANCEL: Self = Self::unread("bracket entry cancel failed");
    pub(super) const CANCEL: Self = Self::new("cancel failed", "cancel");
    pub(super) const CANCEL_ALL: Self = Self::new("cancel-all failed", "cancel-all");
    pub(super) const MODIFY: Self = Self::new("modify failed", "modify");
    pub(super) const BATCH_MODIFY: Self = Self::new("batch modify failed", "batch modify");
    pub(super) const TWAP_CREATE: Self = Self::new("twap-create rejected", "twap-create");
    pub(super) const TWAP_CANCEL: Self = Self::new("twap-cancel rejected", "twap-cancel");
    pub(super) const TRAILING_STOP: Self = Self::unread("trailing stop rejected");
    pub(super) const SCHEDULE_CANCEL: Self = Self::unread("schedule-cancel rejected");
    pub(super) const SCHEDULE_CANCEL_CLEAR: Self = Self::unread("schedule-cancel clear rejected");
}

/// What the exchange answered `ok` to a send: the payload, read by the one
/// accessor that matches the action sent. Each decodes through the SDK's
/// [`OkResponse`] and keeps this CLI's own error for a reply of another shape.
#[derive(Debug)]
pub(super) struct Answer {
    payload: serde_json::Value,
    wording: Wording,
}

impl Answer {
    pub(super) fn new(payload: serde_json::Value, wording: Wording) -> Self {
        Self { payload, wording }
    }

    fn body(&self) -> String {
        labelled_untrusted_text(&self.payload.to_string())
    }

    /// The statuses of an order, modify or cancel answer.
    pub(super) fn statuses(self) -> Result<Vec<OrderResponseStatus>, CliError> {
        let what = self.wording.reply;
        let unexpected_type = || {
            CliError::Internal(anyhow::anyhow!(
                "{what} response had unexpected type: {}",
                self.body()
            ))
        };
        match exchange_reply::ok_response(&self.payload) {
            Ok(OkResponse::Order { statuses } | OkResponse::Cancel { statuses }) => Ok(statuses),
            Ok(_) => Err(unexpected_type()),
            Err(err) => {
                let Some(typed) = exchange_reply::typed(&self.payload)
                    .filter(|typed| matches!(typed.kind, "order" | "cancel"))
                else {
                    return Err(unexpected_type());
                };
                let Some(statuses) = typed.data.and_then(|data| data.get("statuses")) else {
                    return Err(CliError::Internal(anyhow::anyhow!(
                        "{what} response missing statuses: {}",
                        self.body()
                    )));
                };
                // The statuses' own decode error: the SDK enum wraps the same
                // failure in other words, and this text is part of the contract.
                let err = serde_json::from_value::<Vec<OrderResponseStatus>>(statuses.clone())
                    .err()
                    .unwrap_or(err);
                Err(CliError::Internal(anyhow::anyhow!(
                    "{what} response decode failed: {err}; body={}",
                    self.body()
                )))
            }
        }
    }

    /// The ID of the TWAP a `twapOrder` started.
    pub(super) fn twap_id(self) -> Result<u64, CliError> {
        match exchange_reply::ok_response(&self.payload) {
            Ok(OkResponse::TwapOrder {
                status: TwapOrderStatus::Running { twap_id },
            }) => Ok(twap_id),
            Ok(OkResponse::TwapOrder {
                status: TwapOrderStatus::Error(text),
            }) => Err(status_error("create TWAP", &text)),
            _ => Err(CliError::Internal(anyhow::anyhow!(
                "{} returned unexpected response: {}",
                self.wording.reply,
                self.payload
            ))),
        }
    }

    /// A `twapCancel` the exchange carried out.
    pub(super) fn twap_cancelled(self) -> Result<(), CliError> {
        match exchange_reply::ok_response(&self.payload) {
            Ok(OkResponse::TwapCancel {
                status: TwapCancelStatus::Success,
            }) => Ok(()),
            Ok(OkResponse::TwapCancel {
                status: TwapCancelStatus::Error(text),
            }) => Err(status_error("cancel TWAP", &text)),
            _ => Err(match self.payload.pointer("/data/status") {
                Some(status) => CliError::Internal(anyhow::anyhow!(
                    "{} returned unexpected status: {status}",
                    self.wording.reply
                )),
                None => CliError::Internal(anyhow::anyhow!(
                    "{} returned unexpected response: {}",
                    self.wording.reply,
                    self.payload
                )),
            }),
        }
    }

    /// The ID of the order a trailing stop placed. The SDK types only the
    /// accepted answer; a refusal's `error` is read where the exchange puts
    /// it, and any other answer leaves the outcome unknown.
    pub(super) fn trailing_stop_oid(self) -> Result<u64, CliError> {
        if let Ok(OkResponse::TrailingStop { oid }) = exchange_reply::ok_response(&self.payload) {
            return Ok(oid);
        }
        if let Some(error) = self
            .payload
            .pointer("/data/error")
            .or_else(|| self.payload.pointer("/data/status/error"))
            .and_then(serde_json::Value::as_str)
        {
            return Err(status_error("place trailing stop", error));
        }
        Err(CliError::unknown_outcome(format!(
            "The exchange answered ok without an order ID ({}). Check `{}` before placing another trailing stop",
            self.body(),
            hints::CHECK_LIVE_COMMAND
        )))
    }
}

/// Fail on the first rejected status, with the exchange text sanitized and
/// labelled. `action` completes "Couldn't ...", such as `cancel order`.
pub(super) fn ensure_action_statuses_ok(
    statuses: &[OrderResponseStatus],
    action: &str,
) -> Result<(), CliError> {
    match statuses.iter().find_map(OrderResponseStatus::error) {
        Some(err) => Err(status_error(action, err)),
        None => Ok(()),
    }
}

pub(super) fn modification_result_oid(statuses: &[OrderResponseStatus]) -> Option<u64> {
    statuses.iter().find_map(OrderResponseStatus::oid)
}

/// The `scheduleCancel` that clears the switch. The SDK's `ScheduleCancel`
/// serializes `time: null`, and the exchange takes the clear without the key,
/// so this action omits it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ScheduleCancelRawAction {
    #[serde(rename = "type")]
    pub(super) action_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) time: Option<u64>,
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
            assert_eq!(placed.status().as_str(), label);
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
        let statuses = Answer::new(response, Wording::PLACEMENT)
            .statuses()
            .unwrap();
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

        let waiting_only = Answer::new(
            serde_json::json!({
                "type": "order",
                "data": { "statuses": ["waitingForTrigger", "waitingForFill"] }
            }),
            Wording::PLACEMENT,
        )
        .statuses()
        .unwrap();
        ensure_action_statuses_ok(&waiting_only, "place order").unwrap();
        assert_eq!(modification_result_oid(&waiting_only), None);
    }

    #[test]
    fn the_decoder_reads_cancel_responses_and_names_the_action_when_the_shape_is_wrong() {
        let cancelled = Answer::new(
            serde_json::json!({"type": "cancel", "data": {"statuses": ["success"]}}),
            Wording::CANCEL,
        )
        .statuses()
        .unwrap();
        assert!(matches!(
            cancelled.as_slice(),
            [OrderResponseStatus::Success]
        ));

        // The whole text is the CLI's contract: the decode error is the one a
        // direct read of the statuses gives, not the SDK enum's wrapper.
        let statuses_error =
            serde_json::from_value::<Vec<OrderResponseStatus>>(serde_json::json!([42]))
                .unwrap_err();
        for (response, wording, text) in [
            (
                serde_json::json!({"type": "default"}),
                Wording::CANCEL,
                "cancel response had unexpected type: ".to_string()
                    + &labelled_untrusted_text(r#"{"type":"default"}"#),
            ),
            (
                serde_json::json!({"type": "twapOrder", "data": {"status": {"error": "x"}}}),
                Wording::CANCEL,
                "cancel response had unexpected type: ".to_string()
                    + &labelled_untrusted_text(
                        r#"{"data":{"status":{"error":"x"}},"type":"twapOrder"}"#,
                    ),
            ),
            (
                serde_json::json!({"data": {"statuses": []}}),
                Wording::CANCEL,
                "cancel response had unexpected type: ".to_string()
                    + &labelled_untrusted_text(r#"{"data":{"statuses":[]}}"#),
            ),
            (
                serde_json::json!({"type": "order"}),
                Wording::MODIFY,
                "modify response missing statuses: ".to_string()
                    + &labelled_untrusted_text(r#"{"type":"order"}"#),
            ),
            (
                serde_json::json!({"type": "cancel", "data": {"other": 1}}),
                Wording::CANCEL,
                "cancel response missing statuses: ".to_string()
                    + &labelled_untrusted_text(r#"{"data":{"other":1},"type":"cancel"}"#),
            ),
            (
                serde_json::json!({"type": "order", "data": {"statuses": [42]}}),
                Wording::CANCEL_ALL,
                format!(
                    "cancel-all response decode failed: {statuses_error}; body={}",
                    labelled_untrusted_text(r#"{"data":{"statuses":[42]},"type":"order"}"#)
                ),
            ),
        ] {
            let err = Answer::new(response, wording).statuses().unwrap_err();
            assert_eq!(err.to_string(), text);
        }
    }

    #[test]
    fn a_twap_answer_is_its_id_or_the_exchange_text_and_anything_else_is_unexpected() {
        let twap = |status: serde_json::Value| {
            Answer::new(
                serde_json::json!({"type": "twapOrder", "data": {"status": status}}),
                Wording::TWAP_CREATE,
            )
        };
        assert_eq!(
            twap(serde_json::json!({"running": {"twapId": 77_738_308}}))
                .twap_id()
                .unwrap(),
            77_738_308
        );
        let err = twap(serde_json::json!({"error": "Invalid TWAP duration"}))
            .twap_id()
            .unwrap_err();
        assert_eq!(err.exit_code(), 13);
        assert!(err.to_string().contains("Couldn't create TWAP"), "{err}");
        assert!(err.to_string().contains("[untrusted remote data]"), "{err}");

        let odd = Answer::new(serde_json::json!({"type": "default"}), Wording::TWAP_CREATE);
        assert_eq!(
            odd.twap_id().unwrap_err().to_string(),
            r#"twap-create returned unexpected response: {"type":"default"}"#
        );

        let cancel = |status: serde_json::Value| {
            Answer::new(
                serde_json::json!({"type": "twapCancel", "data": {"status": status}}),
                Wording::TWAP_CANCEL,
            )
        };
        cancel(serde_json::json!("success"))
            .twap_cancelled()
            .unwrap();
        let err = cancel(serde_json::json!({"error": "TWAP was never placed"}))
            .twap_cancelled()
            .unwrap_err();
        assert_eq!(err.exit_code(), 13);
        assert!(err.to_string().contains("Couldn't cancel TWAP"), "{err}");
        assert_eq!(
            cancel(serde_json::json!("weird"))
                .twap_cancelled()
                .unwrap_err()
                .to_string(),
            r#"twap-cancel returned unexpected status: "weird""#
        );
        assert_eq!(
            Answer::new(serde_json::json!({"type": "default"}), Wording::TWAP_CANCEL)
                .twap_cancelled()
                .unwrap_err()
                .to_string(),
            r#"twap-cancel returned unexpected response: {"type":"default"}"#
        );
    }

    #[test]
    fn a_trailing_stop_answer_is_its_oid_or_labelled_rejection_or_an_unknown_outcome() {
        let answer = |payload| Answer::new(payload, Wording::TRAILING_STOP);
        let ok = serde_json::json!({"type": "trailingStop", "data": {"oid": 77738308}});
        assert_eq!(answer(ok).trailing_stop_oid().unwrap(), 77_738_308);

        let rejected = serde_json::json!({"type": "trailingStop", "data": {"error": "Bad stop"}});
        let err = answer(rejected).trailing_stop_oid().unwrap_err();
        assert_eq!(err.exit_code(), 13);
        assert!(err.to_string().contains("[untrusted remote data]"), "{err}");

        let odd = serde_json::json!({"type": "default"});
        assert!(matches!(
            answer(odd).trailing_stop_oid().unwrap_err(),
            CliError::UnknownOutcome(_)
        ));
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
