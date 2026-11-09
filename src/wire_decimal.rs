//! Decimals as the exchange's info endpoints send them: a JSON string, a JSON
//! number, or either in scientific notation (`"1e-7"`), depending on the field
//! and on the amount.
//!
//! `#[serde(with = "wire_decimal")]` decodes a field of that kind;
//! [`deserialize_option`] one that may also be `null` or absent (add
//! `#[serde(default)]`). [`from_value`] reads one out of an already decoded
//! document.

use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// `raw` as a decimal, accepting scientific notation.
pub fn parse(raw: &str) -> Result<Decimal, rust_decimal::Error> {
    raw.parse::<Decimal>()
        .or_else(|_| Decimal::from_scientific(raw))
}

/// A decimal from a JSON string or number.
pub fn from_value(value: &Value) -> Result<Decimal, String> {
    let raw = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        other => return Err(format!("expected a decimal, found {other}")),
    };
    parse(&raw).map_err(|err| err.to_string())
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Decimal, D::Error> {
    from_value(&Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
}

pub fn deserialize_option<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Decimal>, D::Error> {
    Option::<Value>::deserialize(deserializer)?
        .map(|value| from_value(&value).map_err(serde::de::Error::custom))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct Fields {
        #[serde(with = "super")]
        plain: Decimal,
        #[serde(default, deserialize_with = "super::deserialize_option")]
        maybe: Option<Decimal>,
    }

    fn decode(json: &str) -> Result<Fields, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn strings_numbers_and_scientific_notation_decode_alike() {
        for raw in [r#""0.0000001""#, "0.0000001", r#""1e-7""#, "1e-7"] {
            let fields = decode(&format!(r#"{{"plain": {raw}}}"#)).unwrap();
            assert_eq!(fields.plain, Decimal::new(1, 7), "{raw}");
        }
        assert_eq!(decode(r#"{"plain": 2}"#).unwrap().plain, Decimal::from(2));
    }

    #[test]
    fn an_optional_decimal_may_be_null_or_absent() {
        assert_eq!(decode(r#"{"plain": 1}"#).unwrap().maybe, None);
        assert_eq!(
            decode(r#"{"plain": 1, "maybe": null}"#).unwrap().maybe,
            None
        );
        assert_eq!(
            decode(r#"{"plain": 1, "maybe": "1.5"}"#).unwrap().maybe,
            Some(Decimal::new(15, 1))
        );
    }

    #[test]
    fn anything_else_is_an_error() {
        assert!(decode(r#"{"plain": true}"#).is_err());
        assert!(decode(r#"{"plain": "abc"}"#).is_err());
        assert!(decode(r#"{"plain": 1, "maybe": []}"#).is_err());
    }
}
