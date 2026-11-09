//! Referral code rules: what Hyperliquid accepts to apply or to register, and
//! the code setup and wallet creation offer as a default.

use crate::errors::CliError;
use crate::settings::Settings;

/// Hyperliquid accepts referral codes of up to 32 characters to apply, but
/// caps the codes a user registers at 20.
pub const MAX_REFERRAL_CODE_LEN: usize = 32;
pub const MAX_REFERRER_CODE_LEN: usize = 20;

/// The referral code setup and wallet creation offer. Skips the config file and
/// rejects a code that wouldn't be accepted by `referral set`.
pub fn suggested_default_referral_code(
    settings: &Settings,
    is_testnet: bool,
) -> Result<Option<String>, CliError> {
    let code = settings.suggested_default_referral_code(is_testnet);
    if let Some(code) = code.as_deref() {
        validate_referral_code(code)?;
    }
    Ok(code)
}

/// A code to apply with `referral set`.
pub fn validate_referral_code(code: &str) -> Result<(), CliError> {
    validate_code(code, MAX_REFERRAL_CODE_LEN, "referral code")
}

/// A code to create with `referral register`.
pub fn validate_referrer_code(code: &str) -> Result<(), CliError> {
    validate_code(code, MAX_REFERRER_CODE_LEN, "referrer code")
}

fn validate_code(code: &str, max_len: usize, label: &str) -> Result<(), CliError> {
    let trimmed = code.trim();
    if trimmed != code {
        return Err(CliError::Unsupported(format!(
            "{label} must not contain leading or trailing whitespace"
        )));
    }
    if trimmed.is_empty() {
        return Err(CliError::Unsupported(format!("{label} cannot be empty")));
    }
    if trimmed.len() > max_len {
        return Err(CliError::Unsupported(format!(
            "{label} must be {max_len} characters or fewer"
        )));
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return Err(CliError::Unsupported(format!(
            "{label} may only contain ASCII letters, numbers, '-' or '_'"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_referral_codes() {
        validate_referral_code("TESTNET").unwrap();
        validate_referral_code("ABC-123_ok").unwrap();
        assert_eq!(validate_referral_code("").unwrap_err().exit_code(), 13);
        assert_eq!(validate_referral_code(" ABC ").unwrap_err().exit_code(), 13);
        assert_eq!(
            validate_referral_code("bad code").unwrap_err().exit_code(),
            13
        );
        assert_eq!(
            validate_referral_code("abcdefghijklmnopqrstuvwxyz0123456789")
                .unwrap_err()
                .exit_code(),
            13
        );
    }

    #[test]
    fn registered_codes_are_capped_lower_than_applied_codes() {
        let code = "A".repeat(MAX_REFERRER_CODE_LEN + 1);

        assert!(validate_referral_code(&code).is_ok());
        assert!(validate_referrer_code(&code).is_err());
        assert!(validate_referral_code(&"A".repeat(MAX_REFERRAL_CODE_LEN + 1)).is_err());
    }

    #[test]
    fn suggested_referral_code_is_validated() {
        let settings = |code: &str| {
            Settings::detached(
                crate::env::EnvSource::fixed([(
                    crate::env::DEFAULT_REFERRAL_CODE,
                    code.to_string(),
                )]),
                None,
            )
        };
        assert_eq!(
            suggested_default_referral_code(&settings("  GOOD  "), false)
                .unwrap()
                .as_deref(),
            Some("GOOD")
        );
        assert!(suggested_default_referral_code(&settings("bad code!"), false).is_err());
    }
}
