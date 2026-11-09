use alloy::sol;
use clap::Args;
use hypersdk::hypercore::Chain;
use hypersdk::{Address, Decimal};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::cli_command::CliCommand;
use crate::command_spec::{CommandSpec, GroupSpec, InputKind, NoArgs, YES, YES_FLAG, arg};
use crate::commands::common::rows::{ListRow, One, Row, Rows, or_missing, style_column};
use crate::commands::common::selector::{OptionalAddressArgs, USER_SELECTOR};
use crate::commands::common::support::print_dry_run;
use crate::consent::{self, ConsentBlock};
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::exchange::actions;
use crate::exchange::user_signed;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::{self, JsonValueOutput};
use crate::quantity::{self, Scale};
use crate::signable::{Signs, UserSigned};
use crate::signing::SelectedSigner;
use crate::wire::{AddressRole, InfoRequest, address};

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

    fn receipt_action(self) -> &'static str {
        match self {
            Self::Delegate => "delegate",
            Self::Undelegate => "undelegate",
            Self::Deposit => "deposit",
            Self::Withdraw => "withdraw",
            Self::ClaimRewards => "claim-rewards",
            Self::LinkInitiate | Self::LinkFinalize => "staking-link",
        }
    }

    fn note(self) -> Option<&'static str> {
        (self == Self::Withdraw).then_some(WITHDRAWAL_NOTE)
    }

    /// Whether a link kind is the finalize step. Asked of the link kinds only.
    fn is_finalize(self) -> bool {
        self == Self::LinkFinalize
    }

    fn link_phase(self) -> &'static str {
        if self.is_finalize() {
            "finalize"
        } else {
            "initiate"
        }
    }
}

/// Responses are camelCase; the CLI's own JSON is snake_case.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all(deserialize = "camelCase", serialize = "snake_case"))]
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
#[serde(rename_all(deserialize = "camelCase", serialize = "snake_case"))]
pub struct DelegationRow {
    pub validator: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    pub locked_until_timestamp: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all(deserialize = "camelCase", serialize = "snake_case"))]
pub struct RewardRow {
    pub time: u64,
    pub source: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub total_amount: Decimal,
}

impl Row for RewardRow {
    const HEADERS: &'static [&'static str] = &["Time", "Source", "Total Amount"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.time.to_string(),
            self.source.clone(),
            self.total_amount.to_string(),
        ]
    }
}

impl ListRow for RewardRow {
    const EMPTY: &'static str = "No results.";
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ValidatorStats {
    #[serde(with = "rust_decimal::serde::str")]
    pub uptime_fraction: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub predicted_apr: Decimal,
    pub n_samples: u64,
}

/// The stake is in HYPE, with the per-period stats as objects.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ValidatorRow {
    validator: String,
    signer: Option<String>,
    name: String,
    description: Option<String>,
    n_recent_blocks: Option<u64>,
    stake: u64,
    #[serde(with = "rust_decimal::serde::str")]
    stake_hype: Decimal,
    is_jailed: bool,
    unjailable_after: Option<u64>,
    is_active: bool,
    #[serde(with = "rust_decimal::serde::str")]
    commission: Decimal,
    stats: Vec<ValidatorPeriodStats>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct ValidatorPeriodStats {
    period: String,
    #[serde(with = "rust_decimal::serde::str")]
    uptime_fraction: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    predicted_apr: Decimal,
    n_samples: u64,
}

impl From<ValidatorSummary> for ValidatorRow {
    fn from(summary: ValidatorSummary) -> Self {
        Self {
            stake_hype: Scale::HYPE_CORE.amount(summary.stake),
            validator: summary.validator,
            signer: summary.signer,
            name: summary.name,
            description: summary.description,
            n_recent_blocks: summary.n_recent_blocks,
            stake: summary.stake,
            is_jailed: summary.is_jailed,
            unjailable_after: summary.unjailable_after,
            is_active: summary.is_active,
            commission: summary.commission,
            stats: summary
                .stats
                .into_iter()
                .map(|(period, stats)| ValidatorPeriodStats {
                    period,
                    uptime_fraction: stats.uptime_fraction,
                    predicted_apr: stats.predicted_apr,
                    n_samples: stats.n_samples,
                })
                .collect(),
        }
    }
}

impl ValidatorRow {
    /// The APR the table shows: the day period's, else the first reported.
    fn apr(&self) -> Option<Decimal> {
        self.stats
            .iter()
            .find(|stats| stats.period == "day")
            .or_else(|| self.stats.first())
            .map(|stats| stats.predicted_apr)
    }
}

impl Row for ValidatorRow {
    const HEADERS: &'static [&'static str] = &[
        "Name",
        "Validator",
        "Stake",
        "Commission",
        "APR",
        "Active",
        "Jailed",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.name.clone(),
            self.validator.clone(),
            self.stake_hype.to_string(),
            self.commission.to_string(),
            or_missing(self.apr()),
            self.is_active.to_string(),
            self.is_jailed.to_string(),
        ]
    }
}

