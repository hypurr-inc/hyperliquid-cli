mod notifier;
mod release;
mod self_update;

pub use notifier::{
    UpdateCheck, flush_update_check, flush_update_check_silent, maybe_start_update_check,
};
pub use release::{REPO, is_newer_version};
pub use self_update::download_and_replace_binary;
pub use self_update::update;
