use alloy::sol;
use clap::Args;
use hypersdk::hypercore::Chain;
use hypersdk::{Address, Decimal};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, NoArgs, arg};
use crate::commands::account::{OptionalAddressArgs, USER_SELECTOR};
use crate::commands::actions;
use crate::commands::support::print_dry_run;
use crate::commands::user_signed::{self, UserSigned};
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::mode::Mode;
use crate::output::{self, JsonValueOutput, TableData};
use crate::quantity::{self, Scale};
use crate::resolvers::{HexAddressError, parse_hex_address};
use crate::signing::{SelectedSigner, Signs};
use crate::wire::InfoRequest;

const ASSET: &str = "HYPE";
const WITHDRAWAL_NOTE: &str = "7-day withdrawal queue before funds become available in spot";

sol! {
    #[derive(Serialize)]
    struct TokenDelegate {
        string hyperliquidChain;
        address validator;
        uint64 wei;
        bool isUndelegate;
        uint64 nonce;
    }

    #[derive(Serialize)]
    struct CDeposit {
        string hyperliquidChain;
        uint64 wei;
        uint64 nonce;
    }

    #[derive(Serialize)]
    struct CWithdraw {
        string hyperliquidChain;
        uint64 wei;
        uint64 nonce;
    }

    #[derive(Serialize)]
    struct LinkStakingUser {
        string hyperliquidChain;
        address user;
        bool isFinalize;
        uint64 nonce;
    }
}

impl UserSigned for TokenDelegate {
    const ACTION_TYPE: &'static str = "tokenDelegate";
}

impl UserSigned for CDeposit {
    const ACTION_TYPE: &'static str = "cDeposit";
}

impl UserSigned for CWithdraw {
    const ACTION_TYPE: &'static str = "cWithdraw";
}

impl UserSigned for LinkStakingUser {
    const ACTION_TYPE: &'static str = "linkStakingUser";
}

const STAKING_LINK_WARNING: &str = "Staking-link changes fee discount attribution between trading and staking accounts. Finalization may be permanent; verify both account addresses and control before signing.";

/// Arguments for `staking delegate` and `staking undelegate`.
#[derive(Args, Debug, Clone)]
pub struct DelegateArgs {
    /// Validator address to delegate to or undelegate from
    #[arg(long)]
    pub validator: String,

    /// Amount of HYPE to delegate or undelegate
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,
}

/// Arguments for `staking deposit` and `staking withdraw`.
#[derive(Args, Debug, Clone)]
pub struct AmountArgs {
    /// Amount of HYPE to move
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,
}

/// Arguments for `staking link initiate` and `staking link finalize`.
#[derive(Args, Debug, Clone)]
pub struct LinkArgs {
    /// Protocol user address to link. For initiate, the staking user; for finalize, the trading user.
    #[arg(long)]
    pub user: String,

    /// Confirm live execution without prompting
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StakingActionKind {
    Delegate,
    Undelegate,
    Deposit,
    Withdraw,
    ClaimRewards,
    LinkInitiate,
    LinkFinalize,
}

impl StakingActionKind {
    fn reversibility(self) -> ActionReversibility {
        match self {
            Self::Delegate | Self::Undelegate | Self::Deposit | Self::Withdraw => {
                ActionReversibility::PartiallyReversible
            }
            Self::ClaimRewards | Self::LinkInitiate | Self::LinkFinalize => {
                ActionReversibility::Irreversible
            }
        }
    }

