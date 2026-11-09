//! Wallet and account management commands.
//!
//! OWS (Open Wallet Standard) is the only wallet backend: creation, import,
//! listing and signing all go through the OWS vault (default `~/.hyperliquid`,
//! or `HYPERLIQUID_OWS_VAULT_PATH`). The `wallet` and `account` command families
//! are two surfaces over the same [`WalletStore`] workflow.

mod output;

pub use output::{AccountRow, AccountsOutput};

use clap::Args;
use ows_lib::types::WalletInfo;

use crate::command_spec::{
    CliCommand, CommandSpec, DryRunPolicy, GroupSpec, InputKind, Lifecycle, NoArgs, OwsSupport,
    Risk, arg,
};
use crate::commands::support::print_dry_run;
use crate::config::{BankrLogin, Config, nonblank};
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::mode::Mode;
use crate::output::{self as out, OutputFormat};
use crate::ows::WalletSource;
use crate::ows::hints::{
    self, SET_DEFAULT, SETUP, WALLET_IMPORT, WALLET_IMPORT_MNEMONIC, WALLET_LOGIN,
};
use crate::ows::vault::{HyperliquidAccount, Vault};
use crate::prompt;
use crate::settings::{KeySource, Settings, env};
use crate::signing::SelectedSigner;
use crate::wallet_store::{ConfigEdit, NewWallet, StoredWallet, WalletName, WalletStore};
use crate::{builder_fee, referral_code};
use output::{
    BankrStatusOutput, WalletReceipt, print_declined, print_status, print_wallet_address, shell_arg,
};

/// What a secret passed as an argument exposes it to; `prefer` is the command
/// to run without it and what to paste at its prompt, when there is one to
/// recommend.
fn argv_secret_warning(noun: &str, prefer: Option<(&str, &str)>) -> String {
    let advice = prefer
        .map(|(command, paste)| format!(" Prefer `{command}` and paste {paste}."))
        .unwrap_or_default();
    format!(
        "passing {noun} as an argument can expose it in OS process listings and shell history.{advice}"
    )
}

/// A secret from its command-line argument, which is warned about, or from the
/// hidden prompt. Trimmed either way.
fn secret_arg_or_prompt(
    argument: Option<&str>,
    noun: &str,
    prefer: Option<(&str, &str)>,
    prompt: impl FnOnce() -> Result<String, CliError>,
) -> Result<String, CliError> {
    match argument {
        Some(secret) => {
            out::warn(&argv_secret_warning(noun, prefer));
            Ok(secret.trim().to_string())
        }
        None => Ok(prompt()?.trim().to_string()),
    }
}

fn require_promptable(input: &str, format: OutputFormat, mode: &Mode) -> Result<(), CliError> {
    if format == OutputFormat::Json {
        return Err(CliError::MissingInput(format!(
            "Can't prompt for {input} because of JSON output. Pass {input} instead."
        )));
    }
    prompt::ensure_can_prompt(mode, input)
}

fn no_stored_accounts_error() -> CliError {
    CliError::Unsupported(format!(
        "no stored wallets found; run {SETUP} or {WALLET_IMPORT}"
    ))
}

/// Fill the builder and referral defaults the config lacks. A default that
/// fails validation must not block wallet creation, so it is skipped and
/// reported in the returned warnings.
fn apply_setup_defaults(config: &mut Config, settings: &Settings) -> Vec<String> {
    let mut skipped = Vec::new();
    if !config.has_builder_default() {
        match builder_fee::suggested_default_builder(settings) {
            Ok(Some((address, fee))) => {
                config.default_builder_address = Some(address);
                config.default_builder_fee_rate = Some(fee);
            }
            Ok(None) => {}
            Err(err) => skipped.push(format!("! Not saving a default builder: {err}")),
        }
    }
    if !config.has_referral_default() {
        match referral_code::suggested_default_referral_code(settings, config.network.is_testnet())
        {
            Ok(Some(code)) => config.default_referral_code = Some(code),
            Ok(None) => {}
            Err(err) => skipped.push(format!("! Not saving a default referral code: {err}")),
        }
    }
    skipped
}

/// `wallet create|import|import-mnemonic`: the new wallet becomes the default and
/// the config gains packaged builder/referral defaults it lacks.
fn adopt_as_default(settings: &Settings) -> ConfigEdit<'_> {
    Box::new(move |config, wallet, _| {
        for warning in apply_setup_defaults(config, settings) {
            eprintln!("{warning}");
        }
        if let Some(previous) = config.default_wallet_id.as_deref()
            && previous != wallet.id
        {
            eprintln!(
                "! Default wallet changed from '{previous}' to '{}'.",
                wallet.id
            );
        }
        WalletStore::make_default(config, wallet);
    })
}

