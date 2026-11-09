//! Decimal amounts and counts, and the integer wire units Hyperliquid
//! exchanges amounts in.
//!
//! One scaling rule serves every wire format: reject a sign the caller does not
//! accept, reject an amount finer than the scale, then fit the integer into the
//! wire type. Failures are plain values; callers build the [`CliError`] their
//! command reports, so nothing is allocated on success.

use rust_decimal::Decimal;

use crate::errors::CliError;

/// A power-of-ten scale: wire units per whole amount is `10^places`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scale(u32);

impl Scale {
    /// USDC on the exchange wire: micro-units.
    pub(crate) const USDC: Self = Self(6);
    /// HYPE on HyperCore: 8 decimal places.
    pub(crate) const HYPE_CORE: Self = Self(8);
    /// A percent expressed in tenths of a basis point.
    pub(crate) const TENTHS_BPS_PER_PERCENT: Self = Self(3);

    pub(crate) const fn places(places: u32) -> Self {
        Self(places)
    }

    pub(crate) const fn decimals(self) -> u32 {
        self.0
    }

    /// `units` wire units as a whole amount, without trailing zeros.
    pub(crate) fn amount(self, units: u64) -> Decimal {
        Decimal::from_i128_with_scale(i128::from(units), self.0).normalize()
    }
}

/// Why an amount does not fit a wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScaleError {
    Negative,
    NotPositive,
    TooPrecise,
    TooLarge,
}

impl ScaleError {
    /// The configuration error commands report for a bad `name` amount.
    pub(crate) fn config(self, name: &str, scale: Scale) -> CliError {
        CliError::Configuration(match self {
            Self::Negative => format!("{name} cannot be negative"),
            Self::NotPositive => not_positive(name),
            Self::TooPrecise => {
                format!(
                    "{name} supports at most {} decimal places",
                    scale.decimals()
                )
            }
            Self::TooLarge => format!("{name} is too large"),
        })
    }
}

/// `amount` in wire units; `amount` must be greater than zero.
pub(crate) fn positive<T: TryFrom<i128>>(amount: Decimal, scale: Scale) -> Result<T, ScaleError> {
    if amount <= Decimal::ZERO {
        return Err(ScaleError::NotPositive);
    }
    scaled(amount, scale)
}

/// `amount` in wire units; zero is accepted, a negative amount is not.
pub(crate) fn non_negative<T: TryFrom<i128>>(
    amount: Decimal,
    scale: Scale,
) -> Result<T, ScaleError> {
    if amount < Decimal::ZERO {
        return Err(ScaleError::Negative);
    }
    scaled(amount, scale)
}

fn scaled<T: TryFrom<i128>>(amount: Decimal, scale: Scale) -> Result<T, ScaleError> {
    let amount = amount.normalize();
    let spare_places = scale
        .0
        .checked_sub(amount.scale())
        .ok_or(ScaleError::TooPrecise)?;
    let units = 10i128
        .checked_pow(spare_places)
        .and_then(|factor| amount.mantissa().checked_mul(factor))
        .ok_or(ScaleError::TooLarge)?;
    T::try_from(units).map_err(|_| ScaleError::TooLarge)
}

/// `amount` in the wire units of `scale`, or the configuration error naming
/// `name`: the one check every USDC and HYPE amount a command sends goes
/// through.
pub(crate) fn units<T: TryFrom<i128>>(
    amount: Decimal,
    scale: Scale,
    name: &str,
) -> Result<T, CliError> {
    positive(amount, scale).map_err(|err| err.config(name, scale))
}

/// The USDC micro-units of a positive `--amount`.
pub(crate) fn usdc_units<T: TryFrom<i128>>(amount: Decimal) -> Result<T, CliError> {
    units(amount, Scale::USDC, "amount")
}

/// `amount` as the exchange takes a decimal string: no trailing zeros.
pub(crate) fn wire_text(amount: Decimal) -> String {
    amount.normalize().to_string()
}

/// The refusal of an amount that is not greater than zero: the one wording
/// every positive-amount check reports, whatever error class it travels in.
pub(crate) fn not_positive(name: &str) -> String {
    format!("{name} must be greater than zero")
}

