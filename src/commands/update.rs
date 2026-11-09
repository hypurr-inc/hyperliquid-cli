use crate::invocation::Invocation;

use crate::command_spec::{CliCommand, CommandSpec, DryRunPolicy, NoArgs, Risk};

pub struct Update;

impl CliCommand for Update {
    type Args = NoArgs;

    const SPEC: CommandSpec = CommandSpec::local_tool(
        &["update"],
        "system",
        "Update this binary to the latest GitHub release",
        "Update this binary from the latest GitHub release on Linux/macOS. Windows users should rerun install.sh to install the latest .zip release. Use --dry-run to preview without replacing the executable.",
    )
    .risk(Risk::LocalState)
    .dry_run(DryRunPolicy::Optional)
    .dangerous()
    .skips_update_check();

    async fn run(inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
        crate::update::update(inv).await
    }
}
