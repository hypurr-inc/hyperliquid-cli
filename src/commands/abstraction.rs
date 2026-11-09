//! An account's abstraction mode, as `account abstraction` reports it and as
//! order placement reads it.

use hypersdk::Address;
use serde::Serialize;

use crate::commands::rows::Row;
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::wire::InfoRequest;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AbstractionRow {
    pub user: String,
    pub raw_mode: String,
    pub normalized_mode: String,
}

impl Row for AbstractionRow {
    const HEADERS: &'static [&'static str] = &["User", "Mode", "Raw Mode"];

    fn cells(&self) -> Vec<String> {
        vec![
            self.user.clone(),
            self.normalized_mode.clone(),
            self.raw_mode.clone(),
        ]
    }
}

pub async fn fetch_abstraction(api: &ApiClient, user: Address) -> Result<AbstractionRow, CliError> {
    let raw_mode = api
        .info::<String>(
            &InfoRequest::user("userAbstraction", user),
            "loading user abstraction",
        )
        .await?;
    Ok(AbstractionRow {
        user: user.to_string(),
        normalized_mode: normalize_abstraction_mode(&raw_mode).to_string(),
        raw_mode,
    })
}

fn normalize_abstraction_mode(raw: &str) -> &'static str {
    match raw {
        "unifiedAccount" => "unified-account",
        "portfolioMargin" => "portfolio-margin",
        "disabled" => "disabled",
        "default" => "default",
        "dexAbstraction" => "dex-abstraction",
        _ => "unknown",
    }
}
