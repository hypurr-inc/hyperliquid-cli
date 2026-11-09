//! Referral status and set-referrer commands.

use clap::Args;
use hypersdk::Address;
use hypersdk::hypercore::Chain;
use serde::Serialize;

use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, NoArgs, OwsSupport, Risk, arg};
use crate::commands::actions;
use crate::commands::support::print_dry_run;
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::output::TableData;
use crate::settings::Settings;
use crate::wire::InfoRequest;

/// The referral code setup and wallet creation offer. Skips the config file and
/// rejects a code that wouldn't be accepted by `referral set`.
pub fn suggested_default_referral_code(
    settings: &Settings,
    is_testnet: bool,
) -> Result<Option<String>, anyhow::Error> {
    let code = settings.suggested_default_referral_code(is_testnet);
    if let Some(code) = code.as_deref() {
        validate_referral_code(code)?;
    }
    Ok(code)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReferralStatusOutput {
    pub user: String,
    pub referral_code: Option<String>,
    pub referral_count: usize,
    pub referred_by_code: Option<String>,
    pub referred_by: Option<String>,
    pub unclaimed_rewards: Option<String>,
    pub claimed_rewards: Option<String>,
    pub builder_rewards: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ReferralSetConfirmation {
    action: String,
    status: String,
    network: String,
    signer: String,
    query_address: String,
    code: String,
    reversibility: String,
}

struct ReferralSetOutput {
    row: ReferralSetConfirmation,
}

/// Hyperliquid accepts referral codes of up to 32 characters to apply, but
/// caps the codes a user registers at 20.
const MAX_REFERRAL_CODE_LEN: usize = 32;
const MAX_REFERRER_CODE_LEN: usize = 20;
const REVERSIBILITY: ActionReversibility = ActionReversibility::Irreversible;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferralActionKind {
    Set,
    Register,
}

impl ReferralActionKind {
    fn action_type(self) -> &'static str {
        match self {
            Self::Set => "setReferrer",
            Self::Register => "registerReferrer",
        }
    }

    fn would_execute(self) -> &'static str {
        match self {
            Self::Set => "set_referral_code",
            Self::Register => "register_referrer_code",
        }
    }

    fn confirmation_action(self) -> &'static str {
        match self {
            Self::Set => "set-referrer",
            Self::Register => "register-referrer",
        }
    }

    fn submit_context(self) -> &'static str {
        match self {
            Self::Set => "set referrer",
            Self::Register => "register referrer",
        }
    }

    fn validate(self, code: &str) -> Result<(), CliError> {
        match self {
            Self::Set => validate_referral_code(code),
            Self::Register => validate_referrer_code(code),
        }
    }
}

impl TableData for ReferralStatusOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "User",
            "Referral Code",
            "Referral Count",
            "Referred By Code",
            "Referred By",
            "Unclaimed Rewards",
            "Claimed Rewards",
            "Builder Rewards",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.user.clone(),
            display_optional(&self.referral_code),
            self.referral_count.to_string(),
            display_optional(&self.referred_by_code),
            display_optional(&self.referred_by),
            display_optional(&self.unclaimed_rewards),
            display_optional(&self.claimed_rewards),
            display_optional(&self.builder_rewards),
        ]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

impl TableData for ReferralSetOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Action",
            "Status",
            "Network",
            "Signer",
            "Query Address",
            "Code",
            "Reversibility",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.row.action.clone(),
            self.row.status.clone(),
            self.row.network.clone(),
            self.row.signer.clone(),
            self.row.query_address.clone(),
            self.row.code.clone(),
            self.row.reversibility.clone(),
        ]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.row).unwrap_or_else(|_| serde_json::json!({}))
    }
}

#[derive(Debug, Serialize)]
struct ReferrerAction<'a> {
    #[serde(rename = "type")]
    action_type: &'static str,
    code: &'a str,
}

/// Query referral status for the authenticated user.
async fn status(inv: &Invocation, user: Address) -> Result<(), anyhow::Error> {
    let raw = inv
        .remote()?
        .api()
        .info::<serde_json::Value>(
            &InfoRequest::user("referral", user),
            "loading referral status",
        )
        .await?;
    inv.print(&referral_status_from_value(user.to_string(), &raw));
    Ok(())
}

/// `referral set` and `referral register`: preview under `--dry-run`, otherwise
/// sign and submit `code`.
async fn run_code_action(
    inv: &Invocation,
    kind: ReferralActionKind,
    code: &str,
) -> Result<(), anyhow::Error> {
    kind.validate(code)?;
    if inv.dry_run() {
        let signer_context = inv.resolve_signer().ok();
        return print_dry_run(
            inv,
            kind.would_execute(),
            referral_dry_run_value(
                inv.remote()?.chain(),
                signer_context.as_ref().map(|resolved| resolved.address()),
                signer_context
                    .as_ref()
                    .map(|resolved| resolved.query_address()),
                code,
                kind,
            ),
        );
    }
    let resolved = inv.resolve_signer()?;
    let signer = resolved.selected_signer();
    let remote = inv.remote()?;
    actions::send_raw_l1_json_action(
        remote.api(),
        &signer,
        &ReferrerAction {
            action_type: kind.action_type(),
            code,
        },
        actions::RawL1ActionMetadata::new(actions::nonce_now()),
        kind.submit_context(),
    )
    .await?;

    inv.print(&ReferralSetOutput {
        row: ReferralSetConfirmation {
            action: kind.confirmation_action().to_string(),
            status: "submitted".to_string(),
            network: remote.chain().to_string(),
            signer: signer.address().to_string(),
            query_address: signer.query_address().to_string(),
            code: code.to_string(),
            reversibility: REVERSIBILITY.as_str().to_string(),
        },
    });
    Ok(())
}

