use clap::Args;
use hypersdk::Address;
use hypersdk::hypercore::Chain;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::cli_command::CliCommand;
use crate::command_spec::{CommandSpec, GroupSpec, NoArgs, OwsSupport, Risk, arg};
use crate::commands::common::rows::{One, Row, or_missing};
use crate::commands::common::support::print_dry_run;
use crate::dry_run::ActionReversibility;
use crate::env;
use crate::errors::CliError;
use crate::exchange::actions;
use crate::invocation::Invocation;
use crate::referral_code::{validate_referral_code, validate_referrer_code};
use crate::signing::SelectedSigner;
use crate::wire::InfoRequest;

/// Every field is optional: an account with no referral activity answers with few of
/// them. A field of the wrong type is an error, not an absent value.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReferralInfo {
    referrer_state: Option<ReferrerState>,
    referred_by: Option<ReferredBy>,
    unclaimed_rewards: Option<String>,
    claimed_rewards: Option<String>,
    builder_rewards: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReferrerState {
    data: Option<ReferrerData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReferrerData {
    code: Option<String>,
    referral_states: Option<Vec<IgnoredAny>>,
}

#[derive(Debug, Deserialize)]
struct ReferredBy {
    code: Option<String>,
    referrer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReferralStatusRow {
    user: String,
    referral_code: Option<String>,
    referral_count: usize,
    referred_by_code: Option<String>,
    referred_by: Option<String>,
    unclaimed_rewards: Option<String>,
    claimed_rewards: Option<String>,
    builder_rewards: Option<String>,
}

impl ReferralStatusRow {
    fn new(user: Address, info: ReferralInfo) -> Self {
        let data = info.referrer_state.and_then(|state| state.data);
        let (referral_code, referral_count) = match data {
            Some(data) => (
                data.code,
                data.referral_states.map_or(0, |states| states.len()),
            ),
            None => (None, 0),
        };
        let (referred_by_code, referred_by) = match info.referred_by {
            Some(referred_by) => (referred_by.code, referred_by.referrer),
            None => (None, None),
        };
        Self {
            user: user.to_string(),
            referral_code,
            referral_count,
            referred_by_code,
            referred_by,
            unclaimed_rewards: info.unclaimed_rewards,
            claimed_rewards: info.claimed_rewards,
            builder_rewards: info.builder_rewards,
        }
    }
}

impl Row for ReferralStatusRow {
    const HEADERS: &'static [&'static str] = &[
        "User",
        "Referral Code",
        "Referral Count",
        "Referred By Code",
        "Referred By",
        "Unclaimed Rewards",
        "Claimed Rewards",
        "Builder Rewards",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.user.clone(),
            or_missing(self.referral_code.as_deref()),
            self.referral_count.to_string(),
            or_missing(self.referred_by_code.as_deref()),
            or_missing(self.referred_by.as_deref()),
            or_missing(self.unclaimed_rewards.as_deref()),
            or_missing(self.claimed_rewards.as_deref()),
            or_missing(self.builder_rewards.as_deref()),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReferralSetRow {
    action: String,
    status: String,
    network: String,
    signer: String,
    query_address: String,
    code: String,
    reversibility: String,
}

impl Row for ReferralSetRow {
    const HEADERS: &'static [&'static str] = &[
        "Action",
        "Status",
        "Network",
        "Signer",
        "Query Address",
        "Code",
        "Reversibility",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.action.clone(),
            self.status.clone(),
            self.network.clone(),
            self.signer.clone(),
            self.query_address.clone(),
            self.code.clone(),
            self.reversibility.clone(),
        ]
    }
}

const REVERSIBILITY: ActionReversibility = ActionReversibility::Irreversible;

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ReferrerAction<'a> {
    SetReferrer { code: &'a str },
    RegisterReferrer { code: &'a str },
}

impl ReferrerAction<'_> {
    fn code(&self) -> &str {
        match self {
            Self::SetReferrer { code } | Self::RegisterReferrer { code } => code,
        }
    }

    fn would_execute(&self) -> &'static str {
        match self {
            Self::SetReferrer { .. } => "set_referral_code",
            Self::RegisterReferrer { .. } => "register_referrer_code",
        }
    }

    fn confirmation_action(&self) -> &'static str {
        match self {
            Self::SetReferrer { .. } => "set-referrer",
            Self::RegisterReferrer { .. } => "register-referrer",
        }
    }

    fn submit_context(&self) -> &'static str {
        match self {
            Self::SetReferrer { .. } => "set referrer",
            Self::RegisterReferrer { .. } => "register referrer",
        }
    }

    fn validate(&self) -> Result<(), CliError> {
        match self {
            Self::SetReferrer { code } => validate_referral_code(code),
            Self::RegisterReferrer { code } => validate_referrer_code(code),
        }
    }
}

