use clap::Args;
use hypersdk::{Address, Decimal};
use serde::{Deserialize, Serialize};

use crate::account_mode::{AccountMode, fetch_abstraction};
use crate::assets::{SpotTokenListing, USDC_NAME, load_spot_tokens, require_spot_token};
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, NoArgs, arg};
use crate::commands::common::borrow_lend_state::{RawBorrowLendUserState, fetch_user_state};
use crate::commands::common::rows::{ListRow, One, Row, Rows, style_column};
use crate::commands::common::selector::{OptionalAddressArgs, USER_SELECTOR};
use crate::commands::common::support::print_dry_run;
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::exchange::actions;
use crate::exchange_reply;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::{self, TableData};
use crate::quantity::{self, Scale, ScaleError};
use crate::signing::SelectedSigner;
use crate::wire::InfoRequest;

const LIVE_RESERVE_NOTE: &str = "Live reserve state from Hyperliquid API";
const UNAVAILABLE_RESERVE_NOTE: &str =
    "Reserve state rates unavailable from public Hyperliquid API";
const CORE_WRITER_ACTION_ID: u32 = 15;
const REVERSIBILITY: ActionReversibility = ActionReversibility::PartiallyReversible;
/// Token decimals above this overflow the 64-bit wire amount.
const MAX_TOKEN_DECIMALS: u32 = 19;

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ReserveRow {
    token: String,
    token_index: u32,
    #[serde(with = "rust_decimal::serde::str")]
    supply_rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    borrow_rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    total_supply: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    total_borrow: Decimal,
    note: String,
}

#[derive(Args, Debug, Clone)]
pub struct ActionArgs {
    /// Borrow/lend reserve token, for example USDC
    pub token: String,

    /// Human-readable token amount. Omit with --max for the maximum withdraw or the full repay.
    #[arg(long, allow_hyphen_values = true, conflicts_with = "max")]
    pub amount: Option<Decimal>,

    /// Maximum amount. Encodes wei=0 in the verified CoreWriter action shape.
    #[arg(long, conflicts_with = "amount")]
    pub max: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BorrowLendOperation {
    Supply,
    Withdraw,
    Repay,
    Borrow,
}

impl BorrowLendOperation {
    /// The CoreWriter `encodedOperation`. The docs name only supply (`0`) and
    /// withdraw (`1`); repay and borrow have no documented encoding.
    fn core_writer_encoding(self) -> Option<u8> {
        match self {
            Self::Supply => Some(0),
            Self::Withdraw => Some(1),
            Self::Repay | Self::Borrow => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Supply => "supply",
            Self::Withdraw => "withdraw",
            Self::Repay => "repay",
            Self::Borrow => "borrow",
        }
    }

    /// Whether `--max` (a null amount) is a defined request: the maximum
    /// withdraw and the full repay. Supplying "everything" or borrowing "as
    /// much as possible" is not.
    fn accepts_max(self) -> bool {
        matches!(self, Self::Withdraw | Self::Repay)
    }
}

impl Serialize for BorrowLendOperation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReserveState {
    #[serde(with = "rust_decimal::serde::str")]
    borrow_yearly_rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    supply_yearly_rate: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    total_supplied: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    total_borrowed: Decimal,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct UserBorrowLendPosition {
    token: String,
    token_index: u32,
    #[serde(with = "rust_decimal::serde::str")]
    supplied: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    borrowed: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    supply_basis: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    borrow_basis: Decimal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserBorrowLendOutput {
    user: String,
    positions: Vec<UserBorrowLendPosition>,
    health: String,
    health_factor: Option<Decimal>,
    empty_message: String,
}

#[derive(Debug, Clone, Serialize)]
struct BorrowLendActionRow {
    status: String,
    action: String,
    operation: String,
    acting_as: String,
    token: String,
    token_index: u32,
    amount: Option<String>,
    max: bool,
    network: String,
    signer: String,
    reversibility: String,
}
impl Row for ReserveRow {
    const HEADERS: &'static [&'static str] = &[
        "Token",
        "Supply Rate",
        "Borrow Rate",
        "Total Supply",
        "Total Borrow",
        "Note",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.token.clone(),
            self.supply_rate.to_string(),
            self.borrow_rate.to_string(),
            self.total_supply.to_string(),
            self.total_borrow.to_string(),
            self.note.clone(),
        ]
    }

