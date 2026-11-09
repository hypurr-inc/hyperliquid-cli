use clap::Args;
use hypersdk::Address;
use hypersdk::hypercore::types::Action;
use hypersdk::hypercore::types::api::ApproveAgent;
use hypersdk::hypercore::{Chain, PrivateKeySigner};
use serde::Serialize;

use crate::auth;
use crate::cli_command::CliCommand;
use crate::clock::{self, DurationError};
use crate::command_spec::{CommandSpec, GroupSpec, InputKind, OwsSupport, Risk, arg};
use crate::commands::common::support::action_json;
use crate::errors::CliError;
use crate::exchange::actions;
use crate::input_hardening::validate_resource_id;
use crate::invocation::Invocation;
use crate::output::{TableData, serialize_row};
use crate::quantity;
use crate::signing::SelectedSigner;
use crate::terminal::OutputFormat;
use crate::wire::{AddressRole, address};

const MAX_AGENT_EXPIRATION_MS: u64 = clock::DAY.ms(180);
const REVOKE_REPLACEMENT_EXPIRATION_MS: u64 = clock::MINUTE.ms(1);
const MAX_AGENT_NAME_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Create,
    Approve,
    Revoke,
}

impl Operation {
    fn action(self) -> &'static str {
        match self {
            Self::Create => "api_wallet_create",
            Self::Approve => "api_wallet_approve",
            Self::Revoke => "api_wallet_revoke",
        }
    }

    fn generates_by_default(self) -> bool {
        self == Self::Create
    }

    fn note(self, prepared: &PreparedAgentApproval) -> &'static str {
        match self {
            Self::Revoke if prepared.display_name.is_none() => {
                "Unnamed API wallet replaced with a throwaway agent whose key was discarded; no private key was retained. The throwaway stays registered, with no expiry, until the next unnamed approval replaces it."
            }
            Self::Revoke => {
                "Named API wallet replaced with a short-lived throwaway agent; no private key was retained."
            }
            Self::Create | Self::Approve => {
                "Store this private key now; it will not be shown again by this command."
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ApprovalInput<'a> {
    name: Option<&'a str>,
    expires_in: Option<&'a str>,
    agent_address: Option<&'a str>,
    generate: bool,
}

/// Arguments for `api-wallet create`.
#[derive(Args, Debug, Clone)]
pub struct CreateArgs {
    /// Human-readable API wallet name. Named agents replace prior agents with the same name.
    #[arg(long)]
    pub name: Option<String>,

    /// Expire after a duration such as 30d, 12h, or 1w. Maximum is 180d.
    #[arg(long)]
    pub expires_in: Option<String>,

    /// Approve an existing agent address instead of generating a new key.
    #[arg(long, conflicts_with = "generate")]
    pub agent_address: Option<String>,

    /// Generate a new API wallet key before approval. This is the default for create.
    #[arg(long)]
    pub generate: bool,
}

/// Arguments for `api-wallet approve`.
#[derive(Args, Debug, Clone)]
pub struct ApproveArgs {
    /// Human-readable API wallet name. Named agents replace prior agents with the same name.
    #[arg(long)]
    pub name: Option<String>,

    /// Expire after a duration such as 30d, 12h, or 1w. Maximum is 180d.
    #[arg(long)]
    pub expires_in: Option<String>,

    /// Agent wallet address to approve.
    #[arg(long, conflicts_with = "generate")]
    pub agent_address: Option<String>,

    /// Generate a new API wallet key before approval.
    #[arg(long)]
    pub generate: bool,
}

/// Arguments for `api-wallet revoke`.
#[derive(Args, Debug, Clone)]
pub struct RevokeArgs {
    /// Named API wallet to revoke by replacing it with a short-lived throwaway agent.
    #[arg(long, conflicts_with = "unnamed", required_unless_present = "unnamed")]
    pub name: Option<String>,

    /// Revoke the unnamed API wallet by replacing it with a throwaway agent whose key is discarded.
    #[arg(long)]
    pub unnamed: bool,
}

#[derive(Clone)]
struct PreparedAgentApproval {
    agent_address: Address,
    generated_private_key: Option<String>,
    display_name: Option<String>,
    action_agent_name: Option<String>,
    expires_at: Option<u64>,
    generated: bool,
}

#[derive(Clone, Serialize)]
struct ApiWalletActionRow {
    status: String,
    action: String,
    master_address: Option<String>,
    agent_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action_agent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    generated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    private_key: Option<String>,
    note: String,
    approval_action: serde_json::Value,
}

impl std::fmt::Debug for PreparedAgentApproval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedAgentApproval")
            .field("agent_address", &self.agent_address)
            .field(
                "generated_private_key",
                &self.generated_private_key.as_ref().map(|_| "[redacted]"),
            )
            .field("display_name", &self.display_name)
            .field("action_agent_name", &self.action_agent_name)
            .field("expires_at", &self.expires_at)
            .field("generated", &self.generated)
            .finish()
    }
}

