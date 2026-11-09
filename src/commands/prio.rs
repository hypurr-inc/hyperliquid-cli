use clap::Args;
use hypersdk::Decimal;
use hypersdk::hypercore::types::{Action, GossipPriorityBid};
use serde::Serialize;

use crate::cli_command::CliCommand;
use crate::command_spec::{CommandSpec, GroupSpec, NoArgs, OwsSupport, arg};
use crate::commands::common::rows::{ListRow, MISSING, Row, Rows};
use crate::commands::common::support::print_dry_run;
use crate::errors::CliError;
use crate::exchange::actions::send_l1_action;
use crate::exchange_reply;
use crate::invocation::Invocation;
use crate::quantity::{self, Scale};
use crate::signing::SelectedSigner;

/// `maxGas` is in wei of HYPE as the exchange counts it: 1 HYPE is 10^8 wei, the
/// token's HyperCore `weiDecimals`
/// (<https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/priority-fees>).
/// The 1e18 the SDK documents is HyperEVM's scale.
const HYPE_WEI: Scale = Scale::HYPE_CORE;

/// Arguments for `prio bid`.
#[derive(Args, Debug, Clone)]
pub struct BidArgs {
    /// Maximum HYPE to bid. You pay the live current-gas price up to this cap.
    #[arg(long)]
    pub max: Decimal,

    /// IP address to receive prioritized gossip data.
    #[arg(long)]
    pub ip: String,

    /// Slot index (0-2), where 0 is highest priority. The exchange runs three independent auctions.
    #[arg(long, default_value = "0", value_parser = clap::value_parser!(u8).range(0..=2))]
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

impl Row for PrioritySlotRow {
    const HEADERS: &'static [&'static str] = &[
        "Slot",
        "Start Time",
        "Duration",
        "Start Gas",
        "Current Gas",
        "End Gas",
        "Previous Winner",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.slot.to_string(),
            self.start_time_seconds.to_string(),
            self.duration_seconds.to_string(),
            self.start_gas.clone(),
            self.current_gas.clone(),
            self.end_gas.clone(),
            self.previous_winner.clone(),
        ]
    }
}

impl ListRow for PrioritySlotRow {
    const EMPTY: &'static str = "No results.";
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PriorityBidRow {
    slot: u8,
    ip: String,
    max_hype: String,
    bid_hype: String,
    status: String,
}

impl Row for PriorityBidRow {
    const HEADERS: &'static [&'static str] = &["Slot", "IP", "Max HYPE", "Bid HYPE", "Status"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.slot.to_string(),
            self.ip.clone(),
            self.max_hype.clone(),
            self.bid_hype.clone(),
            self.status.clone(),
        ]
    }
}

impl ListRow for PriorityBidRow {
    const EMPTY: &'static str = "No results.";
}

/// The bid cap in wei, once the arguments are known good.
fn validate_bid_args(args: &BidArgs) -> Result<u64, CliError> {
    if args.ip.trim().is_empty() {
        return Err(CliError::Configuration(
            "prio bid requires a non-empty --ip".to_string(),
        ));
    }
    quantity::units(args.max, HYPE_WEI, "--max")
}

async fn bid(
    inv: &Invocation,
    signer: &SelectedSigner,
    args: &BidArgs,
    max_gas: u64,
) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let client = remote.api().sdk();
    let api = remote.api();
    let max = args.max;
    let ip = args.ip.as_str();

    let status = client.gossip_priority_auction_status().await?;
    let slot = status
        .slots
        .get(args.slot as usize)
        .ok_or_else(|| CliError::Configuration(format!("invalid priority slot {}", args.slot)))?;
    let current = match slot.current_gas {
        Some(gas) => quantity::non_negative::<u64>(gas, HYPE_WEI)
            .map_err(|err| err.config("current gas", HYPE_WEI))?,
        None => 0,
    };

    if current >= max_gas && current > 0 {
        return Err(CliError::Unsupported(format!(
            "priority bid not submitted: current gas {} is at or above --max {max}",
            HYPE_WEI.amount(current)
        ))
        .into());
    }

    let bid = if current > 0 { current + 1 } else { max_gas };
    let response = send_l1_action(
        api,
        signer,
        Action::GossipPriorityBid(GossipPriorityBid {
            slot_id: args.slot,
            ip: ip.to_string(),
            max_gas: bid,
        }),
        None,
        "priority bid",
    )
    .await?;
    let status = if exchange_reply::typed(&response).is_some_and(|typed| typed.kind == "default") {
        "submitted".to_string()
    } else {
        format!("submitted with response: {response}")
    };

    let row = PriorityBidRow {
        slot: args.slot,
        ip: ip.to_string(),
        max_hype: max.to_string(),
        bid_hype: HYPE_WEI.amount(bid).to_string(),
        status,
    };
    inv.print(&Rows::one(row));
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
        let status = inv
            .remote()?
            .api()
            .sdk()
            .gossip_priority_auction_status()
            .await?;

        let rows: Vec<PrioritySlotRow> = status
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
                    .map_or_else(|| MISSING.to_string(), |gas| gas.to_string()),
                previous_winner: status
                    .prev_winners
                    .get(slot)
                    .and_then(Clone::clone)
                    .unwrap_or_else(|| MISSING.to_string()),
            })
            .collect();

        inv.print(&Rows::new(rows));
        Ok(())
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
        let max_gas = validate_bid_args(args)?;
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "place_priority_auction_bid",
                serde_json::json!({"ip": args.ip, "max": args.max.to_string(), "slot": args.slot}),
            );
        }
        let resolved = inv.resolve_signer()?;
        bid(inv, &resolved, args, max_gas).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bid_args_are_validated_before_any_dry_run() {
        let args = |max: &str, ip: &str| BidArgs {
            max: max.parse().unwrap(),
            ip: ip.to_string(),
            slot: 0,
        };

        assert_eq!(
            validate_bid_args(&args("1", "203.0.113.10")).unwrap(),
            100_000_000
        );
        for (max, ip) in [
            ("-1", "203.0.113.10"),
            ("0", "203.0.113.10"),
            ("1", "  "),
            // Finer than a wei of HYPE.
            ("0.000000001", "203.0.113.10"),
        ] {
            assert_eq!(
                validate_bid_args(&args(max, ip)).unwrap_err().exit_code(),
                2,
                "{max} {ip:?}"
            );
        }
    }
}
