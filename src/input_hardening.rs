//! Shared validation for agent-supplied identifiers and artifact paths.

use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path};

use crate::errors::CliError;

const DEFAULT_MAX_JSON_FILE_BYTES: u64 = 1024 * 1024;
const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_KEYS: usize = 4096;
const MAX_JSON_STRING_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct FilePolicy {
    label: &'static str,
    allow_stdin: bool,
    max_bytes: u64,
}

impl FilePolicy {
    #[must_use]
    pub const fn payload() -> Self {
        Self {
            label: "payload",
            allow_stdin: true,
            max_bytes: DEFAULT_MAX_JSON_FILE_BYTES,
        }
    }

    #[must_use]
    pub const fn json_artifact(label: &'static str) -> Self {
        Self {
            label,
            allow_stdin: false,
            max_bytes: DEFAULT_MAX_JSON_FILE_BYTES,
        }
    }
}

pub fn validate_resource_id(label: &str, value: &str) -> Result<(), CliError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(CliError::Configuration(format!("{label} cannot be empty")));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(CliError::Configuration(format!(
            "{label} cannot contain control characters"
        )));
    }
    if trimmed.contains('?') || trimmed.contains('#') {
        return Err(CliError::Configuration(format!(
            "{label} cannot contain embedded query or fragment markers"
        )));
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("%2e") || lower.contains("%2f") || lower.contains("%5c") {
        return Err(CliError::Configuration(format!(
            "{label} cannot contain percent-encoded path traversal"
        )));
    }
    if trimmed.contains("../") || trimmed.contains("..\\") {
        return Err(CliError::Configuration(format!(
            "{label} cannot contain path traversal"
        )));
    }
    Ok(())
}

pub fn validate_input_path(path: &str) -> Result<(), CliError> {
    validate_file_path(path, FilePolicy::payload())
}

pub fn validate_file_path(path: &str, policy: FilePolicy) -> Result<(), CliError> {
    if path == "-" {
        if policy.allow_stdin {
            return Ok(());
        }
        return Err(CliError::Configuration(format!(
            "{} path cannot read from stdin",
            policy.label
        )));
    }
    validate_resource_id(&format!("{} path", policy.label), path)?;
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(CliError::Configuration(format!(
            "{} path must be relative to the current working directory",
            policy.label
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(CliError::Configuration(format!(
            "{} path cannot contain parent directory traversal",
            policy.label
        )));
    }
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(CliError::Configuration(format!(
                "{} path cannot be a symlink",
                policy.label
            )));
        }
        if metadata.len() > policy.max_bytes {
            return Err(CliError::Configuration(format!(
                "{} exceeds {} byte limit",
                file_subject(policy.label),
                policy.max_bytes
            )));
        }
    }
    Ok(())
}

pub fn read_json_file(path: &Path, policy: FilePolicy) -> Result<serde_json::Value, CliError> {
    let path_string = path.to_string_lossy();
    validate_file_path(&path_string, policy)?;
    let text = if path_string == "-" {
        read_stdin_payload(policy)?
    } else {
        let bytes = fs::read(path).map_err(|err| {
            CliError::Configuration(format!(
                "failed to read {}: {err}",
                file_subject(policy.label)
            ))
        })?;
        if bytes.len() as u64 > policy.max_bytes {
            return Err(CliError::Configuration(format!(
                "{} exceeds {} byte limit",
                file_subject(policy.label),
                policy.max_bytes
            )));
        }
        String::from_utf8(bytes).map_err(|_| {
            CliError::Configuration(format!(
                "{} must be valid UTF-8 JSON",
                file_subject(policy.label)
            ))
        })?
    };
    parse_json_text(&text, policy.label)
}

pub fn parse_json_text(raw: &str, label: &str) -> Result<serde_json::Value, CliError> {
    reject_terminal_control_content(raw, label)?;
    let value = serde_json::from_str(raw)
        .map_err(|err| CliError::Configuration(format!("invalid {label} JSON: {err}")))?;
    validate_json_limits(&value, label)?;
    Ok(value)
}

pub fn redact_sensitive_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .into_iter()
                .map(|(key, value)| {
                    if is_sensitive_key(&key) {
                        (key, serde_json::json!("[redacted]"))
                    } else {
                        (key, redact_sensitive_json(value))
                    }
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(redact_sensitive_json).collect())
        }
        serde_json::Value::String(value) if value.to_ascii_lowercase().starts_with("bearer ") => {
            serde_json::json!("[redacted]")
        }
        other => other,
    }
}

fn read_stdin_payload(policy: FilePolicy) -> Result<String, CliError> {
    read_stdin_bounded(io::stdin().lock(), policy)
}

/// Read a payload from stdin to EOF, failing once it grows past the byte limit.
fn read_stdin_bounded(reader: impl io::Read, policy: FilePolicy) -> Result<String, CliError> {
    let mut bytes = Vec::new();
    reader
        .take(policy.max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| CliError::Internal(anyhow::anyhow!(err)))?;
    if bytes.len() as u64 > policy.max_bytes {
        return Err(CliError::Configuration(format!(
            "{} input exceeds {} byte limit",
            policy.label, policy.max_bytes
        )));
    }
    String::from_utf8(bytes).map_err(|_| {
        CliError::Configuration(format!("{} input must be valid UTF-8 JSON", policy.label))
    })
}

fn file_subject(label: &str) -> String {
    if label.ends_with("file") || label.ends_with("artifact") {
        label.to_string()
    } else {
        format!("{label} file")
    }
}

/// Whether `text` holds a control character that could drive a terminal.
/// Newline, carriage return and tab are ordinary whitespace in JSON text.
fn has_terminal_control(text: &str) -> bool {
    text.chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
}