impl ListRow for ValidatorRow {
    const EMPTY: &'static str = "No results.";
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct StakingSummaryRow {
    address: String,
    #[serde(flatten)]
    summary: DelegatorSummary,
    #[serde(with = "rust_decimal::serde::str")]
    pending_rewards: Decimal,
    delegations: Vec<DelegationRow>,
    rewards: Vec<RewardRow>,
}

impl Row for StakingSummaryRow {
    const HEADERS: &'static [&'static str] = &[
        "Address",
        "Delegated",
        "Undelegated",
        "Pending Withdrawal",
        "Pending Withdrawals",
        "Pending Rewards",
        "Delegations",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.address.clone(),
            self.summary.delegated.to_string(),
            self.summary.undelegated.to_string(),
            self.summary.total_pending_withdrawal.to_string(),
            self.summary.n_pending_withdrawals.to_string(),
            self.pending_rewards.to_string(),
            self.delegations.len().to_string(),
        ]
    }
}

#[derive(Debug, Clone, Serialize)]
struct ActionRow {
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

impl Row for ActionRow {
    const HEADERS: &'static [&'static str] = &[
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
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.action.clone(),
            self.status.clone(),
            self.network.clone(),
            self.signer.clone(),
            self.acting_as.clone(),
            self.reversibility.clone(),
            self.amount.to_string(),
            self.asset.clone(),
            self.validator.clone().unwrap_or_default(),
            self.note.clone().unwrap_or_default(),
        ]
    }
}

#[derive(Debug, Clone, Serialize)]
struct LinkStakingRow {
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

impl Row for LinkStakingRow {
    const HEADERS: &'static [&'static str] = &[
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
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.status.clone(),
            self.action.clone(),
            self.phase.clone(),
            self.acting_as.clone(),
            self.signer.clone(),
            self.user.clone(),
            self.is_finalize.to_string(),
            self.network.clone(),
            self.reversibility.clone(),
            self.warning.clone(),
        ]
    }

    fn pretty_cells(&self) -> Vec<String> {
        let mut cells = self.cells();
        style_column::<Self>(&mut cells, "Warning", output::colors::yellow);
        cells
    }
}

/// The staking actions sent as raw L1 JSON. The `type` tag is what the
/// exchange dispatches on, and what a dry run names.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum StakingL1Action {
    ClaimRewards,
    /// The agent-signed form of `cDeposit`.
    #[serde(rename = "cDeposit2")]
    CDeposit2 {
        wei: u64,
    },
}

impl StakingL1Action {
    fn type_tag(&self) -> serde_json::Value {
        serde_json::to_value(self).map_or(serde_json::Value::Null, |action| action["type"].clone())
    }
}

async fn staking_info<T: DeserializeOwned>(
    api: &ApiClient,
    kind: &'static str,
    user: Address,
    context: &'static str,
) -> Result<T, CliError> {
    api.info(&InfoRequest::user(kind, user), context).await
}

async fn claim_rewards(inv: &Invocation, signer: &SelectedSigner) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    actions::send_raw_l1_json_action(
        remote.api(),
        signer,
        &StakingL1Action::ClaimRewards,
        None,
        "claim staking rewards",
    )
    .await?;
    print_action_confirmation(
        inv,
        signer,
        remote.chain(),
        StakingActionKind::ClaimRewards,
        Decimal::ZERO,
        None,
    );
    Ok(())
}

