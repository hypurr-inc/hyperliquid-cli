//! The builder fee a user approves or configures as the default: the rate
//! rules, and how the default builder and rate resolve from settings.
//!
//! Commands, setup and wallet creation all depend on these rules, so they live
//! here and not in a command module.

use hypersdk::{Address, Decimal};

use crate::errors::CliError;
use crate::quantity::{self, Scale, ScaleError};
use crate::settings::{BuilderDefaultSetting, Settings};
use crate::wire::{AddressRole, address};

/// A builder max fee rate typed as a percent string, such as `0.001%`: the
/// text the exchange is sent, and the tenths of a basis point it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaxFeeRate {
    percent: String,
    tenths_bps: u64,
}

impl MaxFeeRate {
    pub fn parse(raw: &str) -> Result<Self, CliError> {
        let Some(percent) = raw.strip_suffix('%') else {
            return Err(CliError::Unsupported(
                "builder max fee rate must be a percent string such as 0.001%".to_string(),
            ));
        };
        if percent.trim() != percent || percent.is_empty() {
            return Err(CliError::Unsupported(
                "builder max fee rate must not contain whitespace".to_string(),
            ));
        }
        let value = percent.parse::<Decimal>().map_err(|_| {
            CliError::Unsupported("builder max fee rate must be a decimal percent".to_string())
        })?;
        if value < Decimal::ZERO {
            return Err(CliError::Unsupported(
                "builder max fee rate cannot be negative".to_string(),
            ));
        }
        if value > Decimal::ONE {
            return Err(CliError::Unsupported(
                "builder max fee rate cannot exceed 1%".to_string(),
            ));
        }
        let tenths_bps = quantity::non_negative(value, Scale::TENTHS_BPS_PER_PERCENT).map_err(
            |err| match err {
                ScaleError::TooPrecise => CliError::Unsupported(
                    "builder max fee rate must be a multiple of 0.001%".to_string(),
                ),
                ScaleError::Negative | ScaleError::NotPositive | ScaleError::TooLarge => {
                    CliError::Unsupported(
                        "builder max fee rate is outside supported bounds".to_string(),
                    )
                }
            },
        )?;
        Ok(Self {
            percent: raw.to_string(),
            tenths_bps,
        })
    }

    /// The rate as typed, for example `0.001%`.
    #[must_use]
    pub fn percent(&self) -> &str {
        &self.percent
    }

    #[must_use]
    pub fn tenths_bps(&self) -> u64 {
        self.tenths_bps
    }
}

/// `value` tenths of a basis point as a percent string.
#[must_use]
pub fn percent_from_tenths_bps(value: u64) -> String {
    let percent = Decimal::from(value) / Decimal::from(1000);
    format!("{percent}%")
}

/// Resolve the default builder address and fee rate.
///
/// Priority: `HYPERLIQUID_DEFAULT_BUILDER_ADDRESS` /
/// `HYPERLIQUID_DEFAULT_BUILDER_FEE_RATE` env vars, then the config file
/// `default_builder_address` / `default_builder_fee_rate`, then the build-time
/// default. Returns `Ok(None)` when nothing is configured and `Err` when a layer
/// sets only one side or holds invalid values.
pub fn resolve_default_builder_fee(
    settings: &Settings,
) -> Result<Option<(Address, u64)>, CliError> {
    settings
        .default_builder()?
        .map(parse_default_builder_pair)
        .transpose()
}

/// The builder default setup and wallet creation offer, as the address and
/// fee-rate text they would write to the config. Skips the config file.
pub fn suggested_default_builder(
    settings: &Settings,
) -> Result<Option<(String, String)>, CliError> {
    Ok(settings
        .suggested_default_builder()
        .map(parse_default_builder_pair)
        .transpose()?
        .map(|(address, fee)| (address.to_string(), percent_from_tenths_bps(fee))))
}