fn print_new_wallet(inv: &Invocation, message: &str, new: StoredWallet) {
    let mut receipt = WalletReceipt::new(
        inv,
        message,
        &new.account.address_text,
        Some(new.info.name),
        WalletSource::Stored.label(),
    );
    receipt.is_default = Some(true);
    receipt.config_keys_written = new.config_keys_written;
    inv.print_untimed(&receipt);
}

fn print_wallet_change(
    inv: &Invocation,
    message: &str,
    wallet: &WalletInfo,
) -> Result<(), anyhow::Error> {
    let account = Vault::hyperliquid_account(wallet)?;
    print_wallet_change_with(inv, message, wallet, &account);
    Ok(())
}

fn print_wallet_change_with(
    inv: &Invocation,
    message: &str,
    wallet: &WalletInfo,
    account: &HyperliquidAccount,
) {
    inv.print_untimed(&WalletReceipt::new(
        inv,
        message,
        &account.address_text,
        Some(wallet.name.clone()),
        WalletSource::Vault.label(),
    ));
}

/// Create a vault wallet and make it the default.
pub fn create(inv: &Invocation) -> Result<(), anyhow::Error> {
    let store = WalletStore::new(inv.settings());
    let new = store.add(
        &NewWallet::Create,
        &WalletName::Next("wallet"),
        Some(adopt_as_default(inv.settings())),
    )?;
    print_new_wallet(inv, "Created wallet", new);
    Ok(())
}

/// Import a private key into the vault and make it the default.
pub fn import(inv: &Invocation, private_key: Option<&str>) -> Result<(), anyhow::Error> {
    let private_key = secret_arg_or_prompt(
        private_key,
        "a private key",
        Some((WALLET_IMPORT, "at the hidden prompt")),
        || prompt::read_private_key(inv.mode()),
    )?;

    let store = WalletStore::new(inv.settings());
    let new = store.add(
        &NewWallet::ImportPrivateKey(&private_key),
        &WalletName::Next("imported"),
        Some(adopt_as_default(inv.settings())),
    )?;
    print_new_wallet(inv, "Imported wallet", new);
    Ok(())
}

/// Import a BIP-39 mnemonic into the vault and make it the default.
pub fn import_mnemonic(
    inv: &Invocation,
    mnemonic: Option<&str>,
    alias: Option<&str>,
) -> Result<(), anyhow::Error> {
    let mnemonic = secret_arg_or_prompt(
        mnemonic,
        "a mnemonic",
        Some((WALLET_IMPORT_MNEMONIC, "at the hidden prompt")),
        || {
            prompt::read_secret(
                inv.mode(),
                "Mnemonic phrase: ",
                "a mnemonic phrase on stdin",
            )
        },
    )?;

    let store = WalletStore::new(inv.settings());
    let new = store.add(
        &NewWallet::ImportMnemonic(&mnemonic),
        &WalletName::from_alias(alias, "imported"),
        Some(adopt_as_default(inv.settings())),
    )?;
    print_new_wallet(inv, "Imported wallet", new);
    Ok(())
}

/// `wallet login`: check a Bankr API key against the API, then store it with the
/// wallet address it reports so later runs resolve the signer without a request.
pub fn login(inv: &Invocation, args: &WalletLoginArgs) -> Result<(), anyhow::Error> {
    let api_key = secret_arg_or_prompt(
        args.api_key.as_deref(),
        "a Bankr API key",
        Some((WALLET_LOGIN, "the key at the hidden prompt")),
        || prompt::read_secret(inv.mode(), "Bankr API key: ", "a Bankr API key on stdin"),
    )?;
    if api_key.is_empty() {
        return Err(CliError::MissingInput(
            "bankr api key is empty; get one at https://bankr.bot/api-keys".to_string(),
        )
        .into());
    }
    let api_url = nonblank(args.api_url.as_deref())
        .map(str::to_string)
        .or_else(|| inv.settings().bankr_env_api_url());

    let api_url = crate::bankr::validate_api_url(
        &api_url.unwrap_or_else(|| crate::bankr::DEFAULT_API_URL.to_string()),
    )?;
    let client = crate::bankr::BankrClient::new(api_url.clone(), api_key.clone())?;
    let wallet_address = client.blocking_wallet()?;
    let address = wallet_address.to_string();

    let login = BankrLogin::new(
        &api_key,
        Some(api_url.as_str()).filter(|url| *url != crate::bankr::DEFAULT_API_URL),
        wallet_address,
    );
    let written = inv
        .settings()
        .update_config(|config| config.bankr = login)?;

    let mut receipt =
        BankrStatusOutput::new(inv, "logged_in", format!("Logged in to Bankr as {address}"));
    receipt.address = Some(address);
    receipt.api_url = Some(client.api_url().to_string());
    receipt.config_keys_written = written;
    inv.print_untimed(&receipt);
    Ok(())
}

