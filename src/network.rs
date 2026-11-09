//! The Hyperliquid network a run targets.

use hypersdk::hypercore::{self, Chain};

use std::net::IpAddr;

use crate::errors::CliError;

/// The network a run targets. [`Network::name`] is its one spelling: display,
/// serialization, parsing and the error text all read it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    #[default]
    Mainnet,
    Testnet,
}

impl Network {
    const ALL: [Self; 2] = [Self::Mainnet, Self::Testnet];
    const NAMES: [&'static str; 2] = [Self::ALL[0].name(), Self::ALL[1].name()];

    /// The lowercase name used in config files, `HYPERLIQUID_NETWORK` and JSON.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet => "testnet",
        }
    }

    #[must_use]
    pub fn from_testnet(testnet: bool) -> Self {
        if testnet {
            Self::Testnet
        } else {
            Self::Mainnet
        }
    }

    #[must_use]
    pub fn is_testnet(self) -> bool {
        self == Self::Testnet
    }

    #[must_use]
    pub fn chain(self) -> Chain {
        match self {
            Self::Mainnet => Chain::Mainnet,
            Self::Testnet => Chain::Testnet,
        }
    }

    /// The public API endpoint as a URL, before any loopback test override.
    #[must_use]
    pub fn api_url(self) -> reqwest::Url {
        match self {
            Self::Mainnet => hypercore::mainnet_url(),
            Self::Testnet => hypercore::testnet_url(),
        }
    }

    /// The public API endpoint as shown to users, without a trailing slash.
    #[must_use]
    pub fn api_base_url(self) -> String {
        self.api_url().as_str().trim_end_matches('/').to_string()
    }

    /// Case-insensitive parse shared by the config file and `HYPERLIQUID_NETWORK`.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|network| network.name().eq_ignore_ascii_case(value))
    }

    /// Parse a setting value, naming `source` in the error.
    pub fn parse_setting(source: &str, value: &str) -> Result<Self, CliError> {
        Self::parse(value).ok_or_else(|| {
            CliError::Configuration(format!(
                "{source} must be either 'mainnet' or 'testnet' (got '{value}')"
            ))
        })
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl serde::Serialize for Network {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.name())
    }
}

impl<'de> serde::Deserialize<'de> for Network {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value)
            .ok_or_else(|| serde::de::Error::unknown_variant(&value.to_lowercase(), &Self::NAMES))
    }
}

/// Whether `url` names this machine: `localhost` or a loopback address, IPv4 or
/// IPv6 (URLs bracket IPv6 hosts).
#[must_use]
pub fn is_loopback_url(url: &reqwest::Url) -> bool {
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_urls_are_localhost_and_loopback_addresses_only() {
        for loopback in ["http://LOCALHOST:1", "http://127.0.0.1:1", "http://[::1]:1"] {
            assert!(
                is_loopback_url(&reqwest::Url::parse(loopback).unwrap()),
                "{loopback}"
            );
        }
        for remote in ["http://[2001:db8::1]:1", "http://127.example.com"] {
            assert!(
                !is_loopback_url(&reqwest::Url::parse(remote).unwrap()),
                "{remote}"
            );
        }
    }

    #[test]
    fn every_network_has_one_spelling_everywhere() {
        for network in Network::ALL {
            let name = network.name();
            assert_eq!(network.to_string(), name);
            assert_eq!(
                serde_json::to_string(&network).unwrap(),
                format!("\"{name}\"")
            );
            let upper = name.to_uppercase();
            assert_eq!(Network::parse(&upper), Some(network));
            assert_eq!(
                serde_json::from_str::<Network>(&format!("\"{upper}\"")).unwrap(),
                network
            );
            assert_eq!(network.chain().is_mainnet(), !network.is_testnet());
        }
    }

    #[test]
    fn deserialize_error_lists_the_names() {
        let err = serde_json::from_str::<Network>("\"Bogus\"").unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown variant `bogus`, expected `mainnet` or `testnet`"
        );
    }

    #[test]
    fn api_base_url_is_the_api_url_without_its_trailing_slash() {
        for network in Network::ALL {
            let base = network.api_base_url();
            assert!(!base.ends_with('/'), "{base}");
            assert_eq!(format!("{base}/"), network.api_url().as_str());
        }
        assert_ne!(
            Network::Mainnet.api_base_url(),
            Network::Testnet.api_base_url()
        );
    }

    #[test]
    fn setting_error_names_source() {
        let err = Network::parse_setting("HYPERLIQUID_NETWORK", "bogus").unwrap_err();
        assert!(err.to_string().contains("HYPERLIQUID_NETWORK"));
        assert!(err.to_string().contains("'bogus'"));
    }
}