    fn pretty_cells(&self) -> Vec<String> {
        let mut cells = self.cells();
        style_column::<Self>(&mut cells, "Note", output::colors::gray);
        cells
    }
}

impl ListRow for ReserveRow {
    const EMPTY: &'static str = "No reserve states.";
}

/// Verified against the @nktkas/hyperliquid v0.31.0 exchange schema:
/// `{ type: "borrowLend", operation, token, amount: null|string }`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename = "borrowLend")]
struct BorrowLendAction {
    operation: BorrowLendOperation,
    token: u32,
    /// `null` for `--max`.
    amount: Option<String>,
}

impl BorrowLendAction {
    fn new(operation: BorrowLendOperation, token: u32, args: &ActionArgs) -> Self {
        Self {
            operation,
            token,
            amount: if args.max {
                None
            } else {
                args.amount.map(quantity::wire_text)
            },
        }
    }
}

impl Row for BorrowLendActionRow {
    const HEADERS: &'static [&'static str] = &[
        "Status",
        "Action",
        "Operation",
        "Acting As",
        "Token",
        "Token Index",
        "Amount",
        "Max",
        "Network",
        "Signer",
        "Reversibility",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.status.clone(),
            self.action.clone(),
            self.operation.clone(),
            self.acting_as.clone(),
            self.token.clone(),
            self.token_index.to_string(),
            self.amount.clone().unwrap_or_default(),
            self.max.to_string(),
            self.network.clone(),
            self.signer.clone(),
            self.reversibility.clone(),
        ]
    }
}

impl TableData for UserBorrowLendOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "User",
            "Token",
            "Supplied",
            "Borrowed",
            "Health",
            "Health Factor",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.positions
            .iter()
            .map(|row| {
                vec![
                    self.user.clone(),
                    row.token.clone(),
                    row.supplied.to_string(),
                    row.borrowed.to_string(),
                    self.health.clone(),
                    self.health_factor
                        .map(|factor| factor.to_string())
                        .unwrap_or_default(),
                ]
            })
            .collect()
    }

    fn empty_message(&self) -> Option<&str> {
        Some(&self.empty_message)
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "user": self.user,
            "positions": self.positions,
            "health": self.health,
            "health_factor": self.health_factor.map(|factor| factor.to_string()),
        })
    }
}

fn action_preview(
    chain: hypersdk::hypercore::Chain,
    operation: BorrowLendOperation,
    args: &ActionArgs,
    token: &SpotTokenListing,
    wei: u64,
    exchange_action: &BorrowLendAction,
) -> serde_json::Value {
    let mut preview = serde_json::Map::new();
    preview.insert("operation".into(), operation.as_str().into());
    if let Some(encoding) = operation.core_writer_encoding() {
        preview.insert("encoded_operation".into(), encoding.into());
    }
    preview.insert("token".into(), token.name.clone().into());
    preview.insert("token_index".into(), token.index.into());
    preview.insert(
        "amount".into(),
        args.amount.map(|amount| amount.to_string()).into(),
    );
    preview.insert("max".into(), args.max.into());
    preview.insert("network".into(), chain.to_string().into());
    preview.insert("wei".into(), wei.into());
    preview.insert("reversibility".into(), serde_json::json!(REVERSIBILITY));
    if let Some(encoding) = operation.core_writer_encoding() {
        preview.insert(
            "verified_shape".into(),
            serde_json::json!({
                "transport": "HyperEVM CoreWriter.sendRawAction",
                "action_id": CORE_WRITER_ACTION_ID,
                "encoding_version": 1,
                "solidity_types": ["uint8", "uint64", "uint64"],
                "fields": {
                    "encodedOperation": encoding,
                    "token": token.index,
                    "wei": wei
                }
            }),
        );
    }
    preview.insert("exchange_action".into(), serde_json::json!(exchange_action));
    preview.insert(
        "live_submission".into(),
        "enabled: wallet-signed /exchange borrowLend action shape verified against @nktkas/hyperliquid v0.31.0".into(),
    );
    serde_json::Value::Object(preview)
}

