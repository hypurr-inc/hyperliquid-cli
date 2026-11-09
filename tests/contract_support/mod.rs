#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use hyperliquid_cli::command_registry::CommandRegistry;
use hyperliquid_cli::commands::schema::schema_query;
use hyperliquid_cli::output::TableData;
use serde_json::{Value, json};

/// Set to `1` to rewrite the snapshot fixtures instead of checking them. The
/// one switch for every snapshot in the repository.
const UPDATE_ENV: &str = "HYPERLIQUID_UPDATE_CONTRACTS";

pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/contracts")
        .join(name)
}

/// Check `actual` against the fixture `name`, or rewrite the fixture when
/// `HYPERLIQUID_UPDATE_CONTRACTS=1`.
pub fn assert_json_fixture(name: &str, actual: &Value) {
    let pretty = format!("{}\n", serde_json::to_string_pretty(actual).unwrap());
    let path = fixture_path(name);

    if std::env::var(UPDATE_ENV).is_ok_and(|value| value == "1") {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, pretty).unwrap();
        return;
    }

    let expected = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing contract fixture {} ({err}); rerun with {UPDATE_ENV}=1 and review the diff",
            path.display()
        )
    });
    assert_eq!(
        expected,
        pretty,
        "contract fixture {} drifted; rerun with {UPDATE_ENV}=1 only when the contract change is intentional and reviewed",
        path.display()
    );
}

pub fn command_path(schema: &Value) -> String {
    schema["command_path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part.as_str().unwrap())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The command schemas exactly as `hyperliquid schema` prints them.
pub fn schema_all() -> Vec<Value> {
    match schema_query(&[]).unwrap().to_json_value() {
        Value::Array(schemas) => schemas,
        other => panic!("schema output is not an array: {other}"),
    }
}

/// The fixtures derived from the command registry: its `CommandContract`s
/// serialized (what `hyperliquid schema` prints for each command, minus the
/// derived `json_schema`) and the full schemas, `json_schema` included, of six
/// representative commands. `tests/contract_fixtures.rs` checks them and, with
/// the update switch, rewrites them.
pub fn registry_fixtures() -> Vec<(&'static str, Value)> {
    let schemas = schema_all();
    let representative = [
        "orders create",
        "orders modify",
        "transfer send",
        "wallet address",
        "borrowlend supply",
        "subscribe all-mids",
    ]
    .iter()
    .map(|command| {
        schemas
            .iter()
            .find(|schema| command_path(schema) == *command)
            .unwrap_or_else(|| panic!("missing schema for {command}"))
            .clone()
    })
    .collect::<Vec<_>>();

    vec![
        (
            "registry_inventory.json",
            json!(CommandRegistry::get().commands()),
        ),
        ("schema_representative.json", Value::Array(representative)),
    ]
}
