use alloy::sol;
use clap::Args;
use hypersdk::hypercore::types::{
    Action, AgentSendAsset, AssetTarget, SendAsset, SendToken, SpotSend, UsdSend,
};
use hypersdk::hypercore::{Chain, SpotToken};

use crate::assets::require_spot_token;
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, arg};
use crate::commands::abstraction::fetch_abstraction;
use crate::commands::actions;
use crate::commands::confirm;
use crate::commands::rows::{ListRow, Row, Rows};
use crate::commands::spot_balances::user_spot_balances_raw;
use crate::commands::support::print_dry_run;
use crate::commands::user_signed::{self, UserSigned};
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::http_api::ApiClient;
use crate::invocation::{Invocation, Remote};
use crate::mode::Mode;
use crate::quantity;
use crate::signing::{SelectedSigner, Signs};
use crate::wire::{AddressRole, address};
use hypersdk::{Address, Decimal};
use serde::Serialize;

sol! {
    #[derive(Serialize)]
    struct UsdClassTransfer {
        string hyperliquidChain;
        string amount;
        bool toPerp;
        uint64 nonce;
    }

    #[derive(Serialize)]
    struct Withdraw {
        string hyperliquidChain;
        string destination;
        string amount;
        uint64 time;
    }
}

impl UserSigned for UsdClassTransfer {
    const ACTION_TYPE: &'static str = "usdClassTransfer";
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
pub enum TransferActionKind {
    SpotToPerp,
    PerpToSpot,
    Send,
    SpotSend,
    SendAsset,
    Withdraw,
}

impl TransferActionKind {
    #[must_use]
    pub fn reversibility(self) -> ActionReversibility {
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

/// What the consent prompt shows about an external transfer.
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
    fn render(&self) -> String {
        format!(
            "\
Network: {network}
Signer: {signer}
Acting context: {acting_context}
Recipient: {recipient}
Destination class: {destination_class}
Asset: {asset}
Amount: {amount}
Fee/cap: {fee_or_cap}
Reversibility: {reversibility}",
            network = self.network,
            signer = self.signer,
            acting_context = self.acting_context,
            recipient = self.recipient,
            destination_class = self.destination_class,
            asset = self.asset,
            amount = self.amount,
            fee_or_cap = self.fee_or_cap,
            reversibility = self.reversibility.as_str(),
        )
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

struct SendAssetRoute {
    destination: Address,
    source: ParsedAssetTarget,
    dest: ParsedAssetTarget,
}

fn validate_send_asset_args(args: &SendAssetArgs) -> Result<SendAssetRoute, CliError> {
    quantity::require_positive(args.amount, "amount")?;
    let destination = destination_address(&args.to)?;
    validate_token_symbol(&args.token)?;
    let source = parse_asset_target(&args.source, "--source")?;
    let dest = parse_asset_target(&args.dest, "--dest")?;
    if let Some(from_subaccount) = args.from_subaccount.as_deref() {
        address(
            from_subaccount,
            AddressRole::Explicit(Some("--from-subaccount")),
        )?;
    }
    Ok(SendAssetRoute {
        destination,
        source,
        dest,
    })
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
    ensure_usdc_available(api, signer.query_address(), args.amount).await?;
    confirm_transfer(
        inv.mode(),
        args.yes,
        TransferPreview {
            network: chain,
            signer: signer.address(),
            acting_context: signer.query_address(),
            recipient: destination,
            destination_class: "hyperliquid_user_address",
            asset: "USDC".to_string(),
            amount: args.amount,
            fee_or_cap: EXCHANGE_DEFAULT_FEE,
            reversibility: TransferActionKind::Send.reversibility(),
        },
        &format!("Send {} USDC to {}?", args.amount, args.to),
        "send transfer aborted",
    )?;

    let nonce = actions::nonce_now();
    let action = Action::UsdSend(
        UsdSend {
            destination,
            amount: args.amount,
            time: nonce,
        }
        .into_action(chain),
    );
    actions::send_l1_action(api, signer, action, nonce).await?;

    print_transfer(
        inv,
        "send",
        args.amount,
        "USDC".to_string(),
        Some(args.to.clone()),
    );
    Ok(())
}

async fn spot_send(
    inv: &Invocation,
    signer: &SelectedSigner,
    destination: Address,
    token: SpotToken,
    args: &SpotSendArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let (api, chain) = (remote.api(), remote.chain());
    confirm_transfer(
        inv.mode(),
        args.yes,
        TransferPreview {
            network: chain,
            signer: signer.address(),
            acting_context: signer.query_address(),
            recipient: destination,
            destination_class: "hyperliquid_spot_user_address",
            asset: token.name.clone(),
            amount: args.amount,
            fee_or_cap: EXCHANGE_DEFAULT_FEE,
            reversibility: TransferActionKind::SpotSend.reversibility(),
        },
        &format!(
            "Send {} {} spot token to {}?",
            args.amount, token.name, args.to
        ),
        "spot send transfer aborted",
    )?;

    let nonce = actions::nonce_now();
    let spot_send = SpotSend {
        destination,
        token: SendToken(token.clone()),
        amount: args.amount,
        time: nonce,
    }
    .into_action(chain);
    let action = Action::SpotSend(spot_send);
    actions::send_l1_action(api, signer, action, nonce).await?;

    print_transfer(
        inv,
        "spot-send",
        args.amount,
        token.name,
        Some(args.to.clone()),
    );
    Ok(())
}

/// Sign and submit a `sendAsset` action without printing a receipt.
pub async fn execute_send_asset(
    mode: &Mode,
    remote: &Remote,
    signer: &SelectedSigner,
    args: &SendAssetArgs,
) -> Result<String, anyhow::Error> {
    let route = validate_send_asset_args(args)?;
    let token = require_spot_token(remote.api().sdk(), &args.token).await?;
    submit_send_asset(mode, remote, signer, args, route, token).await
}

async fn submit_send_asset(
    mode: &Mode,
    remote: &Remote,
    signer: &SelectedSigner,
    args: &SendAssetArgs,
    route: SendAssetRoute,
    token: SpotToken,
) -> Result<String, anyhow::Error> {
    let SendAssetRoute {
        destination,
        source,
        dest,
    } = route;
    let (api, chain) = (remote.api(), remote.chain());
    let agent_signed = signer.is_stored_agent();
    if agent_signed {
        ensure_agent_send_target(destination, signer.query_address(), args)?;
    }
    confirm_transfer(
        mode,
        args.yes,
        TransferPreview {
            network: chain,
            signer: signer.address(),
            acting_context: signer.query_address(),
            recipient: destination,
            destination_class: "hyperliquid_asset_context",
            asset: format!("{} {}->{}", token.name, source.display, dest.display),
            amount: args.amount,
            fee_or_cap: EXCHANGE_DEFAULT_FEE,
            reversibility: TransferActionKind::SendAsset.reversibility(),
        },
        &format!(
            "Send {} {} from {} to {} for {}?",
            args.amount, token.name, source.display, dest.display, args.to
        ),
        "send-asset transfer aborted",
    )?;

    let nonce = actions::nonce_now();
    let from_sub_account = args.from_subaccount.clone().unwrap_or_default();
    let action = if agent_signed {
        Action::AgentSendAsset(
            AgentSendAsset {
                destination,
                source_dex: source.target,
                destination_dex: dest.target,
                token: SendToken(token.clone()),
                amount: args.amount,
                from_sub_account,
                nonce,
            }
            .into_action(),
        )
    } else {
        Action::SendAsset(
            SendAsset {
                destination,
                source_dex: source.target,
                destination_dex: dest.target,
                token: SendToken(token.clone()),
                amount: args.amount,
                from_sub_account,
                nonce,
            }
            .into_action(chain),
        )
    };
    actions::send_l1_action(api, signer, action, nonce).await?;
    Ok(token.name)
}

/// An API wallet cannot sign `sendAsset`; it signs `agentSendAsset`, which the
/// exchange only accepts when the destination is the account the funds leave:
/// the master it signs for, or the subaccount named as the source.
fn ensure_agent_send_target(
    destination: Address,
    master: Address,
    args: &SendAssetArgs,
) -> Result<(), CliError> {
    let source = match args.from_subaccount.as_deref() {
        Some(raw) => address(raw, AddressRole::Explicit(Some("--from-subaccount")))?,
        None => master,
    };
    if destination != source {
        return Err(CliError::Unsupported(format!(
            "an API wallet can only send an asset to the account it leaves ({source}): --to is {destination}; move funds to another address with the master wallet"
        )));
    }
    Ok(())
}

async fn withdraw(
    inv: &Invocation,
    signer: &SelectedSigner,
    destination: Address,
    args: &WithdrawArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let (api, chain) = (remote.api(), remote.chain());
    ensure_usdc_available(api, signer.query_address(), args.amount).await?;
    confirm_transfer(
        inv.mode(),
        args.yes,
        TransferPreview {
            network: chain,
            signer: signer.address(),
            acting_context: signer.query_address(),
            recipient: destination,
            destination_class: "arbitrum_address",
            asset: "USDC".to_string(),
            amount: args.amount,
            fee_or_cap: "not_estimated_bridge_or_exchange_fee",
            reversibility: TransferActionKind::Withdraw.reversibility(),
        },
        &format!(
            "Withdraw {} USDC to Arbitrum address {}?",
            args.amount, args.to
        ),
        "withdraw transfer aborted",
    )?;

    let amount = args.amount.normalize().to_string();
    user_signed::submit(api, signer, |hyperliquid_chain, nonce| Withdraw {
        hyperliquidChain: hyperliquid_chain,
        destination: format!("{destination:#x}"),
        amount,
        time: nonce,
    })
    .await?;

    print_transfer(
        inv,
        "withdraw",
        args.amount,
        "USDC".to_string(),
        Some(args.to.clone()),
    );
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
    if to_perp {
        ensure_spot_usdc_balance(api, balance_user, amount).await?;
    } else {
        ensure_usdc_available(api, balance_user, amount).await?;
    }

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
    confirm::unless_yes(args.yes, inv.mode(), "", &question, "transfer aborted")?;

    let amount = amount.normalize();
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
    print_transfer_for(
        inv,
        label,
        amount,
        "USDC".to_string(),
        None,
        subaccount.map(|subaccount| subaccount.to_string()),
    );
    Ok(())
}

/// The `amount` of a `usdClassTransfer`. For a subaccount the exchange reads
/// the target from a ` subaccount:<address>` suffix on the amount, which is
/// part of what the master signs.
fn class_transfer_amount(amount: Decimal, subaccount: Option<Address>) -> String {
    match subaccount {
        Some(subaccount) => format!("{amount} subaccount:{subaccount:#x}"),
        None => amount.to_string(),
    }
}

/// `transfer spot-to-perp` and `transfer perp-to-spot`.
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
            preview["wire_amount"] =
                class_transfer_amount(args.amount.normalize(), Some(subaccount)).into();
        }
        return print_dry_run(inv, kind.would_execute(), preview);
    }
    let signer = inv.resolve_signer()?;
    class_transfer(inv, &signer, args, kind, subaccount).await
}

#[derive(Debug)]
struct ParsedAssetTarget {
    target: AssetTarget,
    display: String,
    wire: String,
}

fn parse_asset_target(input: &str, flag: &str) -> Result<ParsedAssetTarget, CliError> {
    let trimmed = input.trim();
    let (target, display, wire) = match trimmed {
        // Hyperliquid encodes the default perp venue as an empty string in sendAsset.
        "perp" => (AssetTarget::Perp, "perp".to_string(), String::new()),
        "spot" => (AssetTarget::Spot, "spot".to_string(), "spot".to_string()),
        value => match value.strip_prefix("dex:") {
            Some("") => {
                return Err(CliError::Configuration(
                    "asset target dex:<NAME> requires a DEX name".to_string(),
                ));
            }
            Some(dex) => (
                AssetTarget::Dex(dex.to_string()),
                value.to_string(),
                dex.to_string(),
            ),
            None => {
                return Err(CliError::Configuration(format!(
                    "{flag} must be perp, spot, or dex:<NAME>"
                )));
            }
        },
    };
    Ok(ParsedAssetTarget {
        target,
        display,
        wire,
    })
}

async fn spot_usdc_available(api: &ApiClient, user: Address) -> Result<Decimal, CliError> {
    Ok(user_spot_balances_raw(api, user)
        .await?
        .into_iter()
        .find(|balance| balance.coin.eq_ignore_ascii_case("USDC"))
        .map(|balance| balance.total - balance.hold)
        .unwrap_or_default())
}

async fn ensure_spot_usdc_balance(
    api: &ApiClient,
    user: Address,
    amount: Decimal,
) -> Result<(), CliError> {
    let available = spot_usdc_available(api, user).await?;
    if available < amount {
        return Err(CliError::Unsupported(format!(
            "insufficient balance: requested {amount} USDC, available spot USDC {available}"
        )));
    }
    Ok(())
}

/// Refuse a spend of `amount` USDC the account cannot cover. A standard account
/// spends from its perp balance; a pooled one (unified or portfolio margin)
/// keeps every balance in the spot state, where the perp `withdrawable` is not
/// the real balance.
async fn ensure_usdc_available(
    api: &ApiClient,
    user: Address,
    amount: Decimal,
) -> Result<(), CliError> {
    if fetch_abstraction(api, user)
        .await?
        .normalized_mode
        .pools_collateral()
    {
        return ensure_spot_usdc_balance(api, user, amount).await;
    }
    let available = api
        .sdk()
        .clearinghouse_state(user, None)
        .await
        .map_err(from_sdk)?
        .withdrawable;
    if available < amount {
        return Err(CliError::Unsupported(format!(
            "insufficient balance: requested {amount} USDC, available perp withdrawable {available}"
        )));
    }
    Ok(())
}

/// Ask for consent to an external transfer unless `--yes` was passed: the
/// `preview` block goes to stderr before the `question`.
fn confirm_transfer(
    mode: &Mode,
    yes: bool,
    preview: TransferPreview,
    question: &str,
    abort_message: &str,
) -> Result<(), CliError> {
    confirm::unless_yes(yes, mode, &preview.render(), question, abort_message)
}

fn print_transfer(
    inv: &Invocation,
    action: &str,
    amount: Decimal,
    asset: String,
    destination: Option<String>,
) {
    print_transfer_for(inv, action, amount, asset, destination, None);
}

fn print_transfer_for(
    inv: &Invocation,
    action: &str,
    amount: Decimal,
    asset: String,
    destination: Option<String>,
    subaccount: Option<String>,
) {
    inv.print(&Rows::new(vec![TransferConfirmation {
        action: action.to_string(),
        status: "confirmed".to_string(),
        amount,
        asset,
        destination,
        subaccount,
    }]));
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
    .signs(Signs::user_signed::<UsdClassTransfer>())
    .master_only("usdClassTransfer")
    .prompt()
    .args(&[
        arg("amount")
            .kind(InputKind::Amount)
            .describe("USDC amount."),
        arg("subaccount")
            .kind(InputKind::ActingAccountSelector)
            .describe("Move the USDC of this subaccount instead of the signer's own account: subaccount address, wallet name, or wallet id.")
            .resource_id("subaccount selector"),
        arg("yes")
            .kind(InputKind::Boolean)
            .describe("Skip confirmation prompt for deliberate automation."),
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
    .signs(Signs::user_signed::<UsdClassTransfer>())
    .master_only("usdClassTransfer")
    .prompt()
    .args(&[
        arg("amount")
            .kind(InputKind::Amount)
            .describe("USDC amount."),
        arg("subaccount")
            .kind(InputKind::ActingAccountSelector)
            .describe("Move the USDC of this subaccount instead of the signer's own account: subaccount address, wallet name, or wallet id.")
            .resource_id("subaccount selector"),
        arg("yes")
            .kind(InputKind::Boolean)
            .describe("Skip confirmation prompt for deliberate automation."),
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
    .signs(Signs::Digest)
    .master_only("usdSend")
    .prompt()
    .args(&[
        arg("to")
            .kind(InputKind::RawDestinationAddress)
            .describe("Explicit destination address. Wallet aliases are not resolved.")
            .resource_id("destination address"),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("USDC amount."),
        arg("yes")
            .kind(InputKind::Boolean)
            .describe("Skip confirmation prompt for deliberate automation."),
    ]);

    async fn run(inv: &Invocation, args: &SendArgs) -> Result<(), anyhow::Error> {
        let destination = validate_send_args(args)?;
        if inv.dry_run() {
            let kind = TransferActionKind::Send;
            return print_dry_run(
                inv,
                kind.would_execute(),
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
    .signs(Signs::Digest)
    .master_only("spotSend")
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
        arg("yes")
            .kind(InputKind::Boolean)
            .describe("Skip confirmation prompt for deliberate automation."),
    ]);

    async fn run(inv: &Invocation, args: &SpotSendArgs) -> Result<(), anyhow::Error> {
        let destination = validate_spot_send_args(args)?;
        let token = require_spot_token(inv.remote()?.api().sdk(), &args.token).await?;
        if inv.dry_run() {
            let kind = TransferActionKind::SpotSend;
            return print_dry_run(
                inv,
                kind.would_execute(),
                serde_json::json!({
                    "to": destination.to_string(),
                    "token": token.name,
                    "token_index": token.index,
                    "amount": args.amount.normalize().to_string(),
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
        arg("yes")
            .kind(InputKind::Boolean)
            .describe("Skip confirmation prompt for deliberate automation."),
    ]);

    async fn run(inv: &Invocation, args: &SendAssetArgs) -> Result<(), anyhow::Error> {
        let route = validate_send_asset_args(args)?;
        let remote = inv.remote()?;
        let token = require_spot_token(remote.api().sdk(), &args.token).await?;
        if inv.dry_run() {
            let kind = TransferActionKind::SendAsset;
            let mut preview = serde_json::json!({
                "to": route.destination.to_string(),
                "source": route.source.display,
                "source_wire": route.source.wire,
                "dest": route.dest.display,
                "dest_wire": route.dest.wire,
                "token": token.name,
                "token_index": token.index,
                "amount": args.amount.normalize().to_string(),
                "from_subaccount": args.from_subaccount.clone(),
            });
            // An API wallet previews the action it would sign, and the
            // refusal the live run would give.
            if let Some(master) = inv
                .preview_signer()?
                .ok()
                .filter(|signer| signer.is_stored_agent())
                .map(SelectedSigner::query_address)
            {
                ensure_agent_send_target(route.destination, master, args)?;
                preview["action_type"] = "agentSendAsset".into();
            }
            return print_dry_run(inv, kind.would_execute(), preview);
        }
        let resolved = inv.resolve_signer()?;
        let token_name =
            submit_send_asset(inv.mode(), remote, &resolved, args, route, token).await?;
        print_transfer(
            inv,
            "send-asset",
            args.amount,
            token_name,
            Some(args.to.clone()),
        );
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
    .signs(Signs::user_signed::<Withdraw>())
    .master_only("withdraw3")
    .prompt()
    .args(&[
        arg("to")
            .kind(InputKind::RawDestinationAddress)
            .resource_id("destination address"),
        arg("amount")
            .kind(InputKind::Amount)
            .describe("USDC amount."),
        arg("yes")
            .kind(InputKind::Boolean)
            .describe("Skip confirmation prompt for deliberate automation."),
    ]);

    async fn run(inv: &Invocation, args: &WithdrawArgs) -> Result<(), anyhow::Error> {
        let destination = validate_withdraw_args(args)?;
        if inv.dry_run() {
            let kind = TransferActionKind::Withdraw;
            return print_dry_run(
                inv,
                kind.would_execute(),
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
    fn asset_targets_parse_perp_spot_and_dex() {
        let perp = parse_asset_target(" perp ", "--source").unwrap();
        assert!(matches!(perp.target, AssetTarget::Perp));
        assert_eq!(perp.wire, "");

        let dex = parse_asset_target("dex:xyz", "--dest").unwrap();
        assert!(matches!(&dex.target, AssetTarget::Dex(name) if name == "xyz"));
        assert_eq!(
            (dex.display.as_str(), dex.wire.as_str()),
            ("dex:xyz", "xyz")
        );

        for bad in ["", "dex:", "futures"] {
            assert_eq!(
                parse_asset_target(bad, "--dest").unwrap_err().exit_code(),
                2,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_dex_name_keeps_a_repeated_prefix() {
        let dex = parse_asset_target("dex:dex:xyz", "--dest").unwrap();

        assert!(matches!(&dex.target, AssetTarget::Dex(name) if name == "dex:xyz"));
    }

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
    }

    fn send_asset_args(from_subaccount: Option<&str>) -> SendAssetArgs {
        SendAssetArgs {
            to: String::new(),
            source: "spot".to_string(),
            dest: "perp".to_string(),
            token: "USDC".to_string(),
            amount: Decimal::ONE,
            from_subaccount: from_subaccount.map(str::to_string),
            yes: true,
        }
    }

    #[test]
    fn an_api_wallet_may_only_send_an_asset_to_the_account_it_leaves() {
        let master = Address::repeat_byte(1);
        let subaccount = Address::repeat_byte(2);
        let sub = format!("{subaccount:#x}");

        assert!(ensure_agent_send_target(master, master, &send_asset_args(None)).is_ok());
        assert!(ensure_agent_send_target(subaccount, master, &send_asset_args(Some(&sub))).is_ok());
        // The master is not the source once a subaccount is named, and a
        // stranger is never the source.
        for (destination, from) in [(master, Some(sub.as_str())), (subaccount, None)] {
            let err =
                ensure_agent_send_target(destination, master, &send_asset_args(from)).unwrap_err();
            assert_eq!(err.exit_code(), 13);
        }
    }
}
