use std::time::{Duration, Instant};

use serde::Serialize;

use crate::assets::DexMeta;
use crate::command_spec::{CliCommand, CommandSpec, NoArgs};
use crate::hints;
use crate::invocation::Invocation;
use crate::output::format::{duration, plural};
use crate::output::{TableData, serialize_row};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StatusOutput {
    pub network: String,
    pub api_url: String,
    pub health: String,
    pub latency_ms: u128,
    pub perps_count: usize,
    pub mids_count: usize,
    pub rate_limit_status: String,
    pub rate_limit_note: String,
}

impl TableData for StatusOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Network", "Health", "Latency", "Markets", "Rate Limits"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![vec![
            self.network.clone(),
            self.health.clone(),
            duration(Duration::from_millis(
                u64::try_from(self.latency_ms).unwrap_or(u64::MAX),
            )),
            format!(
                "{}, {}",
                plural(self.perps_count, "perp", "perps"),
                plural(self.mids_count, "mid", "mids")
            ),
            self.rate_limit_status.clone(),
        ]]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serialize_row(self)
    }
}

/// `rate_limit_status` is a constant, not a read. The request budget the API
/// reports is per account: `account rate-limit`.
fn rate_limit_note() -> String {
    format!(
        "no rate-limit response observed; 429 responses map to exit code 11 and should be retried with backoff; run `{} [ADDRESS]` for an account's request budget",
        hints::ACCOUNT_RATE_LIMIT_COMMAND
    )
}

pub async fn show(inv: &Invocation) -> Result<(), anyhow::Error> {
    let remote = inv.remote()?;
    let api = remote.api();
    let probe_start = Instant::now();
    let perps = DexMeta::load(api, None).await?.markets;
    let mids = api.sdk().all_mids(None).await?;
    let latency = probe_start.elapsed();

    inv.print(&StatusOutput {
        network: remote.network().to_string(),
        api_url: remote.api().base_url().to_string(),
        health: "healthy".to_string(),
        latency_ms: latency.as_millis(),
        perps_count: perps.len(),
        mids_count: mids.len(),
        rate_limit_status: "ok".to_string(),
        rate_limit_note: rate_limit_note(),
    });
    Ok(())
}

pub struct Status;

impl CliCommand for Status {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["status"],
        "market",
        "Show API health and rate limits",
        "Show Hyperliquid API health and rate-limit context.",
    );

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        show(inv).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_row_pluralizes_market_counts_and_formats_latency() {
        let output = StatusOutput {
            network: "mainnet".to_string(),
            api_url: "https://api.hyperliquid.xyz".to_string(),
            health: "healthy".to_string(),
            latency_ms: 3_400,
            perps_count: 1,
            mids_count: 1_200,
            rate_limit_status: "ok".to_string(),
            rate_limit_note: "no rate-limit response observed".to_string(),
        };

        assert_eq!(
            output.rows(),
            vec![vec![
                "mainnet",
                "healthy",
                "3.4s",
                "1 perp, 1,200 mids",
                "ok"
            ]]
        );
    }
}
