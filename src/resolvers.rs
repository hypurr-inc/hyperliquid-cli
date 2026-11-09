//! Strict address parsing: an explicit 0x address only, never a wallet name.
//! Selectors that may name a stored wallet resolve in [`crate::auth`].

use hypersdk::Address;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HexAddressError {
    Format,
    Zero,
}

/// Parse `value` as a 40-hex-digit address whose `0x` prefix is optional, the
/// way `Address::from_str` does. Does not trim.
pub fn parse_hex_address(value: &str, reject_zero: bool) -> Result<Address, HexAddressError> {
    let digits = value.strip_prefix("0x").unwrap_or(value);
    parse_exact_hex_address(&format!("0x{digits}"), reject_zero)
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
    fn exact_hex_addresses_reject_aliases_and_optionally_the_zero_address() {
        let zero = "0x0000000000000000000000000000000000000000";
        assert_eq!(
            parse_exact_hex_address("main", false),
            Err(HexAddressError::Format)
        );
        assert!(parse_exact_hex_address(zero, false).is_ok());
        assert_eq!(
            parse_exact_hex_address(zero, true),
            Err(HexAddressError::Zero)
        );
    }

    #[test]
    fn the_lenient_form_takes_a_bare_hex_address_but_the_exact_form_does_not() {
        let bare = "00000000000000000000000000000000000000ab";
        assert!(parse_hex_address(bare, true).is_ok());
        assert!(parse_hex_address(&format!("0x{bare}"), true).is_ok());
        assert_eq!(
            parse_exact_hex_address(bare, true),
            Err(HexAddressError::Format)
        );
        assert_eq!(
            parse_hex_address("0x0000000000000000000000000000000000000000", true),
            Err(HexAddressError::Zero)
        );
    }
}
