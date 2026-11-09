//! Signer selection: which key signs an authenticated action, and which account
//! an account selector names.
//!
//! One signer precedence, written once in [`resolve_signer`]:
//! `--bankr` > `--ows-signer` > private key (flag, env or legacy config) >
//! `--keystore` > `--account` > the default wallet.
//!
//! One account resolver serves both `--account` (which must yield a signer) and
//! data lookups (which only need an address): a vault wallet by name, id or
//! Hyperliquid address, else a pre-OWS stored account. A vault that is merely
//! missing the wallet falls through to the stored accounts; a vault that fails
//! for any other reason is an error, never a silent miss.

use std::path::PathBuf;

use hypersdk::Address;
use hypersdk::hypercore::PrivateKeySigner;
use ows_lib::types::WalletInfo;

use crate::bankr::{BankrClient, KeySource};
use crate::errors::CliError;
use crate::legacy_accounts::{LegacyAccount, LegacyAccounts};
use crate::mode::Mode;
use crate::ows::vault::{HyperliquidAccount, Vault};
use crate::settings::{Settings, env};
use crate::signing::SelectedSigner;

#[derive(Debug, Clone)]
pub struct ResolvedSigner {
    selected_signer: SelectedSigner,
}

impl ResolvedSigner {
    #[must_use]
    pub fn new(selected_signer: SelectedSigner) -> Self {
        Self { selected_signer }
    }

    #[must_use]
    pub fn selected_signer(&self) -> SelectedSigner {
        self.selected_signer.clone()
    }

    #[must_use]
    pub fn address(&self) -> Address {
        self.selected_signer.address()
    }

    #[must_use]
    pub fn query_address(&self) -> Address {
        self.selected_signer.query_address()
    }
}

/// The signer inputs of one run.
#[derive(Debug, Clone, Default)]
pub struct SignerSelection {
    /// `--bankr`: sign with the wallet linked to the stored Bankr API key.
    pub bankr: bool,
    pub ows: Option<String>,
    pub private_key: Option<String>,
    pub keystore: Option<PathBuf>,
    pub keystore_password: Option<String>,
    pub account: Option<String>,
}

impl SignerSelection {
    /// Gather the run's signer inputs.
    ///
    /// `--bankr`, `--ows-signer`, `--account` and `--keystore` name a signer
    /// explicitly, so an ambient private key (`HYPERLIQUID_PRIVATE_KEY` or the
    /// legacy config key) must not override them: the key is not even read.
    pub fn from_flags(
        settings: &Settings,
        cli_private_key: Option<&str>,
        keystore: Option<PathBuf>,
        keystore_password: Option<String>,
        account: Option<String>,
        ows: Option<String>,
        bankr: bool,
    ) -> Result<Self, CliError> {
        let private_key = if bankr || account.is_some() || ows.is_some() {
            None
        } else if keystore.is_some() {
            cli_private_key
                .filter(|key| !key.is_empty())
                .map(str::to_string)
        } else {
            settings.private_key(cli_private_key)?
        };
        Ok(Self {
            bankr,
            ows,
            private_key,
            keystore,
            keystore_password,
            account,
        })
    }
}

/// Parse a 0x-prefixed 32-byte private key into a signer.
pub fn parse_private_key(private_key: &str) -> Result<PrivateKeySigner, CliError> {
    private_key.parse::<PrivateKeySigner>().map_err(|_| {
        CliError::InvalidAuth("private key must be a 32-byte 0x-prefixed hex string".to_string())
    })
}

/// The 0x-prefixed hex form of `signer`'s key.
pub fn signer_private_key_hex(signer: &PrivateKeySigner) -> String {
    format!("0x{}", hex::encode(signer.to_bytes()))
}

/// Resolve the run's signer inputs into one signer, first match wins.
pub fn resolve_signer(
    selection: &SignerSelection,
    settings: &Settings,
    mode: &Mode,
) -> Result<ResolvedSigner, anyhow::Error> {
    let signer = if selection.bankr {
        resolve_bankr(settings)?
    } else if let Some(selector) = selection.ows.as_deref() {
        resolve_ows_flag(selector, settings)?
    } else if let Some(private_key) = selection.private_key.as_deref() {
        SelectedSigner::private_key(parse_private_key(private_key)?)
    } else if let Some(path) = selection.keystore.as_deref() {
        let password = selection
            .keystore_password
            .clone()
            .or_else(|| settings.keystore_password())
            .ok_or_else(|| {
                CliError::InvalidAuth(format!(
                    "keystore password is required with --keystore; set {} or pass --keystore-password",
                    env::KEYSTORE_PASSWORD
                ))
            })?;
        SelectedSigner::keystore(
            PrivateKeySigner::decrypt_keystore(path, &password).map_err(|err| {
                CliError::InvalidAuth(format!("failed to decrypt keystore: {err}"))
            })?,
        )
    } else if let Some(selector) = selection.account.as_deref() {
        find_account(selector, settings, mode)?.into_signer()?
    } else {
        find_default(settings, mode)?.into_signer()?
    };
    Ok(ResolvedSigner::new(signer))
}

