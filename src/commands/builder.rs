//! Builder fee approval and status commands.
//!
//! Commands:
//! - `hyperliquid builder max-fee --user <ADDRESS> --builder <ADDRESS>`
//! - `hyperliquid builder approved --user <ADDRESS>`
//! - `hyperliquid builder approve --builder <ADDRESS> --max-fee-rate <PERCENT>`

use alloy::sol;
use clap::Args;
use futures::future::try_join_all;
use hypersdk::Address;
use hypersdk::Decimal;
use hypersdk::hypercore::Chain;
use serde::Serialize;

use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, InputKind, arg};
use crate::commands::support::print_signed_dry_run;
use crate::commands::user_signed::{self, UserSigned};
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::mode::Mode;
use crate::output::TableData;
use crate::quantity::{self, Scale, ScaleError};
use crate::resolvers::{HexAddressError, parse_exact_hex_address};
use crate::settings::{BuilderDefaultSetting, Settings};
use crate::signing::{SelectedSigner, Signs};
use crate::wire::InfoRequest;

const APPROVE_REVERSIBILITY: ActionReversibility = ActionReversibility::Reversible;

sol! {
    #[derive(Serialize)]
    struct ApproveBuilderFee {
        string hyperliquidChain;
        string maxFeeRate;
        address builder;
        uint64 nonce;
    }
}

impl UserSigned for ApproveBuilderFee {
    const ACTION_TYPE: &'static str = "approveBuilderFee";
}

#[derive(Args, Debug, Clone)]
pub struct MaxFeeArgs {
    /// User address, stored account alias, or stored account id
    #[arg(long)]
    pub user: String,

    /// Builder address
    #[arg(long)]
    pub builder: String,
}

#[derive(Args, Debug, Clone)]
pub struct ApprovedArgs {
    /// User address, stored account alias, or stored account id
    #[arg(long)]
    pub user: String,
}

#[derive(Args, Debug, Clone)]
pub struct ApproveArgs {
    /// Builder address
    #[arg(long)]
    pub builder: String,

    /// Maximum builder fee rate as a percent string, e.g. 0.001%
    #[arg(long)]
    pub max_fee_rate: String,

