//! The builder fee a user approves, configures as the default or attaches to an
//! order: the rate rules, the one wire form, and how the default builder and
//! rate resolve from settings.
//!
//! Commands, setup and wallet creation all depend on these rules, so they live
//! here and not in a command module.

use hypersdk::{Address, Decimal};
use serde::{Serialize, Serializer};

use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::quantity::{self, Scale, ScaleError};
use crate::settings::{BuilderDefaultSetting, Settings};
use crate::wire::{AddressRole, InfoRequest, address};

/// The most a builder fee may be, in tenths of a basis point (0.001%). The one
/// table of limits: the number and the percent its error text names both
/// come from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeCap {
    /// What a builder may be approved for, and what a spot order may pay.
    Approval,
    /// What a perpetual order may pay a builder.
    PerpOrder,
}

impl FeeCap {
    #[must_use]
    pub const fn tenths_bps(self) -> u64 {
        match self {
            Self::Approval => 1_000,
            Self::PerpOrder => 100,
        }
    }

    /// The cap as a percent, such as `0.1`.
    #[must_use]
    pub fn percent(self) -> Decimal {
        percent(self.tenths_bps())
    }
}

/// `tenths_bps` as a percent, such as `0.01`. The one conversion of the unit.
#[must_use]
pub fn percent(tenths_bps: u64) -> Decimal {
    Scale::TENTHS_BPS_PER_PERCENT.amount(tenths_bps)
}

/// The wire spelling of an address in a builder fee and in `maxBuilderFee`:
/// lowercase. Hyperliquid's Python SDK lowercases builder addresses before
/// hashing and signing an order action; checksum casing changes the msgpack
/// action hash and recovers a random nonexistent signer on the exchange.
fn wire_address(address: Address) -> String {
    format!("{address:#x}")
}

/// A sub-builder that receives `share` of a builder's fee, in units of 0.001%
/// of it (100,000 is all of it). The builder keeps the remainder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubBuilder {
    address: Address,
    share: u64,
}

impl SubBuilder {
    #[must_use]
    pub fn new(address: Address, share: u64) -> Self {
        Self { address, share }
    }

    /// The address as it is sent and shown: lowercase.
    #[must_use]
    pub fn address_wire(&self) -> String {
        wire_address(self.address)
    }

    /// The share as a percent of the builder's fee.
    #[must_use]
    pub fn percent(&self) -> Decimal {
        percent(self.share)
    }
}

/// What Hyperliquid wants in a builder fee `{ b, f, s }`: the builder's address,
/// the fee in tenths of a basis point (0.001%), and the optional sub-builders
/// that split the fee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderFee {
    address: Address,
    tenths_bps: u64,
    subs: Vec<SubBuilder>,
}

impl BuilderFee {
    const MAX_SUB_BUILDERS: usize = 5;
    /// The sub-builder shares sum to at most this: 100%.
    const MAX_SUB_BUILDER_SHARES: u64 = 100_000;

    #[must_use]
    pub fn new(address: Address, tenths_bps: u64, subs: Vec<SubBuilder>) -> Self {
        Self {
            address,
            tenths_bps,
            subs,
        }
    }

    #[must_use]
    pub fn address(&self) -> Address {
        self.address
    }

    /// The address as it is sent and shown: lowercase.
    #[must_use]
    pub fn address_wire(&self) -> String {
        wire_address(self.address)
    }

    #[must_use]
    pub fn tenths_bps(&self) -> u64 {
        self.tenths_bps
    }

    #[must_use]
    pub fn subs(&self) -> &[SubBuilder] {
        &self.subs
    }

    /// The fee as a percent, such as `0.01`.
    #[must_use]
    pub fn percent(&self) -> Decimal {
        percent(self.tenths_bps)
    }

    pub fn ensure_within_perp_cap(&self) -> Result<(), CliError> {
        let cap = FeeCap::PerpOrder;
        if self.tenths_bps > cap.tenths_bps() {
            return Err(CliError::Unsupported(format!(
                "perp order builder fee rate cannot exceed {}%",
                cap.percent()
            )));
        }
        Ok(())
    }
}