async fn action(
    inv: &Invocation,
    signer: &SelectedSigner,
    operation: BorrowLendOperation,
    args: &ActionArgs,
    token: SpotTokenListing,
    exchange_action: BorrowLendAction,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    actions::send_raw_l1_json_action(
        remote.api(),
        signer,
        &exchange_action,
        None,
        "borrow/lend action failed",
    )
    .await?;

    inv.print(&One::new(BorrowLendActionRow {
        status: "submitted".to_string(),
        action: "borrowlend".to_string(),
        operation: operation.as_str().to_string(),
        acting_as: signer.query_address().to_string(),
        token: token.name,
        token_index: token.index,
        amount: exchange_action.amount,
        max: args.max,
        network: remote.chain().to_string(),
        signer: signer.address().to_string(),
        reversibility: REVERSIBILITY.as_str().to_string(),
    }));
    Ok(())
}

fn validate_action_args(operation: BorrowLendOperation, args: &ActionArgs) -> Result<(), CliError> {
    let has_amount = args.amount.is_some();
    if !has_amount && !args.max {
        return Err(CliError::Unsupported(
            "borrow/lend action requires --amount <AMOUNT> or --max".to_string(),
        ));
    }
    if args.max && !operation.accepts_max() {
        return Err(CliError::Unsupported(format!(
            "borrow/lend {} requires --amount; --max is only supported for withdraw and repay",
            operation.as_str()
        )));
    }
    Ok(())
}

async fn reserve_rows(api: &ApiClient) -> Result<Vec<ReserveRow>, CliError> {
    let mut rows = match live_reserve_rows(api).await {
        Ok(rows) => rows,
        Err(BorrowLendInfoError::EndpointUnavailable) => unavailable_reserve_rows(api).await?,
        Err(BorrowLendInfoError::Cli(err)) => return Err(err),
    };

    rows.sort_by(|left, right| {
        (left.token != USDC_NAME)
            .cmp(&(right.token != USDC_NAME))
            .then_with(|| left.token.cmp(&right.token))
    });

    Ok(rows)
}

async fn live_reserve_rows(api: &ApiClient) -> Result<Vec<ReserveRow>, BorrowLendInfoError> {
    let reserve_states = post_info::<Vec<(u32, RawReserveState)>>(
        api,
        &InfoRequest::new("allBorrowLendReserveStates"),
    )
    .await?;
    let token_names = token_names_by_index(api).await?;

    Ok(reserve_states
        .into_iter()
        .map(|(token_index, reserve)| {
            reserve.to_row(
                token_names
                    .get(&token_index)
                    .cloned()
                    .unwrap_or_else(|| format!("#{token_index}")),
                token_index,
            )
        })
        .collect())
}

async fn live_reserve_row(
    api: &ApiClient,
    token_index: u32,
    token_name: String,
) -> Result<ReserveRow, BorrowLendInfoError> {
    let reserve_state = post_info::<RawReserveState>(
        api,
        &InfoRequest::new("borrowLendReserveState").field("token", token_index),
    )
    .await?;
    Ok(reserve_state.to_row(token_name, token_index))
}

async fn unavailable_reserve_rows(api: &ApiClient) -> Result<Vec<ReserveRow>, CliError> {
    Ok(load_spot_tokens(api)
        .await?
        .into_iter()
        .map(unavailable_reserve_row)
        .collect::<Vec<_>>())
}

fn unavailable_reserve_row(token: SpotTokenListing) -> ReserveRow {
    ReserveRow {
        token: token.name,
        token_index: token.index,
        supply_rate: Decimal::ZERO,
        borrow_rate: Decimal::ZERO,
        total_supply: Decimal::ZERO,
        total_borrow: Decimal::ZERO,
        note: UNAVAILABLE_RESERVE_NOTE.to_string(),
    }
}

impl RawReserveState {
    fn to_row(&self, token: String, token_index: u32) -> ReserveRow {
        ReserveRow {
            token,
            token_index,
            supply_rate: self.supply_yearly_rate,
            borrow_rate: self.borrow_yearly_rate,
            total_supply: self.total_supplied,
            total_borrow: self.total_borrowed,
            note: LIVE_RESERVE_NOTE.to_string(),
        }
    }
}

async fn token_names_by_index(
    api: &ApiClient,
) -> Result<std::collections::HashMap<u32, String>, BorrowLendInfoError> {
    Ok(load_spot_tokens(api)
        .await
        .map_err(BorrowLendInfoError::Cli)?
        .into_iter()
        .map(|token| (token.index, token.name))
        .collect())
}

