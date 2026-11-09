//! Hyperliquid API reply shapes, built once so a change to the wire format is
//! made here and not in every test file that needs a market, a position or an
//! order reply.

use serde_json::{Value, json};

pub fn all_mids() -> Value {
    json!({"BTC": "50000", "ETH": "3000"})
}

/// Which leverage modes a market accepts, as the `meta` universe reports it.
#[derive(Clone, Copy)]
pub enum MarginRule {
    /// Cross or isolated: `marginMode` is null and `onlyIsolated` false.
    Either,
    /// `marginMode: "noCross"`: isolated only, and a position may not be
    /// switched to cross.
    NoCross,
    /// `marginMode: "strictIsolated"`: isolated only, with margin that cannot
    /// be removed.
    StrictIsolated,
    /// Only the deprecated `onlyIsolated` flag says anything.
    OnlyIsolatedFlag,
}

impl MarginRule {
    fn margin_mode(self) -> Value {
        match self {
            Self::NoCross => json!("noCross"),
            Self::StrictIsolated => json!("strictIsolated"),
            Self::Either | Self::OnlyIsolatedFlag => Value::Null,
        }
    }

    fn only_isolated(self) -> bool {
        !matches!(self, Self::Either)
    }
}

/// A market with 2 size decimals and 10x max leverage.
pub fn perp(name: &str, rule: MarginRule) -> Value {
    perp_with(name, 2, 10, rule)
}

pub fn perp_with(name: &str, sz_decimals: u32, max_leverage: u32, rule: MarginRule) -> Value {
    json!({
        "name": name,
        "szDecimals": sz_decimals,
        "maxLeverage": max_leverage,
        "onlyIsolated": rule.only_isolated(),
        "marginMode": rule.margin_mode(),
        "growthMode": "disabled"
    })
}

/// A `meta` reply for `universe`, collateralized in token 0 (USDC).
pub fn perp_meta(universe: Vec<Value>) -> Value {
    json!({ "universe": universe, "collateralToken": 0 })
}

pub fn perp_meta_btc_eth() -> Value {
    perp_meta(vec![
        perp_with("BTC", 5, 50, MarginRule::Either),
        perp_with("ETH", 4, 25, MarginRule::Either),
    ])
}

pub fn perp_meta_btc_only() -> Value {
    perp_meta(vec![perp_with("BTC", 5, 50, MarginRule::Either)])
}

pub fn spot_meta_usdc_only() -> Value {
    json!({
        "universe": [],
        "tokens": [usdc_token()]
    })
}

/// Spot metadata whose second pair references a missing token.
pub fn malformed_spot_meta() -> Value {
    json!({
        "universe": [
            {"name": "PURR/USDC", "index": 0, "tokens": [1, 0]},
            {"name": "BROKEN/USDC", "index": 1, "tokens": [9, 0]}
        ],
        "tokens": [
            usdc_token(),
            {
                "name": "PURR",
                "index": 1,
                "tokenId": "0x00000000000000000000000000000001",
                "szDecimals": 0,
                "weiDecimals": 0,
                "evmContract": null
            }
        ]
    })
}

fn usdc_token() -> Value {
    json!({
        "name": "USDC",
        "index": 0,
        "tokenId": "0x00000000000000000000000000000000",
        "szDecimals": 6,
        "weiDecimals": 6,
        "evmContract": null
    })
}

/// A resting buy of 0.1 `coin` at 50000, as `frontendOpenOrders` lists it.
pub fn basic_order(coin: &str, oid: u64) -> Value {
    basic_order_with_cloid(coin, oid, None)
}

pub fn basic_order_with_cloid(coin: &str, oid: u64, cloid: Option<&str>) -> Value {
    json!({
        "timestamp": 1_700_000_000_000_u64,
        "coin": coin,
        "side": "B",
        "limitPx": "50000",
        "sz": "0.1",
        "oid": oid,
        "origSz": "0.1",
        "cloid": cloid,
        "orderType": "Limit",
        "tif": "Gtc",
        "reduceOnly": false
    })
}

/// What `orderStatus` answers for an order that is open.
pub fn order_status_reply(order: Value) -> Value {
    json!({
        "status": "order",
        "order": {"status": "open", "statusTimestamp": 1_700_000_000_001_u64, "order": order}
    })
}

/// The `/exchange` order status of an order left resting.
pub fn resting_status(oid: u64) -> Value {
    json!({"resting": {"oid": oid, "cloid": null}})
}

/// The `/exchange` order status of an order that filled at once.
pub fn filled_status(total_size: &str, oid: u64) -> Value {
    json!({"filled": {"totalSz": total_size, "avgPx": "1", "oid": oid}})
}

pub fn fill(coin: &str, oid: u64) -> Value {
    json!({
        "coin": coin,
        "px": "50000",
        "sz": "0.1",
        "side": "B",
        "time": 1_700_000_000_000_u64,
        "startPosition": "0",
        "dir": "Open Long",
        "closedPnl": "0",
        "hash": "0xabc",
        "oid": oid,
        "crossed": true,
        "fee": "0.01",
        "tid": 43_u64,
        "cloid": null,
        "feeToken": "USDC",
        "liquidation": null
    })
}

