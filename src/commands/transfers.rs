use alloy::sol;
use clap::Args;
use hypersdk::hypercore::Chain;
use hypersdk::hypercore::types::{Action, SpotSend, UsdSend};

use crate::assets::{SpotTokenListing, USDC_NAME, require_spot_token};
use crate::cli_command::CliCommand;
use crate::command_spec::{CommandSpec, GroupSpec, InputKind, YES_FOR_AUTOMATION, arg};
use crate::commands::common::rows::{ListRow, Row, Rows};
use crate::commands::common::support::print_dry_run;
use crate::consent::{self, ConsentBlock};
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::exchange::actions;
use crate::exchange::funds::{self, Pool};
use crate::exchange::send_asset::{AssetMove, AssetVenue, ResolvedMove, from_subaccount_address};
use crate::exchange::user_signed;
use crate::invocation::Invocation;
use crate::quantity;
use crate::signable::{Signs, UsdClassTransfer, UserSigned};
use crate::signing::SelectedSigner;
use crate::wire::{AddressRole, address};
use hypersdk::{Address, Decimal};
use serde::Serialize;

sol! {
    #[derive(Serialize)]
    struct Withdraw {
        string hyperliquidChain;
        string destination;
        string amount;
        uint64 time;
    }
}

impl UserSigned for Withdraw {
    const ACTION_TYPE: &'static str = "withdraw3";
}

/// Arguments for `transfer spot-to-perp`.
#[derive(Args, Debug, Clone)]
pub struct ClassTransferArgs {
    /// Amount of USDC to transfer
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,

    /// Move the USDC of this subaccount instead of the signer's own account: subaccount address, stored account alias, or stored account id
    #[arg(long)]
    pub subaccount: Option<String>,

    /// Confirm live execution without prompting
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Arguments for `transfer send`.
#[derive(Args, Debug, Clone)]
pub struct SendArgs {
    /// Destination Hyperliquid address
    #[arg(long)]
    pub to: String,

    /// Amount of USDC to send
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,

    /// Confirm live execution without prompting
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Arguments for `transfer spot-send`.
#[derive(Args, Debug, Clone)]
pub struct SpotSendArgs {
    /// Destination Hyperliquid address
    #[arg(long)]
    pub to: String,

    /// Spot token symbol, for example HYPE or PURR
    #[arg(long)]
    pub token: String,

    /// Token amount to send
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,

    /// Confirm live execution without prompting
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Arguments for `transfer send-asset`.
#[derive(Args, Debug, Clone)]
pub struct SendAssetArgs {
    /// Destination Hyperliquid address
    #[arg(long)]
    pub to: String,

    /// Source venue: perp, spot, or dex:<NAME>
    #[arg(long)]
    pub source: String,

    /// Destination venue: perp, spot, or dex:<NAME>
    #[arg(long)]
    pub dest: String,

    /// Spot token symbol, for example USDC or HYPE
    #[arg(long)]
    pub token: String,

    /// Token amount to send
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,

    /// Optional source subaccount address
    #[arg(long)]
    pub from_subaccount: Option<String>,

    /// Confirm live execution without prompting
    #[arg(short = 'y', long)]
    pub yes: bool,
}

/// Arguments for `transfer withdraw`.
#[derive(Args, Debug, Clone)]
pub struct WithdrawArgs {
    /// Destination Arbitrum address
    #[arg(long)]
    pub to: String,

    /// Amount of USDC to withdraw
    #[arg(long, allow_hyphen_values = true)]
    pub amount: Decimal,

    /// Confirm live execution without prompting
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Debug, Clone, Serialize)]
struct TransferConfirmation {
    action: String,
    status: String,
    #[serde(with = "rust_decimal::serde::str")]
    amount: Decimal,
    asset: String,
    destination: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subaccount: Option<String>,
}

impl TransferConfirmation {
    fn new(action: &str, amount: Decimal, asset: impl Into<String>) -> Self {
        Self {
            action: action.to_string(),
            status: "confirmed".to_string(),
            amount,
            asset: asset.into(),
            destination: None,
            subaccount: None,
        }
    }

    fn to(self, destination: &str) -> Self {
        Self {
            destination: Some(destination.to_string()),
            ..self
        }
    }

