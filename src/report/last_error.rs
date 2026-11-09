//! The record of the latest failure, for `report-incident`.
//!
//! [`exit_with_error`](super::exit_with_error) writes the error envelope, with
//! the failing command under `data.command`, to the config directory before it
//! prints anything. [`LastError::load`] reads that file back. Writer and reader
//! live here so the file's shape is defined once.

use std::path::Path;

use serde::Deserialize;

use crate::errors::{CliError, ErrorEnvelope};
use crate::settings::EnvSource;

/// The parts of the recorded envelope that an incident report quotes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LastError {
    pub category: Option<String>,
    pub reason: Option<String>,
    pub exit_code: Option<i32>,
    #[serde(default, rename = "data", deserialize_with = "command_from_data")]
    pub command: Option<String>,
}

fn command_from_data<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Data {
        command: Option<String>,
    }
    Ok(Option::<Data>::deserialize(deserializer)?.and_then(|data| data.command))
}

impl LastError {
    /// The latest recorded failure, or `None` when nothing was recorded or the
    /// file isn't a recorded envelope.
    #[must_use]
    pub fn load(env: &EnvSource) -> Option<Self> {
        Self::read(&crate::paths::last_error_file(env)?)
    }

    fn read(path: &Path) -> Option<Self> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    /// `category / reason (exit N)`, with placeholders for anything the record lacks.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "{} / {} (exit {})",
            self.category.as_deref().unwrap_or("unknown"),
            self.reason.as_deref().unwrap_or("unspecified"),
            self.exit_code.unwrap_or(1)
        )
    }

    /// What an incident report may quote: the classification only, never the
    /// message, the remote text, or the command's arguments.
    #[must_use]
    pub fn classification(&self) -> serde_json::Value {
        let mut fields = serde_json::Map::new();
        if let Some(category) = &self.category {
            fields.insert("category".into(), category.clone().into());
        }
        if let Some(reason) = &self.reason {
            fields.insert("reason".into(), reason.clone().into());
        }
        if let Some(exit_code) = self.exit_code {
            fields.insert("exit_code".into(), exit_code.into());
        }
        serde_json::Value::Object(fields)
    }
}

/// Write `err` as the latest failure. Best effort: failing to write the record
/// must not hide the error being reported.
pub(super) fn record(err: &CliError, command: Option<&str>, env: &EnvSource) {
    let Some(path) = crate::paths::last_error_file(env) else {
        return;
    };
    let _ = write(&path, err, command);
}

fn write(path: &Path, err: &CliError, command: Option<&str>) -> std::io::Result<()> {
    let mut envelope = ErrorEnvelope::from_cli_error(err);
    if let Some(command) = command {
        envelope.data.insert("command".into(), command.into());
    }
    let json = serde_json::to_string_pretty(&envelope).map_err(std::io::Error::other)?;
    crate::private_fs::write_atomic(path, json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_failure_round_trips_through_the_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last_error.json");

        write(&path, &CliError::rate_limited(), Some("orders create")).unwrap();

        assert_eq!(
            LastError::read(&path),
            Some(LastError {
                category: Some("rate_limited".into()),
                reason: Some("rate_limited".into()),
                exit_code: Some(11),
                command: Some("orders create".into()),
            })
        );
    }

    #[test]
    fn failure_without_a_resolved_command_reads_back_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last_error.json");

        write(&path, &CliError::AuthRequired, None).unwrap();

        let loaded = LastError::read(&path).unwrap();
        assert_eq!(loaded.reason.as_deref(), Some("auth_required"));
        assert_eq!(loaded.command, None);
    }

    #[test]
    fn a_missing_or_foreign_file_is_no_record() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(LastError::read(&dir.path().join("absent.json")), None);

        let foreign = dir.path().join("foreign.json");
        std::fs::write(&foreign, "[1, 2]").unwrap();
        assert_eq!(LastError::read(&foreign), None);
    }

    #[test]
    fn summary_and_classification_quote_only_the_classification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last_error.json");
        write(
            &path,
            &CliError::Configuration("secret detail".into()),
            Some("orders create"),
        )
        .unwrap();

        let loaded = LastError::read(&path).unwrap();
        assert_eq!(loaded.summary(), "configuration / invalid_config (exit 2)");
        assert_eq!(
            loaded.classification(),
            serde_json::json!({
                "category": "configuration",
                "reason": "invalid_config",
                "exit_code": 2,
            })
        );
        assert_eq!(
            LastError::default().summary(),
            "unknown / unspecified (exit 1)"
        );
    }
}