impl std::fmt::Debug for ApiWalletActionRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiWalletActionRow")
            .field("status", &self.status)
            .field("action", &self.action)
            .field("master_address", &self.master_address)
            .field("agent_address", &self.agent_address)
            .field("name", &self.name)
            .field("action_agent_name", &self.action_agent_name)
            .field("expires_at", &self.expires_at)
            .field("generated", &self.generated)
            .field(
                "private_key",
                &self.private_key.as_ref().map(|_| "[redacted]"),
            )
            .field("note", &self.note)
            .field("approval_action", &self.approval_action)
            .finish()
    }
}

struct ApiWalletActionOutput {
    row: ApiWalletActionRow,
}

impl TableData for ApiWalletActionOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Status",
            "Action",
            "Master",
            "Agent",
            "Name",
            "Expires At",
            "Generated",
            "Private Key",
            "Note",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.row.status.clone(),
            self.row.action.clone(),
            self.row
                .master_address
                .clone()
                .unwrap_or_else(|| "signer required for live approval".to_string()),
            self.row.agent_address.clone(),
            self.row
                .name
                .clone()
                .unwrap_or_else(|| "unnamed".to_string()),
            self.row
                .expires_at
                .map(|value| value.to_string())
                .unwrap_or_else(|| "protocol default".to_string()),
            self.row.generated.to_string(),
            self.row
                .private_key
                .clone()
                .unwrap_or_else(|| "not printed".to_string()),
            self.row.note.clone(),
        ]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serialize_row(&self.row)
    }
}

#[derive(Debug, Clone, Serialize)]
struct ApiAgentRow {
    name: String,
    address: String,
    valid_until: Option<u64>,
}

struct ApiAgentListOutput {
    master_address: String,
    rows: Vec<ApiAgentRow>,
}

impl TableData for ApiAgentListOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Master", "Name", "Address", "Valid Until"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    self.master_address.clone(),
                    row.name.clone(),
                    row.address.clone(),
                    row.valid_until
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "protocol default".to_string()),
                ]
            })
            .collect()
    }

    fn empty_message(&self) -> Option<&str> {
        Some("No API wallets.")
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "master_address": self.master_address,
            "api_wallets": self.rows,
        })
    }
}

async fn create(inv: &Invocation, args: &CreateArgs) -> Result<(), anyhow::Error> {
    let prepared = prepare_agent_approval(
        Operation::Create,
        ApprovalInput {
            name: args.name.as_deref(),
            expires_in: args.expires_in.as_deref(),
            agent_address: args.agent_address.as_deref(),
            generate: args.generate,
        },
        clock::now_ms(),
    )?;
    approve_prepared(inv, Operation::Create, prepared).await
}

async fn approve(inv: &Invocation, args: &ApproveArgs) -> Result<(), anyhow::Error> {
    let prepared = prepare_agent_approval(
        Operation::Approve,
        ApprovalInput {
            name: args.name.as_deref(),
            expires_in: args.expires_in.as_deref(),
            agent_address: args.agent_address.as_deref(),
            generate: args.generate,
        },
        clock::now_ms(),
    )?;
    approve_prepared(inv, Operation::Approve, prepared).await
}

async fn list(inv: &Invocation, master_address: Address) -> Result<(), anyhow::Error> {
    let rows = inv
        .remote()?
        .api()
        .sdk()
        .api_agents(master_address)
        .await?
        .into_iter()
        .map(|agent| ApiAgentRow {
            name: agent.name,
            address: agent.address.to_string(),
            valid_until: agent.valid_until,
        })
        .collect();
    inv.print(&ApiAgentListOutput {
        master_address: master_address.to_string(),
        rows,
    });
    Ok(())
}

