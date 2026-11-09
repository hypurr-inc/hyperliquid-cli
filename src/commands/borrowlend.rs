use clap::Args;
use hypersdk::hypercore::{HttpClient, SpotToken};
use hypersdk::{Address, Decimal};
use serde::{Deserialize, Serialize};

use crate::assets::require_spot_token;
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, NoArgs, arg};
use crate::commands::actions;
use crate::commands::rows::{ListRow, One, Row, Rows, style_column};
use crate::commands::selector::{OptionalAddressArgs, USER_SELECTOR};
use crate::commands::support::print_dry_run;
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::errors::from_sdk;
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

    /// Human-readable token amount. Omit with --max for maximum withdraw.
    #[arg(long, allow_hyphen_values = true, conflicts_with = "max")]
    pub amount: Option<Decimal>,

    /// Maximum amount. Encodes wei=0 in the verified CoreWriter action shape.
    #[arg(long, conflicts_with = "amount")]
    pub max: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BorrowLendOperation {
    Supply,
    Withdraw,
}

impl BorrowLendOperation {
    fn encoded(self) -> u8 {
        match self {
            Self::Supply => 0,
            Self::Withdraw => 1,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Supply => "supply",
            Self::Withdraw => "withdraw",
        }
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

/// `borrowLendUserState`: the account's borrow/lend positions and the
/// protocol's own verdict on their health. Unlike spot balances it is the
/// account's actual supply and borrow, whatever the account abstraction.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawBorrowLendUserState {
    token_to_state: Vec<(u32, RawTokenState)>,
    /// The protocol's verdict: `healthy` or a state it is not.
    pub(crate) health: String,
    #[serde(default)]
    health_factor: Option<JsonDecimal>,
}

impl RawBorrowLendUserState {
    pub(crate) fn health_factor(&self) -> Option<Decimal> {
        self.health_factor.map(|factor| factor.0)
    }
}

pub(crate) async fn fetch_user_state(
    api: &ApiClient,
    user: Address,
) -> Result<RawBorrowLendUserState, CliError> {
    api.info(
        &InfoRequest::user("borrowLendUserState", user),
        "loading user borrow/lend state",
    )
    .await
}

#[derive(Debug, Deserialize)]
struct RawTokenState {
    borrow: RawLegState,
    supply: RawLegState,
}

#[derive(Debug, Deserialize)]
struct RawLegState {
    basis: JsonDecimal,
    value: JsonDecimal,
}

/// A decimal the API may send as a JSON string or number.
#[derive(Debug, Clone, Copy)]
struct JsonDecimal(Decimal);

impl<'de> Deserialize<'de> for JsonDecimal {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(text) => text,
            serde_json::Value::Number(number) => number.to_string(),
            other => {
                return Err(serde::de::Error::custom(format!(
                    "expected a decimal, found {other}"
                )));
            }
        };
        raw.parse::<Decimal>()
            .or_else(|_| Decimal::from_scientific(&raw))
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BorrowLendExchangeAction {
    #[serde(rename = "type")]
    action_type: &'static str,
    operation: &'static str,
    token: u32,
    amount: Option<String>,
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

async fn rates(inv: &Invocation) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let rows = reserve_rows(remote.api()).await?;
    inv.print(&Rows::new(rows));
    Ok(())
}

async fn get(inv: &Invocation, token: &str) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let token = require_spot_token(remote.api().sdk(), token).await?;
    let row = match live_reserve_row(remote.api(), token.index, token.name.clone()).await {
        Ok(row) => row,
        Err(BorrowLendInfoError::EndpointUnavailable) => unavailable_reserve_row(token),
        Err(BorrowLendInfoError::Cli(err)) => return Err(err.into()),
    };

    inv.print(&Rows::new(vec![row]));
    Ok(())
}

async fn user(inv: &Invocation, user: Address) -> Result<(), anyhow::Error> {
    let api = inv.remote()?.api();
    let state = fetch_user_state(api, user).await?;
    let token_names = if state.token_to_state.is_empty() {
        Default::default()
    } else {
        match token_names_by_index(api.sdk()).await {
            Ok(names) => names,
            Err(BorrowLendInfoError::Cli(err)) => return Err(err.into()),
            Err(BorrowLendInfoError::EndpointUnavailable) => Default::default(),
        }
    };

    inv.print(&UserBorrowLendOutput::new(user, state, &token_names));
    Ok(())
}

/// The dry-run preview of `operation` on `token`, `wei` being its wire amount.
fn action_preview(
    chain: hypersdk::hypercore::Chain,
    operation: BorrowLendOperation,
    args: &ActionArgs,
    token: &SpotToken,
    wei: u64,
) -> serde_json::Value {
    serde_json::json!({
        "operation": operation.as_str(),
        "encoded_operation": operation.encoded(),
        "token": token.name,
        "token_index": token.index,
        "amount": args.amount.map(|amount| amount.to_string()),
        "max": args.max,
        "network": chain.to_string(),
        "wei": wei,
        "reversibility": REVERSIBILITY,
        "verified_shape": {
            "transport": "HyperEVM CoreWriter.sendRawAction",
            "action_id": CORE_WRITER_ACTION_ID,
            "encoding_version": 1,
            "solidity_types": ["uint8", "uint64", "uint64"],
            "fields": {
                "encodedOperation": operation.encoded(),
                "token": token.index,
                "wei": wei
            }
        },
        "exchange_action": {
            "type": "borrowLend",
            "operation": operation.as_str(),
            "token": token.index,
            "amount": borrowlend_exchange_amount(args)
        },
        "live_submission": "enabled: wallet-signed /exchange borrowLend action shape verified against @nktkas/hyperliquid v0.31.0"
    })
}