    /// Confirm live execution without prompting
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BuilderFeeRow {
    user: String,
    builder: String,
    max_fee_tenths_bps: u64,
    max_fee_rate: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderMaxFeeOutput {
    row: BuilderFeeRow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderApprovedOutput {
    rows: Vec<BuilderFeeRow>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BuilderApproveRow {
    status: String,
    action: String,
    signer: String,
    query_address: String,
    builder: String,
    max_fee_rate: String,
    max_fee_tenths_bps: u64,
    network: String,
    reversibility: String,
}

struct BuilderApproveOutput {
    row: BuilderApproveRow,
}

const BUILDER_FEE_HEADERS: &[&str] = &["User", "Builder", "Max Fee", "Tenths Bps"];

fn builder_fee_cells(row: &BuilderFeeRow) -> Vec<String> {
    vec![
        row.user.clone(),
        row.builder.clone(),
        row.max_fee_rate.clone(),
        row.max_fee_tenths_bps.to_string(),
    ]
}

impl TableData for BuilderMaxFeeOutput {
    fn headers(&self) -> Vec<&str> {
        BUILDER_FEE_HEADERS.to_vec()
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![builder_fee_cells(&self.row)]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.row).unwrap_or_else(|_| serde_json::json!({}))
    }
}

impl TableData for BuilderApprovedOutput {
    fn headers(&self) -> Vec<&str> {
        BUILDER_FEE_HEADERS.to_vec()
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows.iter().map(builder_fee_cells).collect()
    }

    fn empty_message(&self) -> Option<&str> {
        Some("No approved builders.")
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

impl TableData for BuilderApproveOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Status",
            "Action",
            "Signer",
            "Query Address",
            "Builder",
            "Max Fee",
            "Tenths Bps",
            "Network",
            "Reversibility",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.row.status.clone(),
            self.row.action.clone(),
            self.row.signer.clone(),
            self.row.query_address.clone(),
            self.row.builder.clone(),
            self.row.max_fee_rate.clone(),
            self.row.max_fee_tenths_bps.to_string(),
            self.row.network.clone(),
            self.row.reversibility.clone(),
        ]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.row).unwrap_or_else(|_| serde_json::json!({}))
    }
}

async fn max_fee(inv: &Invocation, user: Address, builder: Address) -> Result<(), anyhow::Error> {
    let max_fee_tenths_bps = load_max_fee(inv.remote()?.api(), user, builder).await?;
    inv.print(&BuilderMaxFeeOutput {
        row: BuilderFeeRow {
            user: user.to_string(),
            builder: builder.to_string(),
            max_fee_tenths_bps,
            max_fee_rate: percent_from_tenths_bps(max_fee_tenths_bps),
        },
    });
    Ok(())
}

async fn approved(inv: &Invocation, user: Address) -> Result<(), anyhow::Error> {
    let api = inv.remote()?.api();
    let builders = api
        .info::<Vec<Address>>(
            &InfoRequest::user("approvedBuilders", user),
            "loading approved builders",
        )
        .await?;

    let rows = try_join_all(builders.into_iter().map(|builder| async move {
        let max_fee_tenths_bps = load_max_fee(api, user, builder).await?;
        Ok::<_, CliError>(BuilderFeeRow {
            user: user.to_string(),
            builder: builder.to_string(),
            max_fee_tenths_bps,
            max_fee_rate: percent_from_tenths_bps(max_fee_tenths_bps),
        })
    }))
    .await?;

    inv.print(&BuilderApprovedOutput { rows });
    Ok(())
}

fn approve_dry_run_value(
    chain: Chain,
    signer: Option<Address>,
    query_address: Option<Address>,
    builder: Address,
    args: &ApproveArgs,
) -> Result<serde_json::Value, CliError> {
    let max_fee_tenths_bps = validate_max_fee_rate(&args.max_fee_rate)?;
    Ok(serde_json::json!({
        "network": chain.to_string(),
        "signer": signer.map(|address| address.to_string()),
        "query_address": query_address.map(|address| address.to_string()),
        "builder": builder.to_string(),
        "max_fee_rate": args.max_fee_rate,
        "max_fee_tenths_bps": max_fee_tenths_bps,
        "reversibility": APPROVE_REVERSIBILITY,
        "action": {
            "type": ApproveBuilderFee::ACTION_TYPE,
            "hyperliquidChain": chain.to_string(),
            "signatureChainId": chain.arbitrum_id().to_string(),
            "builder": builder.to_string(),
            "maxFeeRate": args.max_fee_rate,
        }
    }))
}

async fn load_max_fee(api: &ApiClient, user: Address, builder: Address) -> Result<u64, CliError> {
    api.info::<u64>(
        &InfoRequest::user("maxBuilderFee", user).address("builder", builder),
        "loading max builder fee",
    )
    .await
}

async fn approve(
    inv: &Invocation,
    signer: &SelectedSigner,
    builder: Address,
    args: &ApproveArgs,
) -> Result<(), anyhow::Error> {
    let max_fee_tenths_bps = validate_max_fee_rate(&args.max_fee_rate)?;
    if !args.yes {
        confirm_builder_approval(inv.mode(), builder, &args.max_fee_rate, max_fee_tenths_bps)?;
    }

    let remote = inv.remote()?;
    submit_approval(remote.api(), signer, builder, &args.max_fee_rate).await?;

    inv.print(&BuilderApproveOutput {
        row: approve_output_row(
            remote.chain(),
            signer,
            builder,
            &args.max_fee_rate,
            max_fee_tenths_bps,
        ),
    });
    Ok(())
}

pub async fn submit_approval(
    api: &ApiClient,
    signer: &SelectedSigner,
    builder: Address,
    max_fee_rate: &str,
) -> Result<u64, anyhow::Error> {
    let max_fee_tenths_bps = validate_max_fee_rate(max_fee_rate)?;
    user_signed::submit(api, signer, |hyperliquid_chain, nonce| ApproveBuilderFee {
        hyperliquidChain: hyperliquid_chain,
        maxFeeRate: max_fee_rate.to_string(),
        builder,
        nonce,
    })
    .await?;
    Ok(max_fee_tenths_bps)
}

fn approve_output_row(
    chain: Chain,
    signer: &SelectedSigner,
    builder: Address,
    max_fee_rate: &str,
    max_fee_tenths_bps: u64,
) -> BuilderApproveRow {
    BuilderApproveRow {
        status: "submitted".to_string(),
        action: "approve-builder-fee".to_string(),
        signer: signer.address().to_string(),
        query_address: signer.query_address().to_string(),
        builder: builder.to_string(),
        max_fee_rate: max_fee_rate.to_string(),
        max_fee_tenths_bps,
        network: chain.to_string(),
        reversibility: APPROVE_REVERSIBILITY.as_str().to_string(),
    }
}

pub fn parse_builder_address(raw: &str) -> Result<Address, CliError> {
    parse_exact_hex_address(raw, true).map_err(|err| match err {
        HexAddressError::Format => CliError::Unsupported(format!("Invalid builder address: {raw}")),
        HexAddressError::Zero => {
            CliError::Unsupported("builder address cannot be the zero address".to_string())
        }
    })
}

/// Resolve the default builder address and fee rate.
///
/// Priority: `HYPERLIQUID_DEFAULT_BUILDER_ADDRESS` /
/// `HYPERLIQUID_DEFAULT_BUILDER_FEE_RATE` env vars, then the config file
/// `default_builder_address` / `default_builder_fee_rate`, then the build-time
/// default. Returns `Ok(None)` when nothing is configured and `Err` when a layer
/// sets only one side or holds invalid values.
pub fn resolve_default_builder_fee(
    settings: &Settings,
) -> Result<Option<(Address, u64)>, CliError> {
    settings
        .default_builder()?
        .map(parse_default_builder_pair)
        .transpose()
}

/// The builder default setup and wallet creation offer, as the address and
/// fee-rate text they would write to the config. Skips the config file.
pub fn suggested_default_builder(
    settings: &Settings,
) -> Result<Option<(String, String)>, CliError> {
    Ok(settings
        .suggested_default_builder()
        .map(parse_default_builder_pair)
        .transpose()?
        .map(|(address, fee)| (address.to_string(), percent_from_tenths_bps(fee))))
}

fn parse_default_builder_pair(setting: BuilderDefaultSetting) -> Result<(Address, u64), CliError> {
    let BuilderDefaultSetting {
        address,
        fee_rate,
        address_label,
        fee_label,
    } = setting;
    match (address, fee_rate) {
        (Some(_), None) => Err(CliError::Configuration(format!(
            "{address_label} is set but {fee_label} is not; both must be configured"
        ))),
        (None, Some(_)) | (None, None) => Err(CliError::Configuration(format!(
            "{fee_label} is set but {address_label} is not; both must be configured"
        ))),
        (Some(address), Some(fee)) => {
            let address = parse_builder_address(&address).map_err(|err| {
                CliError::Configuration(format!("invalid {address_label}: {err}"))
            })?;
            let fee_tenths_bps = validate_max_fee_rate(&fee)
                .map_err(|err| CliError::Configuration(format!("invalid {fee_label}: {err}")))?;
            Ok((address, fee_tenths_bps))
        }
    }
}

pub fn validate_max_fee_rate(raw: &str) -> Result<u64, CliError> {
    let Some(percent) = raw.strip_suffix('%') else {
        return Err(CliError::Unsupported(
            "builder max fee rate must be a percent string such as 0.001%".to_string(),
        ));
    };
    if percent.trim() != percent || percent.is_empty() {
        return Err(CliError::Unsupported(
            "builder max fee rate must not contain whitespace".to_string(),
        ));
    }
    let value = percent.parse::<Decimal>().map_err(|_| {
        CliError::Unsupported("builder max fee rate must be a decimal percent".to_string())
    })?;
    if value < Decimal::ZERO {
        return Err(CliError::Unsupported(
            "builder max fee rate cannot be negative".to_string(),
        ));
    }
    if value > Decimal::ONE {
        return Err(CliError::Unsupported(
            "builder max fee rate cannot exceed 1%".to_string(),
        ));
    }
    quantity::non_negative(value, Scale::TENTHS_BPS_PER_PERCENT).map_err(|err| match err {
        ScaleError::TooPrecise => {
            CliError::Unsupported("builder max fee rate must be a multiple of 0.001%".to_string())
        }
        ScaleError::Negative | ScaleError::NotPositive | ScaleError::TooLarge => {
            CliError::Unsupported("builder max fee rate is outside supported bounds".to_string())
        }
    })
}

pub(crate) fn percent_from_tenths_bps(value: u64) -> String {
    let percent = Decimal::from(value) / Decimal::from(1000);
    format!("{percent}%")
}

fn confirm_builder_approval(
    mode: &Mode,
    builder: Address,
    max_fee_rate: &str,
    max_fee_tenths_bps: u64,
) -> Result<(), CliError> {
    let question = format!(
        "Approve builder {builder} for max fee {max_fee_rate} ({max_fee_tenths_bps} tenths bps)?"
    );
    crate::prompt::consent(mode, "--yes", "", &question, "builder fee approval aborted")
}

pub const GROUP: GroupSpec =
    GroupSpec::new(&["builder"], "Manage builder fee approvals").aliases(&["builders"]);

pub struct BuilderMaxFee;

impl CliCommand for BuilderMaxFee {
    type Args = MaxFeeArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["builder", "max-fee"],
        "builder",
        "Query max builder fee approved by a user",
        "Query the max builder fee a user has approved for a builder, returned in tenths of a basis point and percent form.",
    )
    .args(&[
        arg("user").kind(InputKind::PublicUserSelector).describe("User address, wallet name, or wallet id.").resource_id("account selector"),
        arg("builder").kind(InputKind::ProtocolObjectAddress).resource_id("builder address"),
    ]);