/// `wallet logout`: forget the stored Bankr key and address.
pub fn logout(inv: &Invocation, yes: bool) -> Result<(), anyhow::Error> {
    let Some(file) = inv.settings().existing_config_file()? else {
        print_status(inv, "noop", "No Bankr login is stored");
        return Ok(());
    };

    if !yes {
        require_promptable("--yes", inv.format(), inv.mode())?;
        eprintln!(
            "This removes the stored Bankr API key from {}.",
            file.path().display()
        );
        eprintln!("The wallet and its funds stay with Bankr.");
        if !prompt::confirm(inv.mode(), "Log out of Bankr?", "--yes")? {
            print_declined(inv, "Logout cancelled");
            return Ok(());
        }
    }

    let removed = inv
        .settings()
        .update_config(|config| config.bankr = BankrLogin::default())?;
    if removed.is_empty() {
        print_status(inv, "noop", "No Bankr login is stored");
        return Ok(());
    }

    // Only a hint: the logout itself already succeeded.
    if let Ok(Some(credentials)) = inv.settings().bankr_credentials()
        && credentials.source == KeySource::Environment
    {
        out::warn(&format!(
            "{} is still set in the environment, so --bankr keeps working.",
            env::BANKR_API_KEY
        ));
    }
    let mut receipt = BankrStatusOutput::new(inv, "logged_out", "Logged out of Bankr".to_string());
    receipt.config_keys_written = removed;
    inv.print_untimed(&receipt);
    Ok(())
}

pub fn show(inv: &Invocation, signer: &SelectedSigner) {
    let description = signer.describe();
    inv.print_untimed(&WalletReceipt::new(
        inv,
        "Current wallet",
        &signer.address().to_string(),
        description.alias,
        &description.source,
    ));
}

pub fn address(inv: &Invocation, signer: &SelectedSigner) {
    print_wallet_address(inv, &signer.address().to_string());
}

/// Delete the config file, leaving vault wallets in place.
pub fn reset(inv: &Invocation, yes: bool) -> Result<(), anyhow::Error> {
    let Some(file) = inv.settings().existing_config_file()? else {
        print_status(inv, "noop", "Nothing to reset");
        return Ok(());
    };
    let path = file.path().display();

    if !yes {
        require_promptable("--yes", inv.format(), inv.mode())?;
        eprintln!("This deletes the config file at {path}.");
        eprintln!(
            "It clears the default wallet, network, builder, and referral defaults. Wallets stay in the vault."
        );
        if file.read().is_err() {
            eprintln!("The file isn't valid config, so its contents are discarded.");
        }
        let question = format!("Delete config file {path}?");
        if !prompt::confirm(inv.mode(), &question, "--yes")? {
            print_declined(inv, "Reset cancelled");
            return Ok(());
        }
    }

    inv.settings().remove_config()?;
    print_status(inv, "reset", &format!("Deleted config file {path}"));
    Ok(())
}

pub fn account_add(
    inv: &Invocation,
    private_key: Option<&str>,
    alias: Option<&str>,
    make_default: bool,
) -> Result<(), anyhow::Error> {
    let automation_mode = private_key.is_some() || alias.is_some() || make_default;
    let private_key = secret_arg_or_prompt(private_key, "a private key", None, || {
        prompt::read_private_key(inv.mode())
    })?;

    let store = WalletStore::new(inv.settings());
    let prompted;
    let alias = match alias {
        Some(alias) => Some(alias),
        None if automation_mode => None,
        None => {
            require_promptable("--alias", inv.format(), inv.mode())?;
            prompted = prompt::read_line(inv.mode(), "Wallet name: ", "--alias")?;
            Some(prompted.as_str())
        }
    };
    let name = WalletName::from_alias(alias, "account");

    let make_default_edit = make_default.then(WalletStore::set_default_edit);
    let stored = store.add(
        &NewWallet::ImportPrivateKey(&private_key),
        &name,
        make_default_edit,
    )?;
    print_wallet_change_with(inv, "Account added", &stored.info, &stored.account);
    Ok(())
}

