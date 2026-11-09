use clap::{Args, ValueEnum};
use hypersdk::hypercore::Chain;
use hypersdk::{Address, Decimal};
use serde::Serialize;

use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, arg};
use crate::commands::actions;
use crate::commands::confirm;
use crate::commands::rows::{ListRow, Row, Rows};
use crate::commands::support::print_dry_run;
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::quantity::{self, Scale};
use crate::signing::SelectedSigner;

/// Arguments for `subaccount create`.
#[derive(Args, Debug, Clone)]
pub struct CreateArgs {
    /// Display name for the new subaccount
    #[arg(long)]
    pub name: String,
}

/// Arguments for `subaccount transfer`.
#[derive(Args, Debug, Clone)]
pub struct TransferArgs {
    /// Subaccount acting-account selector: subaccount address, stored account alias, or stored account id
    #[arg(long)]
    pub subaccount: String,

    /// Amount of USDC to transfer
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,

    /// Transfer direction relative to the subaccount
    #[arg(long, value_enum)]
    pub direction: TransferDirection,

    /// Skip the confirmation prompt
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Arguments for `subaccount spot-transfer`.
#[derive(Args, Debug, Clone)]
pub struct SpotTransferArgs {
    /// Subaccount acting-account selector: subaccount address, stored account alias, or stored account id
    #[arg(long)]
    pub subaccount: String,

    /// Spot token identifier, for example PURR:0xc4bf3f870c0e9465323c0b6ed28096c2
    #[arg(long)]
    pub token: String,

    /// Token amount to transfer
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,

    /// Transfer direction relative to the subaccount
    #[arg(long, value_enum)]
    pub direction: TransferDirection,