    fn would_execute(self) -> &'static str {
        match self {
            Self::Delegate => "delegate_hype",
            Self::Undelegate => "undelegate_hype",
            Self::Deposit => "deposit_hype_to_staking",
            Self::Withdraw => "queue_hype_staking_withdrawal",
            Self::ClaimRewards => "claim_staking_rewards",
            Self::LinkInitiate | Self::LinkFinalize => "link_staking_user",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DelegatorSummary {
    #[serde(with = "rust_decimal::serde::str")]
    pub delegated: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub undelegated: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub total_pending_withdrawal: Decimal,
    pub n_pending_withdrawals: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DelegationRow {
    pub validator: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    pub locked_until_timestamp: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RewardRow {
    pub time: u64,
    pub source: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub total_amount: Decimal,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ValidatorSummary {
    pub validator: String,
    pub signer: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub n_recent_blocks: Option<u64>,
    pub stake: u64,
    pub is_jailed: bool,
    pub unjailable_after: Option<u64>,
    pub is_active: bool,
    #[serde(with = "rust_decimal::serde::str")]
    pub commission: Decimal,
    #[serde(default)]
    pub stats: Vec<(String, ValidatorStats)>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ValidatorStats {
    #[serde(with = "rust_decimal::serde::str")]
    pub uptime_fraction: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub predicted_apr: Decimal,
    pub n_samples: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StakingSummaryOutput {
    address: String,
    summary: DelegatorSummary,
    pending_rewards: Decimal,
    delegations: Vec<DelegationRow>,
    rewards: Vec<RewardRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewardsOutput {
    rows: Vec<RewardRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatorsOutput {
    rows: Vec<ValidatorSummary>,
}

struct ActionConfirmationContext<'a> {
    chain: Chain,
    signer: &'a SelectedSigner,
    action_kind: StakingActionKind,
}

#[derive(Debug, Clone, Serialize)]
struct ActionConfirmation {
    action: String,
    status: String,
    network: String,
    signer: String,
    acting_as: String,
    reversibility: String,
    #[serde(with = "rust_decimal::serde::str")]
    amount: Decimal,
    asset: String,
    validator: Option<String>,
    note: Option<String>,
}

struct ActionOutput {
    row: ActionConfirmation,
}

#[derive(Debug, Clone, Serialize)]
struct LinkStakingConfirmation {
    status: String,
    action: String,
    phase: String,
    acting_as: String,
    signer: String,
    user: String,
    is_finalize: bool,
    network: String,
    reversibility: String,
    warning: String,
}

struct LinkStakingOutput {
    row: LinkStakingConfirmation,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimRewardsAction {
    #[serde(rename = "type")]
    action_type: &'static str,
}

impl TableData for StakingSummaryOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Address",
            "Delegated",
            "Undelegated",
            "Pending Withdrawal",
            "Pending Withdrawals",
            "Pending Rewards",
            "Delegations",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.address.clone(),
            self.summary.delegated.to_string(),
            self.summary.undelegated.to_string(),
            self.summary.total_pending_withdrawal.to_string(),
            self.summary.n_pending_withdrawals.to_string(),
            self.pending_rewards.to_string(),
            self.delegations.len().to_string(),
        ]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "address": self.address,
            "delegated": self.summary.delegated,
            "undelegated": self.summary.undelegated,
            "total_pending_withdrawal": self.summary.total_pending_withdrawal,
            "n_pending_withdrawals": self.summary.n_pending_withdrawals,
            "pending_rewards": self.pending_rewards,
            "delegations": self.delegations.iter().map(delegation_json).collect::<Vec<_>>(),
            "rewards": self.rewards.iter().map(reward_json).collect::<Vec<_>>(),
        })
    }
}

impl TableData for RewardsOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Time", "Source", "Total Amount"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    row.time.to_string(),
                    row.source.clone(),
                    row.total_amount.to_string(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::Value::Array(self.rows.iter().map(reward_json).collect())
    }
}

impl TableData for ValidatorsOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Name",
            "Validator",
            "Stake",
            "Commission",
            "APR",
            "Active",
            "Jailed",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    row.name.clone(),
                    row.validator.clone(),
                    format_hype_wei(row.stake),
                    row.commission.to_string(),
                    row.stats
                        .iter()
                        .find(|(period, _)| period == "day")
                        .or_else(|| row.stats.first())
                        .map(|(_, stats)| stats.predicted_apr.to_string())
                        .unwrap_or_default(),
                    row.is_active.to_string(),
                    row.is_jailed.to_string(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.rows
                .iter()
                .map(|row| {
                    serde_json::json!({
                        "validator": row.validator,
                        "signer": row.signer,
                        "name": row.name,
                        "description": row.description,
                        "n_recent_blocks": row.n_recent_blocks,
                        "stake": row.stake,
                        "stake_hype": format_hype_wei(row.stake),
                        "is_jailed": row.is_jailed,
                        "unjailable_after": row.unjailable_after,
                        "is_active": row.is_active,
                        "commission": row.commission,
                        "stats": row.stats.iter().map(|(period, stats)| serde_json::json!({
                            "period": period,
                            "uptime_fraction": stats.uptime_fraction,
                            "predicted_apr": stats.predicted_apr,
                            "n_samples": stats.n_samples,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect(),
        )
    }
}

impl TableData for ActionOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Action",
            "Status",
            "Network",
            "Signer",
            "Acting As",
            "Reversibility",
            "Amount",
            "Asset",
            "Validator",
            "Note",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.row.action.clone(),
            self.row.status.clone(),
            self.row.network.clone(),
            self.row.signer.clone(),
            self.row.acting_as.clone(),
            self.row.reversibility.clone(),
            self.row.amount.to_string(),
            self.row.asset.clone(),
            self.row.validator.clone().unwrap_or_default(),
            self.row.note.clone().unwrap_or_default(),
        ]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.row).unwrap_or_else(|_| serde_json::json!({}))
    }
}

impl TableData for LinkStakingOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Status",
            "Action",
            "Phase",
            "Acting As",
            "Signer",
            "User",
            "Is Finalize",
            "Network",
            "Reversibility",
            "Warning",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.row.status.clone(),
            self.row.action.clone(),
            self.row.phase.clone(),
            self.row.acting_as.clone(),
            self.row.signer.clone(),
            self.row.user.clone(),
            self.row.is_finalize.to_string(),
            self.row.network.clone(),
            self.row.reversibility.clone(),
            self.row.warning.clone(),
        ]]
    }

