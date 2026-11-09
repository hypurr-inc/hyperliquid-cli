//! The account selector the public read commands take first.

use clap::Args;
use hypersdk::Address;

use crate::command_spec::{ArgSpec, InputKind, arg};
use crate::invocation::Invocation;

/// The account selector as the schema describes it. Reads that need an address
/// mark it `.schema_required()`.
pub const USER_SELECTOR: ArgSpec = arg("address")
    .kind(InputKind::PublicUserSelector)
    .describe("Ethereum address, wallet name, or wallet id.")
    .resource_id("account selector");

/// An optional account selector: an address, wallet name or wallet id.
#[derive(Args, Debug, Clone)]
pub struct OptionalAddressArgs {
    /// Ethereum address, wallet name, or wallet id.
    /// Defaults to the selected/default signer when omitted.
    pub address: Option<String>,
}

impl OptionalAddressArgs {
    /// The address this selector names, or the selected signer's.
    pub async fn resolve(&self, inv: &Invocation) -> Result<Address, anyhow::Error> {
        inv.resolve_user(self.address.as_deref()).await
    }
}
