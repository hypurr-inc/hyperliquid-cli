//! What an order command shows under `--dry-run`: the action it would execute
//! and the args it would run with. Plain data, built by the planners and
//! finished and printed by [`OrderRun`](super::run::OrderRun).

use crate::margin::MarginModeArg;

pub(super) struct OrderDryRunPlan {
    would_execute: &'static str,
    args: serde_json::Value,
    /// Whether `args` names the acting account (`on_behalf_of`). Every order
    /// preview does but chase's and bracket's, whose documents have never
    /// carried the key at their top level.
    names_account: bool,
}

impl OrderDryRunPlan {
    pub(super) fn new(would_execute: &'static str, args: serde_json::Value) -> Self {
        Self {
            would_execute,
            args,
            names_account: true,
        }
    }

    /// A plan whose args do not name the acting account.
    pub(super) fn unattributed(would_execute: &'static str, args: serde_json::Value) -> Self {
        Self {
            names_account: false,
            ..Self::new(would_execute, args)
        }
    }

    /// Name the account the command acts for, as the user selected it.
    pub(super) fn with_account(mut self, on_behalf_of: Option<&str>) -> Self {
        if self.names_account {
            self.insert("on_behalf_of", serde_json::json!(on_behalf_of));
        }
        self
    }

    /// Show `mode` as the margin mode.
    pub(super) fn with_margin_mode(mut self, mode: MarginModeArg) -> Self {
        self.insert("margin_mode", serde_json::json!(mode.to_string()));
        self
    }

    /// Add what a preview read on top of the plan, such as `side_effects`.
    pub(super) fn with_extra(mut self, key: &str, value: serde_json::Value) -> Self {
        self.insert(key, value);
        self
    }

    fn insert(&mut self, key: &str, value: serde_json::Value) {
        if let Some(object) = self.args.as_object_mut() {
            object.insert(key.to_string(), value);
        }
    }

    #[must_use]
    pub(super) fn would_execute(&self) -> &'static str {
        self.would_execute
    }

    #[must_use]
    pub(super) fn into_args(self) -> serde_json::Value {
        self.args
    }
}