    fn print(self, inv: &Invocation) {
        inv.print(&Rows::new(vec![self]));
    }
}

impl Row for TransferConfirmation {
    const HEADERS: &'static [&'static str] =
        &["Action", "Status", "Amount", "Asset", "Destination"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.action.clone(),
            self.status.clone(),
            self.amount.to_string(),
            self.asset.clone(),
            self.destination.clone().unwrap_or_default(),
        ]
    }
}

impl ListRow for TransferConfirmation {
    const EMPTY: &'static str = "No results.";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferActionKind {
    SpotToPerp,
    PerpToSpot,
    Send,
    SpotSend,
    SendAsset,
    Withdraw,
}

impl TransferActionKind {
    fn reversibility(self) -> ActionReversibility {
        match self {
            Self::SpotToPerp | Self::PerpToSpot => ActionReversibility::Reversible,
            Self::Send | Self::SpotSend | Self::SendAsset | Self::Withdraw => {
                ActionReversibility::Irreversible
            }
        }
    }

    fn would_execute(self) -> &'static str {
        match self {
            Self::SpotToPerp => "transfer_spot_to_perp",
            Self::PerpToSpot => "transfer_perp_to_spot",
            Self::Send => "send_usdc",
            Self::SpotSend => "spot_send",
            Self::SendAsset => "send_asset",
            Self::Withdraw => "withdraw_usdc",
        }
    }
}

const EXCHANGE_DEFAULT_FEE: &str = "not_estimated_exchange_default";

struct TransferPreview {
    network: Chain,
    signer: Address,
    acting_context: Address,
    recipient: Address,
    destination_class: &'static str,
    asset: String,
    amount: Decimal,
    fee_or_cap: &'static str,
    reversibility: ActionReversibility,
}

impl TransferPreview {
    fn block(&self) -> ConsentBlock {
        ConsentBlock::signed(self.network, self.signer)
            .field("Acting context", self.acting_context)
            .field("Recipient", self.recipient)
            .field("Destination class", self.destination_class)
            .field("Asset", &self.asset)
            .field("Amount", self.amount)
            .field("Fee/cap", self.fee_or_cap)
            .field("Reversibility", self.reversibility.as_str())
    }
}

fn destination_address(raw: &str) -> Result<Address, CliError> {
    address(raw, AddressRole::Explicit(Some("--to")))
}

fn validate_send_args(args: &SendArgs) -> Result<Address, CliError> {
    quantity::require_positive(args.amount, "amount")?;
    destination_address(&args.to)
}

fn validate_spot_send_args(args: &SpotSendArgs) -> Result<Address, CliError> {
    quantity::require_positive(args.amount, "amount")?;
    let destination = destination_address(&args.to)?;
    validate_token_symbol(&args.token)?;
    Ok(destination)
}

fn validate_withdraw_args(args: &WithdrawArgs) -> Result<Address, CliError> {
    quantity::require_positive(args.amount, "amount")?;
    destination_address(&args.to)
}

async fn send(
    inv: &Invocation,
    signer: &SelectedSigner,
    destination: Address,
    args: &SendArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let (api, chain) = (remote.api(), remote.chain());
    funds::ensure_usdc(api, signer.query_address(), Pool::Spendable, args.amount).await?;
    let preview = TransferPreview {
        network: chain,
        signer: signer.address(),
        acting_context: signer.query_address(),
        recipient: destination,
        destination_class: "hyperliquid_user_address",
        asset: USDC_NAME.to_string(),
        amount: args.amount,
        fee_or_cap: EXCHANGE_DEFAULT_FEE,
        reversibility: TransferActionKind::Send.reversibility(),
    };
    consent::approve(
        inv,
        args.yes,
        &preview.block(),
        &format!("Send {} USDC to {}?", args.amount, args.to),
        "send transfer aborted",
    )?;

    let nonce = api.nonces().next();
    let action = Action::UsdSend(
        UsdSend {
            destination,
            amount: args.amount,
            time: nonce,
        }
        .into_action(chain),
    );
    actions::send_default_l1_action_with_nonce(api, signer, action, nonce).await?;

    TransferConfirmation::new("send", args.amount, USDC_NAME)
        .to(&args.to)
        .print(inv);
    Ok(())
}

