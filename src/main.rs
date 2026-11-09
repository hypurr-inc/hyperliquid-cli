use clap::Parser;
use std::ffi::OsString;
use std::path::PathBuf;

use hyperliquid_cli::command_spec::CliCommand;
use hyperliquid_cli::commands::update::Update;
use hyperliquid_cli::errors;
use hyperliquid_cli::invocation::{Invocation, InvocationFlags, SignerFlags};
use hyperliquid_cli::mode::Mode;
use hyperliquid_cli::output;

#[cfg(test)]
mod cli_contract_tests;
mod cli_runtime;
mod cli_shell;

use cli_shell::{
    PANIC_COMMAND, ParsedCli, exit_for_clap_error, install_panic_hook, print_help_to_stdout,
    resolve_output_format, try_parse_cli_from,
};

const GLOBAL_OPTIONS: &str = "Global Options";

/// Global options whose values are secrets. Suggested commands never repeat them.
const SECRET_GLOBALS: &[&str] = &["private_key", "keystore_password"];

fn root_after_help() -> String {
    format!(
        "\
Examples:
  hyperliquid --format json --select coin,price --max-results 5 mids
  hyperliquid --format json schema orders create
  hyperliquid --dry-run orders create --coin BTC --side buy --price 1 --size 0.001

{}
Learn more:
  hyperliquid <command> --help
  hyperliquid --format json schema <command>",
        hyperliquid_cli::settings::env_help_section()
    )
}

#[derive(Parser, Debug)]
#[command(
    name = "hyperliquid",
    version,
    about = "Trade and query Hyperliquid from the command line",
    propagate_version = true,
    disable_help_flag = true,
    disable_version_flag = true,
    after_long_help = root_after_help()
)]
struct Cli {
    /// Output format: pretty, table, or json. Defaults to json when stdout is not a terminal or HYPERLIQUID_AGENT is set; overrides HYPERLIQUID_FORMAT
    #[allow(
        dead_code,
        reason = "declares the option for clap; the format precedence reads it from the ArgMatches so a failed parse resolves the same way"
    )]
    #[arg(long, short, global = true, value_enum, default_value = "pretty", help_heading = GLOBAL_OPTIONS)]
    format: output::OutputFormat,

    /// Shortcut for --format json
    #[allow(dead_code, reason = "see `format`")]
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    json: bool,
    /// Private key to sign with. Prefer HYPERLIQUID_PRIVATE_KEY: command-line options appear in process lists
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    private_key: Option<String>,

    /// Foundry-compatible keystore file to decrypt for signing
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    keystore: Option<PathBuf>,

    /// Password for --keystore. Prefer HYPERLIQUID_KEYSTORE_PASSWORD: command-line options appear in process lists
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    keystore_password: Option<String>,

    /// Wallet name, id, or address to use as the signer
    #[arg(
        long,
        global = true,
        help_heading = GLOBAL_OPTIONS,
        conflicts_with_all = ["private_key", "keystore", "keystore_password", "ows_signer"]
    )]
    account: Option<String>,

    /// OWS signer selector (0x address, or wallet name/id from the OWS vault)
    #[arg(
        long,
        global = true,
        value_name = "SELECTOR",
        alias = "wallet",
        help_heading = GLOBAL_OPTIONS,
        conflicts_with_all = ["private_key", "keystore", "keystore_password", "account"]
    )]
    ows_signer: Option<String>,

    /// Sign with the Bankr wallet linked by `wallet login` (or BANKR_API_KEY)
    #[arg(
        long,
        global = true,
        help_heading = GLOBAL_OPTIONS,
        conflicts_with_all = ["private_key", "keystore", "keystore_password", "account", "ows_signer"]
    )]
    bankr: bool,

    /// Use testnet instead of mainnet
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    testnet: bool,

    /// Field selection for JSON output (comma-separated). Unknown fields are omitted when the output shape is dynamic
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    select: Option<String>,

    /// Strip envelope and metadata, return only data
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    results_only: bool,

    /// Limit the top-level result count for JSON, table, and pretty output
    #[arg(long, global = true, value_name = "N", value_parser = hyperliquid_cli::stream::parse_positive_usize, help_heading = GLOBAL_OPTIONS)]
    max_results: Option<usize>,

    /// Validate and preview mutating commands without side effects
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    dry_run: bool,

    /// Never prompt. Commands fail with the missing input named instead
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    non_interactive: bool,

    /// Disable color in output
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    no_color: bool,

    /// Disable release update checks for this invocation
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    no_update_check: bool,

    /// Raw JSON payload for mutating commands
    #[arg(long, global = true, conflicts_with = "payload_file", help_heading = GLOBAL_OPTIONS)]
    payload_json: Option<String>,

    /// Raw JSON payload file for mutating commands, or '-' for stdin
    #[arg(long, global = true, conflicts_with = "payload_json", help_heading = GLOBAL_OPTIONS)]
    payload_file: Option<String>,
}

