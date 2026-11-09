//! The wallet management workflow shared by `wallet`, `account` and `setup`:
//! vault writes paired with config updates, rollback, naming and default markers.
//!
//! Wallet selection and the vault's own operations stay on [`Vault`]; errors keep
//! their types (`wallet_not_found`, ambiguity, config and lock failures), so each
//! reaches the user with its own exit code.

use ows_lib::types::WalletInfo;

use crate::config::Config;
use crate::errors::CliError;
use crate::hints::wallet as hints;
use crate::ows::vault::{HyperliquidAccount, Vault};
use crate::settings::{ConfigTransaction, Settings};

pub enum NewWallet<'a> {
    Create,
    ImportPrivateKey(&'a str),
    ImportMnemonic(&'a str),
}

pub enum WalletName<'a> {
    Exact(&'a str),
    /// The first free name among `base`, `base-2`, `base-3`, ...
    Next(&'a str),
}

impl<'a> WalletName<'a> {
    /// The alias the user gave, trimmed; none or a blank one is `Next(base)`.
    #[must_use]
    pub fn from_alias(alias: Option<&'a str>, base: &'a str) -> Self {
        match crate::env::nonblank(alias) {
            Some(alias) => Self::Exact(alias),
            None => Self::Next(base),
        }
    }
}

/// Edits the config once the wallet exists, under the config lock. The config
/// keys it changes are reported back from the diff, not declared by the edit.
pub type ConfigEdit<'a> = Box<dyn FnOnce(&mut Config, &WalletInfo, &HyperliquidAccount) + 'a>;

pub struct StoredWallet {
    pub info: WalletInfo,
    pub account: HyperliquidAccount,
    pub config_keys_written: Vec<String>,
}

pub struct WalletStore<'a> {
    settings: &'a Settings,
    vault: Vault,
}

impl<'a> WalletStore<'a> {
    #[must_use]
    pub fn new(settings: &'a Settings) -> Self {
        Self {
            settings,
            vault: Vault::from_settings(settings),
        }
    }

    #[must_use]
    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    pub fn next_name(&self, base: &str) -> Result<String, CliError> {
        let wallets = self.vault.list()?;
        let taken = |name: &str| wallets.iter().any(|wallet| wallet.name == name);
        if !taken(base) {
            return Ok(base.to_string());
        }
        Ok((2..)
            .map(|index| format!("{base}-{index}"))
            .find(|candidate| !taken(candidate))
            .expect("an unbounded range always yields a free name"))
    }

    pub fn is_configured_default(&self, wallet: &WalletInfo) -> Result<bool, CliError> {
        Ok(Vault::is_default(
            self.settings.default_wallet_id()?.as_deref(),
            wallet,
        ))
    }

    /// Default markers for Hyperliquid-capable `wallets`: the configured default,
    /// or, with none configured, the vault's only wallet, which is what signing uses.
    pub fn default_markers(&self, wallets: &[WalletInfo]) -> Result<Vec<bool>, CliError> {
        Ok(Vault::default_markers(
            wallets,
            self.settings.default_wallet_id()?.as_deref(),
        ))
    }

    /// Create or import a wallet and, when `config_edit` is given, record it in
    /// the config.
    ///
    /// With an edit, the config is locked and loaded before the wallet exists so
    /// a broken file fails first, and the wallet is removed again if the config
    /// can't be saved.
    pub fn add(
        &self,
        source: &NewWallet<'_>,
        name: &WalletName<'_>,
        config_edit: Option<ConfigEdit<'_>>,
    ) -> Result<StoredWallet, anyhow::Error> {
        self.vault.warn_if_passphrase_unset();

        let editing: Option<(ConfigTransaction<'_>, ConfigEdit<'_>)> = match config_edit {
            Some(edit) => Some((self.settings.begin_config_update()?, edit)),
            None => None,
        };

        let name = match name {
            WalletName::Exact(name) => (*name).to_string(),
            WalletName::Next(base) => self.next_name(base)?,
        };
        let info = match source {
            NewWallet::Create => self.vault.create(&name),
            NewWallet::ImportPrivateKey(key) => self.vault.import_private_key(&name, key),
            NewWallet::ImportMnemonic(mnemonic) => self.vault.import_mnemonic(&name, mnemonic),
        }?;

        let account = match Vault::hyperliquid_account(&info) {
            Ok(account) => account,
            Err(err) => return Err(self.roll_back(&info, err.into())),
        };

        let mut config_keys_written = Vec::new();
        if let Some((transaction, edit)) = editing {
            config_keys_written = transaction
                .commit(|config| edit(config, &info, &account))
                .map_err(|err| self.roll_back(&info, err))?;
        }

        Ok(StoredWallet {
            info,
            account,
            config_keys_written,
        })
    }

    /// Remove a wallet that was just written. A failed removal is reported with
    /// the original error so the user knows the vault still holds it.
    fn roll_back(&self, wallet: &WalletInfo, cause: anyhow::Error) -> anyhow::Error {
        match self.vault.delete(&wallet.id) {
            Ok(()) => cause,
            Err(err) => anyhow::anyhow!(
                "{cause} (and removing the new wallet '{}' from the vault failed: {err}; delete it with `{}`)",
                wallet.name,
                hints::wallet_delete(&wallet.id)
            ),
        }
    }

    /// How a wallet becomes the default signer in the config: the one place
    /// that key is written for a wallet.
    pub fn make_default(config: &mut Config, wallet: &WalletInfo) {
        config.default_wallet_id = Some(wallet.id.clone());
    }

    pub fn set_default_edit() -> ConfigEdit<'static> {
        Box::new(|config, wallet, _| Self::make_default(config, wallet))
    }

    /// Make `wallet` the default signer. A wallet that can't sign for Hyperliquid
    /// is refused before the config is touched; the account it would sign as is
    /// returned.
    pub fn set_default(&self, wallet: &WalletInfo) -> Result<HyperliquidAccount, anyhow::Error> {
        let account = Vault::hyperliquid_account(wallet)?;
        self.settings
            .update_config(|config| Self::make_default(config, wallet))?;
        Ok(account)
    }

    /// Delete `wallet` from the vault. When it was the configured default, clear
    /// that reference too.
    pub fn remove(&self, wallet: &WalletInfo, was_default: bool) -> Result<(), anyhow::Error> {
        self.vault.delete(&wallet.id)?;
        if was_default {
            self.settings
                .update_config(|config| config.default_wallet_id = None)?;
        }
        Ok(())
    }

    pub fn rename(&self, selector: &str, new_name: &str) -> Result<WalletInfo, CliError> {
        self.vault.rename(selector, new_name)?;
        self.vault.get(new_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigFile;
    use crate::env::{self, EnvSource};
    use crate::ows::vault::test_support::{KEY, import_test_wallet};
    use tempfile::TempDir;

    fn settings(tmp: &TempDir) -> Settings {
        let vault_dir = tmp.path().join("vault");
        Settings::new(
            EnvSource::fixed([
                (env::OWS_VAULT_PATH, vault_dir.to_str().unwrap()),
                (env::OWS_PASSPHRASE, "test-passphrase"),
            ]),
            Some(ConfigFile::at(
                tmp.path().join("config").join("config.json"),
            )),
        )
    }

    fn saved_config(tmp: &TempDir) -> Config {
        ConfigFile::at(tmp.path().join("config").join("config.json"))
            .read()
            .unwrap()
            .unwrap_or_default()
    }

    fn make_default() -> Option<ConfigEdit<'static>> {
        Some(WalletStore::set_default_edit())
    }

    #[test]
    fn next_name_skips_taken_names() {
        let tmp = TempDir::new().unwrap();
        let settings = settings(&tmp);
        let store = WalletStore::new(&settings);
        assert_eq!(store.next_name("wallet").unwrap(), "wallet");
        import_test_wallet(&tmp.path().join("vault"), "wallet");
        import_test_wallet(&tmp.path().join("vault"), "wallet-2");
        assert_eq!(store.next_name("wallet").unwrap(), "wallet-3");
    }

    #[test]
    fn add_imports_the_wallet_and_records_the_default() {
        let tmp = TempDir::new().unwrap();
        let settings = settings(&tmp);
        let store = WalletStore::new(&settings);

        let stored = store
            .add(
                &NewWallet::ImportPrivateKey(KEY),
                &WalletName::Next("imported"),
                make_default(),
            )
            .unwrap();

        assert_eq!(stored.info.name, "imported");
        assert_eq!(stored.config_keys_written, ["default_wallet_id"]);
        assert_eq!(
            saved_config(&tmp).default_wallet_id.as_deref(),
            Some(stored.info.id.as_str())
        );
        assert_eq!(store.vault().list().unwrap().len(), 1);
    }

    #[test]
    fn config_keys_written_come_from_what_the_edit_changed() {
        let tmp = TempDir::new().unwrap();
        let settings = settings(&tmp);
        let store = WalletStore::new(&settings);
        let first = store
            .add(
                &NewWallet::ImportPrivateKey(KEY),
                &WalletName::Exact("first"),
                make_default(),
            )
            .unwrap();
        assert!(
            first
                .config_keys_written
                .contains(&"default_wallet_id".to_string())
        );

        // The same file again: an edit that sets only a referral code reports
        // exactly that key, not the keys it left alone.
        let second = store
            .add(
                &NewWallet::Create,
                &WalletName::Exact("second"),
                Some(Box::new(|config, _, _| {
                    config.default_referral_code = Some("CODE".to_string());
                })),
            )
            .unwrap();
        assert_eq!(second.config_keys_written, ["default_referral_code"]);
        assert_eq!(
            saved_config(&tmp).default_wallet_id.as_deref(),
            Some(first.info.id.as_str())
        );
    }

    #[test]
    fn add_without_an_edit_leaves_the_config_alone() {
        let tmp = TempDir::new().unwrap();
        let settings = settings(&tmp);
        let store = WalletStore::new(&settings);
        store
            .add(&NewWallet::Create, &WalletName::Exact("plain"), None)
            .unwrap();
        assert!(!tmp.path().join("config").exists());
    }

    #[test]
    fn broken_config_fails_before_a_wallet_exists() {
        let tmp = TempDir::new().unwrap();
        let settings = settings(&tmp);
        let store = WalletStore::new(&settings);
        let config_path = tmp.path().join("config").join("config.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "{not json").unwrap();

        let err = store
            .add(
                &NewWallet::ImportPrivateKey(KEY),
                &WalletName::Exact("w"),
                make_default(),
            )
            .err()
            .unwrap();

        assert!(err.to_string().contains("invalid config file"));
        assert!(store.vault().list().unwrap().is_empty());
    }

    #[test]
    fn failed_config_save_rolls_the_wallet_back() {
        let tmp = TempDir::new().unwrap();
        let settings = settings(&tmp);
        let store = WalletStore::new(&settings);
        let config_path = tmp.path().join("config").join("config.json");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "{}").unwrap();
        // Replacing the file with a directory makes the final rename fail.
        let edit: ConfigEdit<'_> = Box::new(|config, wallet, _| {
            config.default_wallet_id = Some(wallet.id.clone());
            std::fs::remove_file(&config_path).unwrap();
            std::fs::create_dir(&config_path).unwrap();
        });

        let err = store
            .add(
                &NewWallet::ImportPrivateKey(KEY),
                &WalletName::Exact("w"),
                Some(edit),
            )
            .err()
            .unwrap();

        assert!(err.to_string().contains("Failed to write config file"));
        assert_eq!(crate::errors::from_anyhow(err).exit_code(), 2);
        assert!(store.vault().list().unwrap().is_empty());
    }

    #[test]
    fn blank_aliases_take_the_next_free_name() {
        for alias in [None, Some(""), Some("   ")] {
            assert!(matches!(
                WalletName::from_alias(alias, "imported"),
                WalletName::Next("imported")
            ));
        }
        assert!(matches!(
            WalletName::from_alias(Some("  main "), "imported"),
            WalletName::Exact("main")
        ));
    }

    #[test]
    fn vault_failures_keep_their_exit_codes() {
        let tmp = TempDir::new().unwrap();
        let settings = settings(&tmp);
        let store = WalletStore::new(&settings);
        import_test_wallet(&tmp.path().join("vault"), "main");

        let err = store.rename("missing", "other").unwrap_err();
        assert!(matches!(err, CliError::OwsWalletNotFound { .. }), "{err}");
        assert_eq!(err.exit_code(), 10);

        let err = store
            .add(
                &NewWallet::ImportPrivateKey(KEY),
                &WalletName::Exact("main"),
                None,
            )
            .err()
            .unwrap();
        assert_eq!(crate::errors::from_anyhow(err).exit_code(), 13);
    }

    #[test]
    fn set_default_refuses_a_wallet_without_a_hyperliquid_account_before_writing() {
        let tmp = TempDir::new().unwrap();
        let settings = settings(&tmp);
        let store = WalletStore::new(&settings);
        let mut wallet = import_test_wallet(&tmp.path().join("vault"), "main");

        let account = store.set_default(&wallet).unwrap();
        assert_eq!(account.chain_id, "eip155:1");
        assert_eq!(
            saved_config(&tmp).default_wallet_id.as_deref(),
            Some(wallet.id.as_str())
        );

        std::fs::remove_file(tmp.path().join("config").join("config.json")).unwrap();
        wallet.accounts.clear();
        let err = store.set_default(&wallet).unwrap_err();
        let err = crate::errors::from_anyhow(err);
        assert!(matches!(err, CliError::OwsNoChainAccount { .. }), "{err}");
        assert!(
            !tmp.path().join("config").join("config.json").exists(),
            "the config must not be written for a wallet that can't sign"
        );
    }

    #[test]
    fn default_markers_use_the_config_or_the_only_wallet() {
        let tmp = TempDir::new().unwrap();
        let vault_dir = tmp.path().join("vault");
        let first = import_test_wallet(&vault_dir, "first");
        let second = import_test_wallet(&vault_dir, "second");
        let both = vec![first, second];

        let unconfigured = settings(&tmp);
        let store = WalletStore::new(&unconfigured);
        assert_eq!(store.default_markers(&both[..1]).unwrap(), [true]);
        assert_eq!(store.default_markers(&both).unwrap(), [false, false]);

        store.set_default(&both[1]).unwrap();
        assert_eq!(store.default_markers(&both).unwrap(), [false, true]);
        assert!(store.is_configured_default(&both[1]).unwrap());
        assert!(!store.is_configured_default(&both[0]).unwrap());
    }

    #[test]
    fn remove_clears_a_dangling_default() {
        let tmp = TempDir::new().unwrap();
        let settings = settings(&tmp);
        let store = WalletStore::new(&settings);
        let stored = store
            .add(
                &NewWallet::ImportPrivateKey(KEY),
                &WalletName::Exact("w"),
                make_default(),
            )
            .unwrap();

        store.remove(&stored.info, true).unwrap();

        assert!(store.vault().list().unwrap().is_empty());
        assert_eq!(saved_config(&tmp).default_wallet_id, None);
    }
}