/// Reject an amount that is not greater than zero where no scaling applies.
pub(crate) fn require_positive(amount: Decimal, name: &str) -> Result<(), CliError> {
    if amount <= Decimal::ZERO {
        return Err(CliError::Configuration(not_positive(name)));
    }
    Ok(())
}

/// A count of at least 1, as the clap value parser of an option that takes one.
pub fn parse_positive_usize(raw: &str) -> Result<usize, String> {
    let value = raw
        .parse::<usize>()
        .map_err(|err| format!("invalid positive integer: {err}"))?;
    if value == 0 {
        return Err("value must be at least 1".to_string());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(raw: &str) -> Decimal {
        raw.parse().unwrap()
    }

    #[test]
    fn scales_exact_amounts() {
        assert_eq!(positive::<u64>(dec("1.5"), Scale::USDC), Ok(1_500_000));
        assert_eq!(positive::<u64>(dec("0.000001"), Scale::USDC), Ok(1));
        assert_eq!(
            positive::<u64>(dec("1.50000000"), Scale::USDC),
            Ok(1_500_000)
        );
        assert_eq!(
            positive::<i64>(dec("12"), Scale::HYPE_CORE),
            Ok(1_200_000_000)
        );
    }

    #[test]
    fn rejects_the_wrong_sign() {
        assert_eq!(
            positive::<u64>(Decimal::ZERO, Scale::USDC),
            Err(ScaleError::NotPositive)
        );
        assert_eq!(
            positive::<u64>(dec("-1"), Scale::USDC),
            Err(ScaleError::NotPositive)
        );
        assert_eq!(non_negative::<u64>(Decimal::ZERO, Scale::USDC), Ok(0));
        assert_eq!(
            non_negative::<u64>(dec("-0.1"), Scale::USDC),
            Err(ScaleError::Negative)
        );
    }

    #[test]
    fn rejects_amounts_finer_than_the_scale() {
        assert_eq!(
            positive::<u64>(dec("0.0000001"), Scale::USDC),
            Err(ScaleError::TooPrecise)
        );
    }

    #[test]
    fn overflow_is_an_error_not_a_panic() {
        assert_eq!(
            positive::<u64>(Decimal::MAX, Scale::USDC),
            Err(ScaleError::TooLarge)
        );
        assert_eq!(
            positive::<i64>(dec("9223372036854.775808"), Scale::USDC),
            Err(ScaleError::TooLarge)
        );
        assert_eq!(
            positive::<u64>(dec("1"), Scale::places(60)),
            Err(ScaleError::TooLarge)
        );
    }

    #[test]
    fn amount_reverses_scaling_without_trailing_zeros() {
        assert_eq!(Scale::HYPE_CORE.amount(150_000_000).to_string(), "1.5");
        assert_eq!(
            Scale::places(18).amount(10_000_000_000_000_000).to_string(),
            "0.01"
        );
        assert_eq!(Scale::HYPE_CORE.amount(100_000_000).to_string(), "1");
    }

    #[test]
    fn config_errors_name_the_amount_and_scale() {
        let message = ScaleError::TooPrecise
            .config("amount", Scale::HYPE_CORE)
            .to_string();
        assert!(message.ends_with("amount supports at most 8 decimal places"));
        assert_eq!(
            ScaleError::NotPositive
                .config("amount", Scale::USDC)
                .exit_code(),
            2
        );
    }

    #[test]
    fn command_amounts_report_the_flag_they_came_from() {
        assert_eq!(usdc_units::<u64>(dec("1.5")).unwrap(), 1_500_000);
        assert!(
            usdc_units::<u64>(dec("0.0000001"))
                .unwrap_err()
                .to_string()
                .ends_with("amount supports at most 6 decimal places")
        );
        assert!(
            units::<u64>(Decimal::ZERO, Scale::HYPE_CORE, "--max")
                .unwrap_err()
                .to_string()
                .ends_with("--max must be greater than zero")
        );
    }

    #[test]
    fn wire_text_drops_trailing_zeros() {
        assert_eq!(wire_text(dec("1.500")), "1.5");
        assert_eq!(wire_text(dec("10.0")), "10");
    }
}
