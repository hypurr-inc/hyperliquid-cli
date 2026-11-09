//! What every exchange-facing command builds the same way: `/info` request
//! bodies and explicit address arguments.

use hypersdk::Address;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::errors::CliError;
use crate::resolvers::{HexAddressError, parse_exact_hex_address};

/// An `/info` request body: `{"type": <kind>, ...fields}`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InfoRequest(Map<String, Value>);

impl InfoRequest {
    pub(crate) fn new(kind: &'static str) -> Self {
        let mut body = Map::new();
        body.insert("type".to_string(), Value::from(kind));
        Self(body)
    }

    /// A request about one account: `{"type": <kind>, "user": <user>}`.
    pub(crate) fn user(kind: &'static str, user: Address) -> Self {
        Self::new(kind).address("user", user)
    }

    pub(crate) fn field(mut self, key: &'static str, value: impl Into<Value>) -> Self {
        self.0.insert(key.to_string(), value.into());
        self
    }

    /// [`Self::field`] when `value` is present; otherwise the field is left out.
    pub(crate) fn field_if(self, key: &'static str, value: Option<impl Into<Value>>) -> Self {
        match value {
            Some(value) => self.field(key, value),
            None => self,
        }
    }

    pub(crate) fn address(self, key: &'static str, address: Address) -> Self {
        self.field(key, format!("{address:#x}"))
    }

    /// [`Self::address`] when `address` is present; otherwise the field is left out.
    pub(crate) fn address_if(self, key: &'static str, address: Option<Address>) -> Self {
        match address {
            Some(address) => self.address(key, address),
            None => self,
        }
    }

    /// The `startTime`/`endTime` window of a history request.
    pub(crate) fn time_range(self, start: Option<u64>, end: Option<u64>) -> Self {
        self.field_if("startTime", start).field_if("endTime", end)
    }
}

impl Serialize for InfoRequest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

/// Parse an explicit `0x`-prefixed 40-hex-digit address argument.
///
/// Failures are configuration errors worded for `flag` when the address comes
/// from a named flag. `reject_zero` also refuses the zero address.
pub(crate) fn config_address(
    raw: &str,
    flag: Option<&str>,
    reject_zero: bool,
) -> Result<Address, CliError> {
    parse_exact_hex_address(raw, reject_zero).map_err(|err| {
        let scope = flag.map(|flag| format!("{flag}: ")).unwrap_or_default();
        CliError::Configuration(match err {
            HexAddressError::Zero => format!("{scope}address must not be the zero address"),
            HexAddressError::Format => {
                format!("{scope}address must be a 0x-prefixed 40-byte hex string")
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDRESS: &str = "0xAbCdEf0123456789aBcDeF0123456789ABcdEF01";

    #[test]
    fn user_request_carries_type_and_lowercase_user() {
        let user: Address = ADDRESS.parse().unwrap();

        assert_eq!(
            serde_json::to_value(InfoRequest::user("userFees", user)).unwrap(),
            serde_json::json!({
                "type": "userFees",
                "user": ADDRESS.to_lowercase(),
            })
        );
    }

    #[test]
    fn absent_optional_fields_are_left_out() {
        let request = InfoRequest::new("userFills")
            .time_range(Some(5), None)
            .field_if("aggregateByTime", None::<bool>);

        assert_eq!(
            serde_json::to_value(request).unwrap(),
            serde_json::json!({"type": "userFills", "startTime": 5})
        );
    }

    #[test]
    fn config_address_words_errors_for_the_flag() {
        let zero = "0x0000000000000000000000000000000000000000";

        let err = config_address(zero, Some("--to"), true).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string()
                .ends_with("--to: address must not be the zero address")
        );
        assert!(config_address(zero, None, false).is_ok());

        let err = config_address("INVALID", None, true).unwrap_err();
        assert!(
            err.to_string()
                .ends_with("address must be a 0x-prefixed 40-byte hex string")
        );
        assert!(config_address(ADDRESS, None, true).is_ok());
    }
}