fn error_output(format: output::OutputFormat) -> errors::ErrorOutput {
    match format {
        output::OutputFormat::Json => errors::ErrorOutput::Json,
        output::OutputFormat::Pretty | output::OutputFormat::Table => errors::ErrorOutput::Human,
    }
}

#[tokio::main]
async fn main() {
    install_panic_hook();
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let ParsedCli {
        cli,
        selected,
        requested_format,
    } = match try_parse_cli_from(&raw_args) {
        Ok(parsed) => parsed,
        Err(err) => exit_for_clap_error(err, &raw_args),
    };
    output::colors::init(cli.no_color);
    if let Some(selected) = &selected {
        let path: Vec<String> = selected
            .entry
            .spec
            .path
            .iter()
            .map(|part| (*part).to_string())
            .collect();
        let _ = PANIC_COMMAND.set(path.join(" "));
        errors::set_command_path(&path);
    }
    let env = output::terminal::OutputEnv::probe(requested_format.is_some());
    // Nothing can report an unreadable HYPERLIQUID_FORMAT in that format.
    let format = match resolve_output_format(requested_format, &env) {
        Ok(format) => format,
        Err(err) => errors::exit_with_error(err, errors::ErrorOutput::Json),
    };
    if let Err(err) = hyperliquid_cli::mode::validate_env() {
        errors::exit_with_error(err, error_output(format));
    }
    let session = output::Session::new(
        format,
        output::JsonRenderOptions::from_cli(
            cli.select.as_deref(),
            cli.results_only,
            cli.max_results,
        ),
        env,
    );
    let no_update_check = cli.no_update_check;
    let flags = InvocationFlags {
        testnet: cli.testnet,
        dry_run: cli.dry_run,
        non_interactive: cli.non_interactive,
        payload_json: cli.payload_json,
        payload_file: cli.payload_file,
        signer: SignerFlags {
            bankr: cli.bankr,
            private_key: cli.private_key,
            keystore: cli.keystore,
            keystore_password: cli.keystore_password,
            account: cli.account,
            ows_signer: cli.ows_signer,
        },
    };
    let mode = Mode::new(
        flags.non_interactive,
        format == output::OutputFormat::Json,
        flags.payload_from_stdin(),
        &env,
    );
    let is_update_command = selected
        .as_ref()
        .is_some_and(|selected| selected.entry.spec.path == Update::SPEC.path);
    let update_check_handle = hyperliquid_cli::update::maybe_start_update_check(
        &mode,
        no_update_check || is_update_command,
    );

    let result = match selected {
        Some(selected) => {
            let inv = Invocation::new(session, mode, flags, selected.entry.spec);
            cli_runtime::run(selected, inv).await
        }
        None => print_help_to_stdout(&[]),
    };
    if let Err(err) = result {
        // This path handles CliError and anyhow errors from command handlers.
        // clap usage errors (exit 2) are handled by exit_for_clap_error before
        // the command runs, so they don't go through this path.
        hyperliquid_cli::update::flush_update_check_silent(update_check_handle).await;
        let cli_err = errors::from_anyhow(err);
        errors::exit_with_error(cli_err, error_output(format));
    }

    hyperliquid_cli::update::flush_update_check(update_check_handle).await;
}