async fn spot_send(
    inv: &Invocation,
    signer: &SelectedSigner,
    destination: Address,
    token: SpotTokenListing,
    args: &SpotSendArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let (api, chain) = (remote.api(), remote.chain());
    let preview = TransferPreview {
        network: chain,
        signer: signer.address(),
        acting_context: signer.query_address(),
        recipient: destination,
        destination_class: "hyperliquid_spot_user_address",
        asset: token.name.clone(),
        amount: args.amount,
        fee_or_cap: EXCHANGE_DEFAULT_FEE,
        reversibility: TransferActionKind::SpotSend.reversibility(),
    };
    consent::approve(
        inv,
        args.yes,
        &preview.block(),
        &format!(
            "Send {} {} spot token to {}?",
            args.amount, token.name, args.to
        ),
        "spot send transfer aborted",
    )?;

    let nonce = api.nonces().next();
    let spot_send = SpotSend {
        destination,
        token: token.send_token(),
        amount: args.amount,
        time: nonce,
    }
    .into_action(chain);
    let action = Action::SpotSend(spot_send);
    actions::send_default_l1_action_with_nonce(api, signer, action, nonce).await?;

    TransferConfirmation::new("spot-send", args.amount, token.name)
        .to(&args.to)
        .print(inv);
    Ok(())
}

fn asset_move(args: &SendAssetArgs) -> Result<AssetMove, CliError> {
    quantity::require_positive(args.amount, "amount")?;
    let to = destination_address(&args.to)?;
    validate_token_symbol(&args.token)?;
    let source = AssetVenue::parse(&args.source, "--source")?;
    let dest = AssetVenue::parse(&args.dest, "--dest")?;
    if let Some(from_subaccount) = args.from_subaccount.as_deref() {
        from_subaccount_address(from_subaccount)?;
    }
    Ok(AssetMove {
        to,
        source,
        dest,
        token: args.token.clone(),
        amount: args.amount,
        from_subaccount: args.from_subaccount.clone(),
    })
}

async fn withdraw(
    inv: &Invocation,
    signer: &SelectedSigner,
    destination: Address,
    args: &WithdrawArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let (api, chain) = (remote.api(), remote.chain());
    funds::ensure_usdc(api, signer.query_address(), Pool::Spendable, args.amount).await?;
    let preview = TransferPreview {
        network: chain,
        signer: signer.address(),
        acting_context: signer.query_address(),
        recipient: destination,
        destination_class: "arbitrum_address",
        asset: USDC_NAME.to_string(),
        amount: args.amount,
        fee_or_cap: "not_estimated_bridge_or_exchange_fee",
        reversibility: TransferActionKind::Withdraw.reversibility(),
    };
    consent::approve(
        inv,
        args.yes,
        &preview.block(),
        &format!(
            "Withdraw {} USDC to Arbitrum address {}?",
            args.amount, args.to
        ),
        "withdraw transfer aborted",
    )?;

    let amount = quantity::wire_text(args.amount);
    user_signed::submit(api, signer, |hyperliquid_chain, nonce| Withdraw {
        hyperliquidChain: hyperliquid_chain,
        destination: format!("{destination:#x}"),
        amount,
        time: nonce,
    })
    .await?;

    TransferConfirmation::new("withdraw", args.amount, USDC_NAME)
        .to(&args.to)
        .print(inv);
    Ok(())
}

fn validate_token_symbol(token: &str) -> Result<(), CliError> {
    if token.trim().is_empty() {
        return Err(CliError::Configuration("token cannot be empty".to_string()));
    }
    Ok(())
}