async fn revoke(inv: &Invocation, args: &RevokeArgs) -> Result<(), anyhow::Error> {
    let prepared = prepare_revoke_replacement(args, clock::now_ms())?;
    approve_prepared(inv, Operation::Revoke, prepared).await
}

fn prepare_revoke_replacement(
    args: &RevokeArgs,
    now: u64,
) -> Result<PreparedAgentApproval, CliError> {
    let throwaway = PrivateKeySigner::random();
    // Hyperliquid deregisters the unnamed API wallet when a new unnamed one is
    // approved, and a named one when a wallet with the same name is approved.
    // Only a named agent can carry an expiry, so the unnamed throwaway cannot
    // be made to lapse.
    let Some(name) = args.name.as_deref() else {
        return Ok(PreparedAgentApproval {
            agent_address: throwaway.address(),
            generated_private_key: None,
            display_name: None,
            action_agent_name: None,
            expires_at: None,
            generated: true,
        });
    };
    let name = validated_agent_name(Some(name), "--name")?.ok_or_else(|| {
        CliError::Configuration("--name is required for api-wallet revoke".to_string())
    })?;
    let expires_at = now
        .checked_add(REVOKE_REPLACEMENT_EXPIRATION_MS)
        .ok_or_else(|| CliError::Configuration("revoke expiration overflowed".to_string()))?;
    Ok(PreparedAgentApproval {
        agent_address: throwaway.address(),
        generated_private_key: None,
        display_name: Some(name.clone()),
        action_agent_name: agent_action_name(Some(name.as_str()), Some(expires_at))?,
        expires_at: Some(expires_at),
        generated: true,
    })
}

async fn approve_prepared(
    inv: &Invocation,
    operation: Operation,
    prepared: PreparedAgentApproval,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let chain = remote.chain();

    if inv.dry_run() {
        let (_, approval_value) = approval_document(chain, &prepared, clock::now_ms())?;
        let master_address = inv.dry_run_signer()?.map(SelectedSigner::address);
        let row = approval_dry_run_row(operation, master_address, &prepared, approval_value);
        print_action_output(inv, row);
        return Ok(());
    }

    ensure_generated_key_projection_is_safe(&prepared, inv)?;

    let signer = inv.resolve_signer()?;
    let nonce = remote.api().nonces().next();
    let (approval, approval_value) = approval_document(chain, &prepared, nonce)?;
    actions::send_l1_action_with_nonce(
        remote.api(),
        &signer,
        Action::ApproveAgent(approval),
        nonce,
        None,
        "approveAgent action failed",
    )
    .await?;

    let note = operation.note(&prepared).to_string();
    let private_key = prepared.generated_private_key;
    print_action_output(
        inv,
        ApiWalletActionRow {
            status: "submitted".to_string(),
            action: operation.action().to_string(),
            master_address: Some(signer.address().to_string()),
            agent_address: prepared.agent_address.to_string(),
            name: prepared.display_name,
            action_agent_name: prepared.action_agent_name,
            expires_at: prepared.expires_at,
            generated: prepared.generated,
            private_key,
            note,
            approval_action: approval_value,
        },
    );
    Ok(())
}

fn approval_document(
    chain: Chain,
    prepared: &PreparedAgentApproval,
    nonce: u64,
) -> Result<(ApproveAgent, serde_json::Value), CliError> {
    let approval = approval_action(chain, prepared, nonce);
    let value = action_json(&Action::ApproveAgent(approval.clone()))?;
    Ok((approval, value))
}

fn approval_dry_run_row(
    operation: Operation,
    master_address: Option<Address>,
    prepared: &PreparedAgentApproval,
    approval_value: serde_json::Value,
) -> ApiWalletActionRow {
    ApiWalletActionRow {
        status: "dry_run".to_string(),
        action: operation.action().to_string(),
        master_address: master_address.map(|address| address.to_string()),
        agent_address: prepared.agent_address.to_string(),
        name: prepared.display_name.clone(),
        action_agent_name: prepared.action_agent_name.clone(),
        expires_at: prepared.expires_at,
        generated: prepared.generated,
        private_key: None,
        note: "Dry-run only; approveAgent action was not signed or submitted, and no private key was printed.".to_string(),
        approval_action: approval_value,
    }
}

