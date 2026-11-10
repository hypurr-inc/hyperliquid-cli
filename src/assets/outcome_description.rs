//! The `key:value|key:value` text an outcome's or question's `description`
//! carries: the one parser, and the keys the CLI reads from it.

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

    /// One line naming the condition: a price-binary market's
    /// `<underlying> above <target> at <expiry>`, else its present fields.
    #[must_use]
    pub fn condition_summary(&self) -> Option<String> {
        if let Some((underlying, target_price, expiry)) = self.price_binary() {
            let expiry = format_expiry_display(expiry).unwrap_or_else(|| expiry.to_string());
            return Some(format!("{underlying} above {target_price} at {expiry}"));
        }

        let mut fields = Vec::new();
        if let Some(condition_class) = self.class() {
            fields.push(condition_class.to_string());
        }
        if let Some(underlying) = self.underlying() {
            fields.push(format!("underlying={underlying}"));
        }
        if let Some(target_price) = self.target_price() {
            fields.push(format!("target_price={target_price}"));
        }
        if let Some(expiry) = self.expiry() {
            fields.push(format!("expiry={expiry}"));
        }
        if let Some(period) = self.period() {
            fields.push(format!("period={period}"));
        }

        (!fields.is_empty()).then(|| fields.join("; "))
    }

    /// A price-binary market's `<underlying> above <target> <side> <expiry>`,
    /// else `fallback_name` unless it is blank.
    #[must_use]
    pub fn market_title(&self, side_name: &str, fallback_name: &str) -> Option<String> {
        if let Some((underlying, target_price, expiry)) = self.price_binary() {
            let expiry = format_expiry_display(expiry).unwrap_or_else(|| expiry.to_string());
            return Some(format!(
                "{underlying} above {target_price} {side_name} {expiry}"
            ));
        }

        (!fallback_name.trim().is_empty()).then(|| fallback_name.to_string())
    }

    /// A price-binary market side's slug; `None` when the expiry is not
    /// `YYYYMMDD-HHMM`.
    #[must_use]
    pub fn slug(&self, side_name: &str) -> Option<String> {
        let (underlying, target_price, expiry) = self.price_binary()?;
        let expiry_slug = format_expiry_slug(expiry)?;
        Some(format!(
            "{}-above-{}-{}-{expiry_slug}",
            slug_token(underlying),
            slug_token(target_price),
            slug_token(side_name)
        ))
    }
}

fn format_expiry_display(raw: &str) -> Option<String> {
    let (_, month, day, hour, minute) = parse_expiry_parts(raw)?;
    let month = month_name(month)?;
    Some(format!("{month} {day} {hour}:{minute}"))
}

fn format_expiry_slug(raw: &str) -> Option<String> {
    let (_, month, day, hour, minute) = parse_expiry_parts(raw)?;
    let month = month_name(month)?.to_ascii_lowercase();
    Some(format!("{month}-{day}-{hour}{minute}"))
}

fn parse_expiry_parts(raw: &str) -> Option<(&str, &str, &str, &str, &str)> {
    let raw = raw.trim();
    if raw.len() != 13 || raw.as_bytes().get(8) != Some(&b'-') {
        return None;
    }
    let year = raw.get(0..4)?;
    let month = raw.get(4..6)?;
    let day = raw.get(6..8)?;
    let hour = raw.get(9..11)?;
    let minute = raw.get(11..13)?;
    [year, month, day, hour, minute]
        .into_iter()
        .all(|part| part.chars().all(|ch| ch.is_ascii_digit()))
        .then_some((year, month, day, hour, minute))
}

fn month_name(month: &str) -> Option<&'static str> {
    match month {
        "01" => Some("Jan"),
        "02" => Some("Feb"),
        "03" => Some("Mar"),
        "04" => Some("Apr"),
        "05" => Some("May"),
        "06" => Some("Jun"),
        "07" => Some("Jul"),
        "08" => Some("Aug"),
        "09" => Some("Sep"),
        "10" => Some("Oct"),
        "11" => Some("Nov"),
        "12" => Some("Dec"),
        _ => None,
    }
}

fn slug_token(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() {
                Some(ch.to_ascii_lowercase())
            } else if ch.is_whitespace() || matches!(ch, '_' | ':') {
                Some('-')
            } else {
                None
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
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

    #[test]
    fn a_price_binary_reads_as_a_title_a_summary_and_a_slug() {
        let description = OutcomeDescription::parse(
            "class:priceBinary|underlying:BTC|targetPrice:100000|expiry:20260510-0000",
        );

        assert_eq!(
            description.market_title("Yes", "fallback").as_deref(),
            Some("BTC above 100000 Yes May 10 00:00")
        );
        assert_eq!(
            description.condition_summary().as_deref(),
            Some("BTC above 100000 at May 10 00:00")
        );
        assert_eq!(
            description.slug("Yes").as_deref(),
            Some("btc-above-100000-yes-may-10-0000")
        );
    }

    #[test]
    fn an_unreadable_expiry_is_shown_raw_and_has_no_slug() {
        let description = OutcomeDescription::parse(
            "class:priceBinary|underlying:BTC|targetPrice:1|expiry:20261310-0000",
        );

        assert_eq!(
            description.condition_summary().as_deref(),
            Some("BTC above 1 at 20261310-0000")
        );
        assert_eq!(description.slug("Yes"), None);
    }

    #[test]
    fn other_descriptions_list_their_fields_or_fall_back_to_the_name() {
        let description = OutcomeDescription::parse("class:other|period:1d");

        assert_eq!(
            description.condition_summary().as_deref(),
            Some("other; period=1d")
        );
        assert_eq!(description.market_title("Yes", "  "), None);
        assert_eq!(
            description.market_title("Yes", "Name").as_deref(),
            Some("Name")
        );
        assert_eq!(OutcomeDescription::default().condition_summary(), None);
    }
}