async fn class_transfer(
    inv: &Invocation,
    signer: &SelectedSigner,
    args: &ClassTransferArgs,
    kind: TransferActionKind,
    subaccount: Option<Address>,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let api = remote.api();
    let balance_user = subaccount.unwrap_or_else(|| signer.query_address());
    let to_perp = kind == TransferActionKind::SpotToPerp;
    let amount = args.amount;
    let pool = if to_perp { Pool::Spot } else { Pool::Spendable };
    funds::ensure_usdc(api, balance_user, pool, amount).await?;

    let direction = if to_perp {
        "from spot to perp"
    } else {
        "from perp to spot"
    };
    let question = match subaccount {
        Some(subaccount) => {
            format!("Transfer {amount} USDC {direction} for subaccount {subaccount}?")
        }
        None => format!("Transfer {amount} USDC {direction}?"),
    };
    consent::approve(
        inv,
        args.yes,
        &ConsentBlock::new(),
        &question,
        "transfer aborted",
    )?;

    let wire_amount = class_transfer_amount(amount, subaccount);
    user_signed::submit(api, signer, |hyperliquid_chain, nonce| UsdClassTransfer {
        hyperliquidChain: hyperliquid_chain,
        amount: wire_amount,
        toPerp: to_perp,
        nonce,
    })
    .await?;

    let label = if to_perp {
        "spot-to-perp"
    } else {
        "perp-to-spot"
    };
    TransferConfirmation {
        subaccount: subaccount.map(|subaccount| subaccount.to_string()),
        ..TransferConfirmation::new(label, amount.normalize(), USDC_NAME)
    }
    .print(inv);
    Ok(())
}

/// The `amount` of a `usdClassTransfer`. For a subaccount the exchange reads
/// the target from a ` subaccount:<address>` suffix on the amount, which is
/// part of what the master signs.
fn class_transfer_amount(amount: Decimal, subaccount: Option<Address>) -> String {
    let amount = quantity::wire_text(amount);
    match subaccount {
        Some(subaccount) => format!("{amount} subaccount:{subaccount:#x}"),
        None => amount,
    }
}

async fn run_class_transfer(
    inv: &Invocation,
    args: &ClassTransferArgs,
    kind: TransferActionKind,
) -> Result<(), anyhow::Error> {
    quantity::require_positive(args.amount, "amount")?;
    let subaccount = args
        .subaccount
        .as_deref()
        .map(|selector| inv.resolve_account(selector))
        .transpose()?;
    if inv.dry_run() {
        let mut preview = serde_json::json!({"amount": args.amount.to_string()});
        if let Some(subaccount) = subaccount {
            preview["subaccount"] = subaccount.to_string().into();
            preview["wire_amount"] = class_transfer_amount(args.amount, Some(subaccount)).into();
        }
        return print_dry_run(inv, kind.would_execute(), preview);
    }
    let signer = inv.resolve_signer()?;
    class_transfer(inv, &signer, args, kind, subaccount).await
}

pub const GROUP: GroupSpec = GroupSpec::new(
    &["transfer"],
    "Move funds between spot, perp, and addresses",
)
.aliases(&["transfers"])
.schema_aliases();

pub struct TransferSpotToPerp;

impl CliCommand for TransferSpotToPerp {
    type Args = ClassTransferArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["transfer", "spot-to-perp"],
        "transfer",
        "Transfer from spot to perp",
        "Transfer USDC from spot to perp.",
    )
    .user_signed::<UsdClassTransfer>()
    .prompt()
    .args(&[
        arg("amount")
            .kind(InputKind::Amount)
            .describe("USDC amount."),
        arg("subaccount")
            .kind(InputKind::ActingAccountSelector)
            .describe("Move the USDC of this subaccount instead of the signer's own account: subaccount address, wallet name, or wallet id.")
            .resource_id("subaccount selector"),
        YES_FOR_AUTOMATION,
    ]);

    async fn run(inv: &Invocation, args: &ClassTransferArgs) -> Result<(), anyhow::Error> {
        run_class_transfer(inv, args, TransferActionKind::SpotToPerp).await
    }
}

pub struct TransferPerpToSpot;

impl CliCommand for TransferPerpToSpot {
    type Args = ClassTransferArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["transfer", "perp-to-spot"],
        "transfer",
        "Transfer from perp to spot",
        "Transfer USDC from perp to spot.",
    )
    .user_signed::<UsdClassTransfer>()
    .prompt()
    .args(&[
        arg("amount")
            .kind(InputKind::Amount)
            .describe("USDC amount."),
        arg("subaccount")
            .kind(InputKind::ActingAccountSelector)
            .describe("Move the USDC of this subaccount instead of the signer's own account: subaccount address, wallet name, or wallet id.")
            .resource_id("subaccount selector"),
        YES_FOR_AUTOMATION,
    ]);

    async fn run(inv: &Invocation, args: &ClassTransferArgs) -> Result<(), anyhow::Error> {
        run_class_transfer(inv, args, TransferActionKind::PerpToSpot).await
    }
}

