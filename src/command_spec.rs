//! One definition per command.
//!
//! A command's [`CommandSpec`]: the policy the schema, help notes and the run
//! pipeline read, as data. A command type pairs it with its `run` in
//! [`crate::cli_command::CliCommand`].

use clap::ArgMatches;
use serde::Serialize;

use crate::assets::{AssetQuery, parse_asset_query};
use crate::errors::CliError;
use crate::input_hardening::validate_resource_id;
use crate::network::Network;
use crate::signable::{Signs, UserSigned};
use crate::stream_bounds::StreamBound;

/// The flag that answers a confirmation prompt. Every command with this arg and
/// [`ConfirmationPolicy::Prompt`] accepts it in place of the prompt.
pub const YES_ARG: &str = crate::yes_arg!();

/// The flag as typed, for messages and prompts that name it.
pub const YES_FLAG: &str = crate::yes_flag!();

/// `--yes` without a description of its own.
pub const YES: ArgSpec = arg(YES_ARG).kind(InputKind::Boolean);
/// `--yes` for commands whose prompt is skipped for deliberate automation.
pub const YES_FOR_AUTOMATION: ArgSpec =
    YES.describe("Skip confirmation prompt for deliberate automation.");
/// `--yes` for commands that only ever act on a live account.
pub const YES_FOR_LIVE_EXECUTION: ArgSpec =
    YES.describe("Skip confirmation prompt for live execution.");

/// `--on-behalf-of`, hardened: the acting-account selector of a command that
/// sends for a vault or subaccount.
pub const ON_BEHALF_OF: ArgSpec = arg("on_behalf_of")
    .kind(InputKind::ActingAccountSelector)
    .resource_id("on-behalf-of selector");