async fn run_code_action(
    inv: &Invocation,
    action: ReferrerAction<'_>,
) -> Result<(), anyhow::Error> {
    action.validate()?;
    if inv.dry_run() {
        return print_dry_run(
            inv,
            action.would_execute(),
            referral_dry_run_value(inv.remote()?.chain(), inv.dry_run_signer()?, &action),
        );
    }
    let signer = inv.resolve_signer()?;
    let remote = inv.remote()?;
    actions::send_raw_l1_json_action(
        remote.api(),
        &signer,
        &action,
        None,
        action.submit_context(),
    )
    .await?;

    inv.print(&One::new(ReferralSetRow {
        action: action.confirmation_action().to_string(),
        status: "submitted".to_string(),
        network: remote.chain().to_string(),
        signer: signer.address().to_string(),
        query_address: signer.query_address().to_string(),
        code: action.code().to_string(),
        reversibility: REVERSIBILITY.as_str().to_string(),
    }));
    Ok(())
}

fn referral_dry_run_value(
    chain: Chain,
    signer: Option<&SelectedSigner>,
    action: &ReferrerAction,
) -> serde_json::Value {
    serde_json::json!({
        "network": chain.to_string(),
        "signer": signer.map(|signer| signer.address().to_string()),
        "query_address": signer.map(|signer| signer.query_address().to_string()),
        "code": action.code(),
        "action": action,
        "reversibility": REVERSIBILITY,
    })
}

pub const GROUP: GroupSpec =
    GroupSpec::new(&["referral"], "Manage referral codes").aliases(&["referrals"]);

#[derive(Args, Debug, Clone)]
pub struct ReferralSetArgs {
    /// Referral code to apply (defaults to TESTNET for testnet verification)
    pub code: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ReferralRegisterArgs {
    /// Referral code to create. Hyperliquid currently caps referrer codes at 20 characters.
    pub code: String,
}

pub struct ReferralSet;

impl CliCommand for ReferralSet {
    type Args = ReferralSetArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["referral", "set"],
        "referral",
        "Set a referral code",
        "Set referral code.",
    )
    .args(&[arg("code")
        .describe("Referral code. Optional on testnet.")
        .resource_id("referral code")]);

    async fn run(inv: &Invocation, args: &ReferralSetArgs) -> Result<(), anyhow::Error> {
        let code = match args.code.as_deref() {
            Some(code) => code.to_string(),
            None => inv
                .settings()
                .default_referral_code(inv.remote()?.network().is_testnet())?
                .ok_or_else(|| {
                    CliError::Unsupported(format!(
                        "referral code is required; pass CODE explicitly, set {}, or use --testnet",
                        env::DEFAULT_REFERRAL_CODE
                    ))
                })?,
        };
        run_code_action(inv, ReferrerAction::SetReferrer { code: &code }).await
    }
}

pub struct ReferralRegister;