    /// Skip the confirmation prompt
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Subaccount transfer direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferDirection {
    Deposit,
    Withdraw,
}

impl TransferDirection {
    pub fn is_deposit(self) -> bool {
        matches!(self, Self::Deposit)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubaccountActionKind {
    Create,
    TransferUsdc,
    TransferSpot,
}

impl SubaccountActionKind {
    #[must_use]
    pub fn reversibility(self) -> ActionReversibility {
        match self {
            Self::Create => ActionReversibility::Irreversible,
            Self::TransferUsdc | Self::TransferSpot => ActionReversibility::PartiallyReversible,
        }
    }
}

impl std::fmt::Display for TransferDirection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deposit => write!(f, "deposit"),
            Self::Withdraw => write!(f, "withdraw"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SubaccountActionConfirmation {
    action: String,
    status: String,
    network: String,
    signer: String,
    acting_as: String,
    reversibility: String,
    vault_address: Option<String>,
    subaccount: Option<String>,
    name: Option<String>,
    direction: Option<String>,
    amount: Option<String>,
    usd: Option<u64>,
    token: Option<String>,
}

impl Row for SubaccountActionConfirmation {
    const HEADERS: &'static [&'static str] = &[
        "Action",
        "Status",
        "Network",
        "Signer",
        "Acting As",
        "Reversibility",
        "Vault Address",
        "Subaccount",
        "Name",
        "Direction",
        "Amount",
        "USD Units",
        "Token",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.action.clone(),
            self.status.clone(),
            self.network.clone(),
            self.signer.clone(),
            self.acting_as.clone(),
            self.reversibility.clone(),
            self.vault_address.clone().unwrap_or_default(),
            self.subaccount.clone().unwrap_or_default(),
            self.name.clone().unwrap_or_default(),
            self.direction.clone().unwrap_or_default(),
            self.amount.clone().unwrap_or_default(),
            self.usd.map(|usd| usd.to_string()).unwrap_or_default(),
            self.token.clone().unwrap_or_default(),
        ]
    }
}

impl ListRow for SubaccountActionConfirmation {
    const EMPTY: &'static str = "No results.";
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateSubaccountAction {
    #[serde(rename = "type")]
    action_type: &'static str,
    name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SubaccountTransferAction {
    #[serde(rename = "type")]
    action_type: &'static str,
    sub_account_user: String,
    is_deposit: bool,
    usd: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SubaccountSpotTransferAction {
    #[serde(rename = "type")]
    action_type: &'static str,
    sub_account_user: String,
    is_deposit: bool,
    token: String,
    amount: String,
}

/// The subaccount name, trimmed; it must not be empty.
fn validated_name(args: &CreateArgs) -> Result<&str, CliError> {
    let name = args.name.trim();
    if name.is_empty() {
        return Err(CliError::Configuration(
            "subaccount name cannot be empty".to_string(),
        ));
    }
    Ok(name)
}

/// The token to transfer, trimmed; the amount must be positive and the token
/// not empty.
fn validated_token(args: &SpotTransferArgs) -> Result<&str, CliError> {
    quantity::require_positive(args.amount, "amount")?;
    let token = args.token.trim();
    if token.is_empty() {
        return Err(CliError::Configuration("token cannot be empty".to_string()));
    }
    Ok(token)
}

fn create_dry_run_value(chain: Chain, name: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "network": chain.to_string(),
        "reversibility": SubaccountActionKind::Create.reversibility(),
    })
}

fn transfer_dry_run_value(
    chain: Chain,
    subaccount: Address,
    usd: u64,
    args: &TransferArgs,
) -> serde_json::Value {
    serde_json::json!({
        "subaccount": subaccount.to_string(),
        "amount": args.amount.to_string(),
        "direction": args.direction.to_string(),
        "is_deposit": args.direction.is_deposit(),
        "usd": usd,
        "asset": "USDC",
        "network": chain.to_string(),
        "reversibility": SubaccountActionKind::TransferUsdc.reversibility(),
    })
}

fn spot_transfer_dry_run_value(
    chain: Chain,
    subaccount: Address,
    token: &str,
    args: &SpotTransferArgs,
) -> serde_json::Value {
    serde_json::json!({
        "subaccount": subaccount.to_string(),
        "token": token,
        "amount": args.amount.to_string(),
        "direction": args.direction.to_string(),
        "is_deposit": args.direction.is_deposit(),
        "asset": token,
        "network": chain.to_string(),
        "reversibility": SubaccountActionKind::TransferSpot.reversibility(),
    })
}

fn usdc_to_wire_units(amount: Decimal) -> Result<u64, CliError> {
    quantity::positive(amount, Scale::USDC).map_err(|err| err.config("amount", Scale::USDC))
}

async fn create(
    inv: &Invocation,
    signer: &SelectedSigner,
    name: &str,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let (api, chain) = (remote.api(), remote.chain());
    let action = CreateSubaccountAction {
        action_type: "createSubAccount",
        name: name.to_string(),
    };

    actions::send_raw_l1_json_action(
        api,
        signer,
        &action,
        actions::RawL1ActionMetadata::new(actions::nonce_now()),
        "create subaccount failed",
    )
    .await?;

    let signer_address = signer.address().to_string();
    print_action(
        inv,
        SubaccountActionConfirmation {
            action: "create".to_string(),
            status: "submitted".to_string(),
            network: chain.to_string(),
            signer: signer_address,
            acting_as: signer.query_address().to_string(),
            reversibility: (SubaccountActionKind::Create.reversibility())
                .as_str()
                .to_string(),
            vault_address: None,
            subaccount: None,
            name: Some(name.to_string()),
            direction: None,
            amount: None,
            usd: None,
            token: None,
        },
    );
    Ok(())
}

async fn transfer(
    inv: &Invocation,
    signer: &SelectedSigner,
    subaccount: Address,
    usd: u64,
    args: &TransferArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let (api, chain) = (remote.api(), remote.chain());
    confirm::unless_yes(
        args.yes,
        inv.mode(),
        &consent_preview(chain, signer, subaccount),
        &transfer_prompt(subaccount, args),
        "subaccount transfer aborted",
    )?;
    let action = SubaccountTransferAction {
        action_type: "subAccountTransfer",
        sub_account_user: subaccount.to_string().to_lowercase(),
        is_deposit: args.direction.is_deposit(),
        usd,
    };
    actions::send_raw_l1_json_action(
        api,
        signer,
        &action,
        actions::RawL1ActionMetadata::new(actions::nonce_now()),
        "subaccount transfer failed",
    )
    .await?;

    let signer_address = signer.address().to_string();
    print_action(
        inv,
        SubaccountActionConfirmation {
            action: "transfer".to_string(),
            status: "confirmed".to_string(),
            network: chain.to_string(),
            signer: signer_address,
            acting_as: signer.query_address().to_string(),
            reversibility: (SubaccountActionKind::TransferUsdc.reversibility())
                .as_str()
                .to_string(),
            vault_address: None,
            subaccount: Some(subaccount.to_string()),
            name: None,
            direction: Some(args.direction.to_string()),
            amount: Some(args.amount.normalize().to_string()),
            usd: Some(usd),
            token: Some("USDC".to_string()),
        },
    );
    Ok(())
}

async fn spot_transfer(
    inv: &Invocation,
    signer: &SelectedSigner,
    subaccount: Address,
    token: &str,
    args: &SpotTransferArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let (api, chain) = (remote.api(), remote.chain());
    confirm::unless_yes(
        args.yes,
        inv.mode(),
        &consent_preview(chain, signer, subaccount),
        &spot_transfer_prompt(subaccount, args),
        "subaccount spot transfer aborted",
    )?;

    let amount = args.amount.normalize().to_string();
    let action = SubaccountSpotTransferAction {
        action_type: "subAccountSpotTransfer",
        sub_account_user: subaccount.to_string().to_lowercase(),
        is_deposit: args.direction.is_deposit(),
        token: token.to_string(),
        amount: amount.clone(),
    };
    actions::send_raw_l1_json_action(
        api,
        signer,
        &action,
        actions::RawL1ActionMetadata::new(actions::nonce_now()),
        "subaccount spot transfer failed",
    )
    .await?;

    let signer_address = signer.address().to_string();
    print_action(
        inv,
        SubaccountActionConfirmation {
            action: "spot-transfer".to_string(),
            status: "confirmed".to_string(),
            network: chain.to_string(),
            signer: signer_address,
            acting_as: signer.query_address().to_string(),
            reversibility: (SubaccountActionKind::TransferSpot.reversibility())
                .as_str()
                .to_string(),
            vault_address: None,
            subaccount: Some(subaccount.to_string()),
            name: None,
            direction: Some(args.direction.to_string()),
            amount: Some(amount),
            usd: None,
            token: Some(token.to_string()),
        },
    );
    Ok(())
}

fn transfer_prompt(subaccount: Address, args: &TransferArgs) -> String {
    if args.direction.is_deposit() {
        format!(
            "Transfer {} USDC to subaccount {}?",
            args.amount, subaccount
        )
    } else {
        format!(
            "Transfer {} USDC from subaccount {}?",
            args.amount, subaccount
        )
    }
}

fn spot_transfer_prompt(subaccount: Address, args: &SpotTransferArgs) -> String {
    if args.direction.is_deposit() {
        format!(
            "Transfer {} {} to subaccount {}?",
            args.amount, args.token, subaccount
        )
    } else {
        format!(
            "Transfer {} {} from subaccount {}?",
            args.amount, args.token, subaccount
        )
    }
}

/// The consent preview: where and as whom the transfer happens.
fn consent_preview(chain: Chain, signer: &SelectedSigner, subaccount: Address) -> String {
    format!(
        "Network: {chain}\nSigner: {}\nSubaccount: {subaccount}",
        signer.address()
    )
}

fn print_action(inv: &Invocation, row: SubaccountActionConfirmation) {
    inv.print(&Rows::new(vec![row]));
}

pub const GROUP: GroupSpec = GroupSpec::new(&["subaccount"], "Manage subaccounts")
    .aliases(&["subaccounts"])
    .schema_aliases();

#[derive(Args, Debug, Clone)]
pub struct SubaccountListArgs {
    /// Master address, wallet name, or wallet id.
    /// Defaults to the selected/default signer when omitted.
    pub address: Option<String>,
}

pub struct SubaccountList;

impl CliCommand for SubaccountList {
    type Args = SubaccountListArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["subaccount", "list"],
        "account",
        "List subaccounts for an address or wallet",
        "Query public subaccounts for a master address or wallet.",
    )
    .cli_aliases(&["ls"])
    .args(&[arg("address")
        .kind(InputKind::PublicUserSelector)
        .describe("Master address, wallet name, or wallet id.")
        .resource_id("account selector")
        .schema_required()]);