fn link_dry_run_value(chain: Chain, user: Address, kind: StakingActionKind) -> serde_json::Value {
    serde_json::json!({
        "phase": kind.link_phase(),
        "user": user.to_string(),
        "is_finalize": kind.is_finalize(),
        "network": chain.to_string(),
        "reversibility": kind.reversibility(),
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
            "isFinalize": kind.is_finalize(),
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
    if let Some(note) = action_kind.note() {
        value["note"] = serde_json::json!(note);
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
    kind: StakingActionKind,
) -> Result<(), anyhow::Error> {
    let question = format!("Submit staking link {} for user {user}?", kind.link_phase());
    consent::approve(
        inv,
        args.yes,
        &ConsentBlock::new().text(STAKING_LINK_WARNING),
        &question,
        &format!(
            "staking-link confirmation required; action cancelled. Rerun with {YES_FLAG} for deliberate automation."
        ),
    )?;
    let remote = inv.remote()?;
    user_signed::submit(remote.api(), signer, |hyperliquid_chain, nonce| {
        LinkStakingUser {
            hyperliquidChain: hyperliquid_chain,
            user,
            isFinalize: kind.is_finalize(),
            nonce,
        }
    })
    .await?;

    inv.print(&One::new(LinkStakingRow {
        status: "submitted".to_string(),
        action: "staking-link".to_string(),
        phase: kind.link_phase().to_string(),
        acting_as: signer.query_address().to_string(),
        signer: signer.address().to_string(),
        user: user.to_string(),
        is_finalize: kind.is_finalize(),
        network: remote.chain().to_string(),
        reversibility: kind.reversibility().as_str().to_string(),
        warning: STAKING_LINK_WARNING.to_string(),
    }));
    Ok(())
}

fn print_action_confirmation(
    inv: &Invocation,
    signer: &SelectedSigner,
    chain: Chain,
    kind: StakingActionKind,
    amount: Decimal,
    validator: Option<String>,
) {
    inv.print(&One::new(ActionRow {
        action: kind.receipt_action().to_string(),
        status: "submitted".to_string(),
        network: chain.to_string(),
        signer: signer.address().to_string(),
        acting_as: signer.query_address().to_string(),
        reversibility: kind.reversibility().as_str().to_string(),
        amount,
        asset: ASSET.to_string(),
        validator,
        note: kind.note().map(str::to_string),
    }));
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
        let user = args.resolve(inv).await?;
        let api = inv.remote()?.api();
        let summary =
            staking_info(api, "delegatorSummary", user, "loading staking summary").await?;
        let delegations = staking_info(api, "delegations", user, "loading delegations").await?;
        let rewards: Vec<RewardRow> =
            staking_info(api, "delegatorRewards", user, "loading staking rewards").await?;
        let pending_rewards = rewards.iter().map(|row| row.total_amount).sum();
        inv.print(&One::new(StakingSummaryRow {
            address: user.to_string(),
            summary,
            pending_rewards,
            delegations,
            rewards,
        }));
        Ok(())
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
        let rows: Vec<ValidatorRow> = inv
            .remote()?
            .api()
            .info::<Vec<ValidatorSummary>>(
                &InfoRequest::new("validatorSummaries"),
                "loading validator summaries",
            )
            .await?
            .into_iter()
            .map(ValidatorRow::from)
            .collect();
        inv.print(&Rows::new(rows));
        Ok(())
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
        let user = args.resolve(inv).await?;
        let rows: Vec<RewardRow> = staking_info(
            inv.remote()?.api(),
            "delegatorRewards",
            user,
            "loading staking rewards",
        )
        .await?;
        inv.print(&Rows::new(rows));
        Ok(())
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
        let user = args.resolve(inv).await?;
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
}

async fn run_delegation(
    inv: &Invocation,
    args: &DelegateArgs,
    kind: StakingActionKind,
) -> Result<(), anyhow::Error> {
    let validator = address(&args.validator, AddressRole::Protocol("validator"))?;
    let wei: u64 = quantity::units(args.amount, Scale::HYPE_CORE, "amount")?;
    if inv.dry_run() {
        return print_dry_run(
            inv,
            kind.would_execute(),
            delegate_dry_run_value(inv.remote()?.chain(), args, kind),
        );
    }
    let signer = inv.resolve_signer()?;
    let remote = inv.remote()?;
    user_signed::submit(remote.api(), &signer, |hyperliquid_chain, nonce| {
        TokenDelegate {
            hyperliquidChain: hyperliquid_chain,
            validator,
            wei,
            isUndelegate: kind == StakingActionKind::Undelegate,
            nonce,
        }
    })
    .await?;
    print_action_confirmation(
        inv,
        &signer,
        remote.chain(),
        kind,
        args.amount,
        Some(args.validator.clone()),
    );
    Ok(())
}

/// `staking deposit`: an API wallet signs `cDeposit2` (an L1 action), anyone
/// else the user-signed `cDeposit`.
async fn run_deposit(inv: &Invocation, args: &AmountArgs) -> Result<(), anyhow::Error> {
    let kind = StakingActionKind::Deposit;
    let wei: u64 = quantity::units(args.amount, Scale::HYPE_CORE, "amount")?;
    let agent_action = StakingL1Action::CDeposit2 { wei };
    if inv.dry_run() {
        let mut preview = amount_dry_run_value(inv.remote()?.chain(), args, kind);
        if inv
            .dry_run_signer()?
            .is_some_and(|signer| signer.agent_form().is_some())
        {
            preview["action_type"] = agent_action.type_tag();
        }
        return print_dry_run(inv, kind.would_execute(), preview);
    }
    let signer = inv.resolve_signer()?;
    let remote = inv.remote()?;
    if signer.agent_form().is_some() {
        actions::send_raw_l1_json_action(
            remote.api(),
            &signer,
            &agent_action,
            None,
            "deposit to staking",
        )
        .await?;
    } else {
        user_signed::submit(remote.api(), &signer, |hyperliquid_chain, nonce| CDeposit {
            hyperliquidChain: hyperliquid_chain,
            wei,
            nonce,
        })
        .await?;
    }
    print_action_confirmation(inv, &signer, remote.chain(), kind, args.amount, None);
    Ok(())
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
    .user_signed::<TokenDelegate>()
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
    .user_signed::<TokenDelegate>()
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
        run_deposit(inv, args).await
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
    .user_signed::<CWithdraw>()
    .args(&[arg("amount")
        .kind(InputKind::Amount)
        .describe("HYPE amount.")]);

    async fn run(inv: &Invocation, args: &AmountArgs) -> Result<(), anyhow::Error> {
        let kind = StakingActionKind::Withdraw;
        let wei: u64 = quantity::units(args.amount, Scale::HYPE_CORE, "amount")?;
        if inv.dry_run() {
            return print_dry_run(
                inv,
                kind.would_execute(),
                amount_dry_run_value(inv.remote()?.chain(), args, kind),
            );
        }
        let signer = inv.resolve_signer()?;
        let remote = inv.remote()?;
        user_signed::submit(remote.api(), &signer, |hyperliquid_chain, nonce| {
            CWithdraw {
                hyperliquidChain: hyperliquid_chain,
                wei,
                nonce,
            }
        })
        .await?;
        print_action_confirmation(inv, &signer, remote.chain(), kind, args.amount, None);
        Ok(())
    }
}

pub struct StakingClaimRewards;

impl CliCommand for StakingClaimRewards {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["staking", "claim-rewards"],
        "staking",
        "Claim referral and builder rewards",
        "Claim accrued referral and builder rewards (claimRewards). Staking rewards are redelegated to the validator automatically and are not claimed by this command.",
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
    .user_signed::<LinkStakingUser>()
    .prompt()
    .args(&[
        arg("user").kind(InputKind::ProtocolObjectAddress).describe("Protocol user address for the staking account to link.").resource_id("staking-link user address"),
        YES,
    ]);

    async fn run(inv: &Invocation, args: &LinkArgs) -> Result<(), anyhow::Error> {
        staking_link(inv, args, StakingActionKind::LinkInitiate).await
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
    .user_signed::<LinkStakingUser>()
    .prompt()
    .args(&[
        arg("user").kind(InputKind::ProtocolObjectAddress).describe("Protocol user address for the trading account to link.").resource_id("staking-link user address"),
        YES,
    ]);

    async fn run(inv: &Invocation, args: &LinkArgs) -> Result<(), anyhow::Error> {
        staking_link(inv, args, StakingActionKind::LinkFinalize).await
    }
}

async fn staking_link(
    inv: &Invocation,
    args: &LinkArgs,
    kind: StakingActionKind,
) -> Result<(), anyhow::Error> {
    let user = address(&args.user, AddressRole::Protocol("staking-link user"))?;
    if inv.dry_run() {
        return print_dry_run(
            inv,
            kind.would_execute(),
            link_dry_run_value(inv.remote()?.chain(), user, kind),
        );
    }
    let signer = inv.resolve_signer()?;
    link_staking_user(inv, &signer, user, args, kind).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amount_to_wei_accepts_only_positive_eight_decimal_amounts() {
        let wei = |amount: &str| {
            quantity::units::<u64>(amount.parse().unwrap(), Scale::HYPE_CORE, "amount")
        };

        assert_eq!(wei("0.00000001").unwrap(), 1);
        assert!(wei("0").is_err());
        assert!(wei("-1").is_err());
        assert!(wei("0.000000001").is_err());
    }

    #[test]
    fn raw_l1_actions_keep_their_wire_shape() {
        let claim = StakingL1Action::ClaimRewards;
        let deposit = StakingL1Action::CDeposit2 { wei: 150_000_000 };

        assert_eq!(
            serde_json::to_value(&claim).unwrap(),
            serde_json::json!({"type": "claimRewards"})
        );
        assert_eq!(
            serde_json::to_value(&deposit).unwrap(),
            serde_json::json!({"type": "cDeposit2", "wei": 150_000_000})
        );
        // The L1 action hash covers the msgpack of the action, so its key
        // order and encoding are part of the contract.
        assert_eq!(
            rmp_serde::to_vec_named(&claim).unwrap(),
            b"\x81\xa4type\xacclaimRewards"
        );
        assert_eq!(
            rmp_serde::to_vec_named(&StakingL1Action::CDeposit2 { wei: 1 }).unwrap(),
            b"\x82\xa4type\xa9cDeposit2\xa3wei\x01"
        );
        assert_eq!(deposit.type_tag(), "cDeposit2");
    }

    fn validator_summary() -> ValidatorSummary {
        serde_json::from_value(serde_json::json!({
            "validator": "0x0000000000000000000000000000000000000002",
            "signer": null,
            "name": "Validator One",
            "description": "desc",
            "nRecentBlocks": 7,
            "stake": 1_234_500_000_u64,
            "isJailed": false,
            "unjailableAfter": null,
            "isActive": true,
            "commission": "0.04",
            "stats": [
                ["week", {"uptimeFraction": "0.9", "predictedApr": "0.02", "nSamples": 3}],
                ["day", {"uptimeFraction": "1", "predictedApr": "0.05", "nSamples": 9}]
            ]
        }))
        .unwrap()
    }

    #[test]
    fn validator_rows_keep_the_public_json_shape() {
        let row = ValidatorRow::from(validator_summary());

        assert_eq!(
            crate::output::serialize_row(&row),
            serde_json::json!({
                "validator": "0x0000000000000000000000000000000000000002",
                "signer": null,
                "name": "Validator One",
                "description": "desc",
                "n_recent_blocks": 7,
                "stake": 1_234_500_000_u64,
                "stake_hype": "12.345",
                "is_jailed": false,
                "unjailable_after": null,
                "is_active": true,
                "commission": "0.04",
                "stats": [
                    {"period": "week", "uptime_fraction": "0.9", "predicted_apr": "0.02", "n_samples": 3},
                    {"period": "day", "uptime_fraction": "1", "predicted_apr": "0.05", "n_samples": 9}
                ]
            })
        );
    }

    #[test]
    fn validator_apr_prefers_the_day_period_and_shows_missing_without_stats() {
        let apr_cell = |summary: ValidatorSummary| ValidatorRow::from(summary).cells()[4].clone();

        assert_eq!(apr_cell(validator_summary()), "0.05");

        let mut bare = validator_summary();
        bare.stats.clear();
        assert_eq!(apr_cell(bare), crate::commands::common::rows::MISSING);
    }

    #[test]
    fn link_confirmation_colours_the_warning_not_the_network() {
        let row = LinkStakingRow {
            status: "submitted".to_string(),
            action: "staking-link".to_string(),
            phase: "initiate".to_string(),
            acting_as: "0xa".to_string(),
            signer: "0xb".to_string(),
            user: "0xc".to_string(),
            is_finalize: false,
            network: "Testnet".to_string(),
            reversibility: "irreversible".to_string(),
            warning: "careful".to_string(),
        };
        let plain = row.cells();
        let pretty = row.pretty_cells();
        let column = |name: &str| {
            LinkStakingRow::HEADERS
                .iter()
                .position(|h| *h == name)
                .unwrap()
        };

        assert_eq!(pretty[column("Network")], plain[column("Network")]);
        assert_eq!(
            pretty[column("Warning")],
            output::colors::yellow(&plain[column("Warning")])
        );
    }
}
