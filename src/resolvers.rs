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
}