pub fn account_ls(inv: &Invocation) -> Result<(), anyhow::Error> {
    let store = WalletStore::new(inv.settings());
    let listed = store.vault().signing_wallets()?;
    let wallets: Vec<WalletInfo> = listed.iter().map(|(wallet, _)| wallet.clone()).collect();
    let markers = store.default_markers(&wallets)?;
    let rows = listed
        .into_iter()
        .zip(markers)
        .enumerate()
        .map(|(index, ((wallet, account), is_default))| {
            AccountRow::for_wallet(index + 1, wallet, &account, is_default)
        })
        .collect();

    inv.print_untimed(&AccountsOutput::new(rows));
    Ok(())
}

/// Pick one stored wallet by selector, listing the choices when none was given.
fn choose_wallet(
    inv: &Invocation,
    store: &WalletStore<'_>,
    selector: Option<&str>,
) -> Result<WalletInfo, anyhow::Error> {
    if selector.is_none() {
        require_promptable("<WALLET>", inv.format(), inv.mode())?;
    }
    if store.vault().list()?.is_empty() {
        return Err(no_stored_accounts_error().into());
    }
    let selector = match selector {
        Some(selector) => selector.trim().to_string(),
        None => {
            print_wallet_choices(&store.vault().signing_wallets()?)?;
            prompt::read_line(inv.mode(), "Select wallet by name or id: ", "<WALLET>")?
                .trim()
                .to_string()
        }
    };
    Ok(store.vault().select(&selector)?)
}

pub fn account_set_default(inv: &Invocation, selector: Option<&str>) -> Result<(), anyhow::Error> {
    let store = WalletStore::new(inv.settings());
    let wallet = choose_wallet(inv, &store, selector)?;
    let account = store.set_default(&wallet)?;
    print_wallet_change_with(inv, "Default account set", &wallet, &account);
    Ok(())
}

/// Delete a wallet from the vault after confirmation.
pub fn account_remove(
    inv: &Invocation,
    selector: Option<&str>,
    yes: bool,
) -> Result<(), anyhow::Error> {
    let store = WalletStore::new(inv.settings());
    let wallet = choose_wallet(inv, &store, selector)?;
    let address = Vault::hyperliquid_account(&wallet)
        .map(|account| account.address_text)
        .unwrap_or_else(|_| "no Hyperliquid account".to_string());
    let is_default = store.is_configured_default(&wallet)?;

    if !yes {
        require_promptable("--yes", inv.format(), inv.mode())?;
        eprintln!("Wallet:  {}", wallet.name);
        eprintln!("Address: {address}");
        eprintln!("Default: {}", if is_default { "yes" } else { "no" });
        eprintln!();
        eprintln!("This deletes the encrypted key from the vault. This cannot be undone.");
        eprintln!("Back up first:");
        eprintln!("    {}", hints::wallet_export(&shell_arg(&wallet.name)));
        eprintln!();
        let confirmed = if inv.mode().terminal.stdin {
            prompt::confirm_typed(
                inv.mode(),
                &format!("To confirm, type the wallet name \"{}\": ", wallet.name),
                &wallet.name,
                "--yes",
            )?
        } else {
            prompt::confirm(
                inv.mode(),
                &format!("Delete wallet {}?", wallet.name),
                "--yes",
            )?
        };
        if !confirmed {
            print_declined(inv, "Delete cancelled");
            return Ok(());
        }
    }
    store.remove(&wallet, is_default)?;
    if is_default {
        eprintln!("! The deleted wallet was the default. Pick another:");
        eprintln!("    {SET_DEFAULT}");
    }
    inv.print_untimed(&WalletReceipt::new(
        inv,
        "Deleted wallet",
        &address,
        Some(wallet.name.clone()),
        WalletSource::Vault.label(),
    ));
    Ok(())
}

pub fn rename(inv: &Invocation, selector: &str, new_name: &str) -> Result<(), anyhow::Error> {
    let store = WalletStore::new(inv.settings());
    let wallet = store.rename(selector, new_name)?;
    print_wallet_change(inv, "Wallet renamed", &wallet)
}

/// Export an OWS wallet's secret.
///
/// NOTE: This command intentionally outputs plaintext private keys or mnemonics
/// to stdout, which is an exception to the AGENTS.md rule "Never log, print,
/// commit, or store plaintext private keys." The export is gated behind an
/// explicit interactive confirmation prompt (or --yes flag) and a warning
/// banner. The command's sole purpose is secret recovery/export.
pub fn export(inv: &Invocation, selector: &str, yes: bool) -> Result<(), anyhow::Error> {
    if !yes {
        require_promptable("--yes", inv.format(), inv.mode())?;
        let question = format!(
            "Export secret for wallet '{selector}'? This will display the private key or mnemonic."
        );
        if !prompt::confirm(inv.mode(), &question, "--yes")? {
            print_declined(inv, "Export cancelled");
            return Ok(());
        }
    }

    let secret = Vault::from_settings(inv.settings()).export(selector)?;

    out::warn("this output contains a plaintext private key or mnemonic. Do not share or log it.");

    if inv.format() == OutputFormat::Json {
        println!("{:#}", serde_json::json!({"secret": secret}));
    } else {
        println!("{secret}");
    }
    Ok(())
}