    fn pretty_rows(&self) -> Vec<Vec<String>> {
        self.rows()
            .into_iter()
            .map(|mut row| {
                if row.len() > 7 {
                    row[7] = output::colors::yellow(&row[7]);
                }
                row
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.row).unwrap_or_else(|_| serde_json::json!({}))
    }
}

async fn summary(inv: &Invocation, user: Address) -> Result<(), anyhow::Error> {
    let api = inv.remote()?.api();
    let summary = staking_info(api, "delegatorSummary", user, "loading staking summary").await?;
    let delegations = staking_info(api, "delegations", user, "loading delegations").await?;
    let rewards: Vec<RewardRow> =
        staking_info(api, "delegatorRewards", user, "loading staking rewards").await?;
    let pending_rewards = rewards.iter().map(|row| row.total_amount).sum();
    inv.print(&StakingSummaryOutput {
        address: user.to_string(),
        summary,
        pending_rewards,
        delegations,
        rewards,
    });
    Ok(())
}

async fn validators(inv: &Invocation) -> Result<(), anyhow::Error> {
    let rows = inv
        .remote()?
        .api()
        .info::<Vec<ValidatorSummary>>(
            &InfoRequest::new("validatorSummaries"),
            "loading validator summaries",
        )
        .await?;
    inv.print(&ValidatorsOutput { rows });
    Ok(())
}

async fn rewards(inv: &Invocation, user: Address) -> Result<(), anyhow::Error> {
    let rows = staking_info(
        inv.remote()?.api(),
        "delegatorRewards",
        user,
        "loading staking rewards",
    )
    .await?;
    inv.print(&RewardsOutput { rows });
    Ok(())
}

async fn history(inv: &Invocation, user: Address) -> Result<(), anyhow::Error> {
    let value: serde_json::Value = staking_info(
        inv.remote()?.api(),
        "delegatorHistory",
        user,
        "loading staking history",
    )
    .await?;
    inv.print(&JsonValueOutput::new(value, "No staking history."));
    Ok(())
}

async fn staking_info<T: DeserializeOwned>(
    api: &ApiClient,
    kind: &'static str,
    user: Address,
    context: &'static str,
) -> Result<T, CliError> {
    api.info(&InfoRequest::user(kind, user), context).await
}

async fn delegate(
    inv: &Invocation,
    signer: &SelectedSigner,
    validator: Address,
    args: &DelegateArgs,
    is_undelegate: bool,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let wei = validate_amount_to_wei(args.amount)?;
    user_signed::submit(remote.api(), signer, |hyperliquid_chain, nonce| {
        TokenDelegate {
            hyperliquidChain: hyperliquid_chain,
            validator,
            wei,
            isUndelegate: is_undelegate,
            nonce,
        }
    })
    .await?;

    let (action_kind, action_name) = if is_undelegate {
        (StakingActionKind::Undelegate, "undelegate")
    } else {
        (StakingActionKind::Delegate, "delegate")
    };
    print_action_confirmation(
        inv,
        ActionConfirmationContext {
            chain: remote.chain(),
            signer,
            action_kind,
        },
        action_name,
        args.amount,
        Some(args.validator.clone()),
        None,
    );
    Ok(())
}

/// Move HYPE from spot to staking balance.
async fn deposit(
    inv: &Invocation,
    signer: &SelectedSigner,
    args: &AmountArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let wei = validate_amount_to_wei(args.amount)?;
    user_signed::submit(remote.api(), signer, |hyperliquid_chain, nonce| CDeposit {
        hyperliquidChain: hyperliquid_chain,
        wei,
        nonce,
    })
    .await?;
    print_action_confirmation(
        inv,
        ActionConfirmationContext {
            chain: remote.chain(),
            signer,
            action_kind: StakingActionKind::Deposit,
        },
        "deposit",
        args.amount,
        None,
        None,
    );
    Ok(())
}

/// Queue HYPE withdrawal from staking to spot balance.
async fn withdraw(
    inv: &Invocation,
    signer: &SelectedSigner,
    args: &AmountArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let wei = validate_amount_to_wei(args.amount)?;
    user_signed::submit(remote.api(), signer, |hyperliquid_chain, nonce| CWithdraw {
        hyperliquidChain: hyperliquid_chain,
        wei,
        nonce,
    })
    .await?;
    print_action_confirmation(
        inv,
        ActionConfirmationContext {
            chain: remote.chain(),
            signer,
            action_kind: StakingActionKind::Withdraw,
        },
        "withdraw",
        args.amount,
        None,
        Some(WITHDRAWAL_NOTE.to_string()),
    );
    Ok(())
}

async fn claim_rewards(inv: &Invocation, signer: &SelectedSigner) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    actions::send_raw_l1_json_action(
        remote.api(),
        signer,
        &ClaimRewardsAction {
            action_type: "claimRewards",
        },
        actions::RawL1ActionMetadata::new(actions::nonce_now()),
        "claim staking rewards",
    )
    .await?;
    print_action_confirmation(
        inv,
        ActionConfirmationContext {
            chain: remote.chain(),
            signer,
            action_kind: StakingActionKind::ClaimRewards,
        },
        "claim-rewards",
        Decimal::ZERO,
        None,
        None,
    );
    Ok(())
}

fn link_dry_run_value(chain: Chain, user: Address, is_finalize: bool) -> serde_json::Value {
    serde_json::json!({
        "phase": link_phase(is_finalize),
        "user": user.to_string(),
        "is_finalize": is_finalize,
        "network": chain.to_string(),
        "reversibility": action_kind_for_link(is_finalize).reversibility(),
        "warning": STAKING_LINK_WARNING,
        "verified_shape": {
            "source": "@nktkas/hyperliquid v0.31.0 linkStakingUser",
            "signature_type": "HyperliquidTransaction:LinkStakingUser",
            "eip712_fields": ["hyperliquidChain", "user", "isFinalize", "nonce"]
        },
        "action": {
            "type": LinkStakingUser::ACTION_TYPE,
            "signatureChainId": chain.arbitrum_id().to_string(),
            "hyperliquidChain": chain.to_string(),
            "user": user.to_string(),
            "isFinalize": is_finalize,
            "nonce": "current_timestamp_ms"
        }
    })
}

fn delegate_dry_run_value(
    chain: Chain,
    args: &DelegateArgs,
    action_kind: StakingActionKind,
) -> serde_json::Value {
    serde_json::json!({
        "validator": args.validator,
        "amount": args.amount.to_string(),
        "asset": ASSET,
        "network": chain.to_string(),
        "reversibility": action_kind.reversibility(),
    })
}

fn amount_dry_run_value(
    chain: Chain,
    args: &AmountArgs,
    action_kind: StakingActionKind,
) -> serde_json::Value {
    let mut value = serde_json::json!({
        "amount": args.amount.to_string(),
        "asset": ASSET,
        "network": chain.to_string(),
        "reversibility": action_kind.reversibility(),
    });
    if action_kind == StakingActionKind::Withdraw
        && let Some(object) = value.as_object_mut()
    {
        object.insert("note".to_string(), serde_json::json!(WITHDRAWAL_NOTE));
    }
    value
}

fn claim_rewards_dry_run_value(chain: Chain) -> serde_json::Value {
    serde_json::json!({
        "asset": ASSET,
        "network": chain.to_string(),
        "reversibility": StakingActionKind::ClaimRewards.reversibility(),
    })
}

/// Link staking and trading accounts for fee discount attribution.
async fn link_staking_user(
    inv: &Invocation,
    signer: &SelectedSigner,
    user: Address,
    args: &LinkArgs,
    is_finalize: bool,
) -> Result<(), anyhow::Error> {
    if !args.yes {
        confirm_staking_link(inv.mode(), user, is_finalize)?;
    }
    let remote = inv.remote()?;
    user_signed::submit(remote.api(), signer, |hyperliquid_chain, nonce| {
        LinkStakingUser {
            hyperliquidChain: hyperliquid_chain,
            user,
            isFinalize: is_finalize,
            nonce,
        }
    })
    .await?;

    inv.print(&LinkStakingOutput {
        row: LinkStakingConfirmation {
            status: "submitted".to_string(),
            action: "staking-link".to_string(),
            phase: link_phase(is_finalize).to_string(),
            acting_as: signer.query_address().to_string(),
            signer: signer.address().to_string(),
            user: user.to_string(),
            is_finalize,
            network: remote.chain().to_string(),
            reversibility: action_kind_for_link(is_finalize)
                .reversibility()
                .as_str()
                .to_string(),
            warning: STAKING_LINK_WARNING.to_string(),
        },
    });
    Ok(())
}

/// A validator or linked-user address argument, never the zero address. `subject` names the address in the zero-address error.
fn protocol_address(raw: &str, subject: &str) -> Result<Address, CliError> {
    parse_hex_address(raw, true).map_err(|err| match err {
        HexAddressError::Format => CliError::Unsupported(format!("Invalid address: {raw}")),
        HexAddressError::Zero => {
            CliError::Configuration(format!("{subject} address must not be the zero address"))
        }
    })
}

fn validate_amount_to_wei(amount: Decimal) -> Result<u64, CliError> {
    quantity::positive(amount, Scale::HYPE_CORE)
        .map_err(|err| err.config("amount", Scale::HYPE_CORE))
}

fn format_hype_wei(wei: u64) -> String {
    Scale::HYPE_CORE.amount(wei).to_string()
}

fn delegation_json(row: &DelegationRow) -> serde_json::Value {
    serde_json::json!({
        "validator": row.validator,
        "amount": row.amount,
        "locked_until_timestamp": row.locked_until_timestamp,
    })
}

fn reward_json(row: &RewardRow) -> serde_json::Value {
    serde_json::json!({
        "time": row.time,
        "source": row.source,
        "total_amount": row.total_amount,
    })
}

fn link_phase(is_finalize: bool) -> &'static str {
    if is_finalize { "finalize" } else { "initiate" }
}

