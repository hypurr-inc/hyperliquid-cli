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

#[derive(Debug, Clone)]
pub struct SchemaOutput {
    value: Value,
}

impl TableData for SchemaOutput {
    fn headers(&self) -> Vec<&str> {
        vec!["Command", "Group", "Dangerous", "Description"]
    }

    fn rows(&self) -> Vec<Vec<String>> {
        let commands: Vec<Value> = match &self.value {
            Value::Array(values) => values.clone(),
            value => vec![value.clone()],
        };
        commands
            .iter()
            .map(|command| {
                vec![
                    command
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    command
                        .get("group")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    command
                        .get("dangerous")
                        .and_then(Value::as_bool)
                        .map(|value| value.to_string())
                        .unwrap_or_default(),
                    command
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ]
            })
            .collect()
    }

    fn to_json_value(&self) -> Value {
        self.value.clone()
    }
}

pub fn show(inv: &Invocation, command_path: &[String]) -> Result<(), anyhow::Error> {
    inv.print_untimed(&schema_query(command_path)?);
    Ok(())
}

/// One command's entry: its contract plus the JSON schema of its inputs.
fn schema_entry(contract: &CommandContract) -> Result<Value, CliError> {
    let mut entry =
        serde_json::to_value(contract).map_err(|err| CliError::Internal(anyhow::anyhow!(err)))?;
    let schema = json_schema(contract)?;
    entry
        .as_object_mut()
        .expect("a contract serializes to an object")
        .insert("json_schema".into(), schema);
    Ok(entry)
}

fn schema_entries<'a>(
    contracts: impl Iterator<Item = &'a CommandContract>,
) -> Result<Value, CliError> {
    contracts
        .map(schema_entry)
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

pub fn schema_query(command_path: &[String]) -> Result<SchemaOutput, anyhow::Error> {
    let contracts = CommandRegistry::get().commands();

    let value = if command_path.is_empty() {
        schema_entries(contracts.iter())?
    } else if let Some(contract) = contracts
        .iter()
        .find(|contract| contract.command_path.as_slice() == command_path)
    {
        schema_entry(contract)?
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
        schema_entries(matches)?
    };

    Ok(SchemaOutput { value })
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