pub struct TransferSend;

impl CliCommand for TransferSend {
    type Args = SendArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["transfer", "send"],
        "transfer",
        "Send USDC to an address",
        "Send USDC to another Hyperliquid address.",
    )
    .digest_signed("usdSend")
    .prompt()
    .args(&[
        arg("to")
            .kind(InputKind::RawDestinationAddress)
            .describe("Explicit destination address. Wallet aliases are not resolved.")
            .resource_id("destination address"),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("USDC amount."),
        YES_FOR_AUTOMATION,
    ]);

    async fn run(inv: &Invocation, args: &SendArgs) -> Result<(), anyhow::Error> {
        let destination = validate_send_args(args)?;
        if inv.dry_run() {
            return print_dry_run(
                inv,
                TransferActionKind::Send.would_execute(),
                serde_json::json!({"to": args.to, "amount": args.amount.to_string()}),
            );
        }
        let resolved = inv.resolve_signer()?;
        send(inv, &resolved, destination, args).await
    }
}

pub struct TransferSpotSend;

impl CliCommand for TransferSpotSend {
    type Args = SpotSendArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["transfer", "spot-send"],
        "transfer",
        "Send a spot token to an address",
        "Send a spot token to another Hyperliquid address.",
    )
    .digest_signed("spotSend")
    .prompt()
    .args(&[
        arg("to")
            .kind(InputKind::RawDestinationAddress)
            .describe("Explicit destination address. Wallet aliases are not resolved.")
            .resource_id("destination address"),
        arg("token")
            .kind(InputKind::MarketSymbol)
            .describe("Spot token symbol, for example HYPE.")
            .resource_id("token"),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("Token amount."),
        YES_FOR_AUTOMATION,
    ]);

    async fn run(inv: &Invocation, args: &SpotSendArgs) -> Result<(), anyhow::Error> {
        let destination = validate_spot_send_args(args)?;
        let token = require_spot_token(inv.remote()?.api(), &args.token).await?;
        if inv.dry_run() {
            return print_dry_run(
                inv,
                TransferActionKind::SpotSend.would_execute(),
                serde_json::json!({
                    "to": destination.to_string(),
                    "token": token.name,
                    "token_index": token.index,
                    "amount": quantity::wire_text(args.amount),
                }),
            );
        }
        let resolved = inv.resolve_signer()?;
        spot_send(inv, &resolved, destination, token, args).await
    }
}

pub struct TransferSendAsset;