fn action_kind_for_link(is_finalize: bool) -> StakingActionKind {
    if is_finalize {
        StakingActionKind::LinkFinalize
    } else {
        StakingActionKind::LinkInitiate
    }
}

fn confirm_staking_link(mode: &Mode, user: Address, is_finalize: bool) -> Result<(), CliError> {
    let phase = link_phase(is_finalize);
    let question = format!("Submit staking link {phase} for user {user}?");
    crate::prompt::consent(
        mode,
        "--yes",
        STAKING_LINK_WARNING,
        &question,
        "staking-link confirmation required; action cancelled. Rerun with --yes for deliberate automation.",
    )
}

fn print_action_confirmation(
    inv: &Invocation,
    context: ActionConfirmationContext<'_>,
    action: &str,
    amount: Decimal,
    validator: Option<String>,
    note: Option<String>,
) {
    inv.print(&ActionOutput {
        row: ActionConfirmation {
            action: action.to_string(),
            status: "submitted".to_string(),
            network: context.chain.to_string(),
            signer: context.signer.address().to_string(),
            acting_as: context.signer.query_address().to_string(),
            reversibility: (context.action_kind.reversibility()).as_str().to_string(),
            amount,
            asset: "HYPE".to_string(),
            validator,
            note,
        },
    });
}

