//! The command contract registry, generated from the registered commands.
//!
//! The clap tree supplies names, args, defaults and help; each command's
//! [`CommandSpec`] supplies policy. Nothing here is written by hand per command.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use clap::builder::ValueParser;
use clap::{Arg, ArgAction, Command};
use serde::Serialize;
use serde_json::{Value, json};

use crate::command_spec::{
    ArgSpec, CommandSpec, ConfirmationPolicy, DryRunPolicy, GroupSpec, InputKind, Lifecycle,
    Mutability, OutputContract, OwsSupport, RawPayloadPolicy, Risk, SignerSupport, StreamBound,
    Transport, YES_ARG,
};
use crate::commands;
use crate::output::serialize_row;

/// The order `schema`, the MCP tool list and the contract fixtures list commands
/// in. Every registered command appears exactly once; a test fails when one does not.
const SCHEMA_ORDER: &[&str] = &[
    "status",
    "meta",
    "perps list",
    "perps get",
    "perps funding-history",
    "perps funding-predicted",
    "perps at-oi-cap",
    "perps dexs",
    "perps dex-limits",
    "spot list",
    "spot get",
    "spot token",
    "book",
    "mids",
    "candles",
    "spread",
    "funding",
    "asset decode",
    "asset search",
    "search",
    "buy",
    "sell",
    "outcome-buy",
    "outcome-sell",
    "watch risk",
    "outcomes list",
    "outcomes get",
    "outcomes questions",
    "outcomes settled",
    "outcomes templates",
    "outcomes split",
    "outcomes merge",
    "outcomes merge-question",
    "outcomes negate",
    "builder max-fee",
    "builder approved",
    "builder approve",
    "account fills",
    "account fees",
    "account rate-limit",
    "account orders",
    "account portfolio",
    "account subaccounts",
    "account portfolio-history",
    "account ledger",
    "account funding",
    "account twap-history",
    "account twap-fills",
    "account role",
    "account multisig-signers",
    "account asset-data",
    "account abstraction",
    "account abstraction set",
    "account invalidate-nonce",
    "account reserve-weight",
    "account evm-big-blocks",
    "subaccount list",
    "subaccount create",
    "subaccount transfer",
    "subaccount spot-transfer",
    "account ls",
    "account add",
    "account set-default",
    "account remove",
    "api-wallet create",
    "api-wallet approve",
    "api-wallet list",
    "api-wallet revoke",
    "wallet create",
    "wallet import",
    "wallet show",
    "wallet address",
    "wallet reset",
    "wallet import-mnemonic",
    "wallet list",
    "wallet rename",
    "wallet delete",
    "wallet export",
    "orders open",
    "orders history",
    "orders status",
    "positions list",
    "staking validators",
    "staking summary",
    "staking rewards",
    "staking history",
    "vault list",
    "vault search",
    "vault get",
    "vault positions",
    "borrowlend rates",
    "borrowlend get",
    "borrowlend user",
    "borrowlend supply",
    "borrowlend withdraw",
    "borrowlend repay",
    "borrowlend borrow",
    "prio status",
    "referral status",
    "orders create",
    "orders scale",
    "orders batch-create",
    "orders tpsl",
    "orders trailing-stop",
    "orders cancel",
    "orders cancel-all",
    "orders modify",
    "orders batch-modify",
    "orders twap-create",
    "orders twap-cancel",
    "positions update-leverage",
    "positions update-margin",
    "positions top-up-margin",
    "orders schedule-cancel",
    "orders chase",
    "orders bracket",
    "transfer spot-to-perp",
    "transfer perp-to-spot",
    "transfer send",
    "transfer spot-send",
    "transfer send-asset",
    "transfer withdraw",
    "staking delegate",
    "staking undelegate",
    "staking deposit",
    "staking withdraw",
    "staking claim-rewards",
    "staking link initiate",
    "staking link finalize",
    "vault deposit",
    "vault withdraw",
    "prio bid",
    "referral set",
    "referral register",
    "subscribe trades",
    "subscribe orderbook",
    "subscribe candles",
    "subscribe all-mids",
    "subscribe order-updates",
    "subscribe fills",
    "subscribe bbo",
    "subscribe active-asset-ctx",
    "subscribe active-asset-data",
    "subscribe user-events",
    "subscribe user-fundings",
    "subscribe notifications",
    "subscribe twap-states",
    "subscribe twap-slice-fills",
    "subscribe clearinghouse-state",
    "subscribe open-orders",
    "subscribe spot-state",
    "schema",
    "update",
    "feedback",
    "setup",
    "mcp",
    "report-incident",
    "wallet login",
    "wallet logout",
];

