//! `account reserve-weight`: the exchange's `reserveRequestWeight` action, which
//! buys extra address-based rate-limit capacity at 0.0005 USDC per request, paid
//! from the perps balance, optionally for another existing user
//! (<https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint#reserve-additional-actions>).
//! An L1 action: hypersdk's `Action::ReserveRequestWeight` and the nktkas SDK
//! (`executeL1Action`) agree, and the destination is hashed as lowercase hex
//! and left out when unset.

use clap::Args;
use hypersdk::hypercore::types::Action;
use hypersdk::{Address, Decimal};
use serde::Serialize;

use crate::cli_command::CliCommand;
use crate::command_spec::{CommandSpec, InputKind, YES_FOR_AUTOMATION, arg};
use crate::commands::common::rows::{One, Row};
use crate::commands::common::support::print_dry_run;
use crate::consent::{self, ConsentBlock};
use crate::errors::CliError;
use crate::exchange::actions::send_l1_action;
use crate::invocation::Invocation;
use crate::wire::{AddressRole, address};

/// USDC per reserved request: 0.0005.
const COST_PER_REQUEST: Decimal = Decimal::from_parts(5, 0, 0, false, 4);

/// Arguments for `account reserve-weight`.
#[derive(Args, Debug, Clone)]
pub struct ReserveWeightArgs {
    /// Number of requests to reserve. Costs 0.0005 USDC each, from the perps balance
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    pub weight: u32,

    /// Credit the reserved capacity to this existing Hyperliquid address instead of the signer
    #[arg(long)]
    pub destination: Option<String>,

    /// Confirm live execution without prompting
    #[arg(short = 'y', long)]
    pub yes: bool,
}

fn cost(weight: u32) -> Decimal {
    Decimal::from(weight) * COST_PER_REQUEST
}

fn validate(args: &ReserveWeightArgs) -> Result<Option<Address>, CliError> {
    args.destination
        .as_deref()
        .map(|raw| address(raw, AddressRole::Explicit(Some("--destination"))))
        .transpose()
}

#[derive(Debug, Clone, Serialize)]
struct ReservedWeight {
    action: &'static str,
    status: &'static str,
    weight: u32,
    #[serde(with = "rust_decimal::serde::str")]
    cost_usdc: Decimal,
    destination: Option<String>,
}

impl Row for ReservedWeight {
    const HEADERS: &'static [&'static str] =
        &["Action", "Status", "Weight", "Cost USDC", "Destination"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.action.to_string(),
            self.status.to_string(),
            self.weight.to_string(),
            self.cost_usdc.to_string(),
            self.destination.clone().unwrap_or_default(),
        ]
    }
}

pub struct AccountReserveWeight;

impl CliCommand for AccountReserveWeight {
    type Args = ReserveWeightArgs;

    const SPEC: CommandSpec = CommandSpec::signed(
        &["account", "reserve-weight"],
        "account",
        "Reserve extra rate-limit capacity for USDC",
        concat!("Reserve additional request weight at 0.0005 USDC per request, paid from the perps balance, for the signer or another existing address. Prompts for confirmation unless ", crate::yes_flag!(), " is passed."),
    )
    .prompt()
    .args(&[
        arg("weight").describe("Requests to reserve; each costs 0.0005 USDC."),
        arg("destination")
            .kind(InputKind::RawDestinationAddress)
            .describe("Existing address to credit instead of the signer. Wallet aliases are not resolved.")
            .resource_id("destination address"),
        YES_FOR_AUTOMATION,
    ]);

    async fn run(inv: &Invocation, args: &ReserveWeightArgs) -> Result<(), anyhow::Error> {
        let destination = validate(args)?;
        let cost = cost(args.weight);
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "reserve_request_weight",
                serde_json::json!({
                    "weight": args.weight,
                    "cost_usdc": cost.to_string(),
                    "destination": destination.map(|address| address.to_string()),
                }),
            );
        }
        let signer = inv.resolve_signer()?;
        let remote = inv.remote()?;
        let credited = destination.unwrap_or_else(|| signer.query_address());
        consent::approve(
            inv,
            args.yes,
            &ConsentBlock::signed(remote.chain(), signer.address())
                .field("Credited to", credited)
                .field("Weight", format!("{} requests", args.weight))
                .field("Cost", format!("{cost} USDC from the perps balance"))
                .field("Reversibility", "irreversible"),
            &format!("Reserve {} requests for {cost} USDC?", args.weight),
            "reserve-weight aborted",
        )?;

        send_l1_action(
            remote.api(),
            &signer,
            Action::ReserveRequestWeight {
                weight: args.weight,
                destination,
            },
            None,
            "reserve request weight",
        )
        .await?;
        inv.print(&One::new(ReservedWeight {
            action: "reserve_request_weight",
            status: "confirmed",
            weight: args.weight,
            cost_usdc: cost,
            destination: destination.map(|address| address.to_string()),
        }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_costs_five_ten_thousandths_of_a_usdc() {
        assert_eq!(cost(1).to_string(), "0.0005");
        assert_eq!(cost(10).to_string(), "0.0050");
        assert_eq!(cost(1_000).to_string(), "0.5000");
        assert_eq!(cost(u32::MAX).to_string(), "2147483.6475");
    }
}
