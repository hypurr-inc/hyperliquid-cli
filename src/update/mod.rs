//! The public path of the update machinery: the entry point, `update` and
//! `issue-report` import the names re-exported here. The three submodules
//! reach each other with `super::`, never back through this file.

mod notifier;
mod release;
mod self_update;

pub use notifier::{
    UpdateCheck, flush_update_check, flush_update_check_silent, maybe_start_update_check,
};
pub use release::REPO;
pub use self_update::update;
