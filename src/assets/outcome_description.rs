//! The `key:value|key:value` text an outcome's or question's `description`
//! carries: the one parser, and the keys the CLI reads from it.

/// A parsed outcome description.
///
/// Parts without a colon are not pairs, keys and values are trimmed, a pair
/// with an empty value is absent, and the first occurrence of a key wins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutcomeDescription {
    pairs: Vec<(String, String)>,
}

impl OutcomeDescription {
    #[must_use]
    pub fn parse(description: &str) -> Self {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for part in description.split('|') {
            let Some((key, value)) = part.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            if value.is_empty() || pairs.iter().any(|(seen, _)| seen == key) {
                continue;
            }
            pairs.push((key.to_string(), value.to_string()));
        }
        Self { pairs }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    }

    #[must_use]
    pub fn class(&self) -> Option<&str> {
        self.get("class")
    }

    #[must_use]
    pub fn underlying(&self) -> Option<&str> {
        self.get("underlying")
    }

    #[must_use]
    pub fn expiry(&self) -> Option<&str> {
        self.get("expiry")
    }

    #[must_use]
    pub fn period(&self) -> Option<&str> {
        self.get("period")
    }

    /// `targetPrice`, which older outcomes spell `target_price`.
    #[must_use]
    pub fn target_price(&self) -> Option<&str> {
        self.get("targetPrice").or_else(|| self.get("target_price"))
    }

    /// `(underlying, target_price, expiry)` of a fully described price-binary
    /// market.
    #[must_use]
    pub fn price_binary(&self) -> Option<(&str, &str, &str)> {
        self.class()
            .filter(|class| class.eq_ignore_ascii_case("priceBinary"))?;
        Some((self.underlying()?, self.target_price()?, self.expiry()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_keep_colons_in_values_and_the_first_duplicate_wins() {
        let description =
            OutcomeDescription::parse("time:20261110-0200|label:a:b|note without colon|time:late");

        assert_eq!(description.get("time"), Some("20261110-0200"));
        assert_eq!(description.get("label"), Some("a:b"));
        assert_eq!(description.get("note without colon"), None);
    }

    #[test]
    fn keys_and_values_are_trimmed_and_empty_values_are_absent() {
        let description = OutcomeDescription::parse(" a : x |b:  |c:z");

        assert_eq!(description.get("a"), Some("x"));
        assert_eq!(description.get("b"), None);
        assert_eq!(description.get("c"), Some("z"));
    }

    #[test]
    fn the_target_price_is_read_under_either_spelling() {
        assert_eq!(
            OutcomeDescription::parse("targetPrice:1").target_price(),
            Some("1")
        );
        assert_eq!(
            OutcomeDescription::parse("target_price:2").target_price(),
            Some("2")
        );
    }

    #[test]
    fn a_price_binary_needs_its_class_and_all_three_fields() {
        let full = OutcomeDescription::parse(
            "class:priceBinary|underlying:BTC|targetPrice:76000|expiry:20260508-0600",
        );
        assert_eq!(full.price_binary(), Some(("BTC", "76000", "20260508-0600")));

        let other = OutcomeDescription::parse("class:other|underlying:BTC|targetPrice:1|expiry:e");
        assert_eq!(other.price_binary(), None);
        let partial = OutcomeDescription::parse("class:priceBinary|underlying:BTC");
        assert_eq!(partial.price_binary(), None);
    }
}