fn borrowlend_wei(args: &ActionArgs, decimals: i64) -> Result<u64, CliError> {
    if args.max {
        return Ok(0);
    }
    let amount = args.amount.ok_or_else(|| {
        CliError::Configuration("amount is required when --max is not set".to_string())
    })?;
    decimal_to_wei(amount, decimals)
}

fn decimal_to_wei(amount: Decimal, decimals: i64) -> Result<u64, CliError> {
    if decimals < 0 {
        return Err(CliError::Unsupported(
            "token decimals cannot be negative".to_string(),
        ));
    }
    let places = u32::try_from(decimals)
        .ok()
        .filter(|places| *places <= MAX_TOKEN_DECIMALS)
        .ok_or_else(|| {
            CliError::Unsupported("token decimals are outside supported bounds".to_string())
        })?;
    quantity::positive(amount, Scale::places(places)).map_err(|err| match err {
        ScaleError::TooPrecise => CliError::Unsupported(format!(
            "amount {amount} has more precision than token supports ({places} decimals)"
        )),
        ScaleError::TooLarge => {
            CliError::Unsupported("borrow/lend amount is too large".to_string())
        }
        ScaleError::Negative | ScaleError::NotPositive => {
            CliError::Unsupported(quantity::not_positive("borrow/lend amount"))
        }
    })
}

impl UserBorrowLendOutput {
    fn new(
        user: Address,
        state: RawBorrowLendUserState,
        token_names: &std::collections::HashMap<u32, String>,
    ) -> Self {
        let health_factor = state.health_factor();
        let positions = state
            .token_to_state
            .into_iter()
            .filter(|(_, token)| !token.supply.value.is_zero() || !token.borrow.value.is_zero())
            .map(|(token_index, token)| UserBorrowLendPosition {
                token: token_names
                    .get(&token_index)
                    .cloned()
                    .unwrap_or_else(|| format!("#{token_index}")),
                token_index,
                supplied: token.supply.value.normalize(),
                borrowed: token.borrow.value.normalize(),
                supply_basis: token.supply.basis.normalize(),
                borrow_basis: token.borrow.basis.normalize(),
            })
            .collect();
        Self {
            user: user.to_string(),
            positions,
            empty_message: format!("No borrow/lend positions (health: {}).", state.health),
            health: state.health,
            health_factor,
        }
    }
}

#[derive(Debug)]
enum BorrowLendInfoError {
    Cli(CliError),
    EndpointUnavailable,
}

impl From<CliError> for BorrowLendInfoError {
    fn from(err: CliError) -> Self {
        Self::Cli(err)
    }
}

async fn post_info<T: for<'de> Deserialize<'de>>(
    api: &ApiClient,
    request: &InfoRequest,
) -> Result<T, BorrowLendInfoError> {
    let reply = api.info_raw(request).await?;
    if exchange_reply::info_request_unavailable(reply.status().as_u16(), reply.body()) {
        return Err(BorrowLendInfoError::EndpointUnavailable);
    }
    reply.decode("borrow/lend").map_err(Into::into)
}

pub const GROUP: GroupSpec = GroupSpec::new(
    &["borrowlend"],
    "Supply, withdraw, borrow, repay, and query borrow/lend reserves",
);

#[derive(Args, Debug, Clone)]
pub struct BorrowlendGetArgs {
    /// Token symbol (e.g., USDC)
    pub token: String,
}

pub struct BorrowlendRates;

impl CliCommand for BorrowlendRates {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["borrowlend", "rates"],
        "market",
        "Show borrow and lend rates",
        "List borrow/lend rates.",
    );

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        let rows = reserve_rows(inv.remote()?.api()).await?;
        inv.print(&Rows::new(rows));
        Ok(())
    }
}

pub struct BorrowlendGet;

