//! Whether the account that pays an order's builder fee has approved it.
//!
//! The exchange refuses a builder fee above the user's approved maximum only
//! after the order is sent. Reading `maxBuilderFee` first turns that into an
//! early failure with the command that fixes it.

use hypersdk::Address;

use crate::builder_fee::{BuilderFee, max_builder_fee, percent_from_tenths_bps};
use crate::errors::CliError;
use crate::hints;
use crate::http_api::ApiClient;
use crate::mode::Mode;
use crate::output::Notice;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BuilderApproval {
    Covered { approved_tenths_bps: u64 },
    Short { approved_tenths_bps: u64 },
    Unread { reason: String },
}

impl BuilderApproval {
    pub(super) fn judge(fee: &BuilderFee, approved_tenths_bps: u64) -> Self {
        if approved_tenths_bps >= fee.tenths_bps() {
            Self::Covered {
                approved_tenths_bps,
            }
        } else {
            Self::Short {
                approved_tenths_bps,
            }
        }
    }

    pub(super) fn to_json(&self, user: Address, fee: &BuilderFee) -> serde_json::Value {
        let (status, approved, reason) = match self {
            Self::Covered {
                approved_tenths_bps,
            } => ("approved", Some(*approved_tenths_bps), None),
            Self::Short {
                approved_tenths_bps,
            } => ("insufficient", Some(*approved_tenths_bps), None),
            Self::Unread { reason } => ("unknown", None, Some(reason.as_str())),
        };
        serde_json::json!({
            "status": status,
            "account": user.to_string(),
            "builder": fee.address_wire(),
            "requested_tenths_bps": fee.tenths_bps(),
            "approved_tenths_bps": approved,
            "reason": reason,
        })
    }

    /// The error a live order fails with, or `None` when it may go on.
    fn refusal(&self, user: Address, fee: &BuilderFee) -> Option<CliError> {
        let Self::Short {
            approved_tenths_bps,
        } = self
        else {
            return None;
        };
        let requested = percent_from_tenths_bps(fee.tenths_bps());
        Some(CliError::Unsupported(format!(
            "Account {user} has approved builder {} for a max fee of {} ({approved_tenths_bps} tenths of a basis point), below the {requested} this order carries. Approve at least that with the main wallet, which an API wallet cannot do:\n    {}",
            fee.address_wire(),
            percent_from_tenths_bps(*approved_tenths_bps),
            hints::builder_approve_command(fee.address_wire(), &requested),
        )))
    }
}

/// The account whose approval a builder fee needs: the signer's own. Acting
/// for a vault or subaccount, which account must have approved the builder is
/// not documented, so no account is checked then.
pub(super) fn approver(vault_address: Option<Address>, signer_account: Address) -> Option<Address> {
    vault_address.is_none().then_some(signer_account)
}

pub(super) async fn check(api: &ApiClient, user: Address, fee: &BuilderFee) -> BuilderApproval {
    match max_builder_fee(api, user, fee.address()).await {
        Ok(approved_tenths_bps) => BuilderApproval::judge(fee, approved_tenths_bps),
        Err(err) => BuilderApproval::Unread {
            reason: err.to_string(),
        },
    }
}

/// Fail before an order is sent when `user` has approved the builder for less
/// than the order's fee. A read that fails does not stop trading: a person at a
/// terminal is told the fee could not be checked, and the exchange decides.
pub(super) async fn ensure_approved(
    api: &ApiClient,
    mode: &Mode,
    user: Address,
    fee: &BuilderFee,
) -> Result<(), CliError> {
    if fee.tenths_bps() == 0 {
        return Ok(());
    }
    let approval = check(api, user, fee).await;
    if let Some(refusal) = approval.refusal(user, fee) {
        return Err(refusal);
    }
    if let (BuilderApproval::Unread { reason }, true) = (&approval, mode.person_on_stderr()) {
        Notice::Caution(format!(
            "Couldn't check that builder {} is approved for {}: {reason}. Sending the order anyway; the exchange rejects it if the fee is not approved.",
            fee.address_wire(),
            percent_from_tenths_bps(fee.tenths_bps()),
        ))
        .eprint();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: Address = Address::repeat_byte(0x11);

    fn fee(tenths_bps: u64) -> BuilderFee {
        BuilderFee::new(Address::repeat_byte(0x22), tenths_bps, Vec::new())
    }

    #[test]
    fn a_fee_is_covered_up_to_and_including_the_approved_maximum() {
        assert_eq!(
            BuilderApproval::judge(&fee(10), 10),
            BuilderApproval::Covered {
                approved_tenths_bps: 10
            }
        );
        assert_eq!(
            BuilderApproval::judge(&fee(11), 10),
            BuilderApproval::Short {
                approved_tenths_bps: 10
            }
        );
    }

    #[test]
    fn a_short_approval_names_the_builder_and_the_command_that_fixes_it() {
        let error = BuilderApproval::judge(&fee(50), 10)
            .refusal(USER, &fee(50))
            .unwrap();
        assert_eq!(error.exit_code(), 13);
        let text = error.to_string();
        assert!(text.contains("0.01%"), "{text}");
        assert!(
            text.contains(
                "hyperliquid builder approve --builder 0x2222222222222222222222222222222222222222 --max-fee-rate 0.05%"
            ),
            "{text}"
        );
        assert!(
            BuilderApproval::judge(&fee(5), 10)
                .refusal(USER, &fee(5))
                .is_none()
        );
        assert!(
            BuilderApproval::Unread {
                reason: "down".to_string()
            }
            .refusal(USER, &fee(5))
            .is_none()
        );
    }
}