    async fn run(inv: &Invocation, args: &SubaccountListArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_user(args.address.as_deref()).await?;
        crate::commands::account::subaccounts(inv, user).await
    }
}

pub struct SubaccountCreate;

impl CliCommand for SubaccountCreate {
    type Args = CreateArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["subaccount", "create"],
        "account",
        "Create a new subaccount signed by the master account",
        "Create a subaccount signed by the configured master account.",
    )
    .args(&[arg("name")
        .describe("Subaccount display name.")
        .resource_id("subaccount name")]);

    async fn run(inv: &Invocation, args: &CreateArgs) -> Result<(), anyhow::Error> {
        let name = validated_name(args)?;
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "create_subaccount",
                create_dry_run_value(inv.remote()?.chain(), name),
            );
        }
        let resolved = inv.resolve_signer()?;
        create(inv, &resolved, name).await
    }
}

pub struct SubaccountTransfer;

impl CliCommand for SubaccountTransfer {
    type Args = TransferArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["subaccount", "transfer"],
        "transfer",
        "Transfer USDC between the master account and a subaccount",
        "Transfer USDC between the master account and a subaccount.",
    )
    .prompt()
    .args(&[
        arg("subaccount").kind(InputKind::ActingAccountSelector).describe("Subaccount acting-account selector: subaccount address, wallet name, or wallet id. This does not apply to transfer recipients.").resource_id("subaccount selector"),
        arg("amount").kind(InputKind::Amount).describe("USDC amount; sent as integer units with 6 decimals."),
        arg("direction").describe("Deposit to or withdraw from the subaccount."),
        arg("yes").kind(InputKind::Boolean),
    ]);

    async fn run(inv: &Invocation, args: &TransferArgs) -> Result<(), anyhow::Error> {
        let usd = usdc_to_wire_units(args.amount)?;
        let subaccount = inv.resolve_account(&args.subaccount)?;
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "subaccount_usdc_transfer",
                transfer_dry_run_value(inv.remote()?.chain(), subaccount, usd, args),
            );
        }
        let resolved = inv.resolve_signer()?;
        transfer(inv, &resolved, subaccount, usd, args).await
    }
}

