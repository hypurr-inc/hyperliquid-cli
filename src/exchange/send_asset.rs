//! The typed core of a `sendAsset`: where an asset moves between, and signing
//! and sending the move in the form the signer can sign.
//!
//! Layer rule: below `commands`. `transfer send-asset` parses its flags into
//! an [`AssetMove`] and `orders` builds one for the HIP-3 helper transfers; both
//! send it here.

use hypersdk::hypercore::types::{Action, AgentSendAsset, AssetTarget, SendAsset};
use hypersdk::{Address, Decimal};

use crate::assets::{SpotTokenListing, require_spot_token};
use crate::errors::CliError;
use crate::exchange::actions;
use crate::http_api::ApiClient;
use crate::signing::{AgentForm, SelectedSigner};
use crate::wire::{AddressRole, address};

#[derive(Debug)]
pub struct AssetVenue {
    target: AssetTarget,
    pub(crate) display: String,
    pub(crate) wire: String,
}

impl AssetVenue {
    pub fn perp() -> Self {
        Self {
            target: AssetTarget::Perp,
            display: "perp".to_string(),
            // Hyperliquid encodes the default perp venue as an empty string in sendAsset.
            wire: String::new(),
        }
    }

    pub(crate) fn spot() -> Self {
        Self {
            target: AssetTarget::Spot,
            display: "spot".to_string(),
            wire: "spot".to_string(),
        }
    }

    pub fn dex(name: &str) -> Self {
        Self {
            target: AssetTarget::Dex(name.to_string()),
            display: format!("dex:{name}"),
            wire: name.to_string(),
        }
    }

    /// `perp`, `spot` or `dex:<NAME>`, as `flag` was given.
    pub(crate) fn parse(input: &str, flag: &str) -> Result<Self, CliError> {
        match input.trim() {
            "perp" => Ok(Self::perp()),
            "spot" => Ok(Self::spot()),
            value => match value.strip_prefix("dex:") {
                Some("") => Err(CliError::Configuration(
                    "asset target dex:<NAME> requires a DEX name".to_string(),
                )),
                Some(dex) => Ok(Self::dex(dex)),
                None => Err(CliError::Configuration(format!(
                    "{flag} must be perp, spot, or dex:<NAME>"
                ))),
            },
        }
    }
}

#[derive(Debug)]
pub struct AssetMove {
    pub to: Address,
    pub source: AssetVenue,
    pub dest: AssetVenue,
    /// Spot token symbol.
    pub token: String,
    pub amount: Decimal,
    /// The source subaccount as the user wrote it, which is what goes on the
    /// wire.
    pub from_subaccount: Option<String>,
}

impl AssetMove {
    /// Refuse a move the exchange would refuse in the agent form, given how the
    /// signer signs ([`SelectedSigner::agent_form`]): the master's own
    /// `sendAsset` has no such rule.
    pub(crate) fn ensure_agent_target(&self, form: Option<AgentForm>) -> Result<(), CliError> {
        match form {
            Some(form) => {
                ensure_agent_send_target(self.to, form.master, self.from_subaccount.as_deref())
            }
            None => Ok(()),
        }
    }
}

pub(crate) fn from_subaccount_address(raw: &str) -> Result<Address, CliError> {
    address(raw, AddressRole::Explicit(Some("--from-subaccount")))
}

pub(crate) struct ResolvedMove {
    pub(crate) mv: AssetMove,
    pub(crate) token: SpotTokenListing,
}

impl ResolvedMove {
    pub(crate) async fn resolve(api: &ApiClient, mv: AssetMove) -> Result<Self, anyhow::Error> {
        let token = require_spot_token(api, &mv.token).await?;
        Ok(Self { mv, token })
    }

    pub(crate) async fn send(
        &self,
        api: &ApiClient,
        signer: &SelectedSigner,
        agent: bool,
    ) -> Result<(), anyhow::Error> {
        let chain = api.chain();
        let nonce = api.nonces().next();
        let AssetMove {
            to,
            source,
            dest,
            amount,
            from_subaccount,
            ..
        } = &self.mv;
        let (destination, amount) = (*to, *amount);
        let from_sub_account = from_subaccount.clone().unwrap_or_default();
        let token = self.token.send_token();
        let action = if agent {
            Action::AgentSendAsset(
                AgentSendAsset {
                    destination,
                    source_dex: source.target.clone(),
                    destination_dex: dest.target.clone(),
                    token,
                    amount,
                    from_sub_account,
                    nonce,
                }
                .into_action(),
            )
        } else {
            Action::SendAsset(
                SendAsset {
                    destination,
                    source_dex: source.target.clone(),
                    destination_dex: dest.target.clone(),
                    token,
                    amount,
                    from_sub_account,
                    nonce,
                }
                .into_action(chain),
            )
        };
        actions::send_default_l1_action_with_nonce(api, signer, action, nonce).await?;
        Ok(())
    }
}

