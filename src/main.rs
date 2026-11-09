use clap::Parser;
use std::ffi::OsString;

use hyperliquid_cli::auth::SignerFlags;
use hyperliquid_cli::errors;
use hyperliquid_cli::invocation::{Invocation, InvocationFlags};
use hyperliquid_cli::output;
use hyperliquid_cli::report;
use hyperliquid_cli::settings::Settings;
use hyperliquid_cli::terminal::{OutputFormat, Terminal};

#[cfg(test)]
mod cli_contract_tests;
mod cli_runtime;
mod cli_shell;

use cli_shell::{
    ParsedCli, exit_for_clap_error, install_panic_hook, name_command_in_panics,
    print_help_to_stdout, startup, try_parse_cli_from,
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
        hyperliquid_cli::env::help_section()
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
    format: OutputFormat,

    /// Shortcut for --format json
    #[allow(dead_code, reason = "see `format`")]
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    json: bool,

    #[command(flatten, next_help_heading = GLOBAL_OPTIONS)]
    signer: SignerFlags,

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
    #[arg(long, global = true, value_name = "N", value_parser = hyperliquid_cli::quantity::parse_positive_usize, help_heading = GLOBAL_OPTIONS)]
    max_results: Option<usize>,

    /// Validate and preview mutating commands without side effects
    #[arg(long, global = true, help_heading = GLOBAL_OPTIONS)]
    dry_run: bool,

    /// Reject a signed L1 action the exchange has not processed this long after signing (for example 30s, 5m)
    #[arg(long, global = true, value_name = "DURATION", value_parser = hyperliquid_cli::clock::parse_lifetime, help_heading = GLOBAL_OPTIONS)]
    expires_after: Option<std::time::Duration>,

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

#[tokio::main]
async fn main() {
    install_panic_hook();
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let settings = Settings::from_process();
    let terminal = Terminal::probe();
    let ParsedCli {
        cli,
        selected,
        requested_format,
    } = match try_parse_cli_from(&raw_args) {
        Ok(parsed) => parsed,
        Err(err) => exit_for_clap_error(err, &raw_args, terminal, settings.env()),
    };
    output::colors::init(cli.no_color, settings.env(), terminal);
    let command = selected
        .as_ref()
        .map(|selected| selected.entry.spec.path.join(" "));
    if let Some(command) = &command {
        name_command_in_panics(command);
    }
    let skip_update_check = cli.no_update_check
        || selected
            .as_ref()
            .is_some_and(|selected| selected.entry.spec.skips_update_check);
    let flags = InvocationFlags {
        testnet: cli.testnet,
        dry_run: cli.dry_run,
        non_interactive: cli.non_interactive,
        payload_json: cli.payload_json,
        payload_file: cli.payload_file,
        signer: cli.signer,
        expires_after: cli.expires_after,
    };
    let env = settings.env().clone();
    let mode = match startup(requested_format, &flags, terminal, &env) {
        Ok(mode) => mode,
        Err((err, format)) => report::exit_with_error(err, format, command.as_deref(), &env),
    };
    let session = output::Session::new(
        output::JsonRenderOptions::from_cli(
            cli.select.as_deref(),
            cli.results_only,
            cli.max_results,
        ),
        &mode,
    );
    let update_check_handle =
        hyperliquid_cli::update::maybe_start_update_check(&mode, skip_update_check, &env);

    let result = match selected {
        Some(selected) => {
            let inv = Invocation::new(settings, session, mode, flags, selected.entry.spec);
            cli_runtime::run(selected, inv).await
        }
        None => print_help_to_stdout(&[]),
    };
    if let Err(err) = result {
        // This path handles CliError and anyhow errors from command handlers.
        // clap usage errors (exit 2) are handled by exit_for_clap_error before
        // the command runs, so they don't go through this path.
        hyperliquid_cli::update::flush_update_check_silent(update_check_handle).await;
        report::exit_with_error(
            errors::from_anyhow(err),
            mode.format,
            command.as_deref(),
            &env,
        );
    }

    hyperliquid_cli::update::flush_update_check(update_check_handle).await;
}
