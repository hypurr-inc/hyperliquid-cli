//! The Hyperliquid network a run targets.

use hypersdk::hypercore::{self, Chain, HttpClient, WebSocket};

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

    /// An info/exchange client for this network, pointed at `api_override` when given.
    #[must_use]
    pub fn http_client(self, api_override: Option<reqwest::Url>) -> HttpClient {
        let client = HttpClient::new(self.chain());
        match api_override {
            Some(url) => client.with_url(url),
            None => client,
        }
    }

    /// A WebSocket for this network's public endpoint. The API endpoint
    /// override does not apply to WebSockets.
    #[must_use]
    pub fn websocket(self) -> WebSocket {
        match self {
            Self::Mainnet => hypercore::mainnet_ws(),
            Self::Testnet => hypercore::testnet_ws(),
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

impl From<Chain> for Network {
    fn from(chain: Chain) -> Self {
        match chain {
            Chain::Mainnet => Self::Mainnet,
            Chain::Testnet => Self::Testnet,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_network_has_one_spelling_everywhere() {
        for network in Network::ALL {
            let name = network.name();
            assert_eq!(network.to_string(), name);
            assert_eq!(
                serde_json::to_string(&network).unwrap(),
                format!("\"{name}\"")
            );
            assert_eq!(Network::parse(&name.to_uppercase()), Some(network));
            assert_eq!(Network::from(network.chain()), network);
        }
        assert_eq!(Network::NAMES, ["mainnet", "testnet"]);
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
    fn deserialization_is_case_insensitive() {
        let parsed: Network = serde_json::from_str("\"TESTNET\"").unwrap();
        assert_eq!(parsed, Network::Testnet);
        let parsed: Network = serde_json::from_str("\"Mainnet\"").unwrap();
        assert_eq!(parsed, Network::Mainnet);
    }

    #[test]
    fn chain_and_url_mapping() {
        assert!(Network::Mainnet.chain().is_mainnet());
        assert!(!Network::Testnet.chain().is_mainnet());
        assert_eq!(
            Network::Mainnet.api_base_url(),
            "https://api.hyperliquid.xyz"
        );
        assert_eq!(
            Network::Testnet.api_base_url(),
            "https://api.hyperliquid-testnet.xyz"
        );
    }

    #[test]
    fn setting_error_names_source() {
        let err = Network::parse_setting("HYPERLIQUID_NETWORK", "bogus").unwrap_err();
        assert!(err.to_string().contains("HYPERLIQUID_NETWORK"));
        assert!(err.to_string().contains("'bogus'"));
    }
}