impl CliCommand for TransferSendAsset {
    type Args = SendAssetArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["transfer", "send-asset"],
        "transfer",
        "Send an asset between spot, perp, or DEX contexts",
        "Send an asset between accounts, spot, perp, or DEX contexts. An API wallet signs agentSendAsset instead, which only sends to the account the funds leave (the master, or --from-subaccount).",
    )
    .signs(Signs::Digest)
    .prompt()
    .args(&[
        arg("to")
            .kind(InputKind::RawDestinationAddress)
            .describe("Explicit destination address. Wallet aliases are not resolved.")
            .resource_id("destination address"),
        arg("source")
            .kind(InputKind::VenueSelector)
            .resource_id("source asset target"),
        arg("dest")
            .kind(InputKind::VenueSelector)
            .resource_id("destination asset target"),
        arg("token")
            .kind(InputKind::MarketSymbol)
            .resource_id("token"),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("Token amount."),
        arg("from_subaccount")
            .describe("Optional explicit source subaccount address.")
            .resource_id("from subaccount address"),
        YES_FOR_AUTOMATION,
    ]);

    async fn run(inv: &Invocation, args: &SendAssetArgs) -> Result<(), anyhow::Error> {
        let remote = inv.remote()?;
        let resolved = ResolvedMove::resolve(remote.api(), asset_move(args)?).await?;
        let AssetMove {
            to,
            source,
            dest,
            amount,
            from_subaccount,
            ..
        } = &resolved.mv;
        let token = &resolved.token;
        if inv.dry_run() {
            let mut preview = serde_json::json!({
                "to": to.to_string(),
                "source": source.display,
                "source_wire": source.wire,
                "dest": dest.display,
                "dest_wire": dest.wire,
                "token": token.name,
                "token_index": token.index,
                "amount": quantity::wire_text(*amount),
                "from_subaccount": from_subaccount,
            });
            let form = inv.dry_run_signer()?.and_then(SelectedSigner::agent_form);
            if resolved.mv.agent_form(form)?.is_some() {
                preview["action_type"] = "agentSendAsset".into();
            }
            return print_dry_run(inv, TransferActionKind::SendAsset.would_execute(), preview);
        }
        let signer = inv.resolve_signer()?;
        let form = resolved.mv.agent_form(signer.agent_form())?;
        let preview = TransferPreview {
            network: remote.chain(),
            signer: signer.address(),
            acting_context: signer.query_address(),
            recipient: *to,
            destination_class: "hyperliquid_asset_context",
            asset: format!("{} {}->{}", token.name, source.display, dest.display),
            amount: *amount,
            fee_or_cap: EXCHANGE_DEFAULT_FEE,
            reversibility: TransferActionKind::SendAsset.reversibility(),
        };
        consent::approve(
            inv,
            args.yes,
            &preview.block(),
            &format!(
                "Send {amount} {} from {} to {} for {}?",
                token.name, source.display, dest.display, args.to
            ),
            "send-asset transfer aborted",
        )?;
        resolved.send(remote.api(), &signer, form).await?;
        TransferConfirmation::new("send-asset", *amount, token.name.clone())
            .to(&args.to)
            .print(inv);
        Ok(())
    }
}

pub struct TransferWithdraw;

impl CliCommand for TransferWithdraw {
    type Args = WithdrawArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["transfer", "withdraw"],
        "transfer",
        "Withdraw to Arbitrum",
        "Withdraw USDC to Arbitrum.",
    )
    .user_signed::<Withdraw>()
    .prompt()
    .args(&[
        arg("to")
            .kind(InputKind::RawDestinationAddress)
            .resource_id("destination address"),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("USDC amount."),
        YES_FOR_AUTOMATION,
    ]);

    async fn run(inv: &Invocation, args: &WithdrawArgs) -> Result<(), anyhow::Error> {
        let destination = validate_withdraw_args(args)?;
        if inv.dry_run() {
            return print_dry_run(
                inv,
                TransferActionKind::Withdraw.would_execute(),
                serde_json::json!({"to": args.to, "amount": args.amount.to_string()}),
            );
        }
        let resolved = inv.resolve_signer()?;
        withdraw(inv, &resolved, destination, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_consent_preview_names_every_fact_of_the_transfer() {
        let preview = TransferPreview {
            network: Chain::Testnet,
            signer: Address::repeat_byte(1),
            acting_context: Address::repeat_byte(2),
            recipient: Address::repeat_byte(3),
            destination_class: "arbitrum_address",
            asset: "USDC".to_string(),
            amount: Decimal::from(5),
            fee_or_cap: EXCHANGE_DEFAULT_FEE,
            reversibility: ActionReversibility::Irreversible,
        }
        .block()
        .render();

        let labels: Vec<&str> = preview
            .lines()
            .filter_map(|line| line.split_once(": ").map(|(label, _)| label))
            .collect();
        assert_eq!(
            labels,
            [
                "Network",
                "Signer",
                "Acting context",
                "Recipient",
                "Destination class",
                "Asset",
                "Amount",
                "Fee/cap",
                "Reversibility"
            ]
        );
    }

    #[test]
    fn a_subaccount_class_transfer_signs_the_amount_with_its_suffix() {
        let subaccount = Address::repeat_byte(0xcc);

        assert_eq!(
            class_transfer_amount(Decimal::new(55, 1), Some(subaccount)),
            "5.5 subaccount:0xcccccccccccccccccccccccccccccccccccccccc"
        );
        assert_eq!(class_transfer_amount(Decimal::new(55, 1), None), "5.5");
        assert_eq!(class_transfer_amount(Decimal::new(500, 2), None), "5");
    }
}