/// Sign and submit a `sendAsset` without asking or printing a receipt.
pub async fn send_asset(
    api: &ApiClient,
    signer: &SelectedSigner,
    mv: AssetMove,
) -> Result<(), anyhow::Error> {
    let resolved = ResolvedMove::resolve(api, mv).await?;
    let form = signer.agent_form();
    resolved.mv.ensure_agent_target(form)?;
    resolved.send(api, signer, form.is_some()).await
}

/// An API wallet cannot sign `sendAsset`; it signs `agentSendAsset`, which the
/// exchange only accepts when the destination is the account the funds leave:
/// the master it signs for, or the subaccount named as the source.
fn ensure_agent_send_target(
    destination: Address,
    master: Address,
    from_subaccount: Option<&str>,
) -> Result<(), CliError> {
    let source = match from_subaccount {
        Some(raw) => from_subaccount_address(raw)?,
        None => master,
    };
    if destination != source {
        return Err(CliError::Unsupported(format!(
            "an API wallet can only send an asset to the account it leaves ({source}): --to is {destination}; move funds to another address with the master wallet"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_venues_parse_perp_spot_and_dex() {
        let perp = AssetVenue::parse(" perp ", "--source").unwrap();
        assert!(matches!(perp.target, AssetTarget::Perp));
        assert_eq!(perp.wire, "");

        let dex = AssetVenue::parse("dex:xyz", "--dest").unwrap();
        assert!(matches!(&dex.target, AssetTarget::Dex(name) if name == "xyz"));
        assert_eq!(
            (dex.display.as_str(), dex.wire.as_str()),
            ("dex:xyz", "xyz")
        );

        for bad in ["", "dex:", "futures"] {
            assert_eq!(
                AssetVenue::parse(bad, "--dest").unwrap_err().exit_code(),
                2,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_dex_name_keeps_a_repeated_prefix() {
        let dex = AssetVenue::parse("dex:dex:xyz", "--dest").unwrap();

        assert!(matches!(&dex.target, AssetTarget::Dex(name) if name == "dex:xyz"));
    }

    #[test]
    fn a_typed_venue_is_what_its_spelling_parses_to() {
        for (typed, spelled) in [
            (AssetVenue::perp(), "perp"),
            (AssetVenue::dex("xyz"), "dex:xyz"),
        ] {
            let parsed = AssetVenue::parse(spelled, "--dest").unwrap();
            assert_eq!(
                (typed.display, typed.wire),
                (parsed.display, parsed.wire),
                "{spelled}"
            );
        }
    }

    #[test]
    fn an_api_wallet_may_only_send_an_asset_to_the_account_it_leaves() {
        let master = Address::repeat_byte(1);
        let subaccount = Address::repeat_byte(2);
        let sub = format!("{subaccount:#x}");

        assert!(ensure_agent_send_target(master, master, None).is_ok());
        assert!(ensure_agent_send_target(subaccount, master, Some(&sub)).is_ok());
        // The master is not the source once a subaccount is named, and a
        // stranger is never the source.
        for (destination, from) in [(master, Some(sub.as_str())), (subaccount, None)] {
            let err = ensure_agent_send_target(destination, master, from).unwrap_err();
            assert_eq!(err.exit_code(), 13);
        }
    }

    fn move_to(to: Address) -> AssetMove {
        AssetMove {
            to,
            source: AssetVenue::spot(),
            dest: AssetVenue::perp(),
            token: "USDC".to_string(),
            amount: Decimal::ONE,
            from_subaccount: None,
        }
    }

    #[test]
    fn the_agent_form_refuses_a_stranger_as_the_destination() {
        let master = Address::repeat_byte(1);
        let form = Some(AgentForm { master });
        let to_master = move_to(master);

        assert!(to_master.ensure_agent_target(None).is_ok());
        assert!(to_master.ensure_agent_target(form).is_ok());
        // The master's own form has no such rule.
        let to_stranger = move_to(Address::repeat_byte(9));
        assert_eq!(
            to_stranger
                .ensure_agent_target(form)
                .unwrap_err()
                .exit_code(),
            13
        );
        assert!(to_stranger.ensure_agent_target(None).is_ok());
    }
}
