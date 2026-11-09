//! Exchange-action plumbing: signing an action and sending it to `/exchange`,
//! the user-signed (EIP-712) form, the funds check a spend needs and the
//! actions more than one command submits.
//!
//! Layer rule: this module sits below `commands` and never imports it. A
//! command decides what to send and what to print; this module sends it.

pub(crate) mod actions;
pub(crate) mod builder_approval;
pub(crate) mod funds;
pub mod send_asset;
pub(crate) mod user_signed;
