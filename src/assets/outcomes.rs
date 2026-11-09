//! Outcome market sides as `outcomeMeta` lists them.

use serde::{Deserialize, Serialize};

use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::wire::InfoRequest;

use super::asset_id::{outcome_asset_id, outcome_coin, outcome_encoding, outcome_token};

/// One side of an outcome market.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OutcomeSideRow {
    pub outcome: u64,
    pub side: u64,
    pub encoding: u64,
    pub coin: String,
    pub token: String,
    pub asset_id: u64,
    pub outcome_name: String,
    pub side_name: String,
    pub description: String,
    pub side_token: Option<u64>,
}

/// Every outcome side, ordered by outcome and side. A side whose encoding does
/// not fit the protocol's asset-id space fails the load: dropping it would
/// show a market list that silently lacks sides.
pub async fn load_outcome_sides(api: &ApiClient) -> Result<Vec<OutcomeSideRow>, CliError> {
    let response = api
        .info::<OutcomeMetaResponse>(&InfoRequest::new("outcomeMeta"), "loading outcome metadata")
        .await?;
    rows_from_meta(response)
}

fn rows_from_meta(response: OutcomeMetaResponse) -> Result<Vec<OutcomeSideRow>, CliError> {
    let mut rows = Vec::new();
    for outcome in response.outcomes {
        for (side, side_spec) in outcome.side_specs.into_iter().enumerate() {
            let side = side as u64;
            let encoding = outcome_encoding(outcome.outcome, side)?;
            rows.push(OutcomeSideRow {
                outcome: outcome.outcome,
                side,
                encoding,
                coin: outcome_coin(encoding),
                token: outcome_token(encoding),
                asset_id: outcome_asset_id(encoding)?,
                outcome_name: outcome.name.clone(),
                side_name: side_spec.name,
                description: outcome.description.clone(),
                side_token: side_spec.token,
            });
        }
    }
    rows.sort_by_key(|row| (row.outcome, row.side));
    Ok(rows)
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct OutcomeMetaResponse {
    outcomes: Vec<OutcomeMetaOutcome>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct OutcomeMetaOutcome {
    outcome: u64,
    name: String,
    description: String,
    #[serde(default)]
    side_specs: Vec<OutcomeSideSpec>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct OutcomeSideSpec {
    name: String,
    token: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(value: serde_json::Value) -> OutcomeMetaResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn sides_are_ordered_by_outcome_then_side_and_carry_their_names() {
        let rows = rows_from_meta(response(serde_json::json!({
            "outcomes": [
                {"outcome": 2, "name": "B", "description": "d2",
                 "sideSpecs": [{"name": "Yes", "token": 21}, {"name": "No"}]},
                {"outcome": 1, "name": "A", "description": "d1",
                 "sideSpecs": [{"name": "Up"}]}
            ]
        })))
        .unwrap();

        let order: Vec<(u64, u64)> = rows.iter().map(|row| (row.outcome, row.side)).collect();
        assert_eq!(order, [(1, 0), (2, 0), (2, 1)]);
        assert_eq!(rows[0].side_name, "Up");
        assert_eq!(rows[1].coin, "#20");
        assert_eq!(rows[1].token, "+20");
        assert_eq!(rows[1].asset_id, 100_000_020);
        assert_eq!(rows[1].side_token, Some(21));
        assert_eq!(rows[2].side_token, None);
    }

    #[test]
    fn a_side_that_cannot_be_encoded_fails_the_load_instead_of_vanishing() {
        let err = rows_from_meta(response(serde_json::json!({
            "outcomes": [
                {"outcome": 1, "name": "A", "description": "", "sideSpecs": [{"name": "Yes"}]},
                {"outcome": u64::MAX, "name": "Z", "description": "", "sideSpecs": [{"name": "Yes"}]}
            ]
        })))
        .unwrap_err();

        assert!(err.to_string().contains("overflowed"), "{err}");
    }

    #[test]
    fn a_third_side_is_not_a_binary_outcome() {
        let err = rows_from_meta(response(serde_json::json!({
            "outcomes": [{"outcome": 1, "name": "A", "description": "",
                "sideSpecs": [{"name": "Yes"}, {"name": "No"}, {"name": "Maybe"}]}]
        })))
        .unwrap_err();

        assert!(err.to_string().contains("binary"), "{err}");
    }
}