impl Serialize for BuilderFee {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Sub {
            b: String,
            s: u64,
        }

        #[derive(Serialize)]
        struct Wire {
            b: String,
            f: u64,
            /// Left off the wire when empty, so a plain builder fee keeps its `{ b, f }` shape.
            #[serde(skip_serializing_if = "Vec::is_empty")]
            s: Vec<Sub>,
        }

        Wire {
            b: wire_address(self.address),
            f: self.tenths_bps,
            s: self
                .subs
                .iter()
                .map(|sub| Sub {
                    b: wire_address(sub.address),
                    s: sub.share,
                })
                .collect(),
        }
        .serialize(serializer)
    }
}

/// The sub-builders `--sub-builder ADDRESS:SHARE` names. A share is a percent
/// of the builder's fee; the wire unit is 0.001% of it. The exchange accepts up
/// to `BuilderFee::MAX_SUB_BUILDERS`, each with a positive share, whose
/// shares sum to at most 100%.
pub fn parse_sub_builders(raw: &[String]) -> Result<Vec<SubBuilder>, CliError> {
    if raw.len() > BuilderFee::MAX_SUB_BUILDERS {
        return Err(CliError::Configuration(format!(
            "--sub-builder can be given at most {} times, {} given",
            BuilderFee::MAX_SUB_BUILDERS,
            raw.len()
        )));
    }
    let mut total = 0u64;
    let mut sub_builders = Vec::with_capacity(raw.len());
    for entry in raw {
        let sub_builder = parse_sub_builder(entry)?;
        total += sub_builder.share;
        sub_builders.push(sub_builder);
    }
    if total > BuilderFee::MAX_SUB_BUILDER_SHARES {
        return Err(CliError::Configuration(
            "--sub-builder shares must sum to at most 100%".to_string(),
        ));
    }
    Ok(sub_builders)
}

fn parse_sub_builder(entry: &str) -> Result<SubBuilder, CliError> {
    let usage = || {
        CliError::Configuration(format!(
            "--sub-builder must be ADDRESS:SHARE with SHARE a percent of the builder fee such as 25%; got '{entry}'"
        ))
    };
    let (address_text, share) = entry.split_once(':').ok_or_else(usage)?;
    let percent = share
        .strip_suffix('%')
        .filter(|body| body.trim() == *body)
        .ok_or_else(usage)?
        .parse::<Decimal>()
        .map_err(|_| usage())?;
    let units = quantity::positive::<u64>(percent, Scale::TENTHS_BPS_PER_PERCENT)
        .map_err(|err| err.config("--sub-builder share", Scale::TENTHS_BPS_PER_PERCENT))?;
    if units > BuilderFee::MAX_SUB_BUILDER_SHARES {
        return Err(CliError::Configuration(
            "--sub-builder share cannot exceed 100%".to_string(),
        ));
    }
    Ok(SubBuilder::new(
        address(address_text, AddressRole::Protocol("sub-builder"))?,
        units,
    ))
}