async fn action(
    inv: &Invocation,
    signer: &SelectedSigner,
    operation: BorrowLendOperation,
    args: &ActionArgs,
    token: SpotToken,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let amount = borrowlend_exchange_amount(args);

    // Verified against @nktkas/hyperliquid v0.31.0 exchange `borrowLend` schema:
    // { type: "borrowLend", operation: "supply"|"withdraw", token, amount: null|string }.
    let action = BorrowLendExchangeAction {
        action_type: "borrowLend",
        operation: operation.as_str(),
        token: token.index,
        amount: amount.clone(),
    };
    actions::send_raw_l1_json_action(
        remote.api(),
        signer,
        &action,
        actions::RawL1ActionMetadata::new(actions::nonce_now()),
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
        amount,
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
    if operation == BorrowLendOperation::Supply && args.max {
        return Err(CliError::Unsupported(
            "borrow/lend supply requires --amount; --max is only supported for withdraw"
                .to_string(),
        ));
    }
    Ok(())
}

async fn reserve_rows(api: &ApiClient) -> Result<Vec<ReserveRow>, CliError> {
    let mut rows = match live_reserve_rows(api).await {
        Ok(rows) => rows,
        Err(BorrowLendInfoError::EndpointUnavailable) => {
            unavailable_reserve_rows(api.sdk()).await?
        }
        Err(BorrowLendInfoError::Cli(err)) => return Err(err),
    };

    rows.sort_by(|left, right| {
        (left.token != "USDC")
            .cmp(&(right.token != "USDC"))
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
    let token_names = token_names_by_index(api.sdk()).await?;

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

async fn unavailable_reserve_rows(client: &HttpClient) -> Result<Vec<ReserveRow>, CliError> {
    Ok(client
        .spot_tokens()
        .await
        .map_err(from_sdk)?
        .into_iter()
        .map(unavailable_reserve_row)
        .collect::<Vec<_>>())
}

fn unavailable_reserve_row(token: hypersdk::hypercore::SpotToken) -> ReserveRow {
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
    client: &HttpClient,
) -> Result<std::collections::HashMap<u32, String>, BorrowLendInfoError> {
    Ok(client
        .spot_tokens()
        .await
        .map_err(from_sdk)
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

fn borrowlend_exchange_amount(args: &ActionArgs) -> Option<String> {
    if args.max {
        None
    } else {
        args.amount.map(|amount| amount.normalize().to_string())
    }
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
            CliError::Unsupported("borrow/lend amount must be greater than zero".to_string())
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
            .filter(|(_, token)| !token.supply.value.0.is_zero() || !token.borrow.value.0.is_zero())
            .map(|(token_index, token)| UserBorrowLendPosition {
                token: token_names
                    .get(&token_index)
                    .cloned()
                    .unwrap_or_else(|| format!("#{token_index}")),
                token_index,
                supplied: token.supply.value.0.normalize(),
                borrowed: token.borrow.value.0.normalize(),
                supply_basis: token.supply.basis.0.normalize(),
                borrow_basis: token.borrow.basis.0.normalize(),
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
    if info_request_type_is_unavailable(reply.status().as_u16(), reply.body()) {
        return Err(BorrowLendInfoError::EndpointUnavailable);
    }
    reply.decode("borrow/lend").map_err(Into::into)
}

fn info_request_type_is_unavailable(status_code: u16, body: &str) -> bool {
    if (200..300).contains(&status_code) {
        return structured_unknown_request_error(body);
    }

    let lower = body.to_ascii_lowercase();
    (status_code == 400 || status_code == 404 || status_code == 422)
        && (lower.contains("unknown")
            || lower.contains("unsupported")
            || lower.contains("invalid request")
            || lower.contains("invalid type"))
        && (lower.contains("borrowlend")
            || lower.contains("borrow/lend")
            || lower.contains("request")
            || lower.contains("type"))
}

fn structured_unknown_request_error(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };

    let status_is_error = value
        .get("status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|status| {
            matches!(
                status.to_ascii_lowercase().as_str(),
                "err" | "error" | "failed" | "failure"
            )
        });

    status_is_error && info_request_type_is_unavailable(400, &value.to_string())
}

pub const GROUP: GroupSpec = GroupSpec::new(
    &["borrowlend"],
    "Supply, withdraw, and query borrow/lend reserves",
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
        rates(inv).await
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
        get(inv, &args.token).await
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
        user(inv, args.resolve(inv)?).await
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
        arg("max").describe("Not supported for supply: the command rejects --max and requires --amount. Only withdraw accepts --max.").schema_string(),
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

async fn borrowlend_action(
    inv: &Invocation,
    operation: BorrowLendOperation,
    args: &ActionArgs,
) -> Result<(), anyhow::Error> {
    validate_action_args(operation, args)?;
    let remote = inv.remote()?;
    let token = require_spot_token(remote.api().sdk(), &args.token).await?;
    let wei = borrowlend_wei(args, token.wei_decimals)?;
    if inv.dry_run() {
        return print_dry_run(
            inv,
            &format!("{}_borrowlend", operation.as_str()),
            action_preview(remote.chain(), operation, args, &token, wei),
        );
    }

    let signer = inv.resolve_signer()?;
    action(inv, &signer, operation, args, token).await
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
}
