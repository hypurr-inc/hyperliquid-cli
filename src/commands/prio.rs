//! Gossip priority auction commands.

use clap::Args;
use hypersdk::Decimal;
use hypersdk::hypercore::types::{Action, GossipPriorityBid};
use serde::Serialize;

use crate::assets::find_spot_token;
use crate::command_spec::{CliCommand, CommandSpec, GroupSpec, NoArgs, OwsSupport, arg};
use crate::commands::actions::{nonce_now, send_l1_action_raw};
use crate::commands::support::{dry_run_details, print_dry_run};
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::invocation::Invocation;
use crate::output::TableData;
use crate::quantity::{self, Scale};
use crate::signing::SelectedSigner;

/// Arguments for `prio bid`.
#[derive(Args, Debug, Clone)]
pub struct BidArgs {
    /// Maximum HYPE to bid. You pay the live current-gas price up to this cap.
    #[arg(long)]
    pub max: Decimal,

    /// IP address to receive prioritized gossip data.
    #[arg(long)]
    pub ip: String,

    /// Slot index (0-4), where 0 is highest priority.
    #[arg(long, default_value = "0", value_parser = clap::value_parser!(u8).range(0..=4))]
    pub slot: u8,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PrioritySlotRow {
    slot: usize,
    start_time_seconds: u64,
    duration_seconds: u64,
    start_gas: String,
    current_gas: String,
    end_gas: String,
    previous_winner: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorityStatusOutput {
    rows: Vec<PrioritySlotRow>,
}

impl TableData for PriorityStatusOutput {
    fn headers(&self) -> Vec<&str> {
        vec![
            "Slot",
            "Start Time",
            "Duration",
            "Start Gas",
            "Current Gas",
            "End Gas",
            "Previous Winner",
        ]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    row.slot.to_string(),
                    row.start_time_seconds.to_string(),
                    row.duration_seconds.to_string(),
                    row.start_gas.clone(),
                    row.current_gas.clone(),
                    row.end_gas.clone(),
                    row.previous_winner.clone(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PriorityBidRow {
    slot: u8,
    ip: String,
    max_hype: String,
    bid_hype: String,
    status: String,
}

struct PriorityBidOutput {
    rows: Vec<PriorityBidRow>,
}

impl TableData for PriorityBidOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Slot", "IP", "Max HYPE", "Bid HYPE", "Status"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|row| {
                vec![
                    row.slot.to_string(),
                    row.ip.clone(),
                    row.max_hype.clone(),
                    row.bid_hype.clone(),
                    row.status.clone(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(&self.rows).unwrap_or_else(|_| serde_json::json!([]))
    }
}

async fn status(inv: &Invocation) -> Result<(), anyhow::Error> {
    let status = inv
        .remote()?
        .api()
        .sdk()
        .gossip_priority_auction_status()
        .await
        .map_err(from_sdk)?;

    let rows = status
        .slots
        .iter()
        .enumerate()
        .map(|(slot, item)| PrioritySlotRow {
            slot,
            start_time_seconds: item.start_time_seconds,
            duration_seconds: item.duration_seconds,
            start_gas: item.start_gas.to_string(),
            current_gas: item
                .current_gas
                .map_or_else(|| "(no bid)".to_string(), |gas| gas.to_string()),
            end_gas: item
                .end_gas
                .map_or_else(|| "-".to_string(), |gas| gas.to_string()),
            previous_winner: status
                .prev_winners
                .get(slot)
                .and_then(Clone::clone)
                .unwrap_or_else(|| "-".to_string()),
        })
        .collect();

    inv.print(&PriorityStatusOutput { rows });
    Ok(())
}

/// Wei decimals of HYPE when the spot token list does not name them.
const DEFAULT_HYPE_WEI_DECIMALS: u32 = 18;

fn validate_bid_args(args: &BidArgs) -> Result<(), CliError> {
    if args.max <= Decimal::ZERO {
        return Err(CliError::Configuration(
            "prio bid requires --max to be positive".to_string(),
        ));
    }
    if args.ip.trim().is_empty() {
        return Err(CliError::Configuration(
            "prio bid requires a non-empty --ip".to_string(),
        ));
    }
    Ok(())
}

async fn bid(
    inv: &Invocation,
    signer: &SelectedSigner,
    args: &BidArgs,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let client = remote.api().sdk();
    let api = remote.api();
    let max = args.max;
    let ip = args.ip.as_str();
    signer.ensure_can_attempt_live_signing()?;

    let decimals = match find_spot_token(client, "HYPE").await? {
        Some(token) => u32::try_from(token.wei_decimals).map_err(|_| {
            CliError::Unsupported("HYPE wei decimals are outside supported bounds".to_string())
        })?,
        None => DEFAULT_HYPE_WEI_DECIMALS,
    };
    let scale = Scale::places(decimals);
    let max_gas =
        quantity::positive::<u64>(max, scale).map_err(|err| err.config("--max", scale))?;

    let status = client
        .gossip_priority_auction_status()
        .await
        .map_err(from_sdk)?;
    let slot = status
        .slots
        .get(args.slot as usize)
        .ok_or_else(|| CliError::Configuration(format!("invalid priority slot {}", args.slot)))?;
    let current = match slot.current_gas {
        Some(gas) => quantity::non_negative::<u64>(gas, scale)
            .map_err(|err| err.config("current gas", scale))?,
        None => 0,
    };

    if current >= max_gas && current > 0 {
        return Err(CliError::Unsupported(format!(
            "priority bid not submitted: current gas {} is at or above --max {max}",
            scale.amount(current)
        ))
        .into());
    }

    let bid = if current > 0 { current + 1 } else { max_gas };
    let response = send_l1_action_raw(
        api,
        signer,
        Action::GossipPriorityBid(GossipPriorityBid {
            slot_id: args.slot,
            ip: ip.to_string(),
            max_gas: bid,
        }),
        nonce_now(),
        None,
        "priority bid",
    )
    .await?;
    let status = match response {
        serde_json::Value::Object(map)
            if map.get("type") == Some(&serde_json::Value::String("default".to_string())) =>
        {
            "submitted".to_string()
        }
        other => format!("submitted with response: {other}"),
    };

    let row = PriorityBidRow {
        slot: args.slot,
        ip: ip.to_string(),
        max_hype: max.to_string(),
        bid_hype: scale.amount(bid).to_string(),
        status,
    };
    inv.print(&PriorityBidOutput { rows: vec![row] });
    Ok(())
}

pub const GROUP: GroupSpec = GroupSpec::new(&["prio"], "Manage gossip priority bids");

pub struct PrioStatus;

impl CliCommand for PrioStatus {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["prio", "status"],
        "prio",
        "Show priority auction status",
        "Show priority auction status.",
    );

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        status(inv).await
    }
}

pub struct PrioBid;

impl CliCommand for PrioBid {
    type Args = BidArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["prio", "bid"],
        "prio",
        "Place a priority bid",
        "Place a priority auction bid.",
    )
    .ows(OwsSupport::LocalOnly)
    .args(&[
        arg("max").describe("Maximum HYPE bid."),
        arg("ip")
            .describe("Validator IP address.")
            .resource_id("validator ip"),
        arg("slot").describe("Priority slot."),
    ]);

    async fn run(inv: &Invocation, args: &BidArgs) -> Result<(), anyhow::Error> {
        validate_bid_args(args)?;
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "prio bid",
                dry_run_details(
                    inv,
                    "place_priority_auction_bid",
                    serde_json::json!({"ip": args.ip, "max": args.max.to_string(), "slot": args.slot}),
                ),
            );
        }
        let resolved = inv.resolve_signer()?;
        bid(inv, &resolved.selected_signer(), args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_output_has_auction_columns() {
        let output = PriorityStatusOutput { rows: vec![] };

        assert!(output.headers().contains(&"Current Gas"));
        assert!(output.headers().contains(&"Previous Winner"));
    }

    #[test]
    fn decimal_current_gas_uses_hype_wei_scale() {
        let scale = Scale::places(18);
        assert_eq!(
            quantity::non_negative::<u64>("0.01".parse::<Decimal>().unwrap(), scale),
            Ok(10_000_000_000_000_000)
        );
        assert_eq!(
            scale.amount(10_000_000_000_000_001).to_string(),
            "0.010000000000000001"
        );
    }

    #[test]
    fn bid_args_are_validated_before_any_dry_run() {
        let args = |max: &str, ip: &str| BidArgs {
            max: max.parse().unwrap(),
            ip: ip.to_string(),
            slot: 0,
        };

        assert!(validate_bid_args(&args("1", "203.0.113.10")).is_ok());
        assert_eq!(
            validate_bid_args(&args("-1", "203.0.113.10"))
                .unwrap_err()
                .exit_code(),
            2
        );
        assert_eq!(
            validate_bid_args(&args("0", "203.0.113.10"))
                .unwrap_err()
                .exit_code(),
            2
        );
        assert_eq!(
            validate_bid_args(&args("1", "  ")).unwrap_err().exit_code(),
            2
        );
    }
}