#[derive(Debug, Clone)]
pub struct CommandRegistry {
    commands: Vec<CommandContract>,
}

impl CommandRegistry {
    pub fn get() -> &'static Self {
        static REGISTRY: LazyLock<CommandRegistry> = LazyLock::new(CommandRegistry::build);
        &REGISTRY
    }

    fn build() -> Self {
        let groups = commands::groups();
        let mut tree = commands::command_tree().clone();
        tree.build();
        let mut contracts = commands::entries()
            .iter()
            .map(|entry| CommandContract::generate(&tree, entry.spec, groups))
            .collect::<Vec<_>>();
        contracts.sort_by_key(|contract| {
            SCHEMA_ORDER
                .iter()
                .position(|key| *key == contract.command_key())
                .unwrap_or(SCHEMA_ORDER.len())
        });
        Self {
            commands: contracts,
        }
    }

    pub fn commands(&self) -> &[CommandContract] {
        &self.commands
    }

    pub fn find_path(&self, path: &[&str]) -> Option<&CommandContract> {
        self.commands.iter().find(|command| {
            command
                .command_path
                .iter()
                .map(String::as_str)
                .eq(path.iter().copied())
        })
    }
}

/// What the schema says about one command. Serializing it yields the entry
/// `hyperliquid schema` prints, except `json_schema`, which
/// [`CommandContract::json_schema`] derives from the same value.
#[derive(Debug, Clone, Serialize)]
pub struct CommandContract {
    pub command: String,
    pub command_path: Vec<String>,
    #[serde(rename = "group")]
    pub category: String,
    pub auth_required: bool,
    pub dangerous: bool,
    #[serde(flatten)]
    pub policy: Policy,
    pub description: String,
    #[serde(rename = "args")]
    pub inputs: Vec<InputContract>,
    #[serde(skip)]
    pub spec: &'static CommandSpec,
}

/// The policy block of a contract. It is also the `x-hyperliquid` extension of
/// the command's JSON schema.
#[derive(Debug, Clone, Serialize)]
pub struct Policy {
    pub aliases: Vec<String>,
    pub lifecycle: Lifecycle,
    pub risk: Risk,
    pub mutability: Mutability,
    pub dry_run: DryRunPolicy,
    pub raw_payload: RawPayloadPolicy,
    pub confirmation: ConfirmationPolicy,
    pub confirmation_bypass: ConfirmationBypass,
    pub ows_signer: OwsSupport,
    pub bankr_signer: SignerSupport,
    pub transport: Vec<Transport>,
    pub output_contract: OutputContract,
    pub stream_bounds: Option<StreamBoundsSchema>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfirmationBypass {
    pub supported: bool,
    pub arg: Option<&'static str>,
}

/// How a streaming command is bounded in machine contexts, read from its
/// [`StreamBound`].
#[derive(Debug, Clone, Serialize)]
pub struct StreamBoundsSchema {
    pub required_in_machine_context: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_when: Option<BTreeMap<&'static str, bool>>,
    pub cli_args: &'static [&'static str],
    pub env: Vec<&'static str>,
    pub output_mode: &'static str,
}

impl From<StreamBound> for StreamBoundsSchema {
    fn from(bound: StreamBound) -> Self {
        Self {
            required_in_machine_context: true,
            required_when: bound
                .required_when()
                .map(|flag| BTreeMap::from([(flag, true)])),
            cli_args: bound.cli_args(),
            env: bound.env().map(|env| env.name).into_iter().collect(),
            output_mode: "ndjson",
        }
    }
}

