//! What an `/exchange` reply and an `/info` refusal look like, decoded once.
//!
//! `/exchange` answers `{"status":"ok","response":{"type":..,"data":..}}` or
//! `{"status":"err","response":"<text>"}`; the words other services use for the
//! same error (`error`, `failed`, `failure`) are one set, [`is_error_status`].
//! [`Reply`] reads the envelope, [`typed`] reads the `type`/`data` of an ok
//! payload, and [`info_request_unavailable`] says when `/info` does not know a
//! request type at all.

use serde_json::Value;

/// The `status` values that mean the request failed. Case-insensitive.
const ERROR_STATUSES: &[&str] = &["err", "error", "failed", "failure"];

/// Whether `status` is one of the words an error reply uses.
#[must_use]
pub fn is_error_status(status: &str) -> bool {
    ERROR_STATUSES
        .iter()
        .any(|word| status.eq_ignore_ascii_case(word))
}

/// The status envelope of one reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply<'a> {
    /// `status: ok`, with the `response` payload (`null` when absent).
    Ok(&'a Value),
    /// An error status. The text is `response` when that is a string, else the
    /// whole body, so a caller always has the exchange's words to quote.
    Err(String),
    /// Neither: no status, or one this CLI does not know.
    Unrecognized(&'a Value),
}

impl<'a> Reply<'a> {
    #[must_use]
    pub fn decode(body: &'a Value) -> Self {
        let status = body
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if status.eq_ignore_ascii_case("ok") {
            const NULL: &Value = &Value::Null;
            Self::Ok(body.get("response").unwrap_or(NULL))
        } else if is_error_status(status) {
            Self::Err(
                body.get("response")
                    .and_then(Value::as_str)
                    .map_or_else(|| body.to_string(), str::to_string),
            )
        } else {
            Self::Unrecognized(body)
        }
    }
}

/// The text of an error reply, `None` for anything else.
#[must_use]
pub fn error_text(body: &Value) -> Option<String> {
    match Reply::decode(body) {
        Reply::Err(text) => Some(text),
        Reply::Ok(_) | Reply::Unrecognized(_) => None,
    }
}

/// An ok payload `{"type": kind, "data": data}`, such as `order`, `cancel`,
/// `default` or `trailingStop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Typed<'a> {
    pub kind: &'a str,
    pub data: Option<&'a Value>,
}

#[must_use]
pub fn typed(payload: &Value) -> Option<Typed<'_>> {
    Some(Typed {
        kind: payload.get("type")?.as_str()?,
        data: payload.get("data"),
    })
}

/// Whether an `/info` answer says the request type itself is unknown or
/// unsupported, as opposed to the request being wrong: a 400, 404 or 422 that
/// names an unknown request, or an error envelope on a success status.
#[must_use]
pub fn info_request_unavailable(status_code: u16, body: &str) -> bool {
    if (200..300).contains(&status_code) {
        let Ok(value) = serde_json::from_str::<Value>(body) else {
            return false;
        };
        return matches!(Reply::decode(&value), Reply::Err(_))
            && info_request_unavailable(400, &value.to_string());
    }

    let lower = body.to_ascii_lowercase();
    matches!(status_code, 400 | 404 | 422)
        && ["unknown", "unsupported", "invalid request", "invalid type"]
            .iter()
            .any(|word| lower.contains(word))
        && ["borrowlend", "borrow/lend", "request", "type"]
            .iter()
            .any(|word| lower.contains(word))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_envelope_is_ok_error_or_unrecognized() {
        let ok = json!({"status": "OK", "response": {"type": "default"}});
        assert_eq!(Reply::decode(&ok), Reply::Ok(&json!({"type": "default"})));
        let bare = json!({"status": "ok"});
        assert_eq!(Reply::decode(&bare), Reply::Ok(&Value::Null));
        for word in ["err", "ERROR", "failed", "Failure"] {
            let err = json!({"status": word, "response": "nope"});
            assert_eq!(Reply::decode(&err), Reply::Err("nope".into()), "{word}");
        }
        let body = json!({"status": "err", "response": {"detail": 1}});
        assert_eq!(Reply::decode(&body), Reply::Err(body.to_string()));
        assert!(matches!(
            Reply::decode(&json!({"message": "hi"})),
            Reply::Unrecognized(_)
        ));
    }

    #[test]
    fn error_text_is_only_for_error_statuses() {
        assert_eq!(
            error_text(&json!({"status": "err", "response": "bad"})).as_deref(),
            Some("bad")
        );
        assert_eq!(
            error_text(&json!({"status": "ok", "response": "bad"})),
            None
        );
        assert_eq!(error_text(&json!("bad")), None);
    }

    #[test]
    fn a_typed_payload_names_its_kind_and_data() {
        let payload = json!({"type": "order", "data": {"statuses": []}});
        let typed = typed(&payload).unwrap();
        assert_eq!(typed.kind, "order");
        assert_eq!(typed.data, Some(&json!({"statuses": []})));
        assert_eq!(
            super::typed(&json!({"type": "default"})).unwrap().data,
            None
        );
        assert!(super::typed(&json!({"data": 1})).is_none());
        assert!(super::typed(&Value::Null).is_none());
    }

    #[test]
    fn info_requests_the_server_does_not_know_are_unavailable() {
        assert!(info_request_unavailable(
            422,
            "Failed to deserialize the JSON body: unknown variant `borrowLendReserveStates`, expected one of request types"
        ));
        assert!(info_request_unavailable(
            400,
            r#"{"error":"Unsupported request type"}"#
        ));
        assert!(info_request_unavailable(
            200,
            r#"{"status":"err","response":"unknown request type borrowlend"}"#
        ));
        assert!(!info_request_unavailable(
            200,
            r#"{"status":"ok","response":"unknown request type"}"#
        ));
        assert!(!info_request_unavailable(200, "unknown request type"));
        assert!(!info_request_unavailable(500, "unknown request type"));
        assert!(!info_request_unavailable(400, "bad address"));
    }
}