pub struct SubaccountSpotTransfer;

impl CliCommand for SubaccountSpotTransfer {
    type Args = SpotTransferArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["subaccount", "spot-transfer"],
        "transfer",
        "Transfer a spot token between the master account and a subaccount",
        "Transfer a spot token between the master account and a subaccount.",
    )
    .prompt()
    .args(&[
        arg("subaccount").kind(InputKind::ActingAccountSelector).describe("Subaccount acting-account selector: subaccount address, wallet name, or wallet id. This does not apply to transfer recipients.").resource_id("subaccount selector"),
        arg("token").kind(InputKind::MarketSymbol).describe("Spot token identifier.").resource_id("token"),
        arg("amount").kind(InputKind::Amount).describe("Token amount."),
        arg("direction").describe("Deposit to or withdraw from the subaccount."),
        arg("yes").kind(InputKind::Boolean),
    ]);

    async fn run(inv: &Invocation, args: &SpotTransferArgs) -> Result<(), anyhow::Error> {
        let token = validated_token(args)?;
        let subaccount = inv.resolve_account(&args.subaccount)?;
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "subaccount_spot_transfer",
                spot_transfer_dry_run_value(inv.remote()?.chain(), subaccount, token, args),
            );
        }
        let resolved = inv.resolve_signer()?;
        spot_transfer(inv, &resolved, subaccount, token, args).await
    }
}
