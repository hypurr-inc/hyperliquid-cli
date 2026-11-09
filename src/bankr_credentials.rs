//! The Bankr API key of a run and the host to send it to.
//!
//! The rule lives here, beside [`Settings`] rather than in it: the key comes
//! from `BANKR_API_KEY`, this CLI's config or the Bankr CLI's own config, and
//! the host from `BANKR_API_URL` or the host stored beside the key.

use crate::config::BankrLogin;
use crate::env::{self, nonblank};
use crate::errors::CliError;
use crate::settings::Settings;

/// Where the Bankr API key came from, which decides whether a stored address
/// belongs to it: a key from the environment or the Bankr CLI is not the wallet
/// that `wallet login` recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Environment,
    Config,
    BankrCli,
}

#[derive(Clone)]
pub struct BankrCredentials {
    pub api_key: String,
    /// `None` means the public endpoint.
    pub api_url: Option<String>,
    pub source: KeySource,
}

impl std::fmt::Debug for BankrCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BankrCredentials")
            .field("api_key", &"[redacted]")
            .field("api_url", &self.api_url)
            .field("source", &self.source)
            .finish()
    }
}

impl Settings {
    /// `BANKR_API_URL`, which overrides the host in every case.
    #[must_use]
    pub fn bankr_env_api_url(&self) -> Option<String> {
        self.env().get_trimmed(env::BANKR_API_URL)
    }

    /// The Bankr credentials of this run: `BANKR_API_KEY` first, then this CLI's
    /// config, then the Bankr CLI's own `~/.bankr/config.json`, so an existing
    /// login works without repeating it. The host is `BANKR_API_URL`, else the
    /// host stored beside the key; `None` means the public endpoint. `Ok(None)`
    /// when no source holds a key.
    pub fn bankr_credentials(&self) -> Result<Option<BankrCredentials>, CliError> {
        let login: BankrLogin = self.bankr_login()?;
        let stored_url = login.api_url().map(str::to_string);
        let (api_key, file_url, source) =
            if let Some(key) = self.env().get_trimmed(env::BANKR_API_KEY) {
                (key, stored_url, KeySource::Environment)
            } else if let Some(key) = login.api_key() {
                (key.to_string(), stored_url, KeySource::Config)
            } else if let Some((key, url)) = self.bankr_cli_credentials()? {
                (key, url.or(stored_url), KeySource::BankrCli)
            } else {
                return Ok(None);
            };
        Ok(Some(BankrCredentials {
            api_key,
            api_url: self.bankr_env_api_url().or(file_url),
            source,
        }))
    }

    /// The key and host in the Bankr CLI's own config. A missing file or key is
    /// no credentials; a file that can't be read or parsed is an error.
    fn bankr_cli_credentials(&self) -> Result<Option<(String, Option<String>)>, CliError> {
        let Some(path) = crate::paths::bankr_cli_config(self.env()) else {
            return Ok(None);
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(CliError::Configuration(format!(
                    "failed to read the Bankr CLI config {}: {err}",
                    path.display()
                )));
            }
        };
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|err| {
            CliError::Configuration(format!(
                "invalid Bankr CLI config {}: {err}",
                path.display()
            ))
        })?;
        let text_field = |name: &str| nonblank(value.get(name).and_then(serde_json::Value::as_str));
        Ok(text_field("apiKey")
            .map(|key| (key.to_string(), text_field("apiUrl").map(str::to_string))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::env::EnvSource;
    use tempfile::TempDir;

    fn bankr_settings(env: &[(&str, &str)], home: &TempDir, config: Option<Config>) -> Settings {
        let home_dir = home.path().to_str().unwrap().to_string();
        let mut vars: Vec<(&str, &str)> = vec![(env::HOME_DIR, home_dir.as_str())];
        vars.extend_from_slice(env);
        Settings::detached(EnvSource::fixed(vars), config)
    }

    fn write_bankr_cli_config(home: &TempDir, contents: &str) {
        let dir = home.path().join(".bankr");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), contents).unwrap();
    }

    #[test]
    fn bankr_credentials_prefer_env_then_config_then_the_bankr_cli() {
        let home = TempDir::new().unwrap();
        write_bankr_cli_config(
            &home,
            r#"{"apiKey": " bk_cli ", "apiUrl": "https://cli.example"}"#,
        );
        let login = Config {
            bankr: BankrLogin::new(
                "bk_config",
                Some("https://config.example"),
                "0x0000000000000000000000000000000000000009"
                    .parse()
                    .unwrap(),
            ),
            ..Config::default()
        };

        let env_key = bankr_settings(
            &[(env::BANKR_API_KEY, " bk_env ")],
            &home,
            Some(login.clone()),
        )
        .bankr_credentials()
        .unwrap()
        .unwrap();
        assert_eq!(env_key.api_key, "bk_env");
        assert_eq!(env_key.source, KeySource::Environment);
        assert_eq!(env_key.api_url.as_deref(), Some("https://config.example"));

        let stored = bankr_settings(&[], &home, Some(login))
            .bankr_credentials()
            .unwrap()
            .unwrap();
        assert_eq!(stored.api_key, "bk_config");
        assert_eq!(stored.source, KeySource::Config);

        let cli = bankr_settings(&[], &home, None)
            .bankr_credentials()
            .unwrap()
            .unwrap();
        assert_eq!(cli.api_key, "bk_cli");
        assert_eq!(cli.source, KeySource::BankrCli);
        assert_eq!(cli.api_url.as_deref(), Some("https://cli.example"));

        let overridden = bankr_settings(
            &[(env::BANKR_API_URL, " https://env.example ")],
            &home,
            None,
        )
        .bankr_credentials()
        .unwrap()
        .unwrap();
        assert_eq!(overridden.api_url.as_deref(), Some("https://env.example"));
    }

    #[test]
    fn no_bankr_credentials_is_none_but_an_unreadable_bankr_cli_file_is_an_error() {
        let home = TempDir::new().unwrap();
        assert!(
            bankr_settings(&[], &home, None)
                .bankr_credentials()
                .unwrap()
                .is_none()
        );

        write_bankr_cli_config(&home, r#"{"apiKey": "   "}"#);
        assert!(
            bankr_settings(&[], &home, None)
                .bankr_credentials()
                .unwrap()
                .is_none()
        );

        write_bankr_cli_config(&home, "{not json");
        let err = bankr_settings(&[], &home, None)
            .bankr_credentials()
            .unwrap_err();
        assert!(matches!(err, CliError::Configuration(_)), "{err}");
        assert!(err.to_string().contains("config.json"), "{err}");
    }
}
