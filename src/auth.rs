//! Signer selection: which key signs an authenticated action, and which account
//! an account selector names.
//!
//! One signer precedence, written once in [`SignerSelection::from_flags`]:
//! `--bankr` > `--ows-signer` > `--keystore` > `--account` > a private key
//! (flag, `HYPERLIQUID_PRIVATE_KEY` or the legacy config key) > the default
//! wallet. A flag that names a signer means the ambient key is not even read.
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

use crate::bankr::BankrClient;
use crate::bankr_credentials::KeySource;
use crate::env;
use crate::errors::CliError;
use crate::legacy_accounts::{LegacyAccount, LegacyAccounts};
use crate::mode::Mode;
use crate::ows::vault::{HyperliquidAccount, Vault};
use crate::settings::Settings;
use crate::signing::SelectedSigner;
use crate::wire::selector_address;

/// The signer flags, flattened into the global options and read by
/// [`SignerSelection::from_flags`]. Naming two signers is refused by clap, so
/// the conflicts are declared here, beside the precedence they guard. They
/// stay per-flag lists because a conflict declared through an `ArgGroup` does
/// not reach a flag given after the subcommand.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct SignerFlags {
    /// Private key to sign with. Prefer HYPERLIQUID_PRIVATE_KEY: command-line options appear in process lists
    #[arg(long, global = true)]
    pub private_key: Option<String>,

    /// Foundry-compatible keystore file to decrypt for signing
    #[arg(long, global = true, conflicts_with = "private_key")]
    pub keystore: Option<PathBuf>,

    /// Password for --keystore. Prefer HYPERLIQUID_KEYSTORE_PASSWORD: command-line options appear in process lists
    #[arg(long, global = true)]
    pub keystore_password: Option<String>,

    /// Wallet name, id, or address to use as the signer
    #[arg(
        long,
        global = true,
        conflicts_with_all = ["private_key", "keystore", "keystore_password", "ows_signer"]
    )]
    pub account: Option<String>,

    /// OWS signer selector (0x address, or wallet name/id from the OWS vault)
    #[arg(
        long,
        global = true,
        value_name = "SELECTOR",
        alias = "wallet",
        conflicts_with_all = ["private_key", "keystore", "keystore_password", "account"]
    )]
    pub ows_signer: Option<String>,

    /// Sign with the Bankr wallet linked by `wallet login` (or BANKR_API_KEY)
    #[arg(
        long,
        global = true,
        conflicts_with_all = ["private_key", "keystore", "keystore_password", "account", "ows_signer"]
    )]
    pub bankr: bool,
}

#[derive(Debug, Clone)]
pub enum SignerSelection {
    Bankr,
    Ows(String),
    Keystore {
        path: PathBuf,
        password: Option<String>,
    },
    Account(String),
    PrivateKey(String),
    /// No signer was named: the default wallet.
    Default,
}

impl SignerSelection {
    /// Pick the run's signer from its flags and ambient key. Clap rejects
    /// `--private-key` together with another signer flag.
    pub fn from_flags(settings: &Settings, flags: &SignerFlags) -> Result<Self, CliError> {
        if flags.bankr {
            return Ok(Self::Bankr);
        }
        if let Some(selector) = &flags.ows_signer {
            return Ok(Self::Ows(selector.clone()));
        }
        if let Some(path) = &flags.keystore {
            return Ok(Self::Keystore {
                path: path.clone(),
                password: flags.keystore_password.clone(),
            });
        }
        if let Some(selector) = &flags.account {
            return Ok(Self::Account(selector.clone()));
        }
        Ok(settings
            .private_key(flags.private_key.as_deref())?
            .map_or(Self::Default, Self::PrivateKey))
    }
}

pub fn parse_private_key(private_key: &str) -> Result<PrivateKeySigner, CliError> {
    private_key.parse::<PrivateKeySigner>().map_err(|_| {
        CliError::InvalidAuth("private key must be a 32-byte 0x-prefixed hex string".to_string())
    })
}

pub fn signer_private_key_hex(signer: &PrivateKeySigner) -> String {
    format!("0x{}", hex::encode(signer.to_bytes()))
}