/// A contract reads as its policy: `contract.risk`, `contract.dry_run`.
impl std::ops::Deref for CommandContract {
    type Target = Policy;

    fn deref(&self) -> &Policy {
        &self.policy
    }
}

impl CommandContract {
    fn generate(tree: &Command, spec: &'static CommandSpec, groups: &[GroupSpec]) -> Self {
        let node = find_node(tree, spec.path);
        let mut args = node
            .get_arguments()
            .filter(|arg| !matches!(arg.get_id().as_str(), "help" | "version"))
            .collect::<Vec<_>>();
        for listed in spec.args {
            assert!(
                args.iter().any(|arg| arg.get_id() == listed.id),
                "command {} lists arg {} that clap does not define",
                spec.key(),
                listed.id
            );
        }
        args.sort_by_key(|arg| {
            spec.args
                .iter()
                .position(|listed| listed.id == arg.get_id())
                .unwrap_or(spec.args.len())
        });
        let usage = spec.schema_command.map_or_else(
            || {
                let mut positionals = args
                    .iter()
                    .filter(|arg| arg.is_positional())
                    .collect::<Vec<_>>();
                positionals.sort_by_key(|arg| arg.get_index());
                positionals
                    .iter()
                    .fold(format!("hyperliquid {}", spec.key()), |usage, arg| {
                        format!("{usage} <{}>", arg.get_id().as_str().to_uppercase())
                    })
            },
            str::to_string,
        );
        let aliases = groups
            .iter()
            .find(|group| group.path == &spec.path[..1] && group.schema_aliases)
            .map(|group| {
                group
                    .aliases
                    .iter()
                    .map(|alias| (*alias).to_string())
                    .collect()
            })
            .unwrap_or_default();
        let inputs = args
            .into_iter()
            .map(|arg| InputContract::generate(arg, spec.arg(arg.get_id().as_str())))
            .collect::<Vec<_>>();
        let confirmation_bypass_arg = inputs
            .iter()
            .any(|input| input.id == YES_ARG)
            .then_some(YES_ARG);
        Self {
            command: usage,
            command_path: spec.path.iter().map(|part| (*part).to_string()).collect(),
            category: spec.category.to_string(),
            auth_required: spec.auth_required,
            dangerous: spec.dangerous,
            policy: Policy {
                aliases,
                lifecycle: spec.lifecycle,
                risk: spec.risk,
                mutability: spec.mutability(),
                dry_run: spec.dry_run,
                raw_payload: spec.raw_payload,
                confirmation: spec.confirmation,
                confirmation_bypass: ConfirmationBypass {
                    supported: confirmation_bypass_arg.is_some(),
                    arg: confirmation_bypass_arg,
                },
                ows_signer: spec.ows_signer,
                bankr_signer: spec.bankr_signer(),
                transport: vec![spec.transport()],
                output_contract: output_contract(spec),
                stream_bounds: spec.stream.map(StreamBoundsSchema::from),
            },
            description: spec.description.to_string(),
            inputs,
            spec,
        }
    }

    pub fn command_key(&self) -> String {
        self.command_path.join(" ")
    }

