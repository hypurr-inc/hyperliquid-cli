//! What every exchange-facing command builds the same way: `/info` request
//! bodies and explicit address arguments.

use hypersdk::Address;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::errors::CliError;
use crate::resolvers::{HexAddressError, parse_exact_hex_address, parse_hex_address};

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

/// What an address argument names. The role fixes how a bad value is judged
/// and which error class and wording it reports: those differ per command and
/// are part of the documented exit-code surface (exit 2 `Configuration`,
/// exit 13 `Unsupported`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddressRole<'a> {
    /// An explicit `0x`-prefixed destination, worded for `flag` when it comes
    /// from a named flag. Never the zero address. Configuration error.
    Explicit(Option<&'a str>),
    /// A builder address; the `0x` prefix is optional and the zero address is
    /// refused. Unsupported error.
    Builder,
    /// A protocol user or validator address; the `0x` prefix is optional. A
    /// malformed value is an unsupported input, the zero address (named by
    /// `subject`) a configuration error.
    Protocol(&'a str),
    /// A watched account; the `0x` prefix is optional and the zero address is
    /// allowed. Unsupported error.
    Watched,
}

/// Parse an address argument as `role` describes.
pub(crate) fn address(raw: &str, role: AddressRole<'_>) -> Result<Address, CliError> {
    match role {
        AddressRole::Explicit(flag) => parse_exact_hex_address(raw, true).map_err(|err| {
            let scope = flag.map(|flag| format!("{flag}: ")).unwrap_or_default();
            CliError::Configuration(match err {
                HexAddressError::Zero => format!("{scope}address must not be the zero address"),
                HexAddressError::Format => {
                    format!("{scope}address must be a 0x-prefixed 40-byte hex string")
                }
            })
        }),
        AddressRole::Builder => parse_hex_address(raw, true).map_err(|err| {
            CliError::Unsupported(match err {
                HexAddressError::Format => format!("Invalid builder address: {raw}"),
                HexAddressError::Zero => "builder address cannot be the zero address".to_string(),
            })
        }),
        AddressRole::Protocol(subject) => parse_hex_address(raw, true).map_err(|err| match err {
            HexAddressError::Format => CliError::Unsupported(format!("Invalid address: {raw}")),
            HexAddressError::Zero => {
                CliError::Configuration(format!("{subject} address must not be the zero address"))
            }
        }),
        AddressRole::Watched => parse_hex_address(raw, false)
            .map_err(|_| CliError::Unsupported(format!("Invalid address: {raw}"))),
    }
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
    fn explicit_addresses_word_errors_for_the_flag() {
        let zero = "0x0000000000000000000000000000000000000000";

        let err = address(zero, AddressRole::Explicit(Some("--to"))).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string()
                .ends_with("--to: address must not be the zero address")
        );

        let err = address("INVALID", AddressRole::Explicit(None)).unwrap_err();
        assert!(
            err.to_string()
                .ends_with("address must be a 0x-prefixed 40-byte hex string")
        );
        assert!(address(ADDRESS, AddressRole::Explicit(None)).is_ok());
    }

    /// The exit code each role reports is part of the CLI contract: 2 is
    /// `Configuration`, 13 is `Unsupported`.
    #[test]
    fn every_role_keeps_its_error_class() {
        let zero = "0x0000000000000000000000000000000000000000";
        let bare = "000000000000000000000000000000000000dEaD";
        let class = |raw: &str, role| address(raw, role).unwrap_err().exit_code();

        assert_eq!(class("nope", AddressRole::Explicit(Some("--to"))), 2);
        assert_eq!(class(bare, AddressRole::Explicit(None)), 2);
        assert_eq!(class(zero, AddressRole::Explicit(None)), 2);

        assert_eq!(class("nope", AddressRole::Builder), 13);
        assert_eq!(class(zero, AddressRole::Builder), 13);
        assert!(address(bare, AddressRole::Builder).is_ok());

        assert_eq!(class("0x02", AddressRole::Protocol("validator")), 13);
        assert_eq!(class(zero, AddressRole::Protocol("validator")), 2);
        assert!(address(bare, AddressRole::Protocol("validator")).is_ok());

        assert_eq!(class("nope", AddressRole::Watched), 13);
        assert!(address(zero, AddressRole::Watched).is_ok());
    }
}
