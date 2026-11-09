//! Client order ids: what the user types, what the exchange stores, and the
//! ids a command generates for orders it tracks.

use hypersdk::hypercore::Cloid;

use crate::clock;
use crate::errors::CliError;

/// The client order id of an order that has none: the exchange's all-zero id.
/// This is the only place that sentinel is written or read.
pub(super) fn wire_cloid(cloid: Option<Cloid>) -> Cloid {
    cloid.unwrap_or_default()
}

pub(super) fn cloid_from_wire(cloid: Cloid) -> Option<Cloid> {
    (cloid != Cloid::default()).then_some(cloid)
}

/// A CLOID given to a command that places an order: exactly 16 bytes of hex.
pub(super) fn parse_cloid(input: &str) -> Result<Cloid, CliError> {
    let Some(stripped) = input.strip_prefix("0x") else {
        return Err(CliError::Configuration(
            "CLOID must be a 0x-prefixed 16-byte hex value".to_string(),
        ));
    };

    if stripped.len() != 32 {
        return Err(CliError::Configuration(format!(
            "CLOID must be exactly 16 bytes (32 hex characters after 0x); got {} hex characters",
            stripped.len()
        )));
    }

    if !stripped.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(CliError::Configuration(
            "CLOID must contain only hexadecimal characters after 0x".to_string(),
        ));
    }

    input.parse::<Cloid>().map_err(|err| {
        CliError::Configuration(format!("CLOID must be a valid 16-byte hex value: {err}"))
    })
}

/// A CLOID given to a command that looks an order up: a short or unprefixed
/// value is padded with leading zeros.
pub(super) fn parse_lookup_cloid(input: &str) -> Result<Cloid, CliError> {
    let stripped = input.strip_prefix("0x").unwrap_or(input);
    if stripped.is_empty()
        || stripped.len() > 32
        || !stripped.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        return Err(CliError::Configuration(
            "CLOID must be a 0x-prefixed hex value up to 16 bytes".to_string(),
        ));
    }

    let padded = format!("0x{stripped:0>32}");
    padded.parse::<Cloid>().map_err(|err| {
        CliError::Configuration(format!("CLOID must be a valid 16-byte hex value: {err}"))
    })
}

/// Unique client order ID for an order a command tracks (a chase quote, a
/// bracket entry): nonce-derived high bytes plus a per-run sequence.
pub(super) fn generated_cloid(seq: u64) -> Cloid {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&clock::now_ms().to_be_bytes());
    bytes[8..].copy_from_slice(&seq.to_be_bytes());
    Cloid::new(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cloid(text: &str) -> Cloid {
        text.parse().unwrap()
    }

    #[test]
    fn the_none_sentinel_round_trips_through_the_wire_value() {
        let id = cloid("0x00000000000000000000000000000001");
        assert_eq!(cloid_from_wire(wire_cloid(Some(id))), Some(id));
        assert_eq!(cloid_from_wire(wire_cloid(None)), None);
    }

    #[test]
    fn an_order_cloid_is_exactly_sixteen_prefixed_bytes() {
        for (input, message) in [
            ("not-a-hex-value", "0x-prefixed"),
            ("0xDEADBEEF", "exactly 16 bytes"),
            ("0xzz000000000000000000000000000001", "only hexadecimal"),
        ] {
            let err = parse_cloid(input).unwrap_err();
            assert!(err.to_string().contains(message), "{input}: {err}");
        }
        assert_eq!(
            parse_cloid("0x00000000000000000000000000000001").unwrap(),
            cloid("0x00000000000000000000000000000001")
        );
    }

    #[test]
    fn a_lookup_cloid_is_padded_and_the_prefix_is_optional() {
        let expected = cloid("0x000000000000000000000000deadbeef");
        assert_eq!(parse_lookup_cloid("0xdeadbeef").unwrap(), expected);
        assert_eq!(parse_lookup_cloid("deadbeef").unwrap(), expected);
        assert!(parse_lookup_cloid("").is_err());
        assert!(parse_lookup_cloid("0x").is_err());
        assert!(parse_lookup_cloid(&format!("0x{}", "1".repeat(33))).is_err());
    }

    #[test]
    fn generated_cloids_differ_by_sequence_and_carry_it_in_the_low_bytes() {
        let first = generated_cloid(1);
        let second = generated_cloid(2);
        assert_ne!(first, second);
        assert_eq!(first[8..], 1u64.to_be_bytes());
        assert_eq!(second[8..], 2u64.to_be_bytes());
        assert_ne!(cloid_from_wire(first), None);
    }
}