impl CliCommand for BorrowlendGet {
    type Args = BorrowlendGetArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["borrowlend", "get"],
        "market",
        "Show a token reserve",
        "Show a single borrow/lend token reserve.",
    )
    .args(&[arg("token")
        .kind(InputKind::MarketSymbol)
        .describe("Token symbol, for example USDC.")
        .resource_id("token")]);

    async fn run(inv: &Invocation, args: &BorrowlendGetArgs) -> Result<(), anyhow::Error> {
        let remote = inv.remote()?;
        let token = require_spot_token(remote.api(), &args.token).await?;
        let row = match live_reserve_row(remote.api(), token.index, token.name.clone()).await {
            Ok(row) => row,
            Err(BorrowLendInfoError::EndpointUnavailable) => unavailable_reserve_row(token),
            Err(BorrowLendInfoError::Cli(err)) => return Err(err.into()),
        };

        inv.print(&Rows::new(vec![row]));
        Ok(())
    }
}

pub struct BorrowlendUser;

impl CliCommand for BorrowlendUser {
    type Args = OptionalAddressArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["borrowlend", "user"],
        "account",
        "Show borrow and lend state for an address or wallet",
        "Show user borrow/lend state.",
    )
    .args(&[USER_SELECTOR.schema_required()]);

    async fn run(inv: &Invocation, args: &OptionalAddressArgs) -> Result<(), anyhow::Error> {
        let user = args.resolve(inv).await?;
        let api = inv.remote()?.api();
        let state = fetch_user_state(api, user).await?;
        let token_names = if state.token_to_state.is_empty() {
            Default::default()
        } else {
            match token_names_by_index(api).await {
                Ok(names) => names,
                Err(BorrowLendInfoError::Cli(err)) => return Err(err.into()),
                Err(BorrowLendInfoError::EndpointUnavailable) => Default::default(),
            }
        };

        inv.print(&UserBorrowLendOutput::new(user, state, &token_names));
        Ok(())
    }
}

pub struct BorrowlendSupply;

impl CliCommand for BorrowlendSupply {
    type Args = ActionArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["borrowlend", "supply"],
        "market",
        "Supply a token to a borrow/lend reserve",
        "Supply a borrow/lend reserve token with a wallet-signed borrowLend exchange action. Runs live without a confirmation prompt; --dry-run previews the action without submitting.",
    )
    .schema_command("hyperliquid borrowlend supply <TOKEN> --amount <AMOUNT>")
    .args(&[
        arg("token").kind(InputKind::MarketSymbol).resource_id("token"),
        arg("amount").kind(InputKind::Amount).describe("Human-readable token amount to supply.").schema_required(),
        arg("max").describe("Not supported for supply: the command rejects --max and requires --amount. Only withdraw and repay accept --max.").schema_string(),
    ]);

    async fn run(inv: &Invocation, args: &ActionArgs) -> Result<(), anyhow::Error> {
        borrowlend_action(inv, BorrowLendOperation::Supply, args).await
    }
}

pub struct BorrowlendWithdraw;

impl CliCommand for BorrowlendWithdraw {
    type Args = ActionArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["borrowlend", "withdraw"],
        "market",
        "Withdraw a token from a borrow/lend reserve",
        "Withdraw from a borrow/lend reserve with a wallet-signed borrowLend exchange action. Runs live without a confirmation prompt; --dry-run previews the action without submitting.",
    )
    .schema_command("hyperliquid borrowlend withdraw <TOKEN> --amount <AMOUNT>")
    .args(&[
        arg("token").kind(InputKind::MarketSymbol).resource_id("token"),
        arg("amount").kind(InputKind::Amount).describe("Human-readable token amount."),
        arg("max").describe("Withdraw maximum amount; sends amount=null in the exchange borrowLend action.").schema_string(),
    ]);

    async fn run(inv: &Invocation, args: &ActionArgs) -> Result<(), anyhow::Error> {
        borrowlend_action(inv, BorrowLendOperation::Withdraw, args).await
    }
}

pub struct BorrowlendRepay;

impl CliCommand for BorrowlendRepay {
    type Args = ActionArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["borrowlend", "repay"],
        "market",
        "Repay a borrowed token to a borrow/lend reserve",
        "Repay borrowed tokens with a wallet-signed borrowLend exchange action. Runs live without a confirmation prompt; --dry-run previews the action without submitting.",
    )
    .schema_command("hyperliquid borrowlend repay <TOKEN> --amount <AMOUNT>")
    .args(&[
        arg("token").kind(InputKind::MarketSymbol).resource_id("token"),
        arg("amount").kind(InputKind::Amount).describe("Human-readable token amount."),
        arg("max").describe("Repay the full borrow; sends amount=null in the exchange borrowLend action.").schema_string(),
    ]);

    async fn run(inv: &Invocation, args: &ActionArgs) -> Result<(), anyhow::Error> {
        borrowlend_action(inv, BorrowLendOperation::Repay, args).await
    }
}