pub fn resolve_signer(
    selection: &SignerSelection,
    settings: &Settings,
    mode: &Mode,
) -> Result<SelectedSigner, anyhow::Error> {
    Ok(match selection {
        SignerSelection::Bankr => resolve_bankr(settings)?,
        SignerSelection::Ows(selector) => resolve_ows_flag(selector, settings)?,
        SignerSelection::Keystore { path, password } => {
            let password = password
                .clone()
                .or_else(|| settings.keystore_password())
                .ok_or_else(|| {
                    CliError::InvalidAuth(format!(
                        "keystore password is required with --keystore; set {} or pass --keystore-password",
                        env::KEYSTORE_PASSWORD
                    ))
                })?;
            SelectedSigner::keystore(PrivateKeySigner::decrypt_keystore(path, &password).map_err(
                |err| CliError::InvalidAuth(format!("failed to decrypt keystore: {err}")),
            )?)
        }
        SignerSelection::Account(selector) => {
            find_account(selector, settings, mode)?.into_signer()?
        }
        SignerSelection::PrivateKey(private_key) => {
            SelectedSigner::private_key(parse_private_key(private_key)?)
        }
        SignerSelection::Default => find_default(settings, mode)?.into_signer()?,
    })
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
        None => client.blocking_wallet()?,
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
    if let Some(address) = selector_address(selector) {
        return Ok(SelectedSigner::ows_address_only(
            selector.to_string(),
            address,
        ));
    }
    let vault = Vault::from_settings(settings);
    let wallet = vault.get(selector)?;
    SelectedSigner::vault_wallet(&vault, &wallet)
}

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
    fn query_address(&self) -> Result<Address, CliError> {
        match self {
            Self::Vault { account, .. } => Ok(account.address),
            Self::Legacy { account, .. } => account.query_address(),
        }
    }

    fn into_signer(self) -> Result<SelectedSigner, anyhow::Error> {
        match self {
            Self::Vault { wallet, vault, .. } => Ok(SelectedSigner::vault_wallet(&vault, &wallet)?),
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

/// The vault wallet an account selector names (see [`Vault::select`]) with the
/// account it signs as. `None` when the vault holds no such wallet, or the
/// wallet has no Hyperliquid account; any other vault failure is an error.
fn find_vault_account(
    vault: &Vault,
    selector: &str,
) -> Result<Option<(WalletInfo, HyperliquidAccount)>, CliError> {
    let found = vault.select(selector).and_then(|wallet| {
        let account = Vault::hyperliquid_account(&wallet)?;
        Ok((wallet, account))
    });
    match found {
        Ok(found) => Ok(Some(found)),
        Err(err) if is_vault_miss(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

fn prompt_account_passphrase(
    mode: &Mode,
    keychain_error: &anyhow::Error,
) -> Result<String, anyhow::Error> {
    if !mode.can_prompt() || !mode.terminal.stdin {
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
    if let Some(address) = selector_address(trimmed) {
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
    use crate::env::EnvSource;
    use crate::ows::vault::test_support::import_test_wallet;
    use crate::signing::Signs;
    use tempfile::TempDir;

    fn resolve(
        selection: &SignerSelection,
        settings: &Settings,
    ) -> Result<SelectedSigner, anyhow::Error> {
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

    #[test]
    fn parses_private_key_and_derives_its_address() {
        let signer = parse_private_key(KEY).unwrap();
        assert_eq!(
            signer.address().to_string(),
            "0xF7Edc8FA1eCc32967F827C9043FcAe6ba73afA5c"
        );
        assert_eq!(signer_private_key_hex(&signer), KEY);
    }

    #[test]
    fn invalid_private_key_maps_to_auth_error() {
        assert_eq!(parse_private_key("not-a-key").unwrap_err().exit_code(), 10);
    }

    #[test]
    fn ows_address_only_selection_has_an_identity_but_no_key() {
        let vault = TempDir::new().unwrap();
        let resolved = resolve(
            &SignerSelection::Ows(RAW_OWS_ADDRESS.to_string()),
            &vault_settings(&vault, None),
        )
        .unwrap();

        assert_eq!(resolved.address().to_string(), RAW_OWS_ADDRESS);
        assert_eq!(resolved.query_address(), resolved.address());
        let description = resolved.describe();
        assert_eq!(description.alias, None);
        assert_eq!(
            description.source,
            format!("OWS signer ({RAW_OWS_ADDRESS})")
        );
        assert!(resolved.ensure_covers(Signs::L1Action).is_ok());
        assert!(resolved.ensure_holds_key().is_err());
    }

    #[test]
    fn ows_flag_does_not_match_a_vault_wallet_by_address_but_account_does() {
        let vault = TempDir::new().unwrap();
        let wallet = import_test_wallet(vault.path(), "main");
        let account = Vault::hyperliquid_account(&wallet).unwrap();
        let settings = vault_settings(&vault, None);

        let by_flag = resolve(
            &SignerSelection::Ows(account.address_text.clone()),
            &settings,
        )
        .unwrap();
        assert!(by_flag.ensure_holds_key().is_err());

        let by_account = resolve(
            &SignerSelection::Account(account.address_text.to_lowercase()),
            &settings,
        )
        .unwrap();
        assert!(by_account.ensure_holds_key().is_ok());
        assert_eq!(by_account.address(), account.address);
        let description = by_account.describe();
        assert_eq!(description.alias.as_deref(), Some("main"));
        assert_eq!(description.source, "OWS wallet (main)");
    }

    #[test]
    fn ows_flag_resolves_wallet_by_name_and_keeps_the_selector() {
        let vault = TempDir::new().unwrap();
        let wallet = import_test_wallet(vault.path(), "main");
        let resolved = resolve(
            &SignerSelection::Ows(format!(" {} ", wallet.id)),
            &vault_settings(&vault, None),
        )
        .unwrap();

        assert_eq!(resolved.describe().alias.as_deref(), Some("main"));
    }

    #[test]
    fn empty_ows_selector_is_a_configuration_error() {
        let vault = TempDir::new().unwrap();
        let err = resolve(
            &SignerSelection::Ows("  ".to_string()),
            &vault_settings(&vault, None),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--ows-signer requires"));
    }

    #[test]
    fn a_private_key_selection_signs_with_that_key() {
        let vault = TempDir::new().unwrap();
        let resolved = resolve(
            &SignerSelection::PrivateKey(KEY.to_string()),
            &vault_settings(&vault, None),
        )
        .unwrap();
        assert_eq!(resolved.describe().source, "private key/config/env");
        assert_eq!(
            resolved.address(),
            parse_private_key(KEY).unwrap().address()
        );
    }

    #[test]
    fn keystore_requires_a_password() {
        let vault = TempDir::new().unwrap();
        let err = resolve(
            &SignerSelection::Keystore {
                path: PathBuf::from("/nonexistent"),
                password: None,
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

        let only = resolve(&SignerSelection::Default, &vault_settings(&vault, None)).unwrap();
        let name = |signer: &SelectedSigner| signer.describe().alias;
        assert_eq!(name(&only).as_deref(), Some("first"));

        import_test_wallet(vault.path(), "second");
        let config = Config {
            default_wallet_id: Some(first.id.clone()),
            ..Config::default()
        };
        let configured = resolve(
            &SignerSelection::Default,
            &vault_settings(&vault, Some(config)),
        )
        .unwrap();
        assert_eq!(name(&configured).as_deref(), Some("first"));

        let err = resolve(&SignerSelection::Default, &vault_settings(&vault, None)).unwrap_err();
        assert!(err.to_string().contains("holds 2 wallets"));
    }

    fn selected(settings: &Settings, flags: SignerFlags) -> SignerSelection {
        SignerSelection::from_flags(settings, &flags).unwrap()
    }

    fn ambient_key() -> Settings {
        Settings::detached(EnvSource::fixed([(env::PRIVATE_KEY, KEY)]), None)
    }

    #[test]
    fn flags_pick_one_signer_in_the_documented_order() {
        let ambient = ambient_key();
        let everything = SignerFlags {
            bankr: true,
            ows_signer: Some("ows".to_string()),
            keystore: Some(PathBuf::from("/keystore.json")),
            keystore_password: Some("pw".to_string()),
            account: Some("main".to_string()),
            private_key: Some(KEY.to_string()),
        };
        assert!(matches!(
            selected(&ambient, everything.clone()),
            SignerSelection::Bankr
        ));

        let without_bankr = SignerFlags {
            bankr: false,
            ..everything
        };
        let SignerSelection::Ows(selector) = selected(&ambient, without_bankr.clone()) else {
            panic!("--ows-signer comes right after --bankr");
        };
        assert_eq!(selector, "ows");

        let without_ows = SignerFlags {
            ows_signer: None,
            ..without_bankr
        };
        let SignerSelection::Keystore { path, password } = selected(&ambient, without_ows.clone())
        else {
            panic!("--keystore comes before --account");
        };
        assert_eq!(path, PathBuf::from("/keystore.json"));
        assert_eq!(password.as_deref(), Some("pw"));

        let without_keystore = SignerFlags {
            keystore: None,
            ..without_ows
        };
        let SignerSelection::Account(selector) = selected(&ambient, without_keystore.clone())
        else {
            panic!("--account comes before a private key");
        };
        assert_eq!(selector, "main");

        let key_only = SignerFlags {
            account: None,
            ..without_keystore
        };
        let SignerSelection::PrivateKey(key) = selected(&ambient, key_only) else {
            panic!("a private key is the last explicit choice");
        };
        assert_eq!(key, KEY);
    }

    #[test]
    fn a_named_signer_means_the_ambient_key_is_not_even_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{"private_key": 5}"#).unwrap();
        let unreadable_key = Settings::new(
            EnvSource::fixed::<&str, &str>([]),
            Some(crate::config::ConfigFile::at(path)),
        );

        assert!(SignerSelection::from_flags(&unreadable_key, &SignerFlags::default()).is_err());
        for flags in [
            SignerFlags {
                account: Some("main".to_string()),
                ..SignerFlags::default()
            },
            SignerFlags {
                keystore: Some(PathBuf::from("/keystore.json")),
                ..SignerFlags::default()
            },
            SignerFlags {
                bankr: true,
                ..SignerFlags::default()
            },
        ] {
            assert!(SignerSelection::from_flags(&unreadable_key, &flags).is_ok());
        }
    }

    #[test]
    fn the_ambient_key_is_the_selection_only_when_nothing_else_is_named() {
        let SignerSelection::PrivateKey(key) = selected(&ambient_key(), SignerFlags::default())
        else {
            panic!("the environment key is selected");
        };
        assert_eq!(key, KEY);

        let bare = Settings::detached(EnvSource::fixed::<&str, &str>([]), None);
        assert!(matches!(
            selected(&bare, SignerFlags::default()),
            SignerSelection::Default
        ));
        let flag = SignerFlags {
            private_key: Some(KEY.to_string()),
            ..SignerFlags::default()
        };
        assert!(matches!(
            selected(&bare, flag),
            SignerSelection::PrivateKey(key) if key == KEY
        ));
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

        let signed = resolve(&SignerSelection::Account("main".to_string()), &settings).unwrap_err();
        assert!(ambiguous(&signed), "signing: {signed}");

        let lookup = lookup_account_address("main", &settings, &Mode::default()).unwrap_err();
        assert!(ambiguous(&lookup), "lookup: {lookup}");

        let config = Config {
            default_wallet_id: Some("main".to_string()),
            ..Config::default()
        };
        let default = resolve(
            &SignerSelection::Default,
            &vault_settings(&_vault, Some(config)),
        )
        .unwrap_err();
        assert!(ambiguous(&default), "default: {default}");
    }

    #[test]
    fn literal_addresses_resolve_without_touching_the_vault() {
        let settings = Settings::detached(EnvSource::fixed::<&str, &str>([]), None);
        let address = "0x0000000000000000000000000000000000000001";
        let user =
            lookup_account_address(&format!(" {address} "), &settings, &Mode::default()).unwrap();
        assert_eq!(user.to_string().to_lowercase(), address);

        let err = lookup_account_address("0xnothex", &settings, &Mode::default()).unwrap_err();
        assert!(err.to_string().contains("Invalid address"));
    }
}
