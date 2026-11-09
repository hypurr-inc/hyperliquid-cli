use crate::cli_command::CliCommand;
use crate::command_spec::{CommandSpec, NoArgs};
use crate::invocation::Invocation;

pub struct Mcp;

impl CliCommand for Mcp {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::local_tool(
        &["mcp"],
        "system",
        "Run Model Context Protocol (MCP) stdio server",
        "Run the Model Context Protocol (MCP) server over standard input and output (stdio), exposing all Hyperliquid CLI commands as tools for AI agents.",
    )
    .hidden_from_mcp();

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        crate::mcp::run_stdio_server(inv.settings().env())
    }
}