fn print_wallet_choices(choices: &[(WalletInfo, HyperliquidAccount)]) -> Result<(), anyhow::Error> {
    if choices.is_empty() {
        return Err(no_stored_accounts_error().into());
    }
    eprintln!("Stored wallets:");
    for (wallet, account) in choices {
        eprintln!(
            "  {}: {} ({})",
            wallet.id, wallet.name, account.address_text
        );
    }
    Ok(())
}

pub const GROUP: GroupSpec = GroupSpec::new(&["wallet"], "Manage wallets").aliases(&["wallets"]);

#[derive(Args, Debug, Clone)]
pub struct AccountAddArgs {
    /// Private key to store (0x-prefixed hex). Omit to use the hidden prompt.
    pub private_key: Option<String>,
    /// Alias for the wallet. Defaults to `account`, `account-2`, ... in non-interactive mode.
    #[arg(long)]
    pub alias: Option<String>,
    /// Wallet type.
    #[arg(long = "type")]
    pub account_type: Option<String>,
    /// Make this wallet the default signer.
    #[arg(long)]
    pub default: bool,
}

#[derive(Args, Debug, Clone)]
pub struct AccountSetDefaultArgs {
    /// Wallet name or id. Omit to choose interactively.
    pub selector: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct AccountRemoveArgs {
    /// Wallet name or id. Omit to choose interactively.
    pub selector: Option<String>,
    /// Confirm removal without prompting.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args, Debug, Clone)]
pub struct WalletImportArgs {
    /// Private key to import (0x-prefixed hex). Omit to use the hidden prompt.
    pub private_key: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct WalletLoginArgs {
    /// Bankr API key (bk_...). Omit to paste it at the hidden prompt.
    #[arg(long)]
    pub api_key: Option<String>,
    /// Bankr API host. Defaults to https://api.bankr.bot
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct WalletLogoutArgs {
    /// Confirm logout without prompting.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args, Debug, Clone)]
pub struct WalletImportMnemonicArgs {
    /// BIP-39 mnemonic phrase. Omit to use the hidden prompt.
    pub mnemonic: Option<String>,
    /// Alias/name for the imported wallet.
    #[arg(long)]
    pub alias: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct WalletRenameArgs {
    /// Wallet name or id to rename.
    pub selector: String,
    /// New name for the wallet.
    pub new_name: String,
}

#[derive(Args, Debug, Clone)]
pub struct WalletDeleteArgs {
    /// Wallet name or id to delete.
    pub selector: String,
    /// Confirm deletion without prompting.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args, Debug, Clone)]
pub struct WalletExportArgs {
    /// Wallet name or id to export.
    pub selector: String,
    /// Skip confirmation prompt.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args, Debug, Clone)]
pub struct WalletResetArgs {
    /// Confirm reset without prompting.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

pub struct AccountAdd;

impl CliCommand for AccountAdd {
    type Args = AccountAddArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "add"],
        "wallet",
        "Add an OWS wallet",
        "Add a signing wallet. Passing private_key exposes it to the child process argv.",
    )
    .long_about("Add an OWS wallet.\n\
    \n\
    Recommended for humans: run `hyperliquid account add` without PRIVATE_KEY and paste the key at the hidden prompt.\n\
    \n\
    For controlled automation, pass PRIVATE_KEY with --alias, --type, and optionally --default. Passing a private key as an argument can expose it in OS process listings and shell history.")
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalSecret)
    .dry_run(DryRunPolicy::Optional)
    .ows(OwsSupport::NotApplicable).dangerous()
    .prompt()
    .local()
    .args(&[
        arg("private_key").describe("0x-prefixed private key. Omit in human contexts to use the hidden prompt; argv secrets are rejected in machine contexts.").secret(),
        arg("alias").describe("Alias for the wallet.").resource_id("account alias"),
        arg("account_type"),
        arg("default").kind(InputKind::Boolean),
    ]);

    async fn run(inv: &Invocation, args: &AccountAddArgs) -> Result<(), anyhow::Error> {
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "store_encrypted_signing_account",
                serde_json::json!({
                    "private_key_supplied": args.private_key.is_some(),
                    "alias": args.alias,
                    "type": args.account_type,
                    "default": args.default
                }),
            );
        }
        account_add(
            inv,
            args.private_key.as_deref(),
            args.alias.as_deref(),
            args.default,
        )
    }
}

pub struct AccountLs;

impl CliCommand for AccountLs {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "ls"],
        "wallet",
        "List wallets",
        "List wallets in the OWS vault.",
    )
    .risk(Risk::LocalState)
    .ows(OwsSupport::NotApplicable)
    .local()
    .cli_aliases(&["list"]);

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        account_ls(inv)
    }
}

pub struct AccountSetDefault;

impl CliCommand for AccountSetDefault {
    type Args = AccountSetDefaultArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "set-default"],
        "wallet",
        "Set the default wallet",
        "Set the default signing wallet.",
    )
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalState)
    .dry_run(DryRunPolicy::Optional)
    .ows(OwsSupport::NotApplicable)
    .dangerous()
    .local()
    .args(&[arg("selector")
        .describe("Wallet name, wallet id, or address.")
        .resource_id("account selector")
        .schema_required()]);

    async fn run(inv: &Invocation, args: &AccountSetDefaultArgs) -> Result<(), anyhow::Error> {
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "set_default_account",
                serde_json::json!({"selector": args.selector}),
            );
        }
        account_set_default(inv, args.selector.as_deref())
    }
}