pub struct BorrowlendBorrow;

impl CliCommand for BorrowlendBorrow {
    type Args = ActionArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["borrowlend", "borrow"],
        "market",
        "Borrow a token against supplied collateral",
        "Borrow USDC or USDT against supplied HYPE or BTC collateral with a wallet-signed borrowLend exchange action. Refused for portfolio-margin accounts, which borrow automatically. Runs live without a confirmation prompt; --dry-run previews the action without submitting.",
    )
    .schema_command("hyperliquid borrowlend borrow <TOKEN> --amount <AMOUNT>")
    .args(&[
        arg("token").kind(InputKind::MarketSymbol).resource_id("token"),
        arg("amount").kind(InputKind::Amount).describe("Human-readable token amount to borrow.").schema_required(),
        arg("max").describe("Not supported for borrow: the command rejects --max and requires --amount.").schema_string(),
    ]);

    async fn run(inv: &Invocation, args: &ActionArgs) -> Result<(), anyhow::Error> {
        borrowlend_action(inv, BorrowLendOperation::Borrow, args).await
    }
}

/// Manual borrowing is available to standard and unified accounts only: under
/// portfolio margin borrowing is automatic and the manual action is disabled
/// (<https://hyperliquid.gitbook.io/hyperliquid-docs/support/faq/manual-borrows>).
async fn ensure_manual_borrow_allowed(api: &ApiClient, account: Address) -> Result<(), CliError> {
    let mode = fetch_abstraction(api, account).await?.normalized_mode;
    if mode == AccountMode::PortfolioMargin {
        return Err(CliError::Unsupported(format!(
            "{account} is a portfolio-margin account: borrowing is automatic there and the manual borrow action is disabled; switch the account to standard or unified-account to borrow manually"
        )));
    }
    Ok(())
}

