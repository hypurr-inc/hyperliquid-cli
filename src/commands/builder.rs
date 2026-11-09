use clap::Args;
use futures::future::try_join_all;
use hypersdk::Address;
use hypersdk::hypercore::Chain;
use serde::Serialize;

use crate::builder_fee::{MaxFeeRate, percent_from_tenths_bps};
use crate::command_spec::{
    CliCommand, CommandSpec, GroupSpec, InputKind, YES_FOR_LIVE_EXECUTION, arg,
};
use crate::commands::builder_approval::{self, ApproveBuilderFee};
use crate::commands::rows::{ListRow, One, Row, Rows};
use crate::commands::support::print_dry_run;
use crate::commands::user_signed::UserSigned;
use crate::consent::{self, ConsentBlock};
use crate::dry_run::ActionReversibility;
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::signing::{SelectedSigner, Signs};
use crate::wire::{AddressRole, InfoRequest, address};

const APPROVE_REVERSIBILITY: ActionReversibility = ActionReversibility::Reversible;

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

/// `builder approve`'s arguments, validated.
struct ApproveRequest {
    builder: Address,
    max_fee_rate: MaxFeeRate,
}

impl ApproveRequest {
    fn parse(args: &ApproveArgs) -> Result<Self, CliError> {
        Ok(Self {
            builder: address(&args.builder, AddressRole::Builder)?,
            max_fee_rate: MaxFeeRate::parse(&args.max_fee_rate)?,
        })
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct BuilderFeeRow {
    user: String,
    builder: String,
    max_fee_tenths_bps: u64,
    max_fee_rate: String,
}

impl BuilderFeeRow {
    fn new(user: Address, builder: Address, max_fee_tenths_bps: u64) -> Self {
        Self {
            user: user.to_string(),
            builder: builder.to_string(),
            max_fee_tenths_bps,
            max_fee_rate: percent_from_tenths_bps(max_fee_tenths_bps),
        }
    }
}

impl Row for BuilderFeeRow {
    const HEADERS: &'static [&'static str] = &["User", "Builder", "Max Fee", "Tenths Bps"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.user.clone(),
            self.builder.clone(),
            self.max_fee_rate.clone(),
            self.max_fee_tenths_bps.to_string(),
        ]
    }
}

impl ListRow for BuilderFeeRow {
    const EMPTY: &'static str = "No approved builders.";
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

impl Row for BuilderApproveRow {
    const HEADERS: &'static [&'static str] = &[
        "Status",
        "Action",
        "Signer",
        "Query Address",
        "Builder",
        "Max Fee",
        "Tenths Bps",
        "Network",
        "Reversibility",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.status.clone(),
            self.action.clone(),
            self.signer.clone(),
            self.query_address.clone(),
            self.builder.clone(),
            self.max_fee_rate.clone(),
            self.max_fee_tenths_bps.to_string(),
            self.network.clone(),
            self.reversibility.clone(),
        ]
    }
}

async fn max_fee(inv: &Invocation, user: Address, builder: Address) -> Result<(), anyhow::Error> {
    let max_fee_tenths_bps = load_max_fee(inv.remote()?.api(), user, builder).await?;
    inv.print(&One::new(BuilderFeeRow::new(
        user,
        builder,
        max_fee_tenths_bps,
    )));
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
        Ok::<_, CliError>(BuilderFeeRow::new(user, builder, max_fee_tenths_bps))
    }))
    .await?;

    inv.print(&Rows::new(rows));
    Ok(())
}

fn approve_dry_run_value(
    chain: Chain,
    signer: Option<&SelectedSigner>,
    request: &ApproveRequest,
) -> serde_json::Value {
    let max_fee_rate = request.max_fee_rate.percent();
    serde_json::json!({
        "network": chain.to_string(),
        "signer": signer.map(|signer| signer.address().to_string()),
        "query_address": signer.map(|signer| signer.query_address().to_string()),
        "builder": request.builder.to_string(),
        "max_fee_rate": max_fee_rate,
        "max_fee_tenths_bps": request.max_fee_rate.tenths_bps(),
        "reversibility": APPROVE_REVERSIBILITY,
        "action": {
            "type": ApproveBuilderFee::ACTION_TYPE,
            "hyperliquidChain": chain.to_string(),
            "signatureChainId": chain.arbitrum_id().to_string(),
            "builder": request.builder.to_string(),
            "maxFeeRate": max_fee_rate,
        }
    })
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
    request: &ApproveRequest,
    yes: bool,
) -> Result<(), anyhow::Error> {
    let question = format!(
        "Approve builder {} for max fee {} ({} tenths bps)?",
        request.builder,
        request.max_fee_rate.percent(),
        request.max_fee_rate.tenths_bps()
    );
    consent::approve(
        inv,
        yes,
        &ConsentBlock::new(),
        &question,
        "builder fee approval aborted",
    )?;

    let remote = inv.remote()?;
    builder_approval::submit(remote.api(), signer, request.builder, &request.max_fee_rate).await?;

    inv.print(&One::new(BuilderApproveRow {
        status: "submitted".to_string(),
        action: "approve-builder-fee".to_string(),
        signer: signer.address().to_string(),
        query_address: signer.query_address().to_string(),
        builder: request.builder.to_string(),
        max_fee_rate: request.max_fee_rate.percent().to_string(),
        max_fee_tenths_bps: request.max_fee_rate.tenths_bps(),
        network: remote.chain().to_string(),
        reversibility: APPROVE_REVERSIBILITY.as_str().to_string(),
    }));
    Ok(())
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
        address(&args.builder, AddressRole::Builder)?;
        Ok(())
    }

    async fn run(inv: &Invocation, args: &MaxFeeArgs) -> Result<(), anyhow::Error> {
        let user = inv.resolve_account(&args.user)?;
        let builder_address = address(&args.builder, AddressRole::Builder)?;
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
    .master_only("approveBuilderFee")
    .prompt()
    .args(&[
        arg("builder").kind(InputKind::ProtocolObjectAddress).resource_id("builder address"),
        arg("max_fee_rate").kind(InputKind::Amount).describe("Maximum builder fee as a percent string, e.g. 0.001%."),
        YES_FOR_LIVE_EXECUTION,
    ]);

    fn validate(args: &ApproveArgs) -> Result<(), CliError> {
        ApproveRequest::parse(args).map(drop)
    }

    async fn run(inv: &Invocation, args: &ApproveArgs) -> Result<(), anyhow::Error> {
        let request = ApproveRequest::parse(args)?;
        if inv.dry_run() {
            let preview_args =
                approve_dry_run_value(inv.remote()?.chain(), inv.dry_run_signer()?, &request);
            return print_dry_run(inv, "approve_builder_fee", preview_args);
        }
        approve(inv, &inv.resolve_signer()?, &request, args.yes).await
    }
}
