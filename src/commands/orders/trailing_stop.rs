//! The exchange tracks the mark price from the moment the stop activates and
//! fires a market order once the mark retraces from its best level by the
//! configured percent or price distance. The trigger never moves back. The
//! plan, its flags and its consent text live with the other order plans; this
//! sends it and reports the result.

use hypersdk::Decimal;
use hypersdk::hypercore::types::Action;
use serde::Serialize;

use super::consent::Consent;
use super::context::OrderSubmissionContext;
use super::exchange::Wording;
use super::planning::TrailingStopPlan;
use super::status::Status;
use crate::commands::common::rows::{ListRow, Row, Rows};
use crate::hints;
use crate::output::sink;
use crate::quantity::wire_text;
use crate::terminal::OutputFormat;

pub(super) async fn trailing_stop(
    context: OrderSubmissionContext<'_>,
    plan: &TrailingStopPlan,
    on_behalf_of: Option<&str>,
) -> Result<(), anyhow::Error> {
    context.consent(&Consent::TrailingStop(plan), None)?;
    let oid = context
        .send(
            Action::TrailingStop(plan.action().clone()),
            Wording::TRAILING_STOP,
        )
        .await?
        .trailing_stop_oid()?;

    context
        .inv
        .print(&Rows::list_of_one(TrailingStopConfirmation {
            oid,
            coin: plan.coin().to_string(),
            side: plan.side().to_string(),
            size: plan.size(),
            reduce_only: plan.reduce_only(),
            retracement: plan.retracement_json(),
            activation_px: plan.activation_px().map(|price| price.to_string()),
            status: Status::WaitingForTrigger,
            retracement_label: plan.retracement_label(),
        }));
    if context.inv.format() == OutputFormat::Pretty {
        print_cancel_hint(oid, on_behalf_of);
    }
    Ok(())
}

fn print_cancel_hint(oid: u64, on_behalf_of: Option<&str>) {
    sink::err_line(&format!(
        "\nCancel it:\n    {}",
        hints::cancel_order_command(&oid.to_string(), on_behalf_of)
    ));
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
            wire_text(self.size),
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
