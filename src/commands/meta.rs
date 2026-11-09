use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::assets::{DeployerFeeScales, PerpDexList, SpotListing, load_spot_markets};
use crate::command_spec::{CliCommand, CommandSpec, NoArgs};
use crate::commands::common::rows::MISSING;
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::output::TableData;
use crate::output::format::thousands;
use crate::wire::InfoRequest;

const SAMPLE_LEN: usize = 5;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MetaSpotRow {
    pub symbol: String,
    pub index: usize,
    pub base: String,
    pub quote: String,
    pub base_sz_decimals: i64,
}

impl From<SpotListing> for MetaSpotRow {
    fn from(market: SpotListing) -> Self {
        Self {
            symbol: market.symbol,
            index: market.index,
            base: market.base,
            quote: market.quote,
            base_sz_decimals: market.base_sz_decimals,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AssetContextRow {
    pub name: String,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

impl AssetContextRow {
    fn mid_for_summary(&self) -> Option<String> {
        ["midPx", "mid", "markPx"]
            .into_iter()
            .find_map(|field| value_to_summary(self.fields.get(field)?))
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DexRow {
    pub name: String,
    #[serde(flatten)]
    pub fees: DeployerFeeScales,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetaOutput {
    raw_meta: Value,
    universe: Vec<Value>,
    spot_universe: Vec<MetaSpotRow>,
    asset_contexts: Vec<AssetContextRow>,
    dexes: Vec<DexRow>,
}

impl MetaOutput {
    #[must_use]
    pub fn new(
        raw_meta: Value,
        universe: Vec<Value>,
        spot_universe: Vec<MetaSpotRow>,
        asset_contexts: Vec<AssetContextRow>,
        dexes: Vec<DexRow>,
    ) -> Self {
        Self {
            raw_meta,
            universe,
            spot_universe,
            asset_contexts,
            dexes,
        }
    }
}

impl TableData for MetaOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Section", "Count", "Sample"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![
            vec![
                "universe".to_string(),
                thousands(self.universe.len() as u64),
                self.universe
                    .iter()
                    .take(SAMPLE_LEN)
                    .filter_map(asset_name_from_value)
                    .collect::<Vec<_>>()
                    .join(", "),
            ],
            vec![
                "spot_universe".to_string(),
                thousands(self.spot_universe.len() as u64),
                self.spot_universe
                    .iter()
                    .take(SAMPLE_LEN)
                    .map(|market| market.symbol.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            ],
            vec![
                "asset_contexts".to_string(),
                thousands(self.asset_contexts.len() as u64),
                self.asset_contexts
                    .iter()
                    .take(SAMPLE_LEN)
                    .map(|ctx| {
                        let mid = ctx.mid_for_summary().unwrap_or_else(|| MISSING.to_string());
                        format!("{}={mid}", ctx.name)
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            ],
            vec![
                "dexes".to_string(),
                thousands(self.dexes.len() as u64),
                self.dexes
                    .iter()
                    .take(SAMPLE_LEN)
                    .map(|dex| dex.name.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            ],
        ]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::json!({
            "meta": self.raw_meta,
            "universe": self.universe,
            "spot_universe": self.spot_universe,
            "asset_contexts": self.asset_contexts,
            "dexes": self.dexes,
        })
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct MetaAndAssetContexts(Value, Vec<Map<String, Value>>);

impl MetaAndAssetContexts {
    fn into_output(
        self,
        spot_universe: Vec<MetaSpotRow>,
        dexes: Vec<DexRow>,
    ) -> Result<MetaOutput, CliError> {
        let raw_meta = self.0;
        let universe = raw_meta
            .get("universe")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| {
                CliError::Internal(anyhow::anyhow!(
                    "decode failed while reading raw exchange metadata: `universe` is missing"
                ))
            })?;
        let asset_contexts = self
            .1
            .into_iter()
            .enumerate()
            .map(|(index, fields)| {
                let name = universe
                    .get(index)
                    .and_then(asset_name_from_value)
                    .unwrap_or_else(|| index.to_string());
                AssetContextRow { name, fields }
            })
            .collect();

        Ok(MetaOutput::new(
            raw_meta,
            universe,
            spot_universe,
            asset_contexts,
            dexes,
        ))
    }
}

pub async fn show(inv: &Invocation) -> Result<(), anyhow::Error> {
    let api = inv.remote()?.api();
    let raw_meta_and_contexts = fetch_meta_and_asset_contexts(api).await?;
    let spot_universe = load_spot_universe(api).await?;
    let dexes = fetch_dexes(api).await?;
    inv.print(&raw_meta_and_contexts.into_output(spot_universe, dexes)?);
    Ok(())
}

async fn fetch_meta_and_asset_contexts(api: &ApiClient) -> Result<MetaAndAssetContexts, CliError> {
    api.info(
        &InfoRequest::new("metaAndAssetCtxs"),
        "loading raw exchange metadata",
    )
    .await
}

async fn fetch_dexes(api: &ApiClient) -> Result<Vec<DexRow>, CliError> {
    let dexes = PerpDexList::load(api).await?;
    Ok(dexes
        .metas(api)
        .await?
        .into_iter()
        .map(|(dex, meta)| DexRow {
            name: dex.name.clone(),
            fees: meta.fee_scales(),
        })
        .collect())
}

async fn load_spot_universe(api: &ApiClient) -> Result<Vec<MetaSpotRow>, CliError> {
    Ok(load_spot_markets(api)
        .await?
        .into_iter()
        .map(MetaSpotRow::from)
        .collect())
}

fn asset_name_from_value(value: &Value) -> Option<String> {
    value
        .get("name")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn value_to_summary(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

pub struct Meta;

impl CliCommand for Meta {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["meta"],
        "market",
        "Show raw exchange metadata",
        "Show raw exchange metadata.",
    );

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        show(inv).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_output_from_raw_response_pairs_names_and_preserves_raw_context_fields() {
        let raw: MetaAndAssetContexts = serde_json::from_value(serde_json::json!([
            {
                "universe": [
                    {
                        "name": "BTC",
                        "szDecimals": 5,
                        "maxLeverage": 50,
                        "onlyIsolated": false
                    }
                ],
                "marginTables": [[50, {"lowerBound": "0.0", "maxLeverage": 50}]]
            },
            [
                {
                    "dayNtlVlm": "123456.7",
                    "funding": "0.0001",
                    "impactPxs": ["99.5", "100.5"],
                    "markPx": "101",
                    "midPx": "100",
                    "openInterest": "42.5",
                    "oraclePx": "100.5",
                    "premium": "0.00002",
                    "prevDayPx": "99"
                }
            ]
        ]))
        .unwrap();

        let output = raw.into_output(vec![], vec![]).unwrap();
        let json = output.to_json_value();

        assert_eq!(json["meta"]["marginTables"][0][0], 50);
        assert_eq!(json["asset_contexts"][0]["name"], "BTC");
        assert_eq!(json["asset_contexts"][0]["funding"], "0.0001");
        assert_eq!(json["asset_contexts"][0]["openInterest"], "42.5");
        assert_eq!(json["asset_contexts"][0]["impactPxs"][0], "99.5");
    }

    #[test]
    fn a_response_without_a_universe_is_a_decode_error_not_an_empty_universe() {
        let raw: MetaAndAssetContexts =
            serde_json::from_value(serde_json::json!([{"marginTables": []}, []])).unwrap();

        let err = raw.into_output(vec![], vec![]).unwrap_err();
        assert!(err.to_string().contains("universe"), "{err}");
    }
}