    fn validate(args: &MaxFeeArgs) -> Result<(), CliError> {
        parse_builder_address(&args.builder)?;
        Ok(())
    }

    async fn run(inv: &Invocation, args: &MaxFeeArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_account(&args.user)?;
        let builder_address = parse_builder_address(&args.builder)?;
        max_fee(inv, user, builder_address).await
    }
}

pub struct BuilderApproved;

impl CliCommand for BuilderApproved {
    type Args = ApprovedArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["builder", "approved"],
        "builder",
        "List all builders approved by a user",
        "List builders approved by a user, enriched with each approved max fee cap.",
    )
    .args(&[arg("user")
        .kind(InputKind::PublicUserSelector)
        .describe("User address, wallet name, or wallet id.")
        .resource_id("account selector")]);

    async fn run(inv: &Invocation, args: &ApprovedArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_account(&args.user)?;
        approved(inv, user).await
    }
}

pub struct BuilderApprove;

impl CliCommand for BuilderApprove {
    type Args = ApproveArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["builder", "approve"],
        "builder",
        "Approve or update a max builder fee for the selected signer",
        "Approve, update, or revoke a max builder fee for the selected master signer. Use 0% to revoke.",
    )
    .signs(Signs::user_signed::<ApproveBuilderFee>())
    .prompt()
    .args(&[
        arg("builder").kind(InputKind::ProtocolObjectAddress).resource_id("builder address"),
        arg("max_fee_rate").kind(InputKind::Amount).describe("Maximum builder fee as a percent string, e.g. 0.001%."),
        arg("yes").kind(InputKind::Boolean).describe("Skip confirmation prompt for live execution."),
    ]);

    fn validate(args: &ApproveArgs) -> Result<(), CliError> {
        parse_builder_address(&args.builder)?;
        validate_max_fee_rate(&args.max_fee_rate)?;
        Ok(())
    }

    async fn run(inv: &Invocation, args: &ApproveArgs) -> Result<(), anyhow::Error> {
        let builder = parse_builder_address(&args.builder)?;
        validate_max_fee_rate(&args.max_fee_rate)?;
        if inv.dry_run() {
            let signer_context = inv.resolve_signer().ok();
            let preview_args = approve_dry_run_value(
                inv.remote()?.chain(),
                signer_context.as_ref().map(|resolved| resolved.address()),
                signer_context
                    .as_ref()
                    .map(|resolved| resolved.query_address()),
                builder,
                args,
            )?;
            return print_signed_dry_run(
                inv,
                "builder approve",
                "approve_builder_fee",
                APPROVE_REVERSIBILITY,
                preview_args,
            );
        }
        let resolved = inv.resolve_signer()?;
        if resolved.is_stored_agent() {
            return Err(CliError::Unsupported(
            "approveBuilderFee must be signed by the master account; select the master wallet, not a stored API wallet"
                .to_string(),
        )
        .into());
        }
        approve(inv, &resolved, builder, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::settings::{
        BUILD_DEFAULT_BUILDER_ADDRESS, BUILD_DEFAULT_BUILDER_FEE_RATE, EnvSource, env,
    };

    #[test]
    fn max_fee_rate_percent_converts_to_tenths_bps() {
        assert_eq!(validate_max_fee_rate("0%").unwrap(), 0);
        assert_eq!(validate_max_fee_rate("0.001%").unwrap(), 1);
        assert_eq!(validate_max_fee_rate("0.01%").unwrap(), 10);
        assert_eq!(validate_max_fee_rate("0.1%").unwrap(), 100);
        assert_eq!(validate_max_fee_rate("1%").unwrap(), 1000);
    }

    #[test]
    fn max_fee_rate_rejects_ambiguous_or_over_limit_values() {
        assert!(validate_max_fee_rate("0.0001%").is_err());
        assert!(validate_max_fee_rate("1.001%").is_err());
        assert!(validate_max_fee_rate("1").is_err());
        assert!(validate_max_fee_rate("-0.001%").is_err());
    }

    fn settings(env: &[(&str, &str)]) -> Settings {
        Settings::detached(EnvSource::fixed(env.iter().copied()), None)
    }

    const ADDRESS: &str = "0x0000000000000000000000000000000000000001";

    #[test]
    fn default_builder_uses_build_time_defaults_when_env_absent() {
        let result = resolve_default_builder_fee(&settings(&[]));
        match (
            BUILD_DEFAULT_BUILDER_ADDRESS.is_empty(),
            BUILD_DEFAULT_BUILDER_FEE_RATE.is_empty(),
        ) {
            (true, true) => assert!(result.unwrap().is_none()),
            (false, false) => assert!(result.unwrap().is_some()),
            _ => assert!(result.is_err()),
        }
    }

    #[test]
    fn default_builder_errs_when_only_address_set() {
        let result =
            resolve_default_builder_fee(&settings(&[(env::DEFAULT_BUILDER_ADDRESS, ADDRESS)]));
        assert!(result.unwrap_err().to_string().contains("FEE_RATE"));
    }

    #[test]
    fn default_builder_errs_when_only_fee_set() {
        let result =
            resolve_default_builder_fee(&settings(&[(env::DEFAULT_BUILDER_FEE_RATE, "0.001%")]));
        assert!(result.unwrap_err().to_string().contains("BUILDER_ADDRESS"));
    }

    #[test]
    fn default_builder_errs_on_invalid_address_or_fee() {
        let bad_address = settings(&[
            (env::DEFAULT_BUILDER_ADDRESS, "not-an-address"),
            (env::DEFAULT_BUILDER_FEE_RATE, "0.001%"),
        ]);
        let err = resolve_default_builder_fee(&bad_address).unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid HYPERLIQUID_DEFAULT_BUILDER_ADDRESS")
        );

        let bad_fee = settings(&[
            (env::DEFAULT_BUILDER_ADDRESS, ADDRESS),
            (env::DEFAULT_BUILDER_FEE_RATE, "not-a-percent"),
        ]);
        assert!(resolve_default_builder_fee(&bad_fee).is_err());
    }

    #[test]
    fn default_builder_reads_valid_env_vars() {
        let (address, fee) = resolve_default_builder_fee(&settings(&[
            (env::DEFAULT_BUILDER_ADDRESS, ADDRESS),
            (env::DEFAULT_BUILDER_FEE_RATE, "0.001%"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(address.to_string(), ADDRESS);
        assert_eq!(fee, 1); // 0.001% = 1 tenth bps
    }

    #[test]
    fn default_builder_falls_back_to_config_and_names_its_fields() {
        let config = Config {
            default_builder_address: Some(ADDRESS.to_string()),
            ..Config::default()
        };
        let settings = Settings::detached(EnvSource::fixed::<&str, &str>([]), Some(config));
        let err = resolve_default_builder_fee(&settings).unwrap_err();
        assert!(err.to_string().contains("default_builder_fee_rate"));
    }

    #[test]
    fn suggestion_normalizes_address_and_fee_text() {
        let suggestion = suggested_default_builder(&settings(&[
            (env::DEFAULT_BUILDER_ADDRESS, ADDRESS),
            (env::DEFAULT_BUILDER_FEE_RATE, "0.0010%"),
        ]))
        .unwrap();
        assert_eq!(
            suggestion,
            Some((ADDRESS.to_string(), "0.001%".to_string()))
        );
    }
}