/// A one-way position of size 1 in `coin` at 4x, with 25 of margin used.
pub fn position(coin: &str, leverage_type: &str) -> Value {
    position_with(coin, json!({"type": leverage_type, "value": 4}), "25", 10)
}

pub fn position_with(coin: &str, leverage: Value, margin_used: &str, max_leverage: u32) -> Value {
    json!({
        "type": "oneWay",
        "position": {
            "coin": coin,
            "szi": "1",
            "leverage": leverage,
            "entryPx": "100",
            "positionValue": "100",
            "unrealizedPnl": "0",
            "returnOnEquity": "0",
            "liquidationPx": null,
            "marginUsed": margin_used,
            "maxLeverage": max_leverage,
            "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
        }
    })
}

/// A `clearinghouseState` with 10000 of account value, nothing in use, and
/// `withdrawable` free to withdraw.
pub fn perp_state(withdrawable: &str, positions: Vec<Value>) -> Value {
    perp_state_with("10000", "0", "0", withdrawable, positions)
}

/// A `clearinghouseState` whose margin summary reads `account_value`,
/// `margin_used` and `maintenance`. Position notional is 100 when there are
/// positions, as [`position`] builds them.
pub fn perp_state_with(
    account_value: &str,
    margin_used: &str,
    maintenance: &str,
    withdrawable: &str,
    positions: Vec<Value>,
) -> Value {
    let notional = if positions.is_empty() { "0" } else { "100" };
    let summary = json!({
        "accountValue": account_value,
        "totalNtlPos": notional,
        "totalRawUsd": account_value,
        "totalMarginUsed": margin_used
    });
    json!({
        "marginSummary": summary,
        "crossMarginSummary": summary,
        "crossMaintenanceMarginUsed": maintenance,
        "withdrawable": withdrawable,
        "assetPositions": positions,
        "time": 1_700_000_000_000_u64
    })
}

/// A clearinghouse state with one BTC position at `leverage`x cross.
pub fn clearinghouse_state(withdrawable: &str, leverage: u32) -> Value {
    json!({
        "marginSummary": {
            "accountValue": "10000",
            "totalNtlPos": "5000",
            "totalRawUsd": "10000",
            "totalMarginUsed": "1000"
        },
        "crossMarginSummary": {
            "accountValue": "10000",
            "totalNtlPos": "5000",
            "totalRawUsd": "10000",
            "totalMarginUsed": "1000"
        },
        "crossMaintenanceMarginUsed": "100",
        "withdrawable": withdrawable,
        "assetPositions": [
            {
                "type": "oneWay",
                "position": {
                    "coin": "BTC",
                    "szi": "0.1",
                    "leverage": {"type": "cross", "value": leverage},
                    "entryPx": "50000",
                    "positionValue": "5100",
                    "unrealizedPnl": "100",
                    "returnOnEquity": "0.1",
                    "liquidationPx": null,
                    "marginUsed": "1000",
                    "maxLeverage": 50,
                    "cumFunding": {"allTime": "0", "sinceOpen": "0", "sinceChange": "0"}
                }
            }
        ],
        "time": 1_700_000_000_000_u64
    })
}

/// An `/exchange` reply to an order left resting.
pub fn order_success_response(oid: u64) -> Value {
    json!({
        "status": "ok",
        "response": {"type": "order", "data": {"statuses": [resting_status(oid)]}}
    })
}

/// An `outcomeMeta` reply listing the outcomes `ids`, each with a Yes and a No side.
pub fn outcome_meta(ids: &[u64]) -> Value {
    let outcomes: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({
                "outcome": id,
                "name": format!("Outcome {id}"),
                "description": "",
                "sideSpecs": [{"name": "Yes"}, {"name": "No"}],
                "quoteToken": "USDC"
            })
        })
        .collect();
    json!({"outcomes": outcomes, "questions": []})
}

/// One vault of the stats dataset `vault list` reads. `kind` is the
/// relationship type: `normal`, `parent` or `child`.
pub fn stats_vault(
    name: &str,
    address: &str,
    leader: &str,
    tvl: &str,
    apr: Value,
    kind: &str,
) -> Value {
    json!({
        "apr": apr,
        "pnls": [["day", ["0.0"]]],
        "summary": {
            "name": name,
            "vaultAddress": address,
            "leader": leader,
            "tvl": tvl,
            "isClosed": false,
            "relationship": {"type": kind},
            "createTimeMillis": 1_700_000_000_000_i64
        }
    })
}

/// An active validator as `validatorSummaries` lists it, with a 4%
/// commission and one day of stats.
pub fn validator(address: &str, name: &str) -> Value {
    json!({
        "validator": address,
        "signer": "0x0000000000000000000000000000000000000003",
        "name": name,
        "description": "test validator",
        "nRecentBlocks": 1,
        "stake": 123456789_u64,
        "isJailed": false,
        "unjailableAfter": null,
        "isActive": true,
        "commission": "0.04",
        "stats": [
            ["day", {"uptimeFraction": "1.0", "predictedApr": "0.025", "nSamples": 1440}]
        ]
    })
}