pub struct AccountRemove;

impl CliCommand for AccountRemove {
    type Args = AccountRemoveArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["account", "remove"],
        "wallet",
        "Delete a wallet and its stored key",
        "Delete a wallet's encrypted key from the vault. This cannot be undone.",
    )
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalState)
    .dry_run(DryRunPolicy::Optional)
    .ows(OwsSupport::NotApplicable)
    .dangerous()
    .prompt()
    .confirms_with_arg("selector")
    .local()
    .args(&[
        arg("selector")
            .describe("Wallet name, wallet id, or address.")
            .resource_id("account selector")
            .schema_required(),
        arg("yes")
            .kind(InputKind::Boolean)
            .describe("Skip confirmation prompt for deliberate automation."),
    ]);

    async fn run(inv: &Invocation, args: &AccountRemoveArgs) -> Result<(), anyhow::Error> {
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "remove_stored_account",
                serde_json::json!({"selector": args.selector, "yes": args.yes}),
            );
        }
        account_remove(inv, args.selector.as_deref(), args.yes)
    }
}

pub struct WalletCreate;

impl CliCommand for WalletCreate {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["wallet", "create"],
        "wallet",
        "Create a wallet",
        "Create a new wallet in the OWS vault.",
    )
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalSecret)
    .dry_run(DryRunPolicy::Optional)
    .ows(OwsSupport::NotApplicable)
    .dangerous()
    .local();

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "create_and_store_encrypted_wallet",
                serde_json::Value::Null,
            );
        }
        create(inv)
    }
}

pub struct WalletImport;

impl CliCommand for WalletImport {
    type Args = WalletImportArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["wallet", "import"],
        "wallet",
        "Import an existing private key",
        "Import a private key into the OWS vault. Passing private_key exposes it to the child process argv.",
    )
    .long_about("Import an existing private key.\n\
    \n\
    Recommended for humans: run `hyperliquid wallet import` without PRIVATE_KEY and paste the key at the hidden prompt.\n\
    \n\
    Warning: `hyperliquid wallet import <PRIVATE_KEY>` can expose the key in OS process listings and shell history. Use the argument form only for controlled automation.")
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalSecret)
    .dry_run(DryRunPolicy::Optional)
    .ows(OwsSupport::NotApplicable).dangerous()
    .prompt()
    .local()
    .args(&[
        arg("private_key").describe("0x-prefixed private key. Omit in human contexts to use the hidden prompt; argv secrets are rejected in machine contexts.").secret(),
    ]);

    async fn run(inv: &Invocation, args: &WalletImportArgs) -> Result<(), anyhow::Error> {
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "validate_and_store_encrypted_wallet",
                serde_json::json!({"private_key_supplied": args.private_key.is_some()}),
            );
        }
        import(inv, args.private_key.as_deref())
    }
}

pub struct WalletImportMnemonic;

