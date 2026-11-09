//! The exchange tracks the mark price from the moment the stop activates and
//! fires a market order once the mark retraces from its best level by the
//! configured percent or price distance. The trigger never moves back. The
//! plan, its flags and its consent text live with the other order plans; this
//! sends it and reports the result.

use std::io::{self, Write};

use hypersdk::Decimal;
use hypersdk::hypercore::types::Action;
use serde::Serialize;

use super::consent::{ConsentKind, confirm_trailing_stop};
use super::context::OrderSubmissionContext;
use super::exchange::status_error;
use super::planning::TrailingStopPlan;
use super::status::Status;
use crate::commands::common::rows::{ListRow, Row, Rows};
use crate::errors::CliError;
use crate::exchange::actions;
use crate::exchange_reply;
use crate::hints;
use crate::response_sanitization::labelled_untrusted_text;
use crate::terminal::OutputFormat;

pub(super) async fn trailing_stop(
    context: OrderSubmissionContext<'_>,
    plan: &TrailingStopPlan,
    on_behalf_of: Option<&str>,
) -> Result<(), anyhow::Error> {
    context.consent(ConsentKind::TrailingStop, None, |target| {
        confirm_trailing_stop(plan, target)
    })?;
    let response = actions::send_l1_action(
        context.api,
        context.signer,
        Action::TrailingStop(plan.action().clone()),
        context.vault_address,
        "trailing stop rejected",
    )
    .await?;
    let oid = parse_trailing_stop_response(&response)?;

    context.inv.print(&Rows::new(vec![TrailingStopConfirmation {
        oid,
        coin: plan.coin().to_string(),
        side: plan.side().to_string(),
        size: plan.size(),
        reduce_only: plan.reduce_only(),
        retracement: plan.retracement_json(),
        activation_px: plan.activation_px().map(|price| price.to_string()),
        status: Status::WaitingForTrigger,
        retracement_label: plan.retracement_label(),
    }]));
    if context.inv.format() == OutputFormat::Pretty {
        print_cancel_hint(oid, on_behalf_of);
    }
    Ok(())
}

fn print_cancel_hint(oid: u64, on_behalf_of: Option<&str>) {
    let mut stderr = io::stderr();
    // A closed stderr must not turn a placed order into a failure.
    let _ = writeln!(
        stderr,
        "\nCancel it:\n    {}",
        hints::cancel_order_command(&oid.to_string(), on_behalf_of)
    );
}

fn parse_trailing_stop_response(response: &serde_json::Value) -> Result<u64, CliError> {
    if let Some(typed) = exchange_reply::typed(response)
        && typed.kind == "trailingStop"
        && let Some(oid) = typed
            .data
            .and_then(|data| data.get("oid"))
            .and_then(serde_json::Value::as_u64)
    {
        return Ok(oid);
    }
    if let Some(error) = response
        .pointer("/data/error")
        .or_else(|| response.pointer("/data/status/error"))
        .and_then(serde_json::Value::as_str)
    {
        return Err(status_error("place trailing stop", error));
    }
    Err(CliError::unknown_outcome(format!(
        "The exchange answered ok without an order ID ({}). Check `{}` before placing another trailing stop",
        labelled_untrusted_text(&response.to_string()),
        hints::CHECK_LIVE_COMMAND
    )))
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct TrailingStopConfirmation {
    oid: u64,
    coin: String,
    side: String,
    #[serde(with = "rust_decimal::serde::str")]
    size: Decimal,
    reduce_only: bool,
    retracement: serde_json::Value,
    activation_px: Option<String>,
    status: Status,
    #[serde(skip)]
    retracement_label: String,
}

impl Row for TrailingStopConfirmation {
    const HEADERS: &'static [&'static str] = &[
        "Coin",
        "Side",
        "Size",
        "Retracement",
        "Activation",
        "Reduce Only",
        "Status",
        "OID",
    ];

    fn cells(&self) -> Vec<String> {
        vec![
            self.coin.clone(),
            self.side.clone(),
            self.size.normalize().to_string(),
            self.retracement_label.clone(),
            self.activation_px
                .clone()
                .unwrap_or_else(|| "immediate".to_string()),
            self.reduce_only.to_string(),
            self.status.to_string(),
            self.oid.to_string(),
        ]
    }
}

impl ListRow for TrailingStopConfirmation {
    const EMPTY: &'static str = "No trailing stop placed.";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_parsing_returns_oid_or_labelled_rejection() {
        let ok = serde_json::json!({"type": "trailingStop", "data": {"oid": 77738308}});
        assert_eq!(parse_trailing_stop_response(&ok).unwrap(), 77_738_308);

        let rejected = serde_json::json!({"type": "trailingStop", "data": {"error": "Bad stop"}});
        let err = parse_trailing_stop_response(&rejected).unwrap_err();
        assert!(err.to_string().contains("[untrusted remote data]"), "{err}");

        let odd = serde_json::json!({"type": "default"});
        assert!(matches!(
            parse_trailing_stop_response(&odd).unwrap_err(),
            CliError::UnknownOutcome(_)
        ));
    }
}