/// [`ON_BEHALF_OF`] with the description the schema gives it.
pub const ON_BEHALF_OF_DESCRIBED: ArgSpec = ON_BEHALF_OF.describe(
    "Acting-account selector for vaultAddress: subaccount/vault address, wallet name, or wallet id.",
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    ReadOnly,
    Streaming,
    InteractiveLocal,
    LiveMutating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    None,
    AccountState,
    FundsMovement,
    LocalSecret,
    LocalState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DryRunPolicy {
    NotSupported,
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RawPayloadPolicy {
    Unsupported,
    DryRunOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationPolicy {
    None,
    Prompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OwsSupport {
    NotRequired,
    AddressSelectorSupported,
    ExperimentalFeatureGated,
    LocalOnly,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignerSupport {
    NotRequired,
    Supported,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputContract {
    JsonValue,
    SchemaArrayOrObject,
    BoundedNdjsonStream,
    CommandResultOrDryRunEnvelope,
    InteractiveLocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    SignerSelector,
    ActingAccountSelector,
    PublicUserSelector,
    RawDestinationAddress,
    ProtocolObjectAddress,
    MarketSymbol,
    VenueSelector,
    Boolean,
    Price,
    Amount,
    FilePath,
    Duration,
    Identifier,
}

/// What the schema reports as a command's effect. Derived once from lifecycle
/// and risk by [`CommandSpec::mutability`]; nothing else interprets that pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mutability {
    ReadOnly,
    Streaming,
    LocalOnly,
    LiveMutating,
}

/// Whether and how a command reaches the Hyperliquid API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkUse {
    /// Never: the run resolves no endpoint and its dry-run preview names no network.
    None,
    /// Maybe: the endpoint is resolved, but the run may finish without it, so
    /// a configured API override is not probed up front.
    Optional,
    /// Always: the endpoint is resolved and a configured override is probed
    /// before the command starts.
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    CliProcess,
    CliInteractive,
}

/// Rejects a malformed value before the command runs.
#[derive(Debug, Clone, Copy)]
pub enum Check {
    ResourceId(&'static str),
    /// Like `ResourceId`, but `#N` / `+N` outcome notation passes untouched.
    AssetInput(&'static str),
}

impl Check {
    fn run(self, value: &str) -> Result<(), CliError> {
        match self {
            Self::ResourceId(label) => validate_resource_id(label, value),
            Self::AssetInput(label) => {
                if matches!(parse_asset_query(value), AssetQuery::Outcome(_)) {
                    return Ok(());
                }
                validate_resource_id(label, value)
            }
        }
    }
}

/// What a command says about one of its clap args beyond what clap knows.
#[derive(Debug, Clone, Copy)]
pub struct ArgSpec {
    pub id: &'static str,
    pub kind: Option<InputKind>,
    /// Schema description when it is not the clap help text plus a period.
    pub description: Option<&'static str>,
    pub check: Option<Check>,
    /// The value is a secret: it must not arrive on the command line in machine
    /// contexts, and no suggested command repeats it. The pipeline's redaction
    /// reads this, not a list of option names.
    pub secret: bool,
    /// The schema lists the arg as required although clap accepts its absence.
    pub schema_required: bool,
    /// The schema lists the arg as single-valued although clap repeats it.
    pub schema_single: bool,
    /// The schema lists the arg as a string although clap treats it as a flag.
    pub schema_string: bool,
}

#[must_use]
pub const fn arg(id: &'static str) -> ArgSpec {
    ArgSpec {
        id,
        kind: None,
        description: None,
        check: None,
        secret: false,
        schema_required: false,
        schema_single: false,
        schema_string: false,
    }
}

impl ArgSpec {
    #[must_use]
    pub const fn kind(mut self, kind: InputKind) -> Self {
        self.kind = Some(kind);
        self
    }

    #[must_use]
    pub const fn describe(mut self, description: &'static str) -> Self {
        self.description = Some(description);
        self
    }

    #[must_use]
    pub const fn resource_id(mut self, label: &'static str) -> Self {
        self.check = Some(Check::ResourceId(label));
        self
    }

    #[must_use]
    pub const fn asset_input(mut self, label: &'static str) -> Self {
        self.check = Some(Check::AssetInput(label));
        self
    }

    #[must_use]
    pub const fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    #[must_use]
    pub const fn schema_required(mut self) -> Self {
        self.schema_required = true;
        self
    }

    #[must_use]
    pub const fn schema_single(mut self) -> Self {
        self.schema_single = true;
        self
    }

    #[must_use]
    pub const fn schema_string(mut self) -> Self {
        self.schema_string = true;
        self
    }
}

/// Everything about a command that is not clap's own knowledge of its args.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    pub path: &'static [&'static str],
    /// The schema's `group`: the family a command is listed under. Not the clap
    /// grouping (see [`GroupSpec`]), which follows `path`.
    pub category: &'static str,
    pub about: &'static str,
    pub long_about: Option<&'static str>,
    pub description: &'static str,
    pub auth_required: bool,
    pub dangerous: bool,
    pub lifecycle: Lifecycle,
    pub risk: Risk,
    pub dry_run: DryRunPolicy,
    pub raw_payload: RawPayloadPolicy,
    pub confirmation: ConfirmationPolicy,
    /// What a run asks the selected signer to sign. `None` for a command that
    /// signs nothing. The signer is checked against it before the command
    /// starts, and the schema reports which providers can.
    pub signs: Option<Signs<'static>>,
    /// The action only the master account may sign: a stored API wallet is
    /// refused for it, live and dry run alike.
    pub master_only: Option<&'static str>,
    pub ows_signer: OwsSupport,
    /// Mainnet asks for confirmation; `--testnet` runs without asking.
    pub testnet_unprompted: bool,
    /// The confirmation prompt applies only when this arg is present.
    pub confirms_with_arg: Option<&'static str>,
    /// Whether and how the run reaches the API; the pipeline resolves the
    /// endpoint, signer and override probe from this alone.
    pub network: NetworkUse,
    /// The MCP server never offers the command as a tool (it is the server).
    pub hidden_from_mcp: bool,
    /// The command is itself the update: the passive update check stays off.
    pub skips_update_check: bool,
    pub stream: Option<StreamBound>,
    /// Error text for `--dry-run` on a command that does not preview, when it is
    /// not the generic one.
    pub dry_run_refusal: Option<&'static str>,
    pub cli_aliases: &'static [&'static str],
    /// Schema `command` text when it is not the path plus positionals.
    pub schema_command: Option<&'static str>,
    /// Output contract when it is not derived from the other fields.
    pub output: Option<OutputContract>,
    pub one_of_required: &'static [&'static [&'static str]],
    pub args: &'static [ArgSpec],
}

impl CommandSpec {
    /// A read-only command that needs no signer and has no preview.
    #[must_use]
    pub const fn read(
        path: &'static [&'static str],
        category: &'static str,
        about: &'static str,
        description: &'static str,
    ) -> Self {
        Self {
            path,
            category,
            about,
            long_about: None,
            description,
            auth_required: false,
            dangerous: false,
            lifecycle: Lifecycle::ReadOnly,
            risk: Risk::None,
            dry_run: DryRunPolicy::NotSupported,
            raw_payload: RawPayloadPolicy::Unsupported,
            confirmation: ConfirmationPolicy::None,
            signs: None,
            master_only: None,
            ows_signer: OwsSupport::NotRequired,
            testnet_unprompted: false,
            confirms_with_arg: None,
            network: NetworkUse::Required,
            hidden_from_mcp: false,
            skips_update_check: false,
            stream: None,
            dry_run_refusal: None,
            cli_aliases: &[],
            schema_command: None,
            output: None,
            one_of_required: &[],
            args: &[],
        }
    }

    /// A read that runs on this machine alone: no network, signer or API
    /// endpoint.
    #[must_use]
    pub const fn local_read(
        path: &'static [&'static str],
        category: &'static str,
        about: &'static str,
        description: &'static str,
    ) -> Self {
        let mut spec = Self::read(path, category, about, description);
        spec.network = NetworkUse::None;
        spec
    }

    /// An interactive command that runs on this machine alone: no network,
    /// signer or API endpoint, and no risk until [`Self::risk`] says otherwise.
    #[must_use]
    pub const fn local_tool(
        path: &'static [&'static str],
        category: &'static str,
        about: &'static str,
        description: &'static str,
    ) -> Self {
        let mut spec = Self::local_read(path, category, about, description);
        spec.lifecycle = Lifecycle::InteractiveLocal;
        spec
    }

    /// A signed exchange action that moves funds, with a dry-run preview. It
    /// signs an L1 action unless [`Self::signs`] says otherwise.
    #[must_use]
    pub const fn signed(
        path: &'static [&'static str],
        category: &'static str,
        about: &'static str,
        description: &'static str,
    ) -> Self {
        let mut spec = Self::read(path, category, about, description);
        spec.auth_required = true;
        spec.dangerous = true;
        spec.lifecycle = Lifecycle::LiveMutating;
        spec.risk = Risk::FundsMovement;
        spec.dry_run = DryRunPolicy::Optional;
        spec.raw_payload = RawPayloadPolicy::DryRunOnly;
        spec.signs = Some(Signs::L1Action);
        spec.ows_signer = OwsSupport::ExperimentalFeatureGated;
        spec
    }

    /// Whether a Bankr hosted wallet can serve this command: it signs trading
    /// actions and spot-to-perp USDC moves only.
    #[must_use]
    pub fn bankr_signer(&self) -> SignerSupport {
        match self.signs {
            Some(signs) if signs.bankr_covers() => SignerSupport::Supported,
            Some(_) => SignerSupport::Unsupported,
            None if self.auth_required => SignerSupport::Supported,
            None => SignerSupport::NotRequired,
        }
    }

    #[must_use]
    pub const fn signs(mut self, signs: Signs<'static>) -> Self {
        self.signs = Some(signs);
        self
    }

    /// Signs the user-signed action `T`, which only the master account may
    /// sign: a stored API wallet is refused, naming `T`'s tag.
    #[must_use]
    pub const fn user_signed<T: UserSigned>(self) -> Self {
        self.signs(Signs::user_signed::<T>())
            .master_only(T::ACTION_TYPE)
    }

    /// Signs the user-signed `action` by digest, hypersdk not exposing its
    /// document, and only the master account may: a stored API wallet is
    /// refused, naming `action`.
    #[must_use]
    pub const fn digest_signed(self, action: &'static str) -> Self {
        self.signs(Signs::Digest).master_only(action)
    }

    /// Refuse a stored API wallet for `action`, which only the master account
    /// may sign.
    #[must_use]
    pub const fn master_only(mut self, action: &'static str) -> Self {
        self.master_only = Some(action);
        self
    }

    #[must_use]
    pub const fn long_about(mut self, text: &'static str) -> Self {
        self.long_about = Some(text);
        self
    }

    #[must_use]
    pub const fn lifecycle(mut self, lifecycle: Lifecycle) -> Self {
        self.lifecycle = lifecycle;
        self
    }

    #[must_use]
    pub const fn risk(mut self, risk: Risk) -> Self {
        self.risk = risk;
        self
    }

    #[must_use]
    pub const fn dry_run(mut self, policy: DryRunPolicy) -> Self {
        self.dry_run = policy;
        self
    }

    #[must_use]
    pub const fn prompt(mut self) -> Self {
        self.confirmation = ConfirmationPolicy::Prompt;
        self
    }

    #[must_use]
    pub const fn ows(mut self, support: OwsSupport) -> Self {
        self.ows_signer = support;
        self
    }

    #[must_use]
    pub const fn auth(mut self) -> Self {
        self.auth_required = true;
        self
    }

    #[must_use]
    pub const fn dangerous(mut self) -> Self {
        self.dangerous = true;
        self
    }

    #[must_use]
    pub const fn testnet_unprompted(mut self) -> Self {
        self.testnet_unprompted = true;
        self
    }

    #[must_use]
    pub const fn confirms_with_arg(mut self, id: &'static str) -> Self {
        self.confirms_with_arg = Some(id);
        self
    }

    #[must_use]
    pub const fn network(mut self, network: NetworkUse) -> Self {
        self.network = network;
        self
    }

    #[must_use]
    pub const fn hidden_from_mcp(mut self) -> Self {
        self.hidden_from_mcp = true;
        self
    }

    #[must_use]
    pub const fn skips_update_check(mut self) -> Self {
        self.skips_update_check = true;
        self
    }

    #[must_use]
    pub const fn stream(mut self, bound: StreamBound) -> Self {
        self.stream = Some(bound);
        self
    }

    #[must_use]
    pub const fn dry_run_refusal(mut self, text: &'static str) -> Self {
        self.dry_run_refusal = Some(text);
        self
    }

    #[must_use]
    pub const fn cli_aliases(mut self, aliases: &'static [&'static str]) -> Self {
        self.cli_aliases = aliases;
        self
    }

    #[must_use]
    pub const fn schema_command(mut self, text: &'static str) -> Self {
        self.schema_command = Some(text);
        self
    }

    #[must_use]
    pub const fn output(mut self, contract: OutputContract) -> Self {
        self.output = Some(contract);
        self
    }

    #[must_use]
    pub const fn one_of_required(mut self, groups: &'static [&'static [&'static str]]) -> Self {
        self.one_of_required = groups;
        self
    }

    #[must_use]
    pub const fn args(mut self, args: &'static [ArgSpec]) -> Self {
        self.args = args;
        self
    }

    #[must_use]
    pub fn key(&self) -> String {
        self.path.join(" ")
    }

    #[must_use]
    pub fn arg(&self, id: &str) -> Option<&ArgSpec> {
        self.args.iter().find(|arg| arg.id == id)
    }

    #[must_use]
    pub const fn mutability(&self) -> Mutability {
        match self.lifecycle {
            Lifecycle::Streaming => Mutability::Streaming,
            Lifecycle::InteractiveLocal => Mutability::LocalOnly,
            Lifecycle::LiveMutating => Mutability::LiveMutating,
            Lifecycle::ReadOnly => match self.risk {
                Risk::LocalSecret | Risk::LocalState => Mutability::LocalOnly,
                Risk::None | Risk::AccountState | Risk::FundsMovement => Mutability::ReadOnly,
            },
        }
    }

    #[must_use]
    pub const fn transport(&self) -> Transport {
        match self.lifecycle {
            Lifecycle::InteractiveLocal => Transport::CliInteractive,
            Lifecycle::ReadOnly | Lifecycle::Streaming | Lifecycle::LiveMutating => {
                Transport::CliProcess
            }
        }
    }

    /// Whether running the command changes state on the exchange or on this
    /// machine. A local command that only reads (`account ls`, `wallet show`)
    /// or declares no risk (`feedback`, `mcp`) changes nothing.
    #[must_use]
    pub const fn changes_state(&self) -> bool {
        match self.lifecycle {
            Lifecycle::LiveMutating => true,
            Lifecycle::InteractiveLocal => !matches!(self.risk, Risk::None),
            Lifecycle::ReadOnly | Lifecycle::Streaming => false,
        }
    }

    /// A command that reads public data only: nothing about the account, the
    /// exchange state or local secrets. The MCP server exposes these by default.
    #[must_use]
    const fn is_public_read(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::ReadOnly) && matches!(self.risk, Risk::None)
    }

    /// Whether the MCP server offers the command as a tool: public reads by
    /// default, and every command but the ones hidden from it when the
    /// operator opts in. Tool discovery and tool calls both ask this.
    #[must_use]
    pub const fn exposed_to_mcp(&self, allow_unsafe: bool) -> bool {
        !self.hidden_from_mcp && (allow_unsafe || self.is_public_read())
    }

    /// Whether running the command may reach the API at all, so a preview
    /// names the network it would use.
    #[must_use]
    pub const fn uses_network(&self) -> bool {
        !matches!(self.network, NetworkUse::None)
    }

    #[must_use]
    pub const fn dry_run_names_signer(&self) -> bool {
        self.auth_required && matches!(self.lifecycle, Lifecycle::LiveMutating)
    }

    /// Whether the command asks for confirmation. `network` is read only by a
    /// command that skips the testnet prompt, so the others need no connection
    /// to decide. `--testnet` and `HYPERLIQUID_NETWORK=testnet` both resolve to
    /// the network; neither is treated differently.
    pub fn prompts_on(
        &self,
        network: impl FnOnce() -> Result<Network, CliError>,
    ) -> Result<bool, CliError> {
        if self.confirmation != ConfirmationPolicy::Prompt {
            return Ok(false);
        }
        if !self.testnet_unprompted {
            return Ok(true);
        }
        Ok(network()? != Network::Testnet)
    }
}

