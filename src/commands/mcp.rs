use crate::command_spec::{CliCommand, CommandSpec, DryRunPolicy, Lifecycle, NoArgs, Risk};
use crate::invocation::Invocation;

pub struct Mcp;

impl CliCommand for Mcp {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["mcp"],
        "system",
        "Run Model Context Protocol (MCP) stdio server",
        "Run the Model Context Protocol (MCP) server over standard input and output (stdio), exposing all Hyperliquid CLI commands as tools for AI agents.",
    )
    .lifecycle(Lifecycle::InteractiveLocal)
    .risk(Risk::None)
    .dry_run(DryRunPolicy::NotSupported)
    .local();

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        crate::mcp::run_stdio_server(inv.settings().env())
    }
}