pub const GROUP: GroupSpec = GroupSpec::new(&["staking"], "Manage staking");

pub const LINK_GROUP: GroupSpec = GroupSpec::new(
    &["staking", "link"],
    "Link trading and staking accounts for fee discount attribution",
);

pub struct StakingSummary;

impl CliCommand for StakingSummary {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["staking", "summary"],
        "staking",
        "Show the staking summary for an address or wallet",
        "Show staking summary for an address or wallet.",
    )
    .args(&[USER_SELECTOR.schema_required()]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        summary(inv, args.resolve(inv)?).await
    }
}

pub struct StakingValidators;

impl CliCommand for StakingValidators {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["staking", "validators"],
        "staking",
        "List validators",
        "List staking validators.",
    );

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        validators(inv).await
    }
}

pub struct StakingRewards;

impl CliCommand for StakingRewards {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["staking", "rewards"],
        "staking",
        "Show staking rewards for an address or wallet",
        "Show staking rewards for an address or wallet.",
    )
    .args(&[USER_SELECTOR.schema_required()]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        rewards(inv, args.resolve(inv)?).await
    }
}

pub struct StakingHistory;

impl CliCommand for StakingHistory {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["staking", "history"],
        "staking",
        "Show staking delegation and withdrawal history for an address or wallet",
        "Show staking delegation and withdrawal history for an address or wallet.",
    )
    .args(&[USER_SELECTOR.schema_required()]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        history(inv, args.resolve(inv)?).await
    }
}

