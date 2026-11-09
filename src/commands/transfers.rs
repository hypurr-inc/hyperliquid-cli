//! Authenticated transfer commands.

use alloy::sol;
use clap::Args;
use hypersdk::hypercore::types::{Action, AssetTarget, SendAsset, SendToken, SpotSend, UsdSend};
use hypersdk::hypercore::{Chain, SpotToken};

use crate::assets::find_spot_token;
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, arg};
use crate::commands::actions;
use crate::commands::spot_balances::user_spot_balances_raw;
use crate::commands::support::{
    default_dry_run_signing_context, print_transfer_dry_run, print_transfer_dry_run_as,
};
use crate::commands::user_signed::{self, UserSigned};
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::http_api::ApiClient;
use crate::invocation::{Invocation, Remote};
use crate::mode::Mode;
use crate::output::TableData;
use crate::quantity;
use crate::signing::{SelectedSigner, Signs};
use crate::wire::config_address;
use hypersdk::hypercore::HttpClient;
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
}

struct TransferOutput {
    rows: Vec<TransferConfirmation>,
}

impl TableData for TransferOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Action", "Status", "Amount", "Asset", "Destination"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    row.action.clone(),
                    row.status.clone(),
                    row.amount.to_string(),
                    row.asset.clone(),
                    row.destination.clone().unwrap_or_default(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
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

    fn command(self) -> &'static str {
        match self {
            Self::SpotToPerp => "transfer spot-to-perp",
            Self::PerpToSpot => "transfer perp-to-spot",
            Self::Send => "transfer send",
            Self::SpotSend => "transfer spot-send",
            Self::SendAsset => "transfer send-asset",
            Self::Withdraw => "transfer withdraw",
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

struct TransferConfirmationContext {
    network: Chain,
    signer: Address,
    acting_context: Address,
    recipient: Option<Address>,
    destination_class: &'static str,
    asset: String,
    amount: Decimal,
    fee_or_cap: &'static str,
    reversibility: ActionReversibility,
}

/// The USDC amount and `--to` address of `transfer send`.
fn validate_send_args(args: &SendArgs) -> Result<Address, CliError> {
    quantity::require_positive(args.amount, "amount")?;
    parse_address(&args.to, "--to")
}

/// The `--to` address of `transfer spot-send`.
fn validate_spot_send_args(args: &SpotSendArgs) -> Result<Address, CliError> {
    quantity::require_positive(args.amount, "amount")?;
    let destination = parse_address(&args.to, "--to")?;
    validate_token_symbol(&args.token)?;
    Ok(destination)
}

/// The parsed `--to`, `--source` and `--dest` of `transfer send-asset`.
struct SendAssetRoute {
    destination: Address,
    source: ParsedAssetTarget,
    dest: ParsedAssetTarget,
}

fn validate_send_asset_args(args: &SendAssetArgs) -> Result<SendAssetRoute, CliError> {
    quantity::require_positive(args.amount, "amount")?;
    let destination = parse_address(&args.to, "--to")?;
    validate_token_symbol(&args.token)?;
    let source = parse_asset_target(&args.source, "--source")?;
    let dest = parse_asset_target(&args.dest, "--dest")?;
    if let Some(from_subaccount) = args.from_subaccount.as_deref() {
        parse_address(from_subaccount, "--from-subaccount")?;
    }
    Ok(SendAssetRoute {
        destination,
        source,
        dest,
    })
}

/// The `--to` address of `transfer withdraw`.
fn validate_withdraw_args(args: &WithdrawArgs) -> Result<Address, CliError> {
    quantity::require_positive(args.amount, "amount")?;
    parse_address(&args.to, "--to")
}

/// Send USDC from perp balance to another Hyperliquid address.
async fn send(
    inv: &Invocation,
    signer: &SelectedSigner,
    balance_user: Address,
    destination: Address,
    args: &SendArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let (api, chain, client) = (remote.api(), remote.chain(), remote.api().sdk());
    ensure_perp_balance(client, balance_user, args.amount).await?;
    let prompt = external_transfer_prompt(
        &format!("Send {} USDC to {}?", args.amount, args.to),
        TransferConfirmationContext {
            network: chain,
            signer: signer.address(),
            acting_context: balance_user,
            recipient: Some(destination),
            destination_class: "hyperliquid_user_address",
            asset: "USDC".to_string(),
            amount: args.amount,
            fee_or_cap: "not_estimated_exchange_default",
            reversibility: TransferActionKind::Send.reversibility(),
        },
    );
    confirm_transfer(inv.mode(), &prompt, "send transfer aborted", args.yes)?;

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

/// Send a spot token to another Hyperliquid address.
async fn spot_send(
    inv: &Invocation,
    signer: &SelectedSigner,
    destination: Address,
    token: SpotToken,
    args: &SpotSendArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let (api, chain) = (remote.api(), remote.chain());
    let prompt = external_transfer_prompt(
        &format!(
            "Send {} {} spot token to {}?",
            args.amount, token.name, args.to
        ),
        TransferConfirmationContext {
            network: chain,
            signer: signer.address(),
            acting_context: signer.query_address(),
            recipient: Some(destination),
            destination_class: "hyperliquid_spot_user_address",
            asset: token.name.clone(),
            amount: args.amount,
            fee_or_cap: "not_estimated_exchange_default",
            reversibility: TransferActionKind::SpotSend.reversibility(),
        },
    );
    confirm_transfer(inv.mode(), &prompt, "spot send transfer aborted", args.yes)?;

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
    let token = resolve_spot_token(remote.api().sdk(), &args.token).await?;
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
    let prompt = external_transfer_prompt(
        &format!(
            "Send {} {} from {} to {} for {}?",
            args.amount, token.name, source.display, dest.display, args.to
        ),
        TransferConfirmationContext {
            network: chain,
            signer: signer.address(),
            acting_context: signer.query_address(),
            recipient: Some(destination),
            destination_class: "hyperliquid_asset_context",
            asset: format!("{} {}->{}", token.name, source.display, dest.display),
            amount: args.amount,
            fee_or_cap: "not_estimated_exchange_default",
            reversibility: TransferActionKind::SendAsset.reversibility(),
        },
    );
    confirm_transfer(mode, &prompt, "send-asset transfer aborted", args.yes)?;

    let nonce = actions::nonce_now();
    let send_asset = SendAsset {
        destination,
        source_dex: source.target,
        destination_dex: dest.target,
        token: SendToken(token.clone()),
        amount: args.amount,
        from_sub_account: args.from_subaccount.clone().unwrap_or_default(),
        nonce,
    }
    .into_action(chain);
    let action = Action::SendAsset(send_asset);
    actions::send_l1_action(api, signer, action, nonce).await?;
    Ok(token.name)
}

/// Withdraw USDC to an Arbitrum address.
async fn withdraw(
    inv: &Invocation,
    signer: &SelectedSigner,
    balance_user: Address,
    destination: Address,
    args: &WithdrawArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let (api, chain, client) = (remote.api(), remote.chain(), remote.api().sdk());
    ensure_perp_balance(client, balance_user, args.amount).await?;
    let prompt = external_transfer_prompt(
        &format!(
            "Withdraw {} USDC to Arbitrum address {}?",
            args.amount, args.to
        ),
        TransferConfirmationContext {
            network: chain,
            signer: signer.address(),
            acting_context: balance_user,
            recipient: Some(destination),
            destination_class: "arbitrum_address",
            asset: "USDC".to_string(),
            amount: args.amount,
            fee_or_cap: "not_estimated_bridge_or_exchange_fee",
            reversibility: TransferActionKind::Withdraw.reversibility(),
        },
    );
    confirm_transfer(inv.mode(), &prompt, "withdraw transfer aborted", args.yes)?;

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

async fn resolve_spot_token(client: &HttpClient, token: &str) -> Result<SpotToken, CliError> {
    validate_token_symbol(token)?;
    let token = token.trim();
    find_spot_token(client, token)
        .await?
        .ok_or_else(|| CliError::Unsupported(format!("spot token \"{token}\" not found")))
}

fn parse_address(input: &str, flag: &str) -> Result<Address, CliError> {
    config_address(input, Some(flag), true)
}

/// Move USDC between the spot and perp balances: spot to perp for
/// [`TransferActionKind::SpotToPerp`], perp to spot otherwise.
async fn class_transfer(
    inv: &Invocation,
    signer: &SelectedSigner,
    balance_user: Address,
    args: &ClassTransferArgs,
    kind: TransferActionKind,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let (api, client) = (remote.api(), remote.api().sdk());
    let to_perp = kind == TransferActionKind::SpotToPerp;
    let amount = args.amount;
    if to_perp {
        ensure_spot_usdc_balance(api, balance_user, amount).await?;
    } else {
        ensure_perp_balance(client, balance_user, amount).await?;
    }

    let prompt = if to_perp {
        format!("Transfer {amount} USDC from spot to perp?")
    } else {
        format!("Transfer {amount} USDC from perp to spot?")
    };
    confirm_transfer(inv.mode(), &prompt, "transfer aborted", args.yes)?;

    let amount = amount.normalize();
    user_signed::submit(api, signer, |hyperliquid_chain, nonce| UsdClassTransfer {
        hyperliquidChain: hyperliquid_chain,
        amount: amount.to_string(),
        toPerp: to_perp,
        nonce,
    })
    .await?;

    let label = if to_perp {
        "spot-to-perp"
    } else {
        "perp-to-spot"
    };
    print_transfer(inv, label, amount, "USDC".to_string(), None);
    Ok(())
}

/// `transfer spot-to-perp` and `transfer perp-to-spot`.
async fn run_class_transfer(
    inv: &Invocation,
    args: &ClassTransferArgs,
    kind: TransferActionKind,
) -> Result<(), anyhow::Error> {
    quantity::require_positive(args.amount, "amount")?;
    if inv.dry_run() {
        return print_transfer_dry_run(
            inv,
            kind.command(),
            kind,
            kind.would_execute(),
            serde_json::json!({"amount": args.amount.to_string()}),
        );
    }
    let signer = inv.resolve_signer()?;
    class_transfer(inv, &signer, signer.query_address(), args, kind).await
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
        "" => {
            return Err(CliError::Configuration(format!(
                "{flag} must be perp, spot, or dex:<NAME>"
            )));
        }
        // Hyperliquid encodes the default perp venue as an empty string in sendAsset.
        "perp" => (AssetTarget::Perp, "perp".to_string(), String::new()),
        "spot" => (AssetTarget::Spot, "spot".to_string(), "spot".to_string()),
        value if value.starts_with("dex:") => {
            let dex = value.trim_start_matches("dex:");
            if dex.is_empty() {
                return Err(CliError::Configuration(
                    "asset target dex:<NAME> requires a DEX name".to_string(),
                ));
            }
            (
                AssetTarget::Dex(dex.to_string()),
                value.to_string(),
                dex.to_string(),
            )
        }
        _ => {
            return Err(CliError::Configuration(format!(
                "{flag} must be perp, spot, or dex:<NAME>"
            )));
        }
    };
    Ok(ParsedAssetTarget {
        target,
        display,
        wire,
    })
}

async fn ensure_spot_usdc_balance(
    api: &ApiClient,
    user: Address,
    amount: Decimal,
) -> Result<(), CliError> {
    let available = user_spot_balances_raw(api, user)
        .await?
        .into_iter()
        .find(|balance| balance.coin.eq_ignore_ascii_case("USDC"))
        .map(|balance| balance.total - balance.hold)
        .unwrap_or_default();
    if available < amount {
        return Err(CliError::Unsupported(format!(
            "insufficient balance: requested {amount} USDC, available spot USDC {available}"
        )));
    }
    Ok(())
}

async fn ensure_perp_balance(
    client: &HttpClient,
    user: Address,
    amount: Decimal,
) -> Result<(), CliError> {
    let available = client
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

/// Ask for consent. `prompt` is an optional preview block followed by the
/// question on its last line. The block goes to stderr before the question.
fn confirm_transfer(
    mode: &Mode,
    prompt: &str,
    abort_message: &'static str,
    yes: bool,
) -> Result<(), CliError> {
    if yes {
        return Ok(());
    }
    let (preview, question) = prompt.rsplit_once('\n').unwrap_or(("", prompt));
    crate::prompt::consent(mode, "--yes", preview, question, abort_message)
}

fn external_transfer_prompt(summary: &str, context: TransferConfirmationContext) -> String {
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
Reversibility: {reversibility}
{summary}",
        network = context.network,
        signer = context.signer,
        acting_context = context.acting_context,
        recipient = context
            .recipient
            .map(|recipient| recipient.to_string())
            .unwrap_or_else(|| "n/a".to_string()),
        destination_class = context.destination_class,
        asset = context.asset,
        amount = context.amount,
        fee_or_cap = context.fee_or_cap,
        reversibility = (context.reversibility).as_str(),
        summary = summary,
    )
}

fn print_transfer(
    inv: &Invocation,
    action: &str,
    amount: Decimal,
    asset: String,
    destination: Option<String>,
) {
    inv.print(&TransferOutput {
        rows: vec![TransferConfirmation {
            action: action.to_string(),
            status: "confirmed".to_string(),
            amount,
            asset,
            destination,
        }],
    });
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
    .prompt()
    .args(&[
        arg("amount")
            .kind(InputKind::Amount)
            .describe("USDC amount."),
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
    .prompt()
    .args(&[
        arg("amount")
            .kind(InputKind::Amount)
            .describe("USDC amount."),
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
            return print_transfer_dry_run(
                inv,
                kind.command(),
                kind,
                kind.would_execute(),
                serde_json::json!({"to": args.to, "amount": args.amount.to_string()}),
            );
        }
        let resolved = inv.resolve_signer()?;
        send(inv, &resolved, resolved.query_address(), destination, args).await
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
        let token = resolve_spot_token(inv.remote()?.api().sdk(), &args.token).await?;
        if inv.dry_run() {
            let kind = TransferActionKind::SpotSend;
            return print_transfer_dry_run_as(
                inv,
                kind.command(),
                kind,
                kind.would_execute(),
                serde_json::json!({
                    "to": destination.to_string(),
                    "token": token.name,
                    "token_index": token.index,
                    "amount": args.amount.normalize().to_string(),
                }),
                Some(default_dry_run_signing_context(inv)),
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
        "Send an asset between accounts, spot, perp, or DEX contexts.",
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
        let token = resolve_spot_token(remote.api().sdk(), &args.token).await?;
        if inv.dry_run() {
            let kind = TransferActionKind::SendAsset;
            return print_transfer_dry_run_as(
                inv,
                kind.command(),
                kind,
                kind.would_execute(),
                serde_json::json!({
                    "to": route.destination.to_string(),
                    "source": route.source.display,
                    "source_wire": route.source.wire,
                    "dest": route.dest.display,
                    "dest_wire": route.dest.wire,
                    "token": token.name,
                    "token_index": token.index,
                    "amount": args.amount.normalize().to_string(),
                    "from_subaccount": args.from_subaccount.clone(),
                }),
                Some(default_dry_run_signing_context(inv)),
            );
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
            return print_transfer_dry_run(
                inv,
                kind.command(),
                kind,
                kind.would_execute(),
                serde_json::json!({"to": args.to, "amount": args.amount.to_string()}),
            );
        }
        let resolved = inv.resolve_signer()?;
        withdraw(inv, &resolved, resolved.query_address(), destination, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_address_accepts_0x_40_byte_hex() {
        assert!(parse_address("0x0000000000000000000000000000000000000001", "--to").is_ok());
    }

    #[test]
    fn parse_address_rejects_zero_address_as_usage_error() {
        let err = parse_address("0x0000000000000000000000000000000000000000", "--to").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("zero address"));
    }

    #[test]
    fn parse_address_rejects_bad_format_as_usage_error() {
        let err = parse_address("INVALID", "--to").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("0x-prefixed"));
    }

    #[test]
    fn transfer_actions_declare_reversibility() {
        assert_eq!(
            TransferActionKind::SpotToPerp.reversibility(),
            ActionReversibility::Reversible
        );
        assert_eq!(
            TransferActionKind::PerpToSpot.reversibility(),
            ActionReversibility::Reversible
        );
        assert_eq!(
            TransferActionKind::Send.reversibility(),
            ActionReversibility::Irreversible
        );
        assert_eq!(
            TransferActionKind::SpotSend.reversibility(),
            ActionReversibility::Irreversible
        );
        assert_eq!(
            TransferActionKind::SendAsset.reversibility(),
            ActionReversibility::Irreversible
        );
        assert_eq!(
            TransferActionKind::Withdraw.reversibility(),
            ActionReversibility::Irreversible
        );
    }
}
