//! What every signing provider shares: turning a provider's reply into a
//! canonical Hyperliquid signature, and proving it was made by the address the
//! caller selected.
//!
//! Each provider is described once by a [`Provider`]; the error codes
//! (`<code>_malformed_response`, `<code>_signer_mismatch`) and the wording of
//! its failures come from there.

use alloy::primitives::B256;
use hypersdk::Address;
use hypersdk::hypercore::types::Signature;

use crate::errors::CliError;

/// A remote or vault signing provider, for error codes and messages.
#[derive(Debug, Clone, Copy)]
pub struct Provider {
    code: &'static str,
    label: &'static str,
}

impl Provider {
    pub const BANKR: Self = Self {
        code: "bankr",
        label: "bankr",
    };
    pub const OWS: Self = Self {
        code: "ows",
        label: "OWS",
    };

    fn malformed(self, detail: impl std::fmt::Display) -> CliError {
        CliError::InvalidAuth(format!("{}_malformed_response: {detail}", self.code))
    }

    /// Parse a provider's signature into the canonical `r || s || v` form with
    /// `v` as 27 or 28. `recovery_id` supplies `v` for a bare 128-hex-char
    /// `r || s`; a 130-hex-char signature carries its own.
    pub fn parse_signature(
        self,
        signature: &str,
        recovery_id: Option<u8>,
    ) -> Result<Signature, CliError> {
        let signature = signature.trim();
        if !signature.is_ascii() {
            return Err(self.malformed("signature is not ASCII hex"));
        }
        let hex = signature.strip_prefix("0x").unwrap_or(signature);
        let label = self.label;
        let canonical = match hex.len() {
            130 => {
                let v = u8::from_str_radix(&hex[128..130], 16)
                    .map_err(|err| self.malformed(format!("invalid {label} recovery byte: {err}")))?;
                format!("0x{}{:02x}", &hex[..128], self.canonical_v(v)?)
            }
            128 => {
                let v = recovery_id.ok_or_else(|| {
                    self.malformed(format!(
                        "{label} returned r+s signature without recovery id"
                    ))
                })?;
                format!("0x{hex}{:02x}", self.canonical_v(v)?)
            }
            len => {
                return Err(self.malformed(format!(
                    "unexpected {label} signature length {len} hex chars; expected 128 or 130"
                )));
            }
        };
        canonical
            .parse::<Signature>()
            .map_err(|err| self.malformed(format!("failed to parse {label} signature: {err}")))
    }

    /// Recovery ids arrive raw (0 or 1) or already offset (27 or 28); anything
    /// else cannot name a public key.
    fn canonical_v(self, v: u8) -> Result<u8, CliError> {
        let v = if v >= 27 { v } else { v + 27 };
        if matches!(v, 27 | 28) {
            Ok(v)
        } else {
            Err(self.malformed(format!("invalid {} recovery byte {v}", self.label)))
        }
    }

    /// Check that `signature` over `digest` recovers to `expected`.
    pub fn verify(
        self,
        signature: &Signature,
        digest: B256,
        expected: Address,
    ) -> Result<(), CliError> {
        let label = self.label;
        let recovered = alloy::primitives::Signature::from(*signature)
            .recover_address_from_prehash(&digest)
            .map_err(|err| self.malformed(format!("failed to recover {label} signer: {err}")))?;
        if recovered == expected {
            return Ok(());
        }
        Err(CliError::InvalidAuth(format!(
            "{}_signer_mismatch: {label} signature recovered {recovered}, expected {expected}",
            self.code
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs() -> String {
        "11".repeat(64)
    }

    fn v_of(signature: &Signature) -> u64 {
        signature.v
    }

    #[test]
    fn embedded_recovery_byte_is_kept_or_offset() {
        for provider in [Provider::BANKR, Provider::OWS] {
            let canonical = provider
                .parse_signature(&format!("{}1b", rs()), None)
                .unwrap();
            let raw = provider
                .parse_signature(&format!("0x{}01", rs()), None)
                .unwrap();
            assert_eq!(v_of(&canonical), 27);
            assert_eq!(v_of(&raw), 28);
        }
    }

    #[test]
    fn bare_r_s_takes_the_supplied_recovery_id() {
        let raw = Provider::OWS.parse_signature(&rs(), Some(1)).unwrap();
        let canonical = Provider::OWS.parse_signature(&rs(), Some(28)).unwrap();
        assert_eq!(v_of(&raw), 28);
        assert_eq!(v_of(&canonical), 28);
    }

    #[test]
    fn bare_r_s_without_a_recovery_id_is_refused_with_the_provider_code() {
        let err = Provider::OWS.parse_signature(&rs(), None).unwrap_err();
        assert_eq!(err.exit_code(), 10);
        assert!(err.to_string().contains("ows_malformed_response"), "{err}");
        assert!(err.to_string().contains("without recovery id"), "{err}");

        let err = Provider::BANKR.parse_signature(&rs(), None).unwrap_err();
        assert!(
            err.to_string().contains("bankr_malformed_response"),
            "{err}"
        );
    }

    #[test]
    fn recovery_bytes_that_name_no_key_are_refused_not_panicked_on() {
        for v in ["02", "1a", "1d", "ff"] {
            for provider in [Provider::BANKR, Provider::OWS] {
                let err = provider
                    .parse_signature(&format!("{}{v}", rs()), None)
                    .unwrap_err();
                assert_eq!(err.exit_code(), 10, "{v}: {err}");
            }
        }
        assert!(Provider::OWS.parse_signature(&rs(), Some(200)).is_err());
    }

    #[test]
    fn unexpected_lengths_are_refused() {
        let err = Provider::OWS.parse_signature("abcd", Some(0)).unwrap_err();
        assert_eq!(err.exit_code(), 10);
        assert!(err.to_string().contains("unexpected OWS signature length"));
        assert!(Provider::BANKR.parse_signature("0x1234", None).is_err());
    }

    #[test]
    fn non_ascii_signatures_are_refused_without_slicing() {
        for provider in [Provider::BANKR, Provider::OWS] {
            let err = provider.parse_signature("0xé", None).unwrap_err();
            assert!(err.to_string().contains("not ASCII"), "{err}");
            let err = provider
                .parse_signature(&format!("0x{}é", "11".repeat(65)), None)
                .unwrap_err();
            assert!(err.to_string().contains("not ASCII"), "{err}");
        }
    }

    #[test]
    fn verify_names_the_provider_on_mismatch() {
        use alloy::signers::SignerSync;
        let signer = hypersdk::hypercore::PrivateKeySigner::random();
        let digest = B256::repeat_byte(0x42);
        let signature: Signature = signer.sign_hash_sync(&digest).unwrap().into();

        Provider::OWS
            .verify(&signature, digest, signer.address())
            .unwrap();
        let other: Address = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let err = Provider::OWS.verify(&signature, digest, other).unwrap_err();
        assert_eq!(err.exit_code(), 10);
        assert!(err.to_string().contains("ows_signer_mismatch"), "{err}");
        assert!(err.to_string().contains("OWS signature recovered"), "{err}");
        let err = Provider::BANKR.verify(&signature, digest, other).unwrap_err();
        assert!(err.to_string().contains("bankr_signer_mismatch"), "{err}");
    }
}