async fn borrowlend_action(
    inv: &Invocation,
    operation: BorrowLendOperation,
    args: &ActionArgs,
) -> Result<(), anyhow::Error> {
    validate_action_args(operation, args)?;
    let remote = inv.remote()?;
    let token = require_spot_token(remote.api(), &args.token).await?;
    let wei = borrowlend_wei(args, token.wei_decimals)?;
    let exchange_action = BorrowLendAction::new(operation, token.index, args);
    if inv.dry_run() {
        if operation == BorrowLendOperation::Borrow
            && let Some(signer) = inv.dry_run_signer()?
        {
            ensure_manual_borrow_allowed(remote.api(), signer.query_address()).await?;
        }
        return print_dry_run(
            inv,
            &format!("{}_borrowlend", operation.as_str()),
            action_preview(
                remote.chain(),
                operation,
                args,
                &token,
                wei,
                &exchange_action,
            ),
        );
    }

    let signer = inv.resolve_signer()?;
    if operation == BorrowLendOperation::Borrow {
        ensure_manual_borrow_allowed(remote.api(), signer.query_address()).await?;
    }
    action(inv, &signer, operation, args, token, exchange_action).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_to_wei_respects_token_decimals() {
        assert_eq!(decimal_to_wei(Decimal::new(123, 1), 6).unwrap(), 12_300_000);
        assert!(decimal_to_wei(Decimal::new(1, 7), 6).is_err());
    }

    #[test]
    fn decimal_to_wei_rejects_unsupported_token_decimals() {
        assert!(decimal_to_wei(Decimal::ONE, -1).is_err());
        assert!(decimal_to_wei(Decimal::ONE, 20).is_err());
        assert_eq!(
            decimal_to_wei(Decimal::ONE, 19).unwrap(),
            10_000_000_000_000_000_000
        );
    }

    #[test]
    fn action_args_require_amount_or_max() {
        let err = validate_action_args(
            BorrowLendOperation::Withdraw,
            &ActionArgs {
                token: "USDC".to_string(),
                amount: None,
                max: false,
            },
        )
        .unwrap_err();

        assert_eq!(err.exit_code(), 13);
    }

    #[test]
    fn supply_rejects_max_sentinel() {
        let err = validate_action_args(
            BorrowLendOperation::Supply,
            &ActionArgs {
                token: "USDC".to_string(),
                amount: None,
                max: true,
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("--max is only supported"));
    }

    #[test]
    fn only_withdraw_and_repay_accept_max() {
        let max = ActionArgs {
            token: "USDC".to_string(),
            amount: None,
            max: true,
        };

        for (operation, accepted) in [
            (BorrowLendOperation::Supply, false),
            (BorrowLendOperation::Withdraw, true),
            (BorrowLendOperation::Repay, true),
            (BorrowLendOperation::Borrow, false),
        ] {
            assert_eq!(
                validate_action_args(operation, &max).is_ok(),
                accepted,
                "{}",
                operation.as_str()
            );
        }
    }

    #[test]
    fn core_writer_names_only_supply_and_withdraw() {
        assert_eq!(BorrowLendOperation::Supply.core_writer_encoding(), Some(0));
        assert_eq!(
            BorrowLendOperation::Withdraw.core_writer_encoding(),
            Some(1)
        );
        assert_eq!(BorrowLendOperation::Repay.core_writer_encoding(), None);
        assert_eq!(BorrowLendOperation::Borrow.core_writer_encoding(), None);
    }

    fn user_state(raw: &str) -> RawBorrowLendUserState {
        serde_json::from_str(raw).unwrap()
    }

    fn names() -> std::collections::HashMap<u32, String> {
        [(0, "USDC".to_string()), (150, "HYPE".to_string())].into()
    }

    #[test]
    fn positions_come_from_the_api_state_and_skip_empty_tokens() {
        let state = user_state(
            r#"{"tokenToState":[
                [0,{"borrow":{"basis":"0.0","value":"0.0"},"supply":{"basis":"5000.5","value":"5074.42"}}],
                [150,{"borrow":{"basis":"3.0","value":"3.75"},"supply":{"basis":"0.0","value":"0.0"}}],
                [7,{"borrow":{"basis":"0.0","value":"0.0"},"supply":{"basis":"0.0","value":"0.0"}}],
                [9,{"borrow":{"basis":"0.0","value":"0.0"},"supply":{"basis":"1","value":2}}]
            ],"health":"healthy","healthFactor":1.5}"#,
        );

        let output = UserBorrowLendOutput::new(Address::ZERO, state, &names());

        let rows: Vec<_> = output
            .positions
            .iter()
            .map(|row| (row.token.as_str(), row.supplied, row.borrowed))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("USDC", Decimal::new(507_442, 2), Decimal::ZERO),
                ("HYPE", Decimal::ZERO, Decimal::new(375, 2)),
                ("#9", Decimal::from(2), Decimal::ZERO),
            ]
        );
        assert_eq!(output.positions[0].supply_basis, Decimal::new(50_005, 1));
        assert_eq!(output.health, "healthy");
        assert_eq!(output.health_factor, Some(Decimal::new(15, 1)));
    }

    #[test]
    fn a_null_health_factor_and_an_empty_state_decode() {
        let output = UserBorrowLendOutput::new(
            Address::ZERO,
            user_state(r#"{"tokenToState":[],"health":"healthy","healthFactor":null}"#),
            &names(),
        );

        assert!(output.positions.is_empty());
        assert_eq!(output.health_factor, None);
        assert_eq!(
            output.to_json_value()["health_factor"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn the_exchange_action_keeps_its_wire_shape() {
        let action = |amount: Option<&str>, max: bool| {
            BorrowLendAction::new(
                BorrowLendOperation::Repay,
                0,
                &ActionArgs {
                    token: "USDC".to_string(),
                    amount: amount.map(|amount| amount.parse().unwrap()),
                    max,
                },
            )
        };

        let fixed = action(Some("5.00"), false);
        assert_eq!(
            serde_json::to_value(&fixed).unwrap(),
            serde_json::json!({"type": "borrowLend", "operation": "repay", "token": 0, "amount": "5"})
        );
        // The L1 action hash covers the msgpack of the action.
        assert_eq!(
            rmp_serde::to_vec_named(&fixed).unwrap(),
            b"\x84\xa4type\xaaborrowLend\xa9operation\xa5repay\xa5token\x00\xa6amount\xa15"
        );
        assert_eq!(
            serde_json::to_value(action(None, true)).unwrap()["amount"],
            serde_json::Value::Null
        );
    }
}