/// `staking delegate` and `staking undelegate`.
async fn run_delegation(
    inv: &Invocation,
    args: &DelegateArgs,
    action_kind: StakingActionKind,
) -> Result<(), anyhow::Error> {
    let validator = protocol_address(&args.validator, "validator")?;
    validate_amount_to_wei(args.amount)?;
    if inv.dry_run() {
        return print_dry_run(
            inv,
            action_kind.would_execute(),
            delegate_dry_run_value(inv.remote()?.chain(), args, action_kind),
        );
    }
    let signer = inv.resolve_signer()?;
    delegate(
        inv,
        &signer,
        validator,
        args,
        action_kind == StakingActionKind::Undelegate,
    )
    .await
}

/// `staking deposit` and `staking withdraw`.
async fn run_amount(
    inv: &Invocation,
    args: &AmountArgs,
    action_kind: StakingActionKind,
) -> Result<(), anyhow::Error> {
    validate_amount_to_wei(args.amount)?;
    if inv.dry_run() {
        return print_dry_run(
            inv,
            action_kind.would_execute(),
            amount_dry_run_value(inv.remote()?.chain(), args, action_kind),
        );
    }
    let signer = inv.resolve_signer()?;
    match action_kind {
        StakingActionKind::Deposit => deposit(inv, &signer, args).await,
        _ => withdraw(inv, &signer, args).await,
    }
}

pub struct StakingDelegate;

impl CliCommand for StakingDelegate {
    type Args = DelegateArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["staking", "delegate"],
        "staking",
        "Delegate stake to a validator",
        "Delegate HYPE to a validator.",
    )
    .signs(Signs::user_signed::<TokenDelegate>())
    .args(&[
        arg("validator")
            .kind(InputKind::ProtocolObjectAddress)
            .describe("Validator address.")
            .resource_id("validator address"),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("HYPE amount."),
    ]);

    async fn run(inv: &Invocation, args: &DelegateArgs) -> Result<(), anyhow::Error> {
        run_delegation(inv, args, StakingActionKind::Delegate).await
    }
}

pub struct StakingUndelegate;

impl CliCommand for StakingUndelegate {
    type Args = DelegateArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["staking", "undelegate"],
        "staking",
        "Undelegate stake from a validator",
        "Undelegate HYPE from a validator.",
    )
    .signs(Signs::user_signed::<TokenDelegate>())
    .args(&[
        arg("validator")
            .kind(InputKind::ProtocolObjectAddress)
            .describe("Validator address.")
            .resource_id("validator address"),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("HYPE amount."),
    ]);

    async fn run(inv: &Invocation, args: &DelegateArgs) -> Result<(), anyhow::Error> {
        run_delegation(inv, args, StakingActionKind::Undelegate).await
    }
}

pub struct StakingDeposit;

impl CliCommand for StakingDeposit {
    type Args = AmountArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["staking", "deposit"],
        "staking",
        "Deposit to staking",
        "Deposit HYPE to staking.",
    )
    .signs(Signs::user_signed::<CDeposit>())
    .args(&[arg("amount")
        .kind(InputKind::Amount)
        .describe("HYPE amount.")]);

    async fn run(inv: &Invocation, args: &AmountArgs) -> Result<(), anyhow::Error> {
        run_amount(inv, args, StakingActionKind::Deposit).await
    }
}