/// What `user` has approved `builder` to charge, in tenths of a basis point.
/// The one reader of `maxBuilderFee`.
pub async fn max_builder_fee(
    api: &ApiClient,
    user: Address,
    builder: Address,
) -> Result<u64, CliError> {
    api.info::<u64>(
        &InfoRequest::user("maxBuilderFee", user).address("builder", builder),
        "loading max builder fee",
    )
    .await
}

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
        if value > FeeCap::Approval.percent() {
            return Err(CliError::Unsupported(format!(
                "builder max fee rate cannot exceed {}%",
                FeeCap::Approval.percent()
            )));
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
    format!("{}%", percent(value))
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
    use crate::env::{self, EnvSource};

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

    fn fee(tenths_bps: u64) -> BuilderFee {
        BuilderFee::new(Address::repeat_byte(0xAB), tenths_bps, Vec::new())
    }

    #[test]
    fn a_builder_fee_is_tenths_of_a_basis_point_and_reads_as_a_percent() {
        assert_eq!(fee(10).percent(), Decimal::new(1, 2));
        assert_eq!(
            fee(FeeCap::PerpOrder.tenths_bps()).percent(),
            Decimal::new(1, 1)
        );
        assert_eq!(fee(FeeCap::Approval.tenths_bps()).percent(), Decimal::ONE);
        assert_eq!(fee(0).percent(), Decimal::ZERO);
        assert_eq!(percent_from_tenths_bps(10), "0.01%");
        assert_eq!(percent_from_tenths_bps(1000), "1%");
    }

    #[test]
    fn the_caps_table_drives_both_the_limit_and_the_words_that_name_it() {
        assert!(fee(100).ensure_within_perp_cap().is_ok());
        let err = fee(101).ensure_within_perp_cap().unwrap_err();
        assert_eq!(err.exit_code(), 13);
        assert_eq!(
            err.to_string(),
            "Unsupported input: perp order builder fee rate cannot exceed 0.1%"
        );
        let err = MaxFeeRate::parse("1.001%").unwrap_err();
        assert!(err.to_string().contains("cannot exceed 1%"), "{err}");
    }

    #[test]
    fn the_wire_form_is_lowercase_and_leaves_empty_sub_builders_off() {
        let address = "0x00000000000000000000000000000000000000Cc"
            .parse::<Address>()
            .unwrap();
        let plain = BuilderFee::new(address, 10, Vec::new());
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            serde_json::json!({"b": "0x00000000000000000000000000000000000000cc", "f": 10})
        );
        assert_eq!(
            plain.address_wire(),
            "0x00000000000000000000000000000000000000cc"
        );
        let split = BuilderFee::new(address, 10, vec![SubBuilder::new(address, 25_000)]);
        assert_eq!(
            serde_json::to_value(&split).unwrap(),
            serde_json::json!({
                "b": "0x00000000000000000000000000000000000000cc",
                "f": 10,
                "s": [{"b": "0x00000000000000000000000000000000000000cc", "s": 25_000}]
            })
        );
        assert_eq!(split.subs()[0].percent(), Decimal::from(25));
    }

    #[test]
    fn sub_builder_shares_are_percents_of_the_builder_fee_in_thousandths_of_a_percent() {
        let sub = |text: &str| parse_sub_builders(&[text.to_string()]);
        let address = "0x00000000000000000000000000000000000000cc";
        let parsed = sub(&format!("{address}:12.345%")).unwrap();
        assert_eq!(parsed[0].address_wire(), address);
        assert_eq!(parsed[0].percent(), Decimal::new(12_345, 3));
        assert_eq!(
            sub(&format!("{address}:100%")).unwrap()[0].percent(),
            Decimal::from(100)
        );
        for bad in [
            "0:1%",
            "nope",
            &format!("{address}:1"),
            &format!("{address}: 1%"),
        ] {
            assert!(sub(bad).is_err(), "{bad}");
        }
        for bad in ["0%", "0.0001%", "100.001%", "-1%"] {
            assert!(sub(&format!("{address}:{bad}")).is_err(), "{bad}");
        }
    }

    #[test]
    fn sub_builders_are_at_most_five_and_their_shares_sum_to_at_most_a_whole_fee() {
        let address = "0x00000000000000000000000000000000000000cc";
        let shares = |percents: &[&str]| {
            parse_sub_builders(
                &percents
                    .iter()
                    .map(|percent| format!("{address}:{percent}"))
                    .collect::<Vec<_>>(),
            )
        };
        assert!(shares(&["20%", "20%", "20%", "20%", "20%"]).is_ok());
        assert!(shares(&["10%", "10%", "10%", "10%", "10%", "10%"]).is_err());
        assert!(shares(&["60%", "40.001%"]).is_err());
    }
}