fn ensure_generated_key_projection_is_safe(
    prepared: &PreparedAgentApproval,
    inv: &Invocation,
) -> Result<(), CliError> {
    if inv.format() == OutputFormat::Json
        && prepared.generated_private_key.is_some()
        && inv.session().json_options().is_active()
    {
        return Err(CliError::Unsupported(
            "live generated API wallet approvals print the private key exactly once; remove --select, --max-results, and --results-only"
                .to_string(),
        ));
    }
    Ok(())
}

fn prepare_agent_approval(
    operation: Operation,
    input: ApprovalInput<'_>,
    now: u64,
) -> Result<PreparedAgentApproval, CliError> {
    let display_name = validated_agent_name(input.name, "--name")?;
    let expires_at = input
        .expires_in
        .map(parse_expires_in)
        .transpose()?
        .map(|duration| {
            now.checked_add(duration).ok_or_else(|| {
                CliError::Configuration("agent wallet expiration overflowed".to_string())
            })
        })
        .transpose()?;
    let action_agent_name = agent_action_name(input.name, expires_at)?;

    if let Some(agent_address) = input.agent_address {
        return Ok(PreparedAgentApproval {
            agent_address: address(agent_address.trim(), AddressRole::Agent)?,
            generated_private_key: None,
            display_name,
            action_agent_name,
            expires_at,
            generated: false,
        });
    }

    if input.generate || operation.generates_by_default() {
        let signer = PrivateKeySigner::random();
        let private_key = auth::signer_private_key_hex(&signer);
        return Ok(PreparedAgentApproval {
            agent_address: signer.address(),
            generated_private_key: Some(private_key),
            display_name,
            action_agent_name,
            expires_at,
            generated: true,
        });
    }

    Err(CliError::Unsupported(
        "api-wallet approve requires --agent-address or --generate".to_string(),
    ))
}

fn parse_expires_in(raw: &str) -> Result<u64, CliError> {
    let refuse = |message: &str| CliError::Configuration(message.to_string());
    let raw = raw.trim();
    if raw.len() < 2 {
        return Err(refuse(
            "--expires-in must use a duration like 30d, 12h, or 1w",
        ));
    }
    let overflowed = || refuse("--expires-in duration overflowed");
    let duration =
        clock::parse_duration(raw, &clock::AGENT_EXPIRY).map_err(|fault| match fault {
            DurationError::Empty | DurationError::Count(_) => {
                refuse("--expires-in amount must be a positive integer")
            }
            DurationError::Zero => refuse(&quantity::not_positive("--expires-in amount")),
            DurationError::Unit => refuse("--expires-in unit must be one of m, h, d, or w"),
            DurationError::TooLarge => overflowed(),
        })?;
    let millis = u64::try_from(duration.as_millis()).map_err(|_| overflowed())?;
    if millis > MAX_AGENT_EXPIRATION_MS {
        return Err(refuse("--expires-in cannot exceed 180d"));
    }
    Ok(millis)
}

fn approval_action(chain: Chain, prepared: &PreparedAgentApproval, nonce: u64) -> ApproveAgent {
    ApproveAgent {
        signature_chain_id: chain.arbitrum_id().to_owned(),
        hyperliquid_chain: chain,
        agent_address: prepared.agent_address,
        agent_name: prepared.action_agent_name.clone(),
        nonce,
    }
}

fn agent_action_name(
    name: Option<&str>,
    expires_at: Option<u64>,
) -> Result<Option<String>, CliError> {
    let name = validated_agent_name(name, "--name")?;
    match (name, expires_at) {
        (Some(name), Some(expires_at)) => Ok(Some(format!("{name} valid_until {expires_at}"))),
        (Some(name), None) => Ok(Some(name)),
        (None, Some(_)) => Err(CliError::Configuration(
            "--expires-in requires --name because Hyperliquid encodes agent expiration in agentName"
                .to_string(),
        )),
        (None, None) => Ok(None),
    }
}

