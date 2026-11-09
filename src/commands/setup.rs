//! Guided first-time setup wizard.
//!
//! Questions and narration go to stderr; only the final result goes to stdout.
//! Every answer is collected, and a summary printed, before the first change to
//! the vault or config.

use clap::Args;
use serde::Serialize;

use crate::builder_fee::{self, MaxFeeRate};
use crate::command_spec::{CliCommand, CommandSpec, InputKind, Lifecycle, OwsSupport, Risk, arg};
use crate::commands::{builder, wallet};
use crate::errors::CliError;
use crate::errors::from_sdk;
use crate::http_api::ApiClient;
use crate::invocation::Invocation;
use crate::mode::Mode;
use crate::network::Network;
use crate::output::{OutputFormat, TableData};
use crate::ows::vault::Vault;
use crate::prompt;
use crate::referral_code;
use crate::settings::{Settings, env};
use crate::signing::SelectedSigner;
use crate::wallet_store::{ConfigEdit, NewWallet, WalletName, WalletStore};
use crate::wire::{self, AddressRole};
use ows_lib::types::WalletInfo;

#[derive(Args, Debug, Clone)]
pub struct SetupArgs {
    /// Create a new wallet and accept default setup choices without prompting.
    #[arg(long, short = 'y')]
    pub yes: bool,

    /// Submit the configured default builder fee approval during setup without an extra prompt.
    #[arg(long, conflicts_with = "no_approve_builder")]
    pub approve_builder: bool,

    /// Skip default builder fee approval during setup, even with --yes.
    #[arg(long, conflicts_with = "approve_builder")]
    pub no_approve_builder: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct SetupSummary {
    message: String,
    address: String,
    wallet_name: String,
    network: String,
    config_path: String,
    vault_path: String,
    connection: String,
    default_builder: String,
    builder_approval: String,
    default_referral_code: String,
    config_keys_written: Vec<String>,
}

impl TableData for SetupSummary {
    fn headers(&self) -> Vec<&str> {
        vec!["Field", "Value"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        vec![
            vec!["Message".to_string(), self.message.clone()],
            vec!["Address".to_string(), self.address.clone()],
            vec!["Wallet".to_string(), self.wallet_name.clone()],
            vec!["Network".to_string(), self.network.clone()],
            vec!["Config".to_string(), self.config_path.clone()],
            vec!["Vault".to_string(), self.vault_path.clone()],
            vec!["Connection".to_string(), self.connection.clone()],
            vec!["Default builder".to_string(), self.default_builder.clone()],
            vec![
                "Builder approval".to_string(),
                self.builder_approval.clone(),
            ],
            vec![
                "Default referral".to_string(),
                self.default_referral_code.clone(),
            ],
            vec![
                "Config keys written".to_string(),
                self.config_keys_written.join(", "),
            ],
        ]
    }

    fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

fn ensure_setup_can_run(
    args: &SetupArgs,
    format: OutputFormat,
    mode: &Mode,
) -> Result<(), CliError> {
    if args.yes {
        return Ok(());
    }
    let blocker = if format == OutputFormat::Json {
        Some("JSON output")
    } else {
        mode.prompt_blocker()
    };
    match blocker {
        None => Ok(()),
        Some(reason) => Err(CliError::MissingInput(format!(
            "Can't run setup with prompts because of {reason}. Accept the defaults instead:\n    hyperliquid setup --yes"
        ))),
    }
}

pub async fn run(inv: &Invocation, args: &SetupArgs) -> Result<(), anyhow::Error> {
    let mode = inv.mode();
    let default_network = inv.remote()?.network();
    ensure_setup_can_run(args, inv.format(), mode)?;
    let settings = inv.settings();
    let store = WalletStore::new(settings);
    // A broken config is an error before any question is asked.
    settings.config()?;

    eprintln!("Welcome to Hyperliquid CLI setup");
    eprintln!(
        "This wizard will create or import a wallet, save your local config, and test the API connection."
    );
    eprintln!();
    eprintln!("Wallet setup:");
    eprintln!("  1) Create new wallet");
    eprintln!("  2) Import existing wallet (private key)");

    let wallet_choice = if args.yes {
        "1".to_string()
    } else {
        prompt::read_line(mode, "Choose an option [1/2]: ", "--yes")?
    };
    let import_wallet = match wallet_choice.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "create" | "c" => false,
        "2" | "import" | "i" => true,
        other => {
            return Err(CliError::Unsupported(format!(
                "setup option '{other}' is not supported; choose 1 to create or 2 to import"
            ))
            .into());
        }
    };
    let network = if args.yes {
        default_network
    } else {
        prompt_network(mode, default_network)?
    };

    // Verify connection and collect fallible defaults before creating any local
    // wallet state so failures do not leave orphaned wallets behind.
    let api = inv.api_client_for(network)?;
    let connection = verify_connection(&api).await?;
    eprintln!("{connection}");
    let builder_suggestion = builder_fee::suggested_default_builder(settings)?;
    let builder_defaults = if args.yes {
        builder_suggestion.clone()
    } else {
        prompt_builder_defaults(mode, settings)?
    };
    let referral_suggestion =
        referral_code::suggested_default_referral_code(settings, network.is_testnet())?;
    let referral_code = if args.yes {
        referral_suggestion.clone()
    } else {
        prompt_referral_default(mode, settings, network)?
    };
    // A private key read at the masked prompt or from stdin. Never printed.
    let imported_key = if import_wallet {
        Some(wallet::prompt_private_key(mode)?.trim().to_string())
    } else {
        None
    };
    let new_wallet = match &imported_key {
        Some(private_key) => NewWallet::ImportPrivateKey(private_key),
        None => NewWallet::Create,
    };
    let approve_builder = resolve_builder_approval(mode, args, builder_defaults.as_ref())?;

    let wallet_name = store.next_name(if import_wallet {
        "setup-imported"
    } else {
        "setup"
    })?;
    let config_path = settings.config_path_display();
    let vault_path_str = store.vault().display_path();

    let default_builder = builder_defaults
        .as_ref()
        .map(|(address, fee)| format!("{address} at {fee}"))
        .unwrap_or_else(|| "not configured".to_string());
    print_answers_summary(&AnswersSummary {
        wallet: if import_wallet {
            format!("import a private key as wallet {wallet_name}")
        } else {
            format!("create a new wallet named {wallet_name}")
        },
        network,
        config_path: &config_path,
        vault_path: &vault_path_str,
        default_builder: &default_builder,
        builder_approval: if builder_defaults.is_none() {
            "not configured"
        } else if approve_builder {
            "submit a signed approval to Hyperliquid"
        } else {
            "skip"
        },
        referral: referral_code.as_deref().unwrap_or("not configured"),
    });

    let record_setup: ConfigEdit<'_> = Box::new(|config, wallet, _| {
        config.network = network;
        config.default_wallet_id = Some(wallet.id.clone());
        config.default_builder_address = builder_defaults
            .as_ref()
            .map(|(address, _)| address.clone());
        config.default_builder_fee_rate = builder_defaults.as_ref().map(|(_, fee)| fee.clone());
        config.default_referral_code = referral_code.clone();
    });
    let stored = store.add(
        &new_wallet,
        &WalletName::Exact(&wallet_name),
        Some(record_setup),
    )?;
    eprintln!("Saved config to {config_path}");