impl CliCommand for WalletImportMnemonic {
    type Args = WalletImportMnemonicArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["wallet", "import-mnemonic"],
        "wallet",
        "Import an existing wallet from a BIP-39 mnemonic phrase",
        "Import an existing wallet from a BIP-39 mnemonic phrase.",
    )
    .long_about("Import an existing wallet from a BIP-39 mnemonic phrase.\n\
    \n\
    Recommended for humans: run `hyperliquid wallet import-mnemonic` without MNEMONIC and paste the phrase at the hidden prompt.\n\
    \n\
    Warning: passing a mnemonic as an argument can expose it in OS process listings and shell history. Use the argument form only for controlled automation.")
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalSecret)
    .dry_run(DryRunPolicy::Optional)
    .ows(OwsSupport::NotApplicable).dangerous()
    .prompt()
    .local()
    .args(&[
        arg("mnemonic").describe("BIP-39 mnemonic phrase. Omit in human contexts to use the hidden prompt; argv secrets are rejected in machine contexts.").secret(),
        arg("alias"),
    ]);

    async fn run(inv: &Invocation, args: &WalletImportMnemonicArgs) -> Result<(), anyhow::Error> {
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "import_mnemonic_to_ows_vault",
                serde_json::json!({"mnemonic_supplied": args.mnemonic.is_some(), "alias": args.alias}),
            );
        }
        import_mnemonic(inv, args.mnemonic.as_deref(), args.alias.as_deref())
    }
}

pub struct WalletLogin;

impl CliCommand for WalletLogin {
    type Args = WalletLoginArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["wallet", "login"],
        "wallet",
        "Log in to a Bankr hosted wallet",
        "Store a Bankr API key and the wallet address it reports.",
    )
    .long_about("Log in to a Bankr hosted wallet.\n\
    \n\
    The key is checked against https://api.bankr.bot before it is stored, then saved owner-only in the config file with the wallet address. Signing commands use that wallet with the global --bankr flag.\n\
    \n\
    Recommended for humans: run `hyperliquid wallet login` without --api-key and paste the key at the hidden prompt.")
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalSecret)
    .dry_run(DryRunPolicy::Optional)
    .ows(OwsSupport::NotApplicable).dangerous()
    .prompt()
    .local()
    .args(&[
        arg("api_key").describe("Bankr API key (bk_...). Omit in human contexts to use the hidden prompt.").secret(),
        arg("api_url"),
    ]);

    async fn run(inv: &Invocation, args: &WalletLoginArgs) -> Result<(), anyhow::Error> {
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "validate_and_store_bankr_api_key",
                serde_json::json!({
                    "api_key_supplied": args.api_key.is_some(),
                    "api_url": args.api_url
                }),
            );
        }
        login(inv, args)
    }
}

pub struct WalletLogout;

impl CliCommand for WalletLogout {
    type Args = WalletLogoutArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["wallet", "logout"],
        "wallet",
        "Forget the stored Bankr login",
        "Remove the Bankr API key and address from the config file.",
    )
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalState)
    .dry_run(DryRunPolicy::Optional)
    .ows(OwsSupport::NotApplicable)
    .prompt()
    .local()
    .args(&[arg("yes")]);

    async fn run(inv: &Invocation, args: &WalletLogoutArgs) -> Result<(), anyhow::Error> {
        if inv.dry_run() {
            return print_dry_run(inv, "remove_bankr_api_key", serde_json::Value::Null);
        }
        logout(inv, args.yes)
    }
}

pub struct WalletShow;

impl CliCommand for WalletShow {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["wallet", "show"],
        "wallet",
        "Show the current wallet",
        "Show current wallet metadata.",
    )
    .risk(Risk::LocalSecret)
    .network(crate::command_spec::NetworkUse::Optional)
    .ows(OwsSupport::AddressSelectorSupported)
    .auth();

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        let signer = inv.resolve_signer()?;
        show(inv, &signer);
        Ok(())
    }
}

pub struct WalletAddress;

impl CliCommand for WalletAddress {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["wallet", "address"],
        "wallet",
        "Print the wallet address",
        "Print the configured wallet address.",
    )
    .risk(Risk::LocalSecret)
    .network(crate::command_spec::NetworkUse::Optional)
    .ows(OwsSupport::AddressSelectorSupported)
    .auth();

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        let signer = inv.resolve_signer()?;
        address(inv, &signer);
        Ok(())
    }
}

pub struct WalletList;

impl CliCommand for WalletList {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["wallet", "list"],
        "wallet",
        "List all wallets",
        "List all wallets in the OWS vault.",
    )
    .risk(Risk::LocalState)
    .ows(OwsSupport::NotApplicable)
    .local()
    .cli_aliases(&["ls"]);

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        account_ls(inv)
    }
}

pub struct WalletRename;

impl CliCommand for WalletRename {
    type Args = WalletRenameArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["wallet", "rename"],
        "wallet",
        "Rename a wallet",
        "Rename a wallet by name or id.",
    )
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalState)
    .ows(OwsSupport::NotApplicable)
    .dangerous()
    .local()
    .dry_run_refusal("--dry-run is not supported for wallet rename")
    .args(&[arg("selector"), arg("new_name")]);

    async fn run(inv: &Invocation, args: &WalletRenameArgs) -> Result<(), anyhow::Error> {
        rename(inv, &args.selector, &args.new_name)
    }
}

