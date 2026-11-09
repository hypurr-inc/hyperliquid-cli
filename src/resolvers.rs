//! Strict address parsing: an explicit 0x address only, never a wallet name.
//! Selectors that may name a stored wallet resolve in [`crate::auth`].

use hypersdk::Address;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HexAddressError {
    Format,
    Zero,
}

/// Parse `value` as a 0x-prefixed 40-hex-digit address. Does not trim.
pub fn parse_exact_hex_address(value: &str, reject_zero: bool) -> Result<Address, HexAddressError> {
    let stripped = value.strip_prefix("0x").ok_or(HexAddressError::Format)?;
    if stripped.len() != 40 || !stripped.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(HexAddressError::Format);
    }
    if reject_zero && stripped.chars().all(|ch| ch == '0') {
        return Err(HexAddressError::Zero);
    }
    value
        .parse::<Address>()
        .map_err(|_| HexAddressError::Format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_hex_addresses_accept_only_a_bare_0x_40_hex_string() {
        let valid = "0x00000000000000000000000000000000000000Ab";
        assert_eq!(
            parse_exact_hex_address(valid, false)
                .unwrap()
                .to_string()
                .to_lowercase(),
            valid.to_lowercase()
        );
        for rejected in [
            "main",
            "",
            "0x",
            "00000000000000000000000000000000000000ab",
            "0X00000000000000000000000000000000000000ab",
            " 0x00000000000000000000000000000000000000ab",
            "0x00000000000000000000000000000000000000a",
            "0x00000000000000000000000000000000000000abc",
            "0x00000000000000000000000000000000000000zz",
        ] {
            assert_eq!(
                parse_exact_hex_address(rejected, false),
                Err(HexAddressError::Format),
                "{rejected:?}"
            );
        }
    }

    #[test]
    fn the_zero_address_is_rejected_only_when_asked() {
        let zero = "0x0000000000000000000000000000000000000000";
        assert!(parse_exact_hex_address(zero, false).is_ok());
        assert_eq!(
            parse_exact_hex_address(zero, true),
            Err(HexAddressError::Zero)
        );
    }
}