fn parse_default_builder_pair(setting: BuilderDefaultSetting) -> Result<(Address, u64), CliError> {
    let BuilderDefaultSetting {
        address: raw_address,
        fee_rate,
        address_label,
        fee_label,
    } = setting;
    match (raw_address, fee_rate) {
        (Some(_), None) => Err(CliError::Configuration(format!(
            "{address_label} is set but {fee_label} is not; both must be configured"
        ))),
        (None, Some(_)) | (None, None) => Err(CliError::Configuration(format!(
            "{fee_label} is set but {address_label} is not; both must be configured"
        ))),
        (Some(raw_address), Some(fee)) => {
            let builder = address(&raw_address, AddressRole::Builder).map_err(|err| {
                CliError::Configuration(format!("invalid {address_label}: {err}"))
            })?;
            let fee = MaxFeeRate::parse(&fee)
                .map_err(|err| CliError::Configuration(format!("invalid {fee_label}: {err}")))?;
            Ok((builder, fee.tenths_bps()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::settings::{EnvSource, env};

    fn tenths_bps(raw: &str) -> Result<u64, CliError> {
        MaxFeeRate::parse(raw).map(|rate| rate.tenths_bps())
    }

    #[test]
    fn max_fee_rate_percent_converts_to_tenths_bps() {
        assert_eq!(tenths_bps("0%").unwrap(), 0);
        assert_eq!(tenths_bps("0.001%").unwrap(), 1);
        assert_eq!(tenths_bps("0.01%").unwrap(), 10);
        assert_eq!(tenths_bps("0.1%").unwrap(), 100);
        assert_eq!(tenths_bps("1%").unwrap(), 1000);
    }

    #[test]
    fn max_fee_rate_keeps_the_typed_text() {
        assert_eq!(MaxFeeRate::parse("0.0010%").unwrap().percent(), "0.0010%");
    }

    #[test]
    fn max_fee_rate_rejects_ambiguous_or_over_limit_values() {
        assert!(tenths_bps("0.0001%").is_err());
        assert!(tenths_bps("1.001%").is_err());
        assert!(tenths_bps("1").is_err());
        assert!(tenths_bps("-0.001%").is_err());
    }

    fn settings(env: &[(&str, &str)]) -> Settings {
        Settings::detached(EnvSource::fixed(env.iter().copied()), None)
    }

    const ADDRESS: &str = "0x0000000000000000000000000000000000000001";

    #[test]
    fn default_builder_errs_when_only_address_set() {
        let result =
            resolve_default_builder_fee(&settings(&[(env::DEFAULT_BUILDER_ADDRESS, ADDRESS)]));
        assert!(result.unwrap_err().to_string().contains("FEE_RATE"));
    }

    #[test]
    fn default_builder_errs_when_only_fee_set() {
        let result =
            resolve_default_builder_fee(&settings(&[(env::DEFAULT_BUILDER_FEE_RATE, "0.001%")]));
        assert!(result.unwrap_err().to_string().contains("BUILDER_ADDRESS"));
    }

    #[test]
    fn default_builder_errs_on_invalid_address_or_fee() {
        let bad_address = settings(&[
            (env::DEFAULT_BUILDER_ADDRESS, "not-an-address"),
            (env::DEFAULT_BUILDER_FEE_RATE, "0.001%"),
        ]);
        let err = resolve_default_builder_fee(&bad_address).unwrap_err();
        assert!(
            err.to_string()
                .contains("invalid HYPERLIQUID_DEFAULT_BUILDER_ADDRESS")
        );

        let bad_fee = settings(&[
            (env::DEFAULT_BUILDER_ADDRESS, ADDRESS),
            (env::DEFAULT_BUILDER_FEE_RATE, "not-a-percent"),
        ]);
        assert!(resolve_default_builder_fee(&bad_fee).is_err());
    }

    #[test]
    fn default_builder_reads_valid_env_vars() {
        let (builder, fee) = resolve_default_builder_fee(&settings(&[
            (env::DEFAULT_BUILDER_ADDRESS, ADDRESS),
            (env::DEFAULT_BUILDER_FEE_RATE, "0.001%"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(builder.to_string(), ADDRESS);
        assert_eq!(fee, 1); // 0.001% = 1 tenth bps
    }

    #[test]
    fn default_builder_falls_back_to_config_and_names_its_fields() {
        let config = Config {
            default_builder_address: Some(ADDRESS.to_string()),
            ..Config::default()
        };
        let settings = Settings::detached(EnvSource::fixed::<&str, &str>([]), Some(config));
        let err = resolve_default_builder_fee(&settings).unwrap_err();
        assert!(err.to_string().contains("default_builder_fee_rate"));
    }

    #[test]
    fn suggestion_normalizes_address_and_fee_text() {
        let suggestion = suggested_default_builder(&settings(&[
            (env::DEFAULT_BUILDER_ADDRESS, ADDRESS),
            (env::DEFAULT_BUILDER_FEE_RATE, "0.0010%"),
        ]))
        .unwrap();
        assert_eq!(
            suggestion,
            Some((ADDRESS.to_string(), "0.001%".to_string()))
        );
    }
}