fn reject_terminal_control_content(raw: &str, label: &str) -> Result<(), CliError> {
    if has_terminal_control(raw) {
        return Err(CliError::Configuration(format!(
            "{label} JSON cannot contain terminal control characters"
        )));
    }
    Ok(())
}

fn validate_json_limits(value: &serde_json::Value, label: &str) -> Result<(), CliError> {
    let mut key_count = 0;
    validate_json_limits_inner(value, label, 0, &mut key_count)
}

fn validate_json_limits_inner(
    value: &serde_json::Value,
    label: &str,
    depth: usize,
    key_count: &mut usize,
) -> Result<(), CliError> {
    if depth > MAX_JSON_DEPTH {
        return Err(CliError::Configuration(format!(
            "{label} JSON exceeds maximum nesting depth"
        )));
    }
    match value {
        serde_json::Value::Object(object) => {
            *key_count += object.len();
            if *key_count > MAX_JSON_KEYS {
                return Err(CliError::Configuration(format!(
                    "{label} JSON contains too many object keys"
                )));
            }
            for (key, value) in object {
                if key.len() > MAX_JSON_STRING_BYTES {
                    return Err(CliError::Configuration(format!(
                        "{label} JSON contains an oversized object key"
                    )));
                }
                validate_json_limits_inner(value, label, depth + 1, key_count)?;
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                validate_json_limits_inner(value, label, depth + 1, key_count)?;
            }
        }
        serde_json::Value::String(value) => {
            if has_terminal_control(value) {
                return Err(CliError::Configuration(format!(
                    "{label} JSON cannot contain terminal control characters"
                )));
            }
            if value.len() > MAX_JSON_STRING_BYTES {
                return Err(CliError::Configuration(format!(
                    "{label} JSON contains an oversized string"
                )));
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    let compact = lower
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>();

    const BROAD_NEEDLES: &[&str] = &[
        "privatekey",
        "secret",
        "password",
        "passphrase",
        "mnemonic",
        "seed",
        "credential",
        "credentials",
        "apikey",
        "authorization",
        "bearer",
    ];
    const EXACT_KEYS: &[&str] = &[
        "accesstoken",
        "refreshtoken",
        "idtoken",
        "bearertoken",
        "authtoken",
        "sessiontoken",
        "csrftoken",
        "xcsrftoken",
        "jwttoken",
        "apitoken",
        "oauthtoken",
        "personalaccesstoken",
        "signature",
    ];

    BROAD_NEEDLES.iter().any(|needle| compact.contains(needle))
        || EXACT_KEYS.contains(&compact.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_agent_hazard_identifiers() {
        for value in ["../x", "x%2ey", "coin?x=1", "coin#frag", "bad\ncoin"] {
            assert!(validate_resource_id("id", value).is_err(), "{value}");
        }
    }

    #[test]
    fn accepts_plain_identifiers_and_stdin_path() {
        validate_resource_id("id", "BTC").unwrap();
        validate_input_path("-").unwrap();
        validate_input_path("payloads/order.json").unwrap();
    }

    #[test]
    fn file_policy_rejects_stdin_when_not_allowed() {
        let err = validate_file_path("-", FilePolicy::json_artifact("orders file")).unwrap_err();
        assert!(err.to_string().contains("stdin"));
    }

    #[test]
    fn redacts_recursive_secret_keys_and_bearer_values() {
        let value = serde_json::json!({
            "api_key": "abc",
            "nested": {
                "access_token": "secret-token",
                "token": "USDC",
                "signature": "0xsig",
                "safe": "Bearer token",
                "next_token": "pagination-token",
                "token_id": "erc20-token-id",
                "transaction_signature": "public-chain-signature"
            }
        });
        let redacted = redact_sensitive_json(value);
        assert_eq!(redacted["api_key"], "[redacted]");
        assert_eq!(redacted["nested"]["access_token"], "[redacted]");
        assert_eq!(redacted["nested"]["token"], "USDC");
        assert_eq!(redacted["nested"]["signature"], "[redacted]");
        assert_eq!(redacted["nested"]["safe"], "[redacted]");
        assert_eq!(redacted["nested"]["next_token"], "pagination-token");
        assert_eq!(redacted["nested"]["token_id"], "erc20-token-id");
        assert_eq!(
            redacted["nested"]["transaction_signature"],
            "public-chain-signature"
        );
    }

    #[test]
    fn parse_json_text_rejects_escaped_terminal_controls_and_oversized_strings() {
        let control_err = parse_json_text(r#"{"note":"bad\u001b"}"#, "payload").unwrap_err();
        assert!(
            control_err
                .to_string()
                .contains("terminal control characters")
        );

        let oversized = format!(r#"{{"note":"{}"}}"#, "x".repeat(MAX_JSON_STRING_BYTES + 1));
        let oversized_err = parse_json_text(&oversized, "payload").unwrap_err();
        assert!(oversized_err.to_string().contains("oversized string"));
    }
}

#[cfg(test)]
mod stdin_payload_tests {
    use super::*;

    #[test]
    fn stdin_payload_reads_multiline_json_to_eof() {
        let input = b"{\n  \"coin\": \"BTC\",\n  \"size\": \"1\"\n}\n".as_slice();
        let text = read_stdin_bounded(input, FilePolicy::payload()).unwrap();
        let value = parse_json_text(&text, "payload").unwrap();
        assert_eq!(value["coin"], "BTC");
    }

    #[test]
    fn stdin_payload_rejects_input_past_the_byte_limit() {
        let policy = FilePolicy::payload();
        let input = vec![b' '; policy.max_bytes as usize + 1];
        let err = read_stdin_bounded(input.as_slice(), policy).unwrap_err();
        assert!(err.to_string().contains("byte limit"));
    }
}