fn validated_agent_name(
    name: Option<&str>,
    flag: &'static str,
) -> Result<Option<String>, CliError> {
    let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
        return Ok(None);
    };
    if name.chars().count() > MAX_AGENT_NAME_LEN {
        return Err(CliError::Configuration(format!(
            "{flag} must be between 1 and {MAX_AGENT_NAME_LEN} characters"
        )));
    }
    Ok(Some(name.to_string()))
}
fn print_action_output(inv: &Invocation, row: ApiWalletActionRow) {
    inv.print_untimed(&ApiWalletActionOutput { row });
}

pub const GROUP: GroupSpec = GroupSpec::new(&["api-wallet"], "Approve and manage API wallets")
    .long_about("Approve and manage API wallets.\n\
\n\
API wallets, also called agent wallets, can sign trading actions on behalf of a master account, but cannot withdraw. Account data queries must use the master or subaccount address, not the API wallet address.")
    .aliases(&["api-wallets"])
    .schema_aliases();

#[derive(Args, Debug, Clone)]
pub struct ApiWalletListArgs {
    /// Master address, wallet name, or wallet id. Defaults to the selected signer.
    pub account: Option<String>,
}

pub struct ApiWalletCreate;

impl CliCommand for ApiWalletCreate {
    type Args = CreateArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["api-wallet", "create"],
        "wallet",
        "Generate or approve an API wallet",
        "Generate or approve a Hyperliquid API/agent wallet. When the CLI generates a local agent keypair, it prints the private key exactly once before approval.",
    )
    .digest_signed("approveAgent")
    .long_about("Generate or approve an API wallet.\n\
\n\
By default this command generates a new local API wallet keypair, signs an approveAgent action for that address with the selected master account, and prints the generated private key exactly once unless command is running in dry-run.")
    .args(&[
        arg("name").describe("Optional API wallet name."),
        arg("expires_in").kind(InputKind::Duration).describe("Expiration duration such as 30d, 12h, or 1w. Requires name."),
        arg("agent_address"),
        arg("generate").kind(InputKind::Boolean).describe("Generate a new API wallet key before approval."),
    ]);

    fn validate(args: &CreateArgs) -> Result<(), CliError> {
        validate_api_wallet_args(args.name.as_deref(), args.agent_address.as_deref())
    }

    async fn run(inv: &Invocation, args: &CreateArgs) -> Result<(), anyhow::Error> {
        create(inv, args).await
    }
}

pub struct ApiWalletApprove;

impl CliCommand for ApiWalletApprove {
    type Args = ApproveArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["api-wallet", "approve"],
        "wallet",
        "Approve an existing or generated API wallet address",
        "Approve an existing or generated API/agent wallet address for the selected master account.",
    )
    .digest_signed("approveAgent")
    .args(&[
        arg("name").describe("Optional API wallet name."),
        arg("expires_in").kind(InputKind::Duration).describe("Expiration duration such as 30d, 12h, or 1w. Requires name."),
        arg("agent_address"),
        arg("generate").kind(InputKind::Boolean),
    ]);

    fn validate(args: &ApproveArgs) -> Result<(), CliError> {
        validate_api_wallet_args(args.name.as_deref(), args.agent_address.as_deref())
    }

    async fn run(inv: &Invocation, args: &ApproveArgs) -> Result<(), anyhow::Error> {
        approve(inv, args).await
    }
}

pub struct ApiWalletList;

impl CliCommand for ApiWalletList {
    type Args = ApiWalletListArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["api-wallet", "list"],
        "wallet",
        "List API wallets approved by a master account",
        "List API wallets approved by a master account. Stored agent accounts query under their master address.",
    )
    .risk(Risk::AccountState)
    .ows(OwsSupport::ExperimentalFeatureGated).auth()
    .cli_aliases(&["ls"])
    .schema_command("hyperliquid api-wallet list")
    .args(&[
        arg("account").kind(InputKind::SignerSelector).resource_id("account selector"),
    ]);

    async fn run(inv: &Invocation, args: &ApiWalletListArgs) -> Result<(), anyhow::Error> {
        let master_address = match args.account.as_deref() {
            Some(account) => inv.resolve_account(account)?,
            None => inv.resolve_signer_account().await?,
        };
        list(inv, master_address).await
    }
}

pub struct ApiWalletRevoke;

