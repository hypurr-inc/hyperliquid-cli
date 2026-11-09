//! Strict address parsing: an explicit 0x address only, never a wallet name.
//! Selectors that may name a stored wallet resolve in [`crate::auth`].

use hypersdk::Address;

use crate::errors::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HexAddressError {
    Format,
    Zero,
}

impl HexAddressError {
    /// The refusal as an error of the caller's `class` (`CliError::Unsupported`,
    /// `CliError::Configuration`, ...). `noun` names what was parsed ("builder
    /// address", "address") and `raw` is what the user typed.
    #[must_use]
    pub fn refusal(self, class: fn(String) -> CliError, noun: &str, raw: &str) -> CliError {
        class(match self {
            Self::Format => format!("Invalid {noun}: {raw}"),
            Self::Zero => format!("{noun} must not be the zero address"),
        })
    }
}

/// Parse `value` as a 40-hex-digit address whose `0x` prefix is optional, the
/// way `Address::from_str` does. Does not trim.
pub fn parse_hex_address(value: &str, reject_zero: bool) -> Result<Address, HexAddressError> {
    parse_digits(value.strip_prefix("0x").unwrap_or(value), reject_zero)
}

/// Parse `value` as a 0x-prefixed 40-hex-digit address. Does not trim.
pub fn parse_exact_hex_address(value: &str, reject_zero: bool) -> Result<Address, HexAddressError> {
    parse_digits(
        value.strip_prefix("0x").ok_or(HexAddressError::Format)?,
        reject_zero,
    )
}

fn parse_digits(digits: &str, reject_zero: bool) -> Result<Address, HexAddressError> {
    if digits.len() != 40 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HexAddressError::Format);
    }
    if reject_zero && digits.bytes().all(|byte| byte == b'0') {
        return Err(HexAddressError::Zero);
    }
    let mut bytes = [0_u8; 20];
    hex::decode_to_slice(digits, &mut bytes).map_err(|_| HexAddressError::Format)?;
    Ok(Address::from(bytes))
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

    #[test]
    fn a_refusal_takes_the_callers_error_class_and_noun() {
        let format = HexAddressError::Format.refusal(CliError::Unsupported, "address", "nope");
        assert!(matches!(&format, CliError::Unsupported(text) if text == "Invalid address: nope"));
        let zero =
            HexAddressError::Zero.refusal(CliError::Configuration, "validator address", "0x0");
        assert!(matches!(
            &zero,
            CliError::Configuration(text) if text == "validator address must not be the zero address"
        ));
    }
}
