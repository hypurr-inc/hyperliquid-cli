//! What commands share and none of them is: output rows, argument types,
//! reads and view types that more than one command prints.
//!
//! Layer rule: nothing here implements `CliCommand` or imports a command
//! module. A helper two commands need moves here, not into the command that
//! happened to write it first; a helper that signs and sends an action belongs
//! in [`crate::exchange`], and a domain fact about accounts or markets in a
//! module under the crate root.

pub(crate) mod issue_report;
pub(crate) mod paging;
pub mod rows;
pub mod selector;
pub(crate) mod support;
