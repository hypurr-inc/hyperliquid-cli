use clap::Args;
use hypersdk::hypercore::Chain;
use hypersdk::{Address, Decimal};
use serde::Serialize;

use crate::assets::USDC_NAME;
use crate::cli_command::CliCommand;
use crate::command_spec::{CommandSpec, GroupSpec, InputKind, YES, arg};
use crate::commands::common::direction::TransferDirection;
use crate::commands::common::rows::{ListRow, Row, Rows};
use crate::commands::common::subaccount_list;
use crate::commands::common::support::print_dry_run;
use crate::consent::{self, ConsentBlock};
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::exchange::actions;
use crate::invocation::Invocation;
use crate::quantity;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubaccountActionKind {
    Create,
    TransferUsdc,
    TransferSpot,
}

impl SubaccountActionKind {
    fn reversibility(self) -> ActionReversibility {
        match self {
            Self::Create => ActionReversibility::Irreversible,
            Self::TransferUsdc | Self::TransferSpot => ActionReversibility::PartiallyReversible,
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
    subaccount: Option<String>,
    name: Option<String>,
    direction: Option<String>,
    amount: Option<String>,
    usd: Option<u64>,
    token: Option<String>,
}

impl SubaccountActionConfirmation {
    fn new(
        action: &str,
        status: &str,
        chain: Chain,
        signer: &SelectedSigner,
        kind: SubaccountActionKind,
    ) -> Self {
        Self {
            action: action.to_string(),
            status: status.to_string(),
            network: chain.to_string(),
            signer: signer.address().to_string(),
            acting_as: signer.query_address().to_string(),
            reversibility: kind.reversibility().as_str().to_string(),
            subaccount: None,
            name: None,
            direction: None,
            amount: None,
            usd: None,
            token: None,
        }
    }
}

impl Row for SubaccountActionConfirmation {
    const HEADERS: &'static [&'static str] = &[
        "Action",
        "Status",
        "Network",
        "Signer",
        "Acting As",
        "Reversibility",
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
#[serde(tag = "type", rename = "createSubAccount")]
struct CreateSubaccountAction {
    name: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename = "subAccountTransfer", rename_all = "camelCase")]
struct SubaccountTransferAction {
    sub_account_user: String,
    is_deposit: bool,
    usd: u64,
}

impl SubaccountTransferAction {
    fn new(subaccount: Address, direction: TransferDirection, usd: u64) -> Self {
        Self {
            sub_account_user: lowercase_address(subaccount),
            is_deposit: direction.is_deposit(),
            usd,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(
    tag = "type",
    rename = "subAccountSpotTransfer",
    rename_all = "camelCase"
)]
struct SubaccountSpotTransferAction {
    sub_account_user: String,
    is_deposit: bool,
    token: String,
    amount: String,
}

impl SubaccountSpotTransferAction {
    fn new(subaccount: Address, token: &str, args: &SpotTransferArgs) -> Self {
        Self {
            sub_account_user: lowercase_address(subaccount),
            is_deposit: args.direction.is_deposit(),
            token: token.to_string(),
            amount: quantity::wire_text(args.amount),
        }
    }
}

fn lowercase_address(address: Address) -> String {
    address.to_string().to_lowercase()
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

fn create_dry_run_value(chain: Chain, action: &CreateSubaccountAction) -> serde_json::Value {
    serde_json::json!({
        "name": action.name,
        "network": chain.to_string(),
        "reversibility": SubaccountActionKind::Create.reversibility(),
    })
}

fn transfer_dry_run_value(
    chain: Chain,
    subaccount: Address,
    action: &SubaccountTransferAction,
    args: &TransferArgs,
) -> serde_json::Value {
    serde_json::json!({
        "subaccount": subaccount.to_string(),
        "amount": args.amount.to_string(),
        "direction": args.direction.to_string(),
        "is_deposit": action.is_deposit,
        "usd": action.usd,
        "asset": USDC_NAME,
        "network": chain.to_string(),
        "reversibility": SubaccountActionKind::TransferUsdc.reversibility(),
    })
}

fn spot_transfer_dry_run_value(
    chain: Chain,
    subaccount: Address,
    action: &SubaccountSpotTransferAction,
    args: &SpotTransferArgs,
) -> serde_json::Value {
    serde_json::json!({
        "subaccount": subaccount.to_string(),
        "token": action.token,
        "amount": args.amount.to_string(),
        "direction": args.direction.to_string(),
        "is_deposit": action.is_deposit,
        "asset": action.token,
        "network": chain.to_string(),
        "reversibility": SubaccountActionKind::TransferSpot.reversibility(),
    })
}

async fn create(
    inv: &Invocation,
    signer: &SelectedSigner,
    action: CreateSubaccountAction,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    actions::send_raw_l1_json_action(
        remote.api(),
        signer,
        &action,
        None,
        "create subaccount failed",
    )
    .await?;

    print_action(
        inv,
        SubaccountActionConfirmation {
            name: Some(action.name),
            ..SubaccountActionConfirmation::new(
                "create",
                "submitted",
                remote.chain(),
                signer,
                SubaccountActionKind::Create,
            )
        },
    );
    Ok(())
}

async fn transfer(
    inv: &Invocation,
    signer: &SelectedSigner,
    subaccount: Address,
    action: SubaccountTransferAction,
    args: &TransferArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let chain = remote.chain();
    consent::approve(
        inv,
        args.yes,
        &consent_preview(chain, signer, subaccount),
        &transfer_prompt(subaccount, args),
        "subaccount transfer aborted",
    )?;
    actions::send_raw_l1_json_action(
        remote.api(),
        signer,
        &action,
        None,
        "subaccount transfer failed",
    )
    .await?;

    print_action(
        inv,
        SubaccountActionConfirmation {
            subaccount: Some(subaccount.to_string()),
            direction: Some(args.direction.to_string()),
            amount: Some(quantity::wire_text(args.amount)),
            usd: Some(action.usd),
            token: Some("USDC".to_string()),
            ..SubaccountActionConfirmation::new(
                "transfer",
                "confirmed",
                chain,
                signer,
                SubaccountActionKind::TransferUsdc,
            )
        },
    );
    Ok(())
}

async fn spot_transfer(
    inv: &Invocation,
    signer: &SelectedSigner,
    subaccount: Address,
    action: SubaccountSpotTransferAction,
    args: &SpotTransferArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let chain = remote.chain();
    consent::approve(
        inv,
        args.yes,
        &consent_preview(chain, signer, subaccount),
        &spot_transfer_prompt(subaccount, args),
        "subaccount spot transfer aborted",
    )?;

    actions::send_raw_l1_json_action(
        remote.api(),
        signer,
        &action,
        None,
        "subaccount spot transfer failed",
    )
    .await?;

    print_action(
        inv,
        SubaccountActionConfirmation {
            subaccount: Some(subaccount.to_string()),
            direction: Some(args.direction.to_string()),
            amount: Some(action.amount),
            token: Some(action.token),
            ..SubaccountActionConfirmation::new(
                "spot-transfer",
                "confirmed",
                chain,
                signer,
                SubaccountActionKind::TransferSpot,
            )
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

fn consent_preview(chain: Chain, signer: &SelectedSigner, subaccount: Address) -> ConsentBlock {
    ConsentBlock::signed(chain, signer.address()).field("Subaccount", subaccount)
}

fn print_action(inv: &Invocation, row: SubaccountActionConfirmation) {
    inv.print(&Rows::one(row));
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
        subaccount_list::subaccounts(inv, user).await
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
        let action = CreateSubaccountAction {
            name: validated_name(args)?.to_string(),
        };
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "create_subaccount",
                create_dry_run_value(inv.remote()?.chain(), &action),
            );
        }
        let resolved = inv.resolve_signer()?;
        create(inv, &resolved, action).await
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
        YES,
    ]);

    async fn run(inv: &Invocation, args: &TransferArgs) -> Result<(), anyhow::Error> {
        let usd = quantity::usdc_units(args.amount)?;
        let subaccount = inv.resolve_account(&args.subaccount)?;
        let action = SubaccountTransferAction::new(subaccount, args.direction, usd);
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "subaccount_usdc_transfer",
                transfer_dry_run_value(inv.remote()?.chain(), subaccount, &action, args),
            );
        }
        let resolved = inv.resolve_signer()?;
        transfer(inv, &resolved, subaccount, action, args).await
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
        YES,
    ]);

    async fn run(inv: &Invocation, args: &SpotTransferArgs) -> Result<(), anyhow::Error> {
        let token = validated_token(args)?;
        let subaccount = inv.resolve_account(&args.subaccount)?;
        let action = SubaccountSpotTransferAction::new(subaccount, token, args);
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "subaccount_spot_transfer",
                spot_transfer_dry_run_value(inv.remote()?.chain(), subaccount, &action, args),
            );
        }
        let resolved = inv.resolve_signer()?;
        spot_transfer(inv, &resolved, subaccount, action, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exchange_actions_keep_their_wire_shape() {
        let subaccount = Address::repeat_byte(0xab);
        let lower = "0xabababababababababababababababababababab";

        let create = CreateSubaccountAction {
            name: "mm".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&create).unwrap(),
            serde_json::json!({"type": "createSubAccount", "name": "mm"})
        );

        let transfer = SubaccountTransferAction::new(subaccount, TransferDirection::Withdraw, 10);
        assert_eq!(
            serde_json::to_value(&transfer).unwrap(),
            serde_json::json!({
                "type": "subAccountTransfer",
                "subAccountUser": lower,
                "isDeposit": false,
                "usd": 10
            })
        );
        // The L1 action hash covers the msgpack of the action, so the key
        // order is part of the contract.
        let mut msgpack = b"\x84\xa4type\xb2subAccountTransfer\xaesubAccountUser".to_vec();
        msgpack.extend(b"\xd9\x2a");
        msgpack.extend(lower.as_bytes());
        msgpack.extend(b"\xa9isDeposit\xc2\xa3usd\x0a");
        assert_eq!(rmp_serde::to_vec_named(&transfer).unwrap(), msgpack);

        let spot_args = SpotTransferArgs {
            subaccount: String::new(),
            token: "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2".to_string(),
            amount: "1.50".parse().unwrap(),
            direction: TransferDirection::Deposit,
            yes: true,
        };
        let spot = SubaccountSpotTransferAction::new(subaccount, &spot_args.token, &spot_args);
        assert_eq!(
            serde_json::to_value(&spot).unwrap(),
            serde_json::json!({
                "type": "subAccountSpotTransfer",
                "subAccountUser": lower,
                "isDeposit": true,
                "token": "PURR:0xc4bf3f870c0e9465323c0b6ed28096c2",
                "amount": "1.5"
            })
        );
    }
}