/// `--bankr`: the wallet linked to the stored Bankr API key. The address comes
/// from the config written by `wallet login`; a key that no login recorded (an
/// environment variable, or the Bankr CLI's own config) is looked up once.
fn resolve_bankr(settings: &Settings) -> Result<SelectedSigner, anyhow::Error> {
    let (client, source) = BankrClient::from_settings(settings)?;
    // The cached address belongs to the key `wallet login` stored, against the
    // endpoint it stored. A key from the environment or the Bankr CLI, or a run
    // pointed at another endpoint, may belong to another wallet.
    let login = settings.bankr_login()?;
    let cached = match source {
        KeySource::Config if crate::bankr::login_endpoint(&login)? == client.api_url() => {
            login.address()?
        }
        _ => None,
    };
    let address = match cached {
        Some(address) => address,
        None => client.blocking_wallet()?.address,
    };
    Ok(SelectedSigner::bankr(client, address))
}

/// `--ows-signer`: a vault wallet by name or id, or a raw address that can only
/// preview. A raw address never matches a vault wallet here, unlike `--account`,
/// and a wallet the vault doesn't hold is an error, never a stored account.
fn resolve_ows_flag(selector: &str, settings: &Settings) -> Result<SelectedSigner, CliError> {
    let selector = selector.trim();
    if selector.is_empty() {
        return Err(CliError::Configuration(
            "--ows-signer requires a non-empty selector".to_string(),
        ));
    }
    if let Ok(address) = selector.parse::<Address>() {
        return Ok(SelectedSigner::ows_address_only(
            selector.to_string(),
            address,
        ));
    }
    let vault = Vault::from_settings(settings);
    let (wallet, account) = vault_account_by_name(&vault, selector)?;
    Ok(SelectedSigner::vault_wallet(
        &wallet,
        account.address,
        vault,
    ))
}

/// The wallet account an account selector or default resolved to.
enum Found {
    Vault {
        wallet: WalletInfo,
        account: HyperliquidAccount,
        vault: Vault,
    },
    Legacy {
        store: LegacyAccounts,
        account: LegacyAccount,
    },
}

impl Found {
    /// The address account data is queried for.
    fn query_address(&self) -> Result<Address, CliError> {
        match self {
            Self::Vault { account, .. } => Ok(account.address),
            Self::Legacy { account, .. } => account.query_address(),
        }
    }

    fn into_signer(self) -> Result<SelectedSigner, anyhow::Error> {
        match self {
            Self::Vault {
                wallet,
                account,
                vault,
            } => Ok(SelectedSigner::vault_wallet(
                &wallet,
                account.address,
                vault,
            )),
            Self::Legacy { store, account } => {
                let signer = parse_private_key(&store.decrypt_private_key(&account)?)?;
                let query_address = account.query_address()?;
                Ok(SelectedSigner::stored_account(
                    signer,
                    account.alias,
                    query_address,
                ))
            }
        }
    }
}

/// Whether `err` says the vault simply lacks what was asked for, as opposed to
/// failing. Only a miss may fall through to the pre-OWS stored accounts.
fn is_vault_miss(err: &CliError) -> bool {
    matches!(
        err,
        CliError::OwsWalletNotFound { .. } | CliError::OwsNoChainAccount { .. }
    )
}

fn vault_account_by_name(
    vault: &Vault,
    selector: &str,
) -> Result<(WalletInfo, HyperliquidAccount), CliError> {
    let wallet = vault.get(selector)?;
    let account = Vault::hyperliquid_account(&wallet)?;
    Ok((wallet, account))
}