    let builder_approval = maybe_approve_default_builder(
        &api,
        approve_builder,
        builder_defaults.as_ref(),
        store.vault(),
        &stored.info,
    )
    .await?;

    let default_referral_code = referral_code
        .clone()
        .unwrap_or_else(|| "not configured".to_string());

    let output = SetupSummary {
        message: "Setup complete".to_string(),
        address: stored.account.address_text,
        wallet_name: stored.info.name,
        network: network.to_string(),
        config_path,
        vault_path: vault_path_str,
        connection,
        default_builder,
        builder_approval,
        default_referral_code,
        config_keys_written: stored.config_keys_written,
    };
    inv.print_untimed(&output);

    let replayable = !import_wallet
        && builder_defaults == builder_suggestion
        && referral_code == referral_suggestion;
    if !args.yes {
        print_replay_command(
            replayable,
            default_network,
            network,
            builder_defaults.is_some() && !approve_builder,
        );
    }
    Ok(())
}

struct AnswersSummary<'a> {
    wallet: String,
    network: Network,
    config_path: &'a str,
    vault_path: &'a str,
    default_builder: &'a str,
    builder_approval: &'a str,
    referral: &'a str,
}

fn print_answers_summary(summary: &AnswersSummary<'_>) {
    eprintln!();
    eprintln!("Setup will:");
    eprintln!("  Wallet:           {}", summary.wallet);
    eprintln!("  Network:          {}", summary.network);
    eprintln!("  Config file:      {}", summary.config_path);
    eprintln!("  Vault:            {}", summary.vault_path);
    eprintln!("  Default builder:  {}", summary.default_builder);
    eprintln!("  Builder approval: {}", summary.builder_approval);
    eprintln!("  Default referral: {}", summary.referral);
    eprintln!();
}