/// A command group such as `orders`: a name with children and no behavior.
#[derive(Debug, Clone, Copy)]
pub struct GroupSpec {
    pub path: &'static [&'static str],
    pub about: &'static str,
    pub long_about: Option<&'static str>,
    /// Hidden alternate spellings.
    pub aliases: &'static [&'static str],
    /// The schema lists `aliases` on every command under the group.
    pub schema_aliases: bool,
}

impl GroupSpec {
    #[must_use]
    pub const fn new(path: &'static [&'static str], about: &'static str) -> Self {
        Self {
            path,
            about,
            long_about: None,
            aliases: &[],
            schema_aliases: false,
        }
    }

    #[must_use]
    pub const fn long_about(mut self, text: &'static str) -> Self {
        self.long_about = Some(text);
        self
    }

    #[must_use]
    pub const fn aliases(mut self, aliases: &'static [&'static str]) -> Self {
        self.aliases = aliases;
        self
    }

    #[must_use]
    pub const fn schema_aliases(mut self) -> Self {
        self.schema_aliases = true;
        self
    }
}

#[derive(clap::Args, Debug, Clone, Default)]
pub struct NoArgs;

pub fn check_args(spec: &CommandSpec, matches: &ArgMatches) -> Result<(), CliError> {
    for arg in spec.args {
        let Some(check) = arg.check else { continue };
        let values = matches.try_get_raw(arg.id).map_err(|err| {
            CliError::Internal(anyhow::anyhow!(
                "{} declares a check on `{}`, which it does not define: {err}",
                spec.key(),
                arg.id
            ))
        })?;
        for value in values.into_iter().flatten() {
            check.run(&value.to_string_lossy())?;
        }
    }
    Ok(())
}

/// The secret args of the command that were typed on the command line.
pub fn supplied_secrets<'a>(
    spec: &'a CommandSpec,
    matches: &'a ArgMatches,
) -> impl Iterator<Item = &'a ArgSpec> {
    spec.args.iter().filter(move |arg| {
        arg.secret && matches.value_source(arg.id) == Some(clap::parser::ValueSource::CommandLine)
    })
}