impl CliCommand for ApiWalletRevoke {
    type Args = RevokeArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["api-wallet", "revoke"],
        "wallet",
        "Revoke an API wallet by replacing it with a throwaway agent",
        "Revoke a named API wallet by replacing it with a short-lived throwaway agent, or the unnamed API wallet (--unnamed) by replacing it with a throwaway agent whose key is discarded.",
    )
    .digest_signed("approveAgent")
    .one_of_required(&[&["name"], &["unnamed"]])
    .args(&[
        arg("name")
            .describe("Named API wallet to revoke.")
            .resource_id("API wallet name"),
        arg("unnamed")
            .kind(InputKind::Boolean)
            .describe("Revoke the unnamed API wallet."),
    ]);

    fn validate(args: &RevokeArgs) -> Result<(), CliError> {
        validated_agent_name(args.name.as_deref(), "--name").map(|_| ())
    }

    async fn run(inv: &Invocation, args: &RevokeArgs) -> Result<(), anyhow::Error> {
        revoke(inv, args).await
    }
}

fn validate_api_wallet_args(
    name: Option<&str>,
    agent_address: Option<&str>,
) -> Result<(), CliError> {
    if let Some(name) = name {
        validate_resource_id("API wallet name", name)?;
        validated_agent_name(Some(name), "--name")?;
    }
    if let Some(agent_address) = agent_address {
        validate_resource_id("agent address", agent_address)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_expiration_durations() {
        assert_eq!(parse_expires_in("30d").unwrap(), 30 * 24 * 60 * 60 * 1000);
        assert_eq!(parse_expires_in("12h").unwrap(), 12 * 60 * 60 * 1000);
        assert_eq!(parse_expires_in("1w").unwrap(), 7 * 24 * 60 * 60 * 1000);
        assert!(parse_expires_in("0d").is_err());
        assert!(parse_expires_in("181d").is_err());
        assert!(parse_expires_in("5x").is_err());
    }

    #[test]
    fn every_expires_in_fault_keeps_its_message_and_exit_class() {
        let fault = |raw: &str| parse_expires_in(raw).unwrap_err();
        for (raw, message) in [
            ("", "--expires-in must use a duration like 30d, 12h, or 1w"),
            ("d", "--expires-in must use a duration like 30d, 12h, or 1w"),
            ("ad", "--expires-in amount must be a positive integer"),
            ("1.5d", "--expires-in amount must be a positive integer"),
            ("ax", "--expires-in amount must be a positive integer"),
            ("0d", "--expires-in amount must be greater than zero"),
            ("0x", "--expires-in amount must be greater than zero"),
            ("05", "--expires-in amount must be greater than zero"),
            ("5x", "--expires-in unit must be one of m, h, d, or w"),
            ("5s", "--expires-in unit must be one of m, h, d, or w"),
            ("30", "--expires-in unit must be one of m, h, d, or w"),
            ("99999999999999999w", "--expires-in duration overflowed"),
            ("181d", "--expires-in cannot exceed 180d"),
            ("26w", "--expires-in cannot exceed 180d"),
        ] {
            let err = fault(raw);
            assert!(
                matches!(&err, CliError::Configuration(text) if text == message),
                "{raw}: {err}"
            );
        }
        assert_eq!(parse_expires_in(" 180d ").unwrap(), MAX_AGENT_EXPIRATION_MS);
        assert_eq!(parse_expires_in("30m").unwrap(), 30 * 60 * 1000);
    }

    #[test]
    fn expiration_is_encoded_in_agent_name() {
        let prepared = prepare_agent_approval(
            Operation::Approve,
            ApprovalInput {
                name: Some("bot"),
                expires_in: Some("1d"),
                agent_address: Some("0x0000000000000000000000000000000000000001"),
                generate: false,
            },
            1_000,
        )
        .unwrap();
        assert_eq!(prepared.display_name.as_deref(), Some("bot"));
        assert_eq!(
            prepared.action_agent_name.as_deref(),
            Some("bot valid_until 86401000")
        );
        assert_eq!(prepared.expires_at, Some(86_401_000));
    }

    #[test]
    fn expiration_requires_a_name() {
        let err = prepare_agent_approval(
            Operation::Approve,
            ApprovalInput {
                name: None,
                expires_in: Some("1d"),
                agent_address: Some("0x0000000000000000000000000000000000000001"),
                generate: false,
            },
            1_000,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--expires-in requires --name"));
    }
}