/// Print the non-interactive command that repeats this run, when the flags can express it.
fn print_replay_command(
    replayable: bool,
    default_network: Network,
    network: Network,
    skip_builder_approval: bool,
) {
    eprintln!();
    if !replayable {
        eprintln!(
            "Importing a wallet or choosing custom builder or referral values can't be repeated without prompts."
        );
        return;
    }
    let mut command = String::new();
    if network == Network::Mainnet && default_network == Network::Testnet {
        command.push_str(&format!("{}=mainnet ", env::NETWORK));
    }
    command.push_str("hyperliquid ");
    if network == Network::Testnet {
        command.push_str("--testnet ");
    }
    command.push_str("setup --yes");
    if skip_builder_approval {
        command.push_str(" --no-approve-builder");
    }
    eprintln!("Run this again without prompts:");
    eprintln!("    {command}");
}

async fn verify_connection(api: &ApiClient) -> Result<String, anyhow::Error> {
    let client = api.sdk();
    let mids = client.all_mids(None).await.map_err(from_sdk)?;
    Ok(format!(
        "Test query succeeded ({} mid prices returned)",
        mids.len()
    ))
}

fn prompt_network(mode: &Mode, default_network: Network) -> Result<Network, anyhow::Error> {
    let default_is_testnet = default_network == Network::Testnet;
    let default_hint = if default_is_testnet { "Y/n" } else { "y/N" };
    let answer = prompt::read_line(mode, &format!("Use testnet? [{default_hint}]: "), "--yes")?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "" => Ok(default_network),
        "y" | "yes" => Ok(Network::Testnet),
        "n" | "no" => Ok(Network::Mainnet),
        other => Err(CliError::Unsupported(format!(
            "network answer '{other}' is not supported; answer yes or no"
        ))
        .into()),
    }
}

fn prompt_builder_defaults(
    mode: &Mode,
    settings: &Settings,
) -> Result<Option<(String, String)>, anyhow::Error> {
    let suggestion = builder_fee::suggested_default_builder(settings)?;
    if let Some((address, fee)) = suggestion.as_ref() {
        eprintln!(
            "Default builder fee setup (optional; press Enter to use the suggested default, or type 'none' to skip):"
        );
        eprintln!("Suggested builder: {address} at {fee}");
    } else {
        eprintln!("Default builder fee setup (optional; press Enter to skip):");
    }

    let address_prompt = suggestion
        .as_ref()
        .map(|(address, _)| format!("Default builder address [{address}]: "))
        .unwrap_or_else(|| "Default builder address: ".to_string());
    let address = prompt::read_line(mode, &address_prompt, "--yes")?;
    let address = address.trim();
    if address.eq_ignore_ascii_case("none") || address.eq_ignore_ascii_case("skip") {
        return Ok(None);
    }
    let address = if address.is_empty() {
        let Some((suggested, _)) = suggestion.as_ref() else {
            return Ok(None);
        };
        suggested.as_str()
    } else {
        address
    };
    let parsed = wire::address(address, AddressRole::Builder)?;

    let fee_prompt = suggestion
        .as_ref()
        .map(|(_, fee)| format!("Default builder fee rate [{fee}]: "))
        .unwrap_or_else(|| "Default builder fee rate (for example 0.001%): ".to_string());
    let fee = prompt::read_line(mode, &fee_prompt, "--yes")?;
    let fee = fee.trim();
    let fee = if fee.is_empty() {
        let Some((_, fee)) = suggestion.as_ref() else {
            return Err(CliError::Configuration(
                "default builder fee rate is required when a builder address is configured"
                    .to_string(),
            )
            .into());
        };
        fee.clone()
    } else {
        fee.to_string()
    };
    MaxFeeRate::parse(&fee)?;
    Ok(Some((parsed.to_string(), fee)))
}

/// Decide, before anything changes, whether setup submits the builder approval.
fn resolve_builder_approval(
    mode: &Mode,
    args: &SetupArgs,
    builder_defaults: Option<&(String, String)>,
) -> Result<bool, anyhow::Error> {
    let Some((builder, fee)) = builder_defaults else {
        if args.approve_builder {
            return Err(CliError::Configuration(
                "--approve-builder requires a configured default builder address and fee rate"
                    .to_string(),
            )
            .into());
        }
        return Ok(false);
    };
    should_approve_default_builder(mode, args, builder, fee)
}