    /// Name of the boolean flag that answers this command's confirmation prompt:
    /// [`YES_ARG`] for every command that has it.
    pub fn confirmation_bypass_arg(&self) -> Option<&'static str> {
        self.policy.confirmation_bypass.arg
    }

    pub fn input(&self, id: &str) -> Option<&InputContract> {
        self.inputs.iter().find(|input| input.id == id)
    }

    /// The name the MCP server offers the command under: the path joined with
    /// `_`, dashes spelled `_` too. Discovery and calls both use it; a test
    /// keeps the names unique.
    pub fn mcp_tool_name(&self) -> String {
        self.command_path.join("_").replace('-', "_")
    }

    /// The JSON schema of the command's inputs, with the contract's policy as
    /// the `x-hyperliquid` extension. `schema` prints it beside the contract
    /// and MCP offers it as the tool's input schema.
    pub fn json_schema(&self) -> Value {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();

        for input in &self.inputs {
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
                prop.insert("input_kind".into(), serialize_row(&kind));
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
        if !self.spec.one_of_required.is_empty() {
            schema.insert(
                "oneOf".into(),
                Value::Array(
                    self.spec
                        .one_of_required
                        .iter()
                        .map(|required| json!({ "required": required }))
                        .collect(),
                ),
            );
        }
        schema.insert("x-hyperliquid".into(), serialize_row(&self.policy));
        schema.insert("additionalProperties".into(), json!(false));
        Value::Object(schema)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgType {
    Boolean,
    Integer,
    String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InputContract {
    pub id: String,
    pub long: Option<String>,
    pub short: Option<String>,
    pub positional_index: Option<usize>,
    pub arg_type: ArgType,
    pub required: bool,
    pub multiple: bool,
    pub enum_values: Vec<String>,
    pub description: Option<String>,
    pub default: Option<String>,
    #[serde(rename = "input_kind")]
    pub kind: Option<InputKind>,
}

impl InputContract {
    fn generate(arg: &Arg, spec: Option<&ArgSpec>) -> Self {
        let flag = matches!(arg.get_action(), ArgAction::SetTrue | ArgAction::SetFalse);
        let repeats = matches!(arg.get_action(), ArgAction::Append);
        Self {
            id: arg.get_id().to_string(),
            long: arg.get_long().map(str::to_string),
            short: arg.get_short().map(String::from),
            positional_index: arg.get_index().map(|index| index - 1),
            arg_type: if spec.is_some_and(|spec| spec.schema_string) {
                ArgType::String
            } else if flag {
                ArgType::Boolean
            } else if is_integer(arg) {
                ArgType::Integer
            } else {
                ArgType::String
            },
            required: arg.is_required_set() || spec.is_some_and(|spec| spec.schema_required),
            multiple: repeats && !spec.is_some_and(|spec| spec.schema_single),
            enum_values: arg
                .get_possible_values()
                .iter()
                .map(|value| value.get_name().to_string())
                .collect(),
            description: spec
                .and_then(|spec| spec.description)
                .map(str::to_string)
                .or_else(|| arg.get_help().map(|help| format!("{help}."))),
            default: if flag {
                None
            } else {
                arg.get_default_values()
                    .first()
                    .map(|value| value.to_string_lossy().to_string())
            },
            kind: spec.and_then(|spec| spec.kind),
        }
    }
}

fn find_node<'a>(tree: &'a Command, path: &[&str]) -> &'a Command {
    path.iter().fold(tree, |node, name| {
        node.find_subcommand(name)
            .unwrap_or_else(|| panic!("command path {path:?} is not in the clap tree"))
    })
}

fn is_integer(arg: &Arg) -> bool {
    let actual = arg.get_value_parser().type_id();
    [
        ValueParser::from(clap::value_parser!(u8)),
        ValueParser::from(clap::value_parser!(u16)),
        ValueParser::from(clap::value_parser!(u32)),
        ValueParser::from(clap::value_parser!(u64)),
        ValueParser::from(clap::value_parser!(usize)),
        ValueParser::from(clap::value_parser!(i8)),
        ValueParser::from(clap::value_parser!(i16)),
        ValueParser::from(clap::value_parser!(i32)),
        ValueParser::from(clap::value_parser!(i64)),
        ValueParser::from(clap::value_parser!(isize)),
    ]
    .iter()
    .any(|integer| integer.type_id() == actual)
}

fn output_contract(spec: &CommandSpec) -> OutputContract {
    if let Some(contract) = spec.output {
        contract
    } else if matches!(
        spec.stream,
        Some(StreamBound::Subscribe | StreamBound::Polled)
    ) {
        OutputContract::BoundedNdjsonStream
    } else if spec.lifecycle == Lifecycle::InteractiveLocal {
        OutputContract::InteractiveLocal
    } else if spec.dry_run == DryRunPolicy::Optional {
        OutputContract::CommandResultOrDryRunEnvelope
    } else {
        OutputContract::JsonValue
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::command_spec::NetworkUse;

    #[test]
    fn schema_order_lists_every_registered_command_exactly_once() {
        let listed = SCHEMA_ORDER.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(
            listed.len(),
            SCHEMA_ORDER.len(),
            "SCHEMA_ORDER has a duplicate"
        );
        let registered = commands::entries()
            .iter()
            .map(|entry| entry.spec.key())
            .collect::<BTreeSet<_>>();
        let missing = registered
            .iter()
            .filter(|key| !listed.contains(key.as_str()))
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "not in SCHEMA_ORDER: {missing:?}. A command is registered twice on purpose: \
             in commands::entries (the order `--help` lists commands) and here (the order \
             `schema` and the contract fixtures list them); both orders are public and pinned."
        );
        let unknown = listed
            .iter()
            .filter(|key| !registered.contains(**key))
            .collect::<Vec<_>>();
        assert!(
            unknown.is_empty(),
            "SCHEMA_ORDER names no registered command: {unknown:?}. Remove it here, or \
             register the command in commands::entries."
        );
    }

    #[test]
    fn every_registered_command_has_exactly_one_contract() {
        let contracts = CommandRegistry::get().commands();
        let paths = contracts
            .iter()
            .map(|command| command.command_path.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(contracts.len(), commands::entries().len());
        assert_eq!(paths.len(), contracts.len(), "two contracts share a path");
    }

    /// The command words after each `hyperliquid ` in `text`, up to the first
    /// flag, placeholder or closing punctuation.
    fn advised_commands(text: &str) -> Vec<Vec<String>> {
        text.match_indices("hyperliquid ")
            .map(|(at, found)| {
                let mut words = Vec::new();
                for token in text[at + found.len()..].split_whitespace() {
                    let word = token.trim_end_matches(['`', ',', ';', '.', ')', ':']);
                    let plain = !word.is_empty()
                        && !word.starts_with('-')
                        && word
                            .chars()
                            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-');
                    if !plain {
                        break;
                    }
                    words.push(word.to_string());
                    if word.len() != token.len() {
                        break;
                    }
                }
                words
            })
            .filter(|words| !words.is_empty())
            .collect()
    }

    #[test]
    fn every_command_the_error_advice_names_is_registered() {
        let advice = crate::errors::command_advice();
        let advised = advice
            .iter()
            .flat_map(|text| advised_commands(text))
            .collect::<Vec<_>>();
        assert!(advised.len() >= 10, "found only {advised:?}");
        for words in advised {
            assert!(
                commands::entries().iter().any(|entry| {
                    words.len() >= entry.spec.path.len()
                        && entry.spec.path.iter().zip(&words).all(|(a, b)| a == b)
                }),
                "error advice names `hyperliquid {}`, which is not a command",
                words.join(" ")
            );
        }
    }

    #[test]
    fn advice_extraction_stops_at_flags_placeholders_and_punctuation() {
        assert_eq!(
            advised_commands("see `hyperliquid account rate-limit`; or `hyperliquid --help`"),
            [["account", "rate-limit"]]
        );
        assert_eq!(
            advised_commands("run hyperliquid orders create --dry-run, then hyperliquid book."),
            [vec!["orders", "create"], vec!["book"]]
        );
        assert_eq!(
            advised_commands("use `hyperliquid outcomes get #1` now"),
            [["outcomes", "get"]]
        );
    }

    #[test]
    fn a_command_that_needs_no_network_is_not_a_live_or_signed_one() {
        let mut probed_locals = Vec::new();
        let mut networkless_live = Vec::new();
        for contract in CommandRegistry::get().commands() {
            let spec = contract.spec;
            if spec.mutability() == Mutability::LocalOnly && spec.network == NetworkUse::Required {
                probed_locals.push(contract.command_key());
            }
            if spec.network == NetworkUse::None
                && (spec.auth_required || spec.signs.is_some() || spec.stream.is_some())
            {
                networkless_live.push(contract.command_key());
            }
        }
        assert!(
            probed_locals.is_empty(),
            "these change local state only, so they must not probe the API override: {probed_locals:?}"
        );
        assert!(
            networkless_live.is_empty(),
            "these name no network but sign or stream: {networkless_live:?}"
        );
    }
}
