//! The entries are the registry's [`CommandContract`]s serialized, plus the JSON
//! schema derived from each one.

use clap::Args;
use serde_json::Value;

use crate::cli_command::CliCommand;
use crate::command_registry::{CommandContract, CommandRegistry};
use crate::command_spec::{CommandSpec, NetworkUse, OutputContract, arg};
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::output::{TableData, serialize_row};

#[derive(Debug, Clone)]
pub struct SchemaOutput {
    json: Value,
    rows: Vec<Vec<String>>,
}

impl SchemaOutput {
    fn one(contract: &CommandContract) -> Self {
        Self {
            json: schema_entry(contract),
            rows: vec![table_row(contract)],
        }
    }

    fn many<'a>(contracts: impl Iterator<Item = &'a CommandContract>) -> Self {
        let contracts: Vec<_> = contracts.collect();
        Self {
            json: Value::Array(
                contracts
                    .iter()
                    .map(|contract| schema_entry(contract))
                    .collect(),
            ),
            rows: contracts
                .iter()
                .map(|contract| table_row(contract))
                .collect(),
        }
    }
}

fn table_row(contract: &CommandContract) -> Vec<String> {
    vec![
        contract.command.clone(),
        contract.category.clone(),
        contract.dangerous.to_string(),
        contract.description.clone(),
    ]
}

impl TableData for SchemaOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Command", "Group", "Dangerous", "Description"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        self.rows.clone()
    }

    fn to_json_value(&self) -> Value {
        self.json.clone()
    }
}

pub fn show(inv: &Invocation, command_path: &[String]) -> Result<(), anyhow::Error> {
    inv.print_untimed(&schema_query(super::registry(), command_path)?);
    Ok(())
}

fn schema_entry(contract: &CommandContract) -> Value {
    #[derive(serde::Serialize)]
    struct Entry<'a> {
        #[serde(flatten)]
        contract: &'a CommandContract,
        json_schema: Value,
    }
    serialize_row(&Entry {
        contract,
        json_schema: contract.json_schema(),
    })
}

pub fn schema_query(
    registry: &CommandRegistry,
    command_path: &[String],
) -> Result<SchemaOutput, anyhow::Error> {
    let contracts = registry.commands();

    let output = if command_path.is_empty() {
        SchemaOutput::many(contracts.iter())
    } else if let Some(contract) = contracts
        .iter()
        .find(|contract| contract.command_path.as_slice() == command_path)
    {
        SchemaOutput::one(contract)
    } else {
        let mut matches = contracts
            .iter()
            .filter(|contract| command_path_is_prefix(command_path, &contract.command_path))
            .peekable();
        if matches.peek().is_none() {
            return Err(CliError::Unsupported(format!(
                "schema command '{}' not found",
                command_path.join(" ")
            ))
            .into());
        }
        SchemaOutput::many(matches)
    };

    Ok(output)
}

fn command_path_is_prefix(prefix: &[String], command_path: &[String]) -> bool {
    prefix.len() < command_path.len()
        && command_path
            .iter()
            .zip(prefix)
            .all(|(actual, requested)| actual == requested)
}

#[derive(Args, Debug, Clone)]
pub struct SchemaArgs {
    /// Optional command path, for example: orders create
    pub command_path: Vec<String>,
}

pub struct Schema;

impl CliCommand for Schema {
    type Args = SchemaArgs;

    const SPEC: CommandSpec = CommandSpec::read(
        &["schema"],
        "system",
        "Show machine-readable command schemas",
        "Show machine-readable command schemas for agents.",
    )
    .long_about(
        "Show machine-readable command schemas.\n\
\n\
Examples:\n  \
hyperliquid --format json schema\n  \
hyperliquid --format json schema orders create\n  \
hyperliquid --format json --select command,description schema orders",
    )
    .network(NetworkUse::None)
    .schema_command("hyperliquid schema")
    .output(OutputContract::SchemaArrayOrObject)
    .args(&[arg("command_path")
        .describe("Optional command path to show a single schema.")
        .resource_id("schema command path")]);

    async fn run(inv: &Invocation, args: &SchemaArgs) -> Result<(), anyhow::Error> {
        show(inv, &args.command_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::registry;

    #[test]
    fn the_table_rows_come_from_the_contracts_the_json_entries_are_built_from() {
        let path = ["orders".to_string(), "create".to_string()];
        let output = schema_query(registry(), &path).unwrap();
        let contract = registry().find_path(&["orders", "create"]).unwrap();
        assert_eq!(
            output.rows(),
            vec![vec![
                contract.command.clone(),
                contract.category.clone(),
                "true".to_string(),
                contract.description.clone(),
            ]]
        );
        let entry = output.to_json_value();
        assert_eq!(entry["command"], contract.command.as_str());
        assert!(entry["json_schema"].is_object());

        let all = schema_query(registry(), &[]).unwrap();
        assert_eq!(all.rows().len(), registry().commands().len());
        assert_eq!(
            all.to_json_value().as_array().unwrap().len(),
            all.rows().len()
        );
    }
}