/// The vault wallet named by `selector`: by name or id, or by its Hyperliquid
/// address (compared case-insensitively). `None` when the vault holds no such
/// wallet; any other vault failure is an error.
fn find_vault_account(
    vault: &Vault,
    selector: &str,
) -> Result<Option<(WalletInfo, HyperliquidAccount)>, CliError> {
    if selector.parse::<Address>().is_ok() {
        for wallet in vault.list()? {
            if let Ok(account) = Vault::hyperliquid_account(&wallet)
                && account.address_text.eq_ignore_ascii_case(selector)
            {
                return Ok(Some((wallet, account)));
            }
        }
        return Ok(None);
    }
    match vault_account_by_name(vault, selector) {
        Ok(found) => Ok(Some(found)),
        Err(err) if is_vault_miss(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

/// Ask on the terminal for the passphrase of pre-OWS stored accounts when the OS
/// keychain can't unlock them, if this run may prompt at all.
fn prompt_account_passphrase(
    mode: &Mode,
    keychain_error: &anyhow::Error,
) -> Result<String, anyhow::Error> {
    if !mode.can_prompt() || !crate::output::terminal::stdin_is_terminal() {
        return Err(CliError::MissingInput(format!(
            "Can't unlock account encryption keys because the OS keychain is unavailable ({keychain_error}). Set {} to use a passphrase instead.",
            env::ACCOUNT_KEY_PASSPHRASE
        ))
        .into());
    }

    eprintln!(
        "OS keychain is unavailable ({keychain_error}). Using a passphrase-derived account encryption key instead."
    );
    Ok(crate::prompt::read_secret(
        mode,
        "Account encryption passphrase: ",
        env::ACCOUNT_KEY_PASSPHRASE,
    )?)
}

/// The pre-OWS stored account chosen by `lookup`, when the user still has a
/// store. Having none, or no account in it, is `missing`.
fn legacy_account(
    settings: &Settings,
    mode: &Mode,
    missing: impl Fn() -> CliError,
    lookup: impl FnOnce(&LegacyAccounts) -> Result<Option<LegacyAccount>, anyhow::Error>,
) -> Result<Found, anyhow::Error> {
    let prompt = |keychain_error: &anyhow::Error| prompt_account_passphrase(mode, keychain_error);
    let Some(store) = LegacyAccounts::open_default(settings, &prompt)? else {
        return Err(missing().into());
    };
    let account = lookup(&store)?.ok_or_else(missing)?;
    Ok(Found::Legacy { store, account })
}

/// The account `selector` names: a vault wallet, else a pre-OWS stored account.
fn find_account(selector: &str, settings: &Settings, mode: &Mode) -> Result<Found, anyhow::Error> {
    let selector = selector.trim();
    let vault = Vault::from_settings(settings);
    if let Some((wallet, account)) = find_vault_account(&vault, selector)? {
        return Ok(Found::Vault {
            wallet,
            account,
            vault,
        });
    }
    legacy_account(
        settings,
        mode,
        || account_selector_not_found(selector),
        |store| store.account_by_selector(selector),
    )
}

/// The configured default wallet, else the only vault wallet, else the default
/// of the pre-OWS stored accounts.
fn find_default(settings: &Settings, mode: &Mode) -> Result<Found, anyhow::Error> {
    let vault = Vault::from_settings(settings);
    let configured = settings.default_wallet_id()?;
    match vault.default_wallet(configured.as_deref()) {
        Ok(wallet) => {
            let account = Vault::hyperliquid_account(&wallet)?;
            return Ok(Found::Vault {
                wallet,
                account,
                vault,
            });
        }
        Err(err) if is_vault_miss(&err) || matches!(err, CliError::AuthRequired) => {}
        Err(err) => return Err(err.into()),
    }
    legacy_account(
        settings,
        mode,
        || CliError::AuthRequired,
        LegacyAccounts::default_or_first_account,
    )
}

pub(crate) fn account_selector_not_found(selector: &str) -> CliError {
    CliError::Unsupported(format!(
        "account selector '{selector}' was not found as an address, alias, or id"
    ))
}

/// The address an account selector stands for in data lookups: the address itself,
/// a vault wallet's Hyperliquid address, or a stored account's query address.
pub(crate) fn lookup_account_address(
    selector: &str,
    settings: &Settings,
    mode: &Mode,
) -> Result<Address, anyhow::Error> {
    let trimmed = selector.trim();
    if let Ok(address) = trimmed.parse::<Address>() {
        return Ok(address);
    }

    if trimmed.starts_with("0x") {
        return Err(CliError::Unsupported(format!(
            "Invalid address: {trimmed}; expected a 0x-prefixed 40-hex-character (20-byte) string"
        ))
        .into());
    }

    Ok(find_account(trimmed, settings, mode)?.query_address()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ows::vault::test_support::import_test_wallet;
    use crate::settings::EnvSource;
    use tempfile::TempDir;

    fn resolve(
        selection: &SignerSelection,
        settings: &Settings,
    ) -> Result<ResolvedSigner, anyhow::Error> {
        resolve_signer(selection, settings, &Mode::default())
    }

    const KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000009";
    const RAW_OWS_ADDRESS: &str = "0x0000000000000000000000000000000000000001";

    fn vault_settings(vault: &TempDir, config: Option<Config>) -> Settings {
        Settings::detached(
            EnvSource::fixed([(env::OWS_VAULT_PATH, vault.path().to_str().unwrap())]),
            config,
        )
    }

    fn selection() -> SignerSelection {
        SignerSelection::default()
    }

    #[test]
    fn parses_private_key_and_derives_address() {
        let signer = parse_private_key(KEY).unwrap();
        assert!(signer.address().to_string().starts_with("0x"));
        assert_eq!(signer_private_key_hex(&signer), KEY);
    }

    #[test]
    fn invalid_private_key_maps_to_auth_error() {
        assert_eq!(parse_private_key("not-a-key").unwrap_err().exit_code(), 10);
    }

    #[test]
    fn ows_flag_beats_every_other_source_and_raw_address_cannot_sign() {
        let vault = TempDir::new().unwrap();
        let resolved = resolve(
            &SignerSelection {
                ows: Some(RAW_OWS_ADDRESS.to_string()),
                private_key: Some(KEY.to_string()),
                keystore: Some(PathBuf::from("/nonexistent")),
                account: Some("main".to_string()),
                ..selection()
            },
            &vault_settings(&vault, None),
        )
        .unwrap();

        assert_eq!(resolved.address().to_string(), RAW_OWS_ADDRESS);
        assert_eq!(resolved.query_address(), resolved.address());
        let description = resolved.selected_signer().describe();
        assert_eq!(description.alias, None);
        assert_eq!(
            description.source,
            format!("OWS signer ({RAW_OWS_ADDRESS})")
        );
        assert!(
            resolved
                .selected_signer()
                .ensure_can_attempt_live_signing()
                .is_err()
        );
    }

    #[test]
    fn ows_flag_does_not_match_a_vault_wallet_by_address_but_account_does() {
        let vault = TempDir::new().unwrap();
        let wallet = import_test_wallet(vault.path(), "main");
        let account = Vault::hyperliquid_account(&wallet).unwrap();
        let settings = vault_settings(&vault, None);

        let by_flag = resolve(
            &SignerSelection {
                ows: Some(account.address_text.clone()),
                ..selection()
            },
            &settings,
        )
        .unwrap();
        assert!(
            by_flag
                .selected_signer()
                .ensure_can_attempt_live_signing()
                .is_err()
        );

        let by_account = resolve(
            &SignerSelection {
                account: Some(account.address_text.to_lowercase()),
                ..selection()
            },
            &settings,
        )
        .unwrap();
        assert!(
            by_account
                .selected_signer()
                .ensure_can_attempt_live_signing()
                .is_ok()
        );
        assert_eq!(by_account.address(), account.address);
        let description = by_account.selected_signer().describe();
        assert_eq!(description.alias.as_deref(), Some("main"));
        assert_eq!(description.source, "OWS wallet (main)");
    }

    #[test]
    fn ows_flag_resolves_wallet_by_name_and_keeps_the_selector() {
        let vault = TempDir::new().unwrap();
        let wallet = import_test_wallet(vault.path(), "main");
        let resolved = resolve(
            &SignerSelection {
                ows: Some(format!(" {} ", wallet.id)),
                ..selection()
            },
            &vault_settings(&vault, None),
        )
        .unwrap();

        assert_eq!(
            resolved.selected_signer().describe().alias.as_deref(),
            Some("main")
        );
    }

    #[test]
    fn empty_ows_selector_is_a_configuration_error() {
        let vault = TempDir::new().unwrap();
        let err = resolve(
            &SignerSelection {
                ows: Some("  ".to_string()),
                ..selection()
            },
            &vault_settings(&vault, None),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--ows-signer requires"));
    }

    #[test]
    fn private_key_beats_keystore() {
        let vault = TempDir::new().unwrap();
        let resolved = resolve(
            &SignerSelection {
                private_key: Some(KEY.to_string()),
                keystore: Some(PathBuf::from("/nonexistent")),
                ..selection()
            },
            &vault_settings(&vault, None),
        )
        .unwrap();
        assert_eq!(
            resolved.selected_signer().describe().source,
            "private key/config/env"
        );
        assert_eq!(
            resolved.address(),
            parse_private_key(KEY).unwrap().address()
        );
    }

    #[test]
    fn keystore_requires_a_password() {
        let vault = TempDir::new().unwrap();
        let err = resolve(
            &SignerSelection {
                keystore: Some(PathBuf::from("/nonexistent")),
                ..selection()
            },
            &vault_settings(&vault, None),
        )
        .unwrap_err();
        assert!(err.to_string().contains(env::KEYSTORE_PASSWORD));
        assert_eq!(
            err.downcast_ref::<CliError>().map(CliError::exit_code),
            Some(10)
        );
    }

    #[test]
    fn default_is_the_configured_wallet_or_the_only_wallet() {
        let vault = TempDir::new().unwrap();
        let first = import_test_wallet(vault.path(), "first");

        let only = resolve(&selection(), &vault_settings(&vault, None)).unwrap();
        let name = |signer: &ResolvedSigner| signer.selected_signer().describe().alias;
        assert_eq!(name(&only).as_deref(), Some("first"));

        import_test_wallet(vault.path(), "second");
        let config = Config {
            default_wallet_id: Some(first.id.clone()),
            ..Config::default()
        };
        let configured = resolve(&selection(), &vault_settings(&vault, Some(config))).unwrap();
        assert_eq!(name(&configured).as_deref(), Some("first"));

        let err = resolve(&selection(), &vault_settings(&vault, None)).unwrap_err();
        assert!(err.to_string().contains("holds 2 wallets"));
    }

    fn flags(
        settings: &Settings,
        cli_key: Option<&str>,
        keystore: Option<&str>,
        account: Option<&str>,
    ) -> SignerSelection {
        SignerSelection::from_flags(
            settings,
            cli_key,
            keystore.map(PathBuf::from),
            None,
            account.map(str::to_string),
            None,
            false,
        )
        .unwrap()
    }

    #[test]
    fn selection_ignores_ambient_key_when_a_signer_is_named() {
        let ambient = Settings::detached(EnvSource::fixed([(env::PRIVATE_KEY, KEY)]), None);

        assert_eq!(flags(&ambient, None, None, Some("main")).private_key, None);
        assert_eq!(
            flags(&ambient, None, None, None).private_key.as_deref(),
            Some(KEY)
        );
        let bare = Settings::detached(EnvSource::fixed::<&str, &str>([]), None);
        assert_eq!(flags(&bare, None, None, None).private_key, None);
    }

    #[test]
    fn an_explicit_keystore_is_not_overridden_by_an_ambient_key() {
        let ambient = Settings::detached(EnvSource::fixed([(env::PRIVATE_KEY, KEY)]), None);

        let selection = flags(&ambient, None, Some("/keystore.json"), None);
        assert_eq!(selection.private_key, None);
        assert_eq!(selection.keystore, Some(PathBuf::from("/keystore.json")));

        // A key given on the command line is as explicit as the keystore.
        let both = flags(&ambient, Some(KEY), Some("/keystore.json"), None);
        assert_eq!(both.private_key.as_deref(), Some(KEY));
    }

    /// A vault holding two wallets named `main`, which `ows-lib` itself refuses
    /// to create: asking for `main` is a vault failure, not a missing wallet.
    fn vault_with_duplicate_names() -> (TempDir, Settings) {
        let vault = TempDir::new().unwrap();
        import_test_wallet(vault.path(), "main");
        let wallets = vault.path().join("wallets");
        let original = std::fs::read_dir(&wallets)
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        std::fs::copy(original.path(), wallets.join("copy.json")).unwrap();
        let settings = vault_settings(&vault, None);
        (vault, settings)
    }

    #[test]
    fn a_vault_that_fails_is_an_error_not_a_missing_account() {
        let (_vault, settings) = vault_with_duplicate_names();
        let ambiguous = |err: &anyhow::Error| err.to_string().contains("matched 2 wallets");

        let signed = resolve(
            &SignerSelection {
                account: Some("main".to_string()),
                ..selection()
            },
            &settings,
        )
        .unwrap_err();
        assert!(ambiguous(&signed), "signing: {signed}");

        // Data lookups used to read any vault failure as "not in the vault".
        let lookup = lookup_account_address("main", &settings, &Mode::default()).unwrap_err();
        assert!(ambiguous(&lookup), "lookup: {lookup}");

        let config = Config {
            default_wallet_id: Some("main".to_string()),
            ..Config::default()
        };
        let default = resolve(&selection(), &vault_settings(&_vault, Some(config))).unwrap_err();
        assert!(ambiguous(&default), "default: {default}");
    }
}
