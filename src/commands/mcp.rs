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
        crate::mcp::run_stdio_server(inv.settings().env(), super::registry())
    }
}

/// What the server offers and how a tool call becomes a command line, read
/// against the real registry. They live here, not in `crate::mcp`, because
/// the library module does not know the commands.
#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::command_registry::CommandContract;
    use crate::commands::registry;
    use crate::mcp::{DEFAULT_DEADLINE, McpSettings, command_args, exposed};

    fn contract(path: &[&str]) -> &'static CommandContract {
        registry().find_path(path).unwrap()
    }

    #[test]
    fn positional_and_repeated_arguments_follow_contract() {
        let args = command_args(
            contract(&["schema"]),
            &json!({"command_path":["orders","create"]}),
        )
        .unwrap();
        assert_eq!(args, ["--format", "json", "schema", "orders", "create"]);
        let cmd = registry()
            .commands()
            .iter()
            .find(|cmd| {
                cmd.inputs
                    .iter()
                    .any(|input| input.id == "coin" && input.positional_index.is_some())
            })
            .unwrap();
        let args = command_args(cmd, &json!({"coin":"BTC"})).unwrap();
        assert_eq!(args.last().unwrap(), "BTC");
        assert!(!args.iter().any(|s| s == "--coin"));
    }

    #[test]
    fn json_options_and_confirmation_boundary() {
        let cmd = contract(&["report-incident"]);
        let args = command_args(cmd, &json!({"context":{"a":true}})).unwrap();
        assert!(args.iter().any(|s| s == "--context={\"a\":true}"));
        let args = command_args(cmd, &json!({"context":false})).unwrap();
        assert!(args.iter().any(|s| s == "--context=false"));
        let err = command_args(contract(&["wallet", "export"]), &json!({"yes":true})).unwrap_err();
        assert!(err.contains("confirmation bypass"), "{err}");
        assert!(command_args(cmd, &json!({"unknown":true})).is_err());
        assert!(command_args(cmd, &json!([])).is_err());
    }

    fn exposed_to(cmd: &CommandContract, allow_unsafe: bool) -> bool {
        cmd.spec.exposed_to_mcp(allow_unsafe)
    }

    #[test]
    fn default_exposure_is_public_reads_only() {
        for cmd in registry().commands() {
            if exposed_to(cmd, false) {
                assert!(!cmd.spec.changes_state(), "{}", cmd.command_key());
                assert!(!cmd.dangerous, "{}", cmd.command_key());
            }
        }
        assert!(exposed_to(contract(&["mids"]), false));
        for path in [
            &["orders", "create"][..],
            &["wallet", "show"],
            &["positions", "list"],
        ] {
            assert!(!exposed_to(contract(path), false), "{path:?}");
            assert!(exposed_to(contract(path), true), "{path:?}");
        }
        let mcp = contract(&["mcp"]);
        assert!(!exposed_to(mcp, false) && !exposed_to(mcp, true));
    }

    #[test]
    fn tool_names_are_unique_across_every_command_that_can_be_offered() {
        let mut seen = std::collections::BTreeMap::new();
        for cmd in registry()
            .commands()
            .iter()
            .filter(|cmd| exposed_to(cmd, true))
        {
            let previous = seen.insert(cmd.mcp_tool_name(), cmd.command_key());
            assert!(
                previous.is_none(),
                "`{}` and `{}` are both the tool {}",
                previous.unwrap_or_default(),
                cmd.command_key(),
                cmd.mcp_tool_name()
            );
        }
        assert_eq!(
            contract(&["report-incident"]).mcp_tool_name(),
            "report_incident"
        );
        assert_eq!(
            contract(&["orders", "create"]).mcp_tool_name(),
            "orders_create"
        );
    }

    #[test]
    fn the_tool_list_offers_exactly_what_a_call_accepts() {
        for allow_unsafe in [false, true] {
            let settings = McpSettings {
                allow_unsafe,
                deadline: DEFAULT_DEADLINE,
            };
            for cmd in exposed(registry(), &settings) {
                assert!(exposed_to(cmd, allow_unsafe), "{}", cmd.command_key());
            }
            assert_eq!(
                exposed(registry(), &settings).count(),
                registry()
                    .commands()
                    .iter()
                    .filter(|cmd| exposed_to(cmd, allow_unsafe))
                    .count()
            );
        }
    }
}