pub fn referral_status_from_value(user: String, raw: &serde_json::Value) -> ReferralStatusOutput {
    let referrer_state = raw.get("referrerState");
    let referrer_data = referrer_state.and_then(|state| state.get("data"));
    let referral_code = referrer_data
        .and_then(|data| data.get("code"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let referral_count = referrer_data
        .and_then(|data| data.get("referralStates"))
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    let referred_by = raw.get("referredBy");

    ReferralStatusOutput {
        user,
        referral_code,
        referral_count,
        referred_by_code: referred_by
            .and_then(|value| value.get("code"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        referred_by: referred_by
            .and_then(|value| value.get("referrer"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        unclaimed_rewards: string_field(raw, "unclaimedRewards"),
        claimed_rewards: string_field(raw, "claimedRewards"),
        builder_rewards: string_field(raw, "builderRewards"),
    }
}

pub fn validate_referral_code(code: &str) -> Result<(), CliError> {
    validate_code(code, MAX_REFERRAL_CODE_LEN, "referral code")
}

fn validate_referrer_code(code: &str) -> Result<(), CliError> {
    validate_code(code, MAX_REFERRER_CODE_LEN, "referrer code")
}

fn referral_dry_run_value(
    chain: Chain,
    signer: Option<Address>,
    query_address: Option<Address>,
    code: &str,
    action: ReferralActionKind,
) -> serde_json::Value {
    serde_json::json!({
        "network": chain.to_string(),
        "signer": signer.map(|address| address.to_string()),
        "query_address": query_address.map(|address| address.to_string()),
        "code": code,
        "action": {
            "type": action.action_type(),
            "code": code,
        },
        "reversibility": REVERSIBILITY,
    })
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

fn string_field(raw: &serde_json::Value, field: &str) -> Option<String> {
    raw.get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn display_optional(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| "None".to_string())
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
            None => {
                // `HYPERLIQUID_DEFAULT_REFERRAL_CODE`, the config file
                // `default_referral_code`, the build-time default, then `TESTNET`
                // on testnet. On mainnet nothing is assumed.
                let remote = inv.remote()?;
                remote
                    .settings()
                    .default_referral_code(remote.network().is_testnet())?
                    .ok_or_else(|| {
                        CliError::Unsupported(
                            "referral code is required; pass CODE explicitly, set HYPERLIQUID_DEFAULT_REFERRAL_CODE, or use --testnet"
                                .to_string(),
                        )
                    })?
            }
        };
        run_code_action(inv, ReferralActionKind::Set, &code).await
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
        run_code_action(inv, ReferralActionKind::Register, &args.code).await
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
        let user = inv.resolve_signer()?.query_address();
        status(inv, user).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn referral_status_extracts_code_and_count() {
        let raw = serde_json::json!({
            "referredBy": {
                "referrer": "0x0000000000000000000000000000000000000001",
                "code": "REFERRED"
            },
            "unclaimedRewards": "1.25",
            "claimedRewards": "2.50",
            "builderRewards": "0.10",
            "referrerState": {
                "stage": "ready",
                "data": {
                    "code": "MINE",
                    "referralStates": [
                        { "user": "0x0000000000000000000000000000000000000002" },
                        { "user": "0x0000000000000000000000000000000000000003" }
                    ]
                }
            }
        });

        let status = referral_status_from_value(
            "0x0000000000000000000000000000000000000009".to_string(),
            &raw,
        );

        assert_eq!(status.referral_code.as_deref(), Some("MINE"));
        assert_eq!(status.referral_count, 2);
        assert_eq!(status.referred_by_code.as_deref(), Some("REFERRED"));
        assert_eq!(status.unclaimed_rewards.as_deref(), Some("1.25"));
    }

    #[test]
    fn referral_status_handles_missing_referrer_state() {
        let status = referral_status_from_value(
            "0x0000000000000000000000000000000000000009".to_string(),
            &serde_json::json!({}),
        );

        assert_eq!(status.referral_code, None);
        assert_eq!(status.referral_count, 0);
        assert!(status.rows()[0].contains(&"None".to_string()));
    }

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

        assert!(ReferralActionKind::Set.validate(&code).is_ok());
        assert!(ReferralActionKind::Register.validate(&code).is_err());
        assert!(
            ReferralActionKind::Set
                .validate(&"A".repeat(MAX_REFERRAL_CODE_LEN + 1))
                .is_err()
        );
    }

    #[test]
    fn suggested_referral_code_is_validated() {
        let settings = |code: &str| {
            Settings::detached(
                crate::settings::EnvSource::fixed([(
                    crate::settings::env::DEFAULT_REFERRAL_CODE,
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
