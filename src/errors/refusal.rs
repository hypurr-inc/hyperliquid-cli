//! The refusals the exchange documents with a fixed string
//! (<https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/error-responses>,
//! plus the strings ccxt's Hyperliquid exception map and the python SDK issue
//! tracker quote).
//!
//! One row per refusal holds everything about it: the [`Spec`] (exit code,
//! reason, hint) and the lowercase fragments of the exchange's text that
//! identify it. Fragments stop before any variable part (`$10`,
//! `{quote_token}`, `bbo was {bbo}`). A new refusal is one row.

use super::spec::Spec;

macro_rules! refusals {
    ($($variant:ident => $spec:expr, $phrases:expr;)+) => {
        /// A refusal the exchange documents with a fixed string. Each one has
        /// its own reason and hint.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Refusal {
            $($variant),+
        }

        struct Row {
            spec: Spec,
            phrases: &'static [&'static str],
        }

        /// In declaration order, so `refusal as usize` indexes it.
        static TABLE: &[Row] = &[$(Row { spec: $spec, phrases: $phrases }),+];

        impl Refusal {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

refusals! {
    MinOrderValue => Spec::new(
        13,
        "unsupported",
        "order_below_minimum_value",
        "Raise the size or price until the order's notional value is at least $10 (spot: 10 units of the quote token).",
    ).hinted(),
    &["order must have minimum value of"];

    ReduceOnly => Spec::new(
        13,
        "unsupported",
        "reduce_only_violation",
        "A reduce-only order may only shrink an existing position on the same market. Check the side and size in `hyperliquid positions list`, or drop --reduce-only.",
    ).hinted(),
    &["reduce only order would increase position"];

    PostOnly => Spec::new(
        13,
        "unsupported",
        "post_only_would_cross",
        "A post-only (--tif alo) order may not match on entry. Price it behind the best bid or ask (`hyperliquid book`), or use --tif gtc.",
    ).hinted(),
    &["post only order would have immediately matched"];

    IocNoMatch => Spec::new(
        13,
        "unsupported",
        "ioc_no_match",
        "An IOC order found nothing to match and was canceled. Move the limit price across the book (`hyperliquid book`) or reduce the size.",
    ).hinted(),
    &["order could not immediately match"];

    BadPrice => Spec::new(
        13,
        "unsupported",
        "bad_price",
        "Round the price to the market's tick: at most 5 significant figures and at most (6 for perps, 8 for spot) minus szDecimals decimal places; integer prices are always allowed.",
    ).hinted(),
    &["price must be divisible by tick size", "order has invalid price"];

    PriceBand => Spec::new(
        13,
        "unsupported",
        "price_out_of_band",
        "The price is too far from the oracle or reference price. Price closer to the current mark (`hyperliquid book`, `hyperliquid perps get`).",
    ).hinted(),
    &["order price too far from oracle", "away from the reference price"];

    ZeroSize => Spec::new(
        13,
        "unsupported",
        "zero_size",
        "The size is zero after rounding to the market's size decimals. Use a larger --size.",
    ).hinted(),
    &["order has zero size"];

    OpenInterestCap => Spec::new(
        13,
        "unsupported",
        "open_interest_cap",
        "The market is at or growing toward its open-interest cap, so orders that increase open interest are refused. Reduce or close a position, trade another market, or retry later.",
    ).hinted(),
    &[
        "open interest is capped",
        "at open interest cap",
        "increase open interest too quickly",
    ];

    TriggerPrice => Spec::new(
        13,
        "unsupported",
        "invalid_trigger_price",
        "The TP/SL trigger price is not valid for this order against the current market (for example a trigger on the wrong side of the mark price). Check the trigger price and side.",
    ).hinted(),
    &["invalid tp/sl price"];

    NoLiquidity => Spec::new(
        13,
        "unsupported",
        "no_market_liquidity",
        "The book has nothing to fill a market order against. Place a limit order, or retry when the book has depth (`hyperliquid book`).",
    ).hinted(),
    &["no liquidity available for market order"];
}

impl Refusal {
    pub(super) fn spec(self) -> &'static Spec {
        &TABLE[self as usize].spec
    }

    pub(super) fn phrases(self) -> &'static [&'static str] {
        TABLE[self as usize].phrases
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_has_one_row_per_refusal_in_declaration_order() {
        assert_eq!(TABLE.len(), Refusal::ALL.len());
        for (index, refusal) in Refusal::ALL.iter().enumerate() {
            assert_eq!(*refusal as usize, index);
        }
    }

    #[test]
    fn every_refusal_has_a_distinct_reason_phrases_and_a_hint() {
        let mut reasons = Refusal::ALL
            .iter()
            .map(|refusal| refusal.spec().reason)
            .collect::<Vec<_>>();
        reasons.sort_unstable();
        reasons.dedup();
        assert_eq!(reasons.len(), Refusal::ALL.len());
        for refusal in Refusal::ALL {
            assert!(!refusal.phrases().is_empty(), "{refusal:?}");
            assert!(refusal.spec().hint.is_some(), "{refusal:?}");
            for phrase in refusal.phrases() {
                assert_eq!(*phrase, phrase.to_lowercase(), "{refusal:?}");
            }
        }
    }
}
