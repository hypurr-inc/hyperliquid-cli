//! What a market-data command takes to name its market and candle window: the
//! candle interval parser and the coin resolvers `orderbook` reads and
//! `subscribe` streams share.
//!
//! Layer rule: below the commands; neither reaches into the other.

use hypersdk::hypercore::CandleInterval;

use crate::assets::{Market, PerpAsset};
use crate::errors::CliError;
use crate::invocation::Invocation;

const VALID_INTERVALS: &str = "1m, 3m, 5m, 15m, 30m, 1h, 2h, 4h, 8h, 12h, 1d, 3d, 1w, 1M";

pub fn parse_candle_interval(input: &str) -> Result<CandleInterval, String> {
    input
        .parse::<CandleInterval>()
        .map_err(|_| format!("invalid interval '{input}'. Valid intervals: {VALID_INTERVALS}"))
}

fn market_info_coin(market: Market, input: &str) -> Result<String, CliError> {
    match market {
        Market::Perp(perp) => Ok(perp.display_name()),
        Market::Spot(spot) => Ok(spot.info_coin()),
        Market::Outcome(_) => Err(CliError::unsupported_outcome_notation(input)),
    }
}

/// The perp `input` names. A HIP-3 market (`dex:COIN`) is looked up live when
/// the metadata snapshot lacks it, as orders do.
pub(crate) async fn resolve_perp(inv: &Invocation, input: &str) -> Result<PerpAsset, CliError> {
    inv.asset_resolver()
        .await?
        .resolve_perp_or_fetch(inv.remote()?.api(), input)
        .await
}

/// The name the exchange lists a perp under: dex-qualified for HIP-3.
pub(crate) async fn perp_coin(inv: &Invocation, input: &str) -> Result<String, CliError> {
    Ok(resolve_perp(inv, input).await?.display_name())
}

/// The name info and websocket endpoints know the perp or spot `input` names
/// by.
pub(crate) async fn market_coin(inv: &Invocation, input: &str) -> Result<String, CliError> {
    let market = inv
        .asset_resolver()
        .await?
        .resolve_or_fetch(inv.remote()?.api(), input)
        .await?;
    market_info_coin(market, input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::test_support::{perp, spot};
    use crate::assets::{AssetMetadata, AssetResolver};

    fn resolve_book_coin(resolver: &AssetResolver, coin: &str) -> Result<String, CliError> {
        market_info_coin(resolver.resolve(coin)?, coin)
    }

    fn fixture_resolver() -> AssetResolver {
        AssetResolver::new(AssetMetadata::from_assets(
            vec![perp("BTC", 0), perp("ETH", 1)],
            vec![
                spot("PURR/USDC", 10_000, "PURR", "USDC"),
                spot("HYPE/USDC", 11_035, "HYPE", "USDC"),
            ],
        ))
    }

    #[test]
    fn book_coin_resolves_to_the_info_api_name_of_each_market_kind() {
        let resolver = fixture_resolver();

        assert_eq!(resolve_book_coin(&resolver, "btc").unwrap(), "BTC");
        assert_eq!(
            resolve_book_coin(&resolver, "purr/usdc").unwrap(),
            "PURR/USDC"
        );
        assert_eq!(resolve_book_coin(&resolver, "hype/usdc").unwrap(), "@1035");
    }

    #[test]
    fn book_resolution_rejects_outcome_notation_with_outcome_guidance() {
        let resolver = fixture_resolver();
        let err = resolve_book_coin(&resolver, "#10").unwrap_err();

        assert_eq!(err.exit_code(), 13);
        assert!(err.to_string().contains("outcome-aware command path"));
        assert!(err.to_string().contains("outcomes get #10"));
    }
}