async fn maybe_approve_default_builder(
    api: &ApiClient,
    approve: bool,
    builder_defaults: Option<&(String, String)>,
    vault: &Vault,
    wallet: &WalletInfo,
) -> Result<String, anyhow::Error> {
    let Some((builder_text, fee)) = builder_defaults else {
        return Ok("not configured".to_string());
    };
    if !approve {
        return Ok("not approved".to_string());
    }

    let builder_address = wire::address(builder_text, AddressRole::Builder)?;
    let max_fee_rate = MaxFeeRate::parse(fee)?;
    let signer = SelectedSigner::vault_wallet(vault, wallet)?;
    let approval = builder::submit_approval(api, &signer, builder_address, &max_fee_rate).await;
    match approval {
        Ok(()) => {
            let message = format!(
                "approved {builder_address} at {fee} ({} tenths bps)",
                max_fee_rate.tenths_bps()
            );
            eprintln!("Builder approval submitted: {message}");
            Ok(message)
        }
        Err(err) => {
            let message = format!("approval failed: {err}");
            eprintln!("Builder approval failed: {err}");
            Ok(message)
        }
    }
}

fn should_approve_default_builder(
    mode: &Mode,
    args: &SetupArgs,
    builder: &str,
    fee: &str,
) -> Result<bool, anyhow::Error> {
    if args.no_approve_builder {
        return Ok(false);
    }
    if args.yes || args.approve_builder {
        return Ok(true);
    }

    let answer = prompt::read_line(
        mode,
        &format!("Approve default builder {builder} for max fee {fee}? [Y/n]: "),
        "--approve-builder",
    )?;
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        other => Err(CliError::Unsupported(format!(
            "builder approval answer '{other}' is not supported; answer yes or no"
        ))
        .into()),
    }
}

fn prompt_referral_default(
    mode: &Mode,
    settings: &Settings,
    network: Network,
) -> Result<Option<String>, anyhow::Error> {
    let suggestion =
        referral_code::suggested_default_referral_code(settings, network.is_testnet())?;
    if let Some(code) = suggestion.as_ref() {
        eprintln!(
            "Default referral setup (optional; press Enter to use the suggested default, or type 'none' to skip):"
        );
        eprintln!("Suggested referral code: {code}");
    } else {
        eprintln!("Default referral setup (optional; press Enter to skip):");
    }

    let prompt_label = suggestion
        .as_ref()
        .map(|code| format!("Default referral code [{code}]: "))
        .unwrap_or_else(|| "Default referral code: ".to_string());
    let code = prompt::read_line(mode, &prompt_label, "--yes")?;
    let code = code.trim();
    if code.eq_ignore_ascii_case("none") || code.eq_ignore_ascii_case("skip") {
        return Ok(None);
    }
    let code = if code.is_empty() {
        let Some(code) = suggestion else {
            return Ok(None);
        };
        code
    } else {
        code.to_string()
    };
    referral_code::validate_referral_code(&code)?;
    Ok(Some(code))
}

pub struct Setup;

impl CliCommand for Setup {
    type Args = SetupArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["setup"],
        "system",
        "Set up a wallet, network, and defaults",
        "Run the guided setup wizard. This is interactive and may not complete in an automation request unless the CLI exits on EOF.",
    )
    .long_about("Set up a wallet, network, and defaults.\n\
    \n\
    The wizard can create a new wallet or import an existing private key through a hidden prompt, choose the default network, save local config, store the wallet in the OWS vault as the default signing account, and verify the selected Hyperliquid API connection.\n\
    \n\
    Generated or imported private keys are never printed.")
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::LocalSecret)
    .ows(OwsSupport::NotApplicable).dangerous()
    .prompt()
    .args(&[
        arg("yes").kind(InputKind::Boolean),
        arg("approve_builder").kind(InputKind::Boolean),
        arg("no_approve_builder").kind(InputKind::Boolean),
    ]);

    async fn run(inv: &Invocation, args: &SetupArgs) -> Result<(), anyhow::Error> {
        run(inv, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_without_yes_fails_before_prompting_in_json_runs() {
        let args = SetupArgs {
            yes: false,
            approve_builder: false,
            no_approve_builder: false,
        };
        let err = ensure_setup_can_run(&args, OutputFormat::Json, &Mode::default()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("hyperliquid setup --yes"));

        let yes = SetupArgs { yes: true, ..args };
        assert!(ensure_setup_can_run(&yes, OutputFormat::Json, &Mode::default()).is_ok());
    }
}