pub struct StakingWithdraw;

impl CliCommand for StakingWithdraw {
    type Args = AmountArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["staking", "withdraw"],
        "staking",
        "Withdraw from staking",
        "Queue HYPE withdrawal from staking.",
    )
    .signs(Signs::user_signed::<CWithdraw>())
    .args(&[arg("amount")
        .kind(InputKind::Amount)
        .describe("HYPE amount.")]);

    async fn run(inv: &Invocation, args: &AmountArgs) -> Result<(), anyhow::Error> {
        run_amount(inv, args, StakingActionKind::Withdraw).await
    }
}

pub struct StakingClaimRewards;

impl CliCommand for StakingClaimRewards {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["staking", "claim-rewards"],
        "staking",
        "Claim staking rewards",
        "Claim staking rewards.",
    );

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        if inv.dry_run() {
            let action_kind = StakingActionKind::ClaimRewards;
            return print_dry_run(
                inv,
                action_kind.would_execute(),
                claim_rewards_dry_run_value(inv.remote()?.chain()),
            );
        }
        let signer = inv.resolve_signer()?;
        claim_rewards(inv, &signer).await
    }
}

pub struct StakingLinkInitiate;

impl CliCommand for StakingLinkInitiate {
    type Args = LinkArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["staking", "link", "initiate"],
        "staking",
        "Initiate staking-link from the trading account, targeting the staking user",
        "Initiate staking-link fee discount attribution from the trading account, targeting the staking user. This is control-affecting; use --dry-run first.",
    )
    .signs(Signs::user_signed::<LinkStakingUser>())
    .prompt()
    .args(&[
        arg("user").kind(InputKind::ProtocolObjectAddress).describe("Protocol user address for the staking account to link.").resource_id("staking-link user address"),
        arg("yes").kind(InputKind::Boolean),
    ]);

    async fn run(inv: &Invocation, args: &LinkArgs) -> Result<(), anyhow::Error> {
        staking_link(inv, args, false).await
    }
}

pub struct StakingLinkFinalize;

impl CliCommand for StakingLinkFinalize {
    type Args = LinkArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["staking", "link", "finalize"],
        "staking",
        "Finalize staking-link from the staking account, targeting the trading user",
        "Finalize staking-link fee discount attribution from the staking account, targeting the trading user. Finalization may be permanent; use --dry-run first.",
    )
    .signs(Signs::user_signed::<LinkStakingUser>())
    .prompt()
    .args(&[
        arg("user").kind(InputKind::ProtocolObjectAddress).describe("Protocol user address for the trading account to link.").resource_id("staking-link user address"),
        arg("yes").kind(InputKind::Boolean),
    ]);

    async fn run(inv: &Invocation, args: &LinkArgs) -> Result<(), anyhow::Error> {
        staking_link(inv, args, true).await
    }
}

async fn staking_link(
    inv: &Invocation,
    args: &LinkArgs,
    is_finalize: bool,
) -> Result<(), anyhow::Error> {
    let user = protocol_address(&args.user, "staking-link user")?;
    if inv.dry_run() {
        let action_kind = action_kind_for_link(is_finalize);
        return print_dry_run(
            inv,
            action_kind.would_execute(),
            link_dry_run_value(inv.remote()?.chain(), user, is_finalize),
        );
    }
    let signer = inv.resolve_signer()?;
    link_staking_user(inv, &signer, user, args, is_finalize).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_to_wei_accepts_only_positive_eight_decimal_amounts() {
        assert_eq!(
            validate_amount_to_wei("0.00000001".parse::<Decimal>().unwrap()).unwrap(),
            1
        );
        assert!(validate_amount_to_wei(Decimal::ZERO).is_err());
        assert!(validate_amount_to_wei("-1".parse::<Decimal>().unwrap()).is_err());
        assert!(validate_amount_to_wei("0.000000001".parse::<Decimal>().unwrap()).is_err());
    }

    #[test]
    fn protocol_address_accepts_a_bare_hex_address_and_names_a_malformed_one() {
        assert!(protocol_address("0000000000000000000000000000000000000002", "validator").is_ok());
        let err = protocol_address("0x02", "validator").unwrap_err();
        assert_eq!(err.exit_code(), 13);
        assert!(err.to_string().contains("Invalid address"));
    }
}