pub struct WalletDelete;

impl CliCommand for WalletDelete {
    type Args = WalletDeleteArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["wallet", "delete"],
        "wallet",
        "Delete a wallet and its stored key",
        "Delete a wallet's encrypted key from the vault. This cannot be undone.",
    )
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalState)
    .dry_run(DryRunPolicy::Optional)
    .ows(OwsSupport::NotApplicable)
    .dangerous()
    .prompt()
    .local()
    .args(&[arg("selector"), arg("yes").kind(InputKind::Boolean)]);

    async fn run(inv: &Invocation, args: &WalletDeleteArgs) -> Result<(), anyhow::Error> {
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "delete_ows_wallet",
                serde_json::json!({"selector": args.selector, "yes": args.yes}),
            );
        }
        account_remove(inv, Some(&args.selector), args.yes)
    }
}

pub struct WalletExport;

impl CliCommand for WalletExport {
    type Args = WalletExportArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["wallet", "export"],
        "wallet",
        "Export a wallet's secret (mnemonic or private key)",
        "Export a wallet's secret (mnemonic or private key).",
    )
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalSecret)
    .ows(OwsSupport::NotApplicable)
    .dangerous()
    .prompt()
    .local()
    .dry_run_refusal("--dry-run is not supported for wallet export")
    .args(&[arg("selector"), arg("yes").kind(InputKind::Boolean)]);

    async fn run(inv: &Invocation, args: &WalletExportArgs) -> Result<(), anyhow::Error> {
        export(inv, &args.selector, args.yes)
    }
}

pub struct WalletReset;

impl CliCommand for WalletReset {
    type Args = WalletResetArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["wallet", "reset"],
        "wallet",
        "Delete the wallet config file and default wallet reference",
        "Delete the config file, clearing default wallet, network, and builder and referral defaults. Wallets stay in the vault.",
    )
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalState)
    .dry_run(DryRunPolicy::Optional)
    .ows(OwsSupport::NotApplicable).dangerous()
    .prompt()
    .local()
    .args(&[
        arg("yes").kind(InputKind::Boolean).describe("Skip confirmation prompt for deliberate automation."),
    ]);

    async fn run(inv: &Invocation, args: &WalletResetArgs) -> Result<(), anyhow::Error> {
        if inv.dry_run() {
            return print_dry_run(
                inv,
                "remove_wallet_configuration",
                serde_json::json!({"yes": args.yes}),
            );
        }
        reset(inv, args.yes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_runs_name_the_missing_selector_without_prompting() {
        let err = require_promptable("<WALLET>", OutputFormat::Json, &Mode::default()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("<WALLET>"));
    }

    #[test]
    fn setup_defaults_fill_only_what_the_config_lacks() {
        let settings = Settings::detached(
            crate::settings::EnvSource::fixed([
                (
                    crate::settings::env::DEFAULT_BUILDER_ADDRESS,
                    "0x0000000000000000000000000000000000000abc",
                ),
                (crate::settings::env::DEFAULT_BUILDER_FEE_RATE, "0.001%"),
                (crate::settings::env::DEFAULT_REFERRAL_CODE, "FRIEND"),
            ]),
            None,
        );

        let mut empty = Config::default();
        assert!(apply_setup_defaults(&mut empty, &settings).is_empty());
        assert_eq!(
            empty
                .default_builder_address
                .as_deref()
                .map(str::to_lowercase)
                .as_deref(),
            Some("0x0000000000000000000000000000000000000abc")
        );
        assert_eq!(empty.default_builder_fee_rate.as_deref(), Some("0.001%"));
        assert_eq!(empty.default_referral_code.as_deref(), Some("FRIEND"));

        let mut configured = Config {
            default_builder_address: Some("0xexisting".to_string()),
            default_referral_code: Some("MINE".to_string()),
            ..Config::default()
        };
        let before = configured.clone();
        assert!(apply_setup_defaults(&mut configured, &settings).is_empty());
        assert_eq!(configured, before);
        assert_eq!(configured.default_referral_code.as_deref(), Some("MINE"));
    }

    #[test]
    fn a_setup_default_that_fails_validation_is_skipped_with_a_warning() {
        let settings = Settings::detached(
            crate::settings::EnvSource::fixed([(
                crate::settings::env::DEFAULT_REFERRAL_CODE,
                "not a code!",
            )]),
            None,
        );

        let mut config = Config::default();
        let warnings = apply_setup_defaults(&mut config, &settings);

        assert_eq!(config.default_referral_code, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("default referral code"),
            "{warnings:?}"
        );
    }
}
