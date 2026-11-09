//! Machine-readable command schema output for agents.
//!
//! The entries are the registry's [`CommandContract`]s serialized, plus the JSON
//! schema derived from each one.

use clap::Args;
use serde_json::{Value, json};

use crate::command_registry::{ArgType, CommandContract, CommandRegistry};
use crate::command_spec::{CliCommand, CommandSpec, OutputContract, arg};
use crate::errors::CliError;
use crate::invocation::Invocation;
use crate::output::TableData;

/// The schema entries asked for: the JSON they print, and the table rows built
/// from the same contracts.
#[derive(Debug, Clone)]
pub struct SchemaOutput {
    json: Value,
    rows: Vec<Vec<String>>,
}

impl SchemaOutput {
    /// The entry of the one command named by its exact path.
    fn one(contract: &CommandContract) -> Result<Self, CliError> {
        Ok(Self {
            json: schema_entry(contract)?,
            rows: vec![table_row(contract)],
        })
    }

    /// A list of entries, in registry order.
    fn many<'a>(contracts: impl Iterator<Item = &'a CommandContract>) -> Result<Self, CliError> {
        let contracts: Vec<_> = contracts.collect();
        Ok(Self {
            json: Value::Array(
                contracts
                    .iter()
                    .map(|contract| schema_entry(contract))
                    .collect::<Result<_, _>>()?,
            ),
            rows: contracts
                .iter()
                .map(|contract| table_row(contract))
                .collect(),
        })
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
    inv.print_untimed(&schema_query(command_path)?);
    Ok(())
}

/// One command's entry: its contract plus the JSON schema of its inputs.
fn schema_entry(contract: &CommandContract) -> Result<Value, CliError> {
    #[derive(serde::Serialize)]
    struct Entry<'a> {
        #[serde(flatten)]
        contract: &'a CommandContract,
        json_schema: Value,
    }
    serde_json::to_value(Entry {
        contract,
        json_schema: json_schema(contract)?,
    })
    .map_err(|err| CliError::Internal(anyhow::anyhow!(err)))
}

pub fn schema_query(command_path: &[String]) -> Result<SchemaOutput, anyhow::Error> {
    let contracts = CommandRegistry::get().commands();

    let output = if command_path.is_empty() {
        SchemaOutput::many(contracts.iter())?
    } else if let Some(contract) = contracts
        .iter()
        .find(|contract| contract.command_path.as_slice() == command_path)
    {
        SchemaOutput::one(contract)?
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
        SchemaOutput::many(matches)?
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

/// The JSON schema of the command's inputs, with the contract's policy as the
/// `x-hyperliquid` extension.
fn json_schema(contract: &CommandContract) -> Result<Value, CliError> {
    let internal = |err: serde_json::Error| CliError::Internal(anyhow::anyhow!(err));
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for input in &contract.inputs {
        let scalar = match input.arg_type {
            ArgType::Boolean => "boolean",
            ArgType::Integer => "number",
            ArgType::String => "string",
        };
        let mut prop = serde_json::Map::new();
        if input.multiple {
            prop.insert("type".into(), json!("array"));
            prop.insert("items".into(), json!({"type": scalar}));
        } else {
            prop.insert("type".into(), json!(scalar));
        }
        if !input.enum_values.is_empty() {
            prop.insert("enum".into(), json!(input.enum_values));
        }
        if let Some(description) = &input.description {
            prop.insert("description".into(), json!(description));
        }
        if let Some(default) = &input.default {
            prop.insert("default".into(), json!(default));
        }
        if let Some(kind) = input.kind {
            prop.insert(
                "input_kind".into(),
                serde_json::to_value(kind).map_err(internal)?,
            );
        }
        properties.insert(input.id.clone(), Value::Object(prop));
        if input.required {
            required.push(json!(input.id));
        }
    }

    let mut schema = serde_json::Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    if !contract.spec.one_of_required.is_empty() {
        schema.insert(
            "oneOf".into(),
            Value::Array(
                contract
                    .spec
                    .one_of_required
                    .iter()
                    .map(|required| json!({ "required": required }))
                    .collect(),
            ),
        );
    }
    schema.insert(
        "x-hyperliquid".into(),
        serde_json::to_value(&contract.policy).map_err(internal)?,
    );
    schema.insert("additionalProperties".into(), json!(false));
    Ok(Value::Object(schema))
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
    .local()
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

    #[test]
    fn the_table_rows_come_from_the_contracts_the_json_entries_are_built_from() {
        let path = ["orders".to_string(), "create".to_string()];
        let output = schema_query(&path).unwrap();
        let contract = CommandRegistry::get()
            .find_path(&["orders", "create"])
            .unwrap();
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

        let all = schema_query(&[]).unwrap();
        assert_eq!(all.rows().len(), CommandRegistry::get().commands().len());
        assert_eq!(
            all.to_json_value().as_array().unwrap().len(),
            all.rows().len()
        );
    }
}
