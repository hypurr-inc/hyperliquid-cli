//! Account address resolution for command arguments, and strict address parsing.
//!
//! A public user or acting-account selector may name a stored wallet; every
//! other address argument is an explicit 0x address only. Signer selection and
//! the account resolver live in [`crate::auth`].

use std::fmt;

use hypersdk::Address;

use crate::auth::lookup_account_address;
use crate::mode::Mode;
use crate::settings::Settings;

/// An address a selector resolved to: whose data a read command queries, or the
/// account a signed action acts on behalf of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountAddress(Address);

impl AccountAddress {
    #[must_use]
    pub fn address(self) -> Address {
        self.0
    }
}

impl fmt::Display for AccountAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub fn resolve_protocol_user_address(
    selector: &str,
    settings: &Settings,
    mode: &Mode,
) -> Result<AccountAddress, anyhow::Error> {
    lookup_account_address(selector, settings, mode).map(AccountAddress)
}

pub fn resolve_acting_account_address(
    selector: &str,
    settings: &Settings,
    mode: &Mode,
) -> Result<AccountAddress, anyhow::Error> {
    lookup_account_address(selector, settings, mode).map(AccountAddress)
}

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

    #[test]
    fn literal_addresses_resolve_without_touching_the_vault() {
        let settings =
            Settings::detached(crate::settings::EnvSource::fixed::<&str, &str>([]), None);
        let address = "0x0000000000000000000000000000000000000001";
        let user =
            resolve_protocol_user_address(&format!(" {address} "), &settings, &Mode::default())
                .unwrap();
        assert_eq!(user.to_string().to_lowercase(), address);

        let err =
            resolve_acting_account_address("0xnothex", &settings, &Mode::default()).unwrap_err();
        assert!(err.to_string().contains("Invalid address"));
    }
}