impl CliCommand for ReferralRegister {
    type Args = ReferralRegisterArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["referral", "register"],
        "referral",
        "Register your own referral code",
        "Register/create your own referral code.",
    )
    .args(&[arg("code").resource_id("referrer code")]);

    async fn run(inv: &Invocation, args: &ReferralRegisterArgs) -> Result<(), anyhow::Error> {
        run_code_action(inv, ReferrerAction::RegisterReferrer { code: &args.code }).await
    }
}

pub struct ReferralStatus;

impl CliCommand for ReferralStatus {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["referral", "status"],
        "referral",
        "Show referral status",
        "Show referral status for the configured signer.",
    )
    .risk(Risk::AccountState)
    .ows(OwsSupport::ExperimentalFeatureGated)
    .auth();

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_signer_account().await?;
        let info = inv
            .remote()?
            .api()
            .info::<ReferralInfo>(
                &InfoRequest::user("referral", user),
                "loading referral status",
            )
            .await?;
        inv.print(&One::new(ReferralStatusRow::new(user, info)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "0x0000000000000000000000000000000000000009";

    fn status_of(info: serde_json::Value) -> Result<ReferralStatusRow, serde_json::Error> {
        Ok(ReferralStatusRow::new(
            USER.parse().unwrap(),
            serde_json::from_value(info)?,
        ))
    }

    #[test]
    fn referral_status_handles_missing_referrer_state() {
        let status = status_of(serde_json::json!({})).unwrap();

        assert_eq!(status.referral_code, None);
        assert_eq!(status.referral_count, 0);
    }

    #[test]
    fn referral_status_reads_the_referrer_and_referred_by_blocks() {
        let status = status_of(serde_json::json!({
            "referredBy": {"referrer": "0x01", "code": "REFERRED"},
            "unclaimedRewards": "1.25",
            "referrerState": {"stage": "ready", "data": {
                "code": "MYCODE",
                "referralStates": [{"user": "0x02"}, {"user": "0x03"}]
            }}
        }))
        .unwrap();

        assert_eq!(status.referral_code.as_deref(), Some("MYCODE"));
        assert_eq!(status.referral_count, 2);
        assert_eq!(status.referred_by_code.as_deref(), Some("REFERRED"));
        assert_eq!(status.referred_by.as_deref(), Some("0x01"));
        assert_eq!(status.unclaimed_rewards.as_deref(), Some("1.25"));
        assert_eq!(status.claimed_rewards, None);
    }

    #[test]
    fn referral_status_rejects_a_field_of_the_wrong_type_instead_of_hiding_it() {
        assert!(status_of(serde_json::json!({"unclaimedRewards": 5})).is_err());
        assert!(
            status_of(serde_json::json!({"referrerState": {"data": {"referralStates": "x"}}}))
                .is_err()
        );
    }

    #[test]
    fn missing_status_values_show_the_shared_missing_cell() {
        let cells = status_of(serde_json::json!({})).unwrap().cells();

        assert_eq!(cells[1], crate::commands::common::rows::MISSING);
    }

    #[test]
    fn the_referrer_actions_keep_their_wire_shape() {
        for (action, tag) in [
            (
                ReferrerAction::SetReferrer { code: "MYCODE" },
                "setReferrer",
            ),
            (
                ReferrerAction::RegisterReferrer { code: "MYCODE" },
                "registerReferrer",
            ),
        ] {
            assert_eq!(
                serde_json::to_value(&action).unwrap(),
                serde_json::json!({"type": tag, "code": "MYCODE"})
            );
            let mut msgpack = vec![0x82, 0xa4];
            msgpack.extend(b"type");
            msgpack.push(0xa0 + tag.len() as u8);
            msgpack.extend(tag.as_bytes());
            msgpack.extend(b"\xa4code\xa6MYCODE");
            assert_eq!(rmp_serde::to_vec_named(&action).unwrap(), msgpack);
            assert_eq!(action.code(), "MYCODE");
        }
    }
}
