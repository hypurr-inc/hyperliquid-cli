//! Everything one run of the CLI needs, built once in `main` and passed by
//! reference to every handler.
//!
//! The output session, the prompting mode, the selected command, the dry-run
//! and payload inputs and the asset metadata cache are known at startup. The
//! network, API endpoint, HTTP client and signer depend on the environment and
//! config file, so they resolve on first use ([`Invocation::remote`]) and
//! local-only commands such as `wallet list` never read them.

use std::path::PathBuf;
use std::sync::OnceLock;

use hypersdk::hypercore::Chain;

use crate::assets::{AssetResolver, MetadataCache};
use crate::auth::{ResolvedSigner, SignerSelection};
use crate::command_spec::CommandSpec;
use crate::dry_run::{DryRunEnvelope, DryRunRunContext};
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::input_hardening::{FilePolicy, parse_json_text, read_json_file};
use crate::mode::Mode;
use crate::network::Network;
use crate::output::{OutputFormat, Session, TableData};
use crate::settings::Settings;

/// Signer flags exactly as parsed from the command line.
#[derive(Debug, Clone, Default)]
pub struct SignerFlags {
    pub bankr: bool,
    pub private_key: Option<String>,
    pub keystore: Option<PathBuf>,
    pub keystore_password: Option<String>,
    pub account: Option<String>,
    pub ows_signer: Option<String>,
}

/// The parsed global flags that shape an invocation.
#[derive(Debug, Clone, Default)]
pub struct InvocationFlags {
    pub testnet: bool,
    pub dry_run: bool,
    pub non_interactive: bool,
    /// `--payload-json`.
    pub payload_json: Option<String>,
    /// `--payload-file`: a path, or `-` for stdin.
    pub payload_file: Option<String>,
    pub signer: SignerFlags,
}

impl InvocationFlags {
    /// `--payload-file -`: stdin carries the payload and cannot answer prompts.
    #[must_use]
    pub fn payload_from_stdin(&self) -> bool {
        self.payload_file.as_deref() == Some("-")
    }

    /// Whether a raw payload was supplied at all.
    #[must_use]
    pub fn has_payload(&self) -> bool {
        self.payload_json.is_some() || self.payload_file.is_some()
    }

    fn read_payload(&self) -> Result<Option<serde_json::Value>, CliError> {
        if let Some(raw) = self.payload_json.as_deref() {
            return parse_json_text(raw, "payload").map(Some);
        }
        self.payload_file
            .as_deref()
            .map(|path| read_json_file(std::path::Path::new(path), FilePolicy::payload()))
            .transpose()
    }
}

/// Network facts resolved from flags, environment and config file.
pub struct Remote {
    settings: Settings,
    selection: SignerSelection,
    network: Network,
    api: ApiClient,
    signer: OnceLock<ResolvedSigner>,
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote")
            .field("network", &self.network)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

impl Remote {
    fn resolve(flags: &InvocationFlags, announce_retries: bool) -> Result<Self, CliError> {
        let settings = Settings::from_process();
        let network = settings.network(flags.testnet)?;
        let signer = &flags.signer;
        let selection = SignerSelection::from_flags(
            &settings,
            signer.private_key.as_deref(),
            signer.keystore.clone(),
            signer.keystore_password.clone(),
            signer.account.clone(),
            signer.ows_signer.clone(),
            signer.bankr,
        )?;
        let api = ApiClient::from_settings(&settings, network, announce_retries)?;
        Ok(Self {
            settings,
            selection,
            network,
            api,
            signer: OnceLock::new(),
        })
    }

    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    #[must_use]
    pub fn selection(&self) -> &SignerSelection {
        &self.selection
    }

    #[must_use]
    pub fn network(&self) -> Network {
        self.network
    }

    #[must_use]
    pub fn chain(&self) -> Chain {
        self.network.chain()
    }

    /// The one HTTP edge to the Hyperliquid API.
    #[must_use]
    pub fn api(&self) -> &ApiClient {
        &self.api
    }
}

/// The context of one CLI run.
#[derive(Debug)]
pub struct Invocation {
    session: Session,
    mode: Mode,
    flags: InvocationFlags,
    command: &'static CommandSpec,
    payload: OnceLock<Option<serde_json::Value>>,
    metadata: MetadataCache,
    remote: OnceLock<Remote>,
}

impl Invocation {
    #[must_use]
    pub fn new(
        session: Session,
        mode: Mode,
        flags: InvocationFlags,
        command: &'static CommandSpec,
    ) -> Self {
        Self {
            session,
            mode,
            flags,
            command,
            payload: OnceLock::new(),
            metadata: MetadataCache::new(),
            remote: OnceLock::new(),
        }
    }

    // ── The command ─────────────────────────────────────────────────────

    /// The spec of the command this run executes.
    #[must_use]
    pub fn command(&self) -> &'static CommandSpec {
        self.command
    }

    /// The command's path as users type it, such as `orders create`: the name a
    /// dry-run preview gives the command.
    #[must_use]
    pub fn command_key(&self) -> String {
        self.command.key()
    }

    // ── Output ──────────────────────────────────────────────────────────

    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    #[must_use]
    pub fn format(&self) -> OutputFormat {
        self.session.format()
    }

    /// Print the answer, then the "Completed in" footer for interactive runs.
    pub fn print(&self, data: &dyn TableData) {
        self.session.print(data);
        if self.mode.timing_footer() {
            eprintln!("{}", self.session.timing_footer());
        }
    }

    /// Print the answer without the timing footer.
    pub fn print_untimed(&self, data: &dyn TableData) {
        self.session.print(data);
    }

    // ── Prompting, dry run and payload ──────────────────────────────────

    #[must_use]
    pub fn mode(&self) -> &Mode {
        &self.mode
    }

    #[must_use]
    pub fn flags(&self) -> &InvocationFlags {
        &self.flags
    }

    #[must_use]
    pub fn dry_run(&self) -> bool {
        self.flags.dry_run
    }

    /// The raw `--payload-json` / `--payload-file` document. `None` when none
    /// was given, and until the run pipeline has called [`Self::load_payload`].
    #[must_use]
    pub fn payload(&self) -> Option<&serde_json::Value> {
        self.payload.get().and_then(Option::as_ref)
    }

    /// Read the raw payload, once. The run pipeline calls this before the
    /// command runs, after the checks that must not touch stdin or files.
    pub fn load_payload(&self) -> Result<(), CliError> {
        if self.payload.get().is_none() {
            let payload = self.flags.read_payload()?;
            let _ = self.payload.set(payload);
        }
        Ok(())
    }

    /// Print a dry-run envelope. A command that resolves the network names it,
    /// and the signer when its previews are signed; a local command names
    /// neither.
    pub fn print_dry_run(&self, envelope: DryRunEnvelope) {
        let context = if self.command.local {
            None
        } else {
            self.remote.get().map(|remote| {
                let signer = self.command.dry_run_names_signer().then(|| {
                    self.resolve_signer()
                        .map(|signer| signer.address().to_string())
                        .map_err(|err| err.to_string())
                });
                DryRunRunContext::new(remote.network(), signer)
            })
        };
        self.session.print(&envelope.with_run_context(context));
    }

    // ── Network, signer and assets ──────────────────────────────────────

    /// The network facts, resolved on first use. A failed resolution is not cached.
    pub fn remote(&self) -> Result<&Remote, CliError> {
        if let Some(remote) = self.remote.get() {
            return Ok(remote);
        }
        let remote = Remote::resolve(&self.flags, self.mode.person_on_stderr())?;
        Ok(self.remote.get_or_init(|| remote))
    }

    /// An API client for `network`, for `setup`, which may pick another network
    /// than the run resolved.
    pub fn api_client_for(&self, network: Network) -> Result<ApiClient, CliError> {
        ApiClient::from_settings(
            self.remote()?.settings(),
            network,
            self.mode.person_on_stderr(),
        )
    }

    /// The selected signer, resolved once per run. A failed resolution is not cached.
    pub fn resolve_signer(&self) -> Result<ResolvedSigner, anyhow::Error> {
        let remote = self.remote()?;
        if let Some(signer) = remote.signer.get() {
            return Ok(signer.clone());
        }
        let signer = crate::auth::resolve_signer(&remote.selection, &remote.settings, &self.mode)?;
        Ok(remote.signer.get_or_init(|| signer).clone())
    }

    /// The account whose data a read command queries: the explicit selector, else
    /// the selected signer's query address.
    pub fn resolve_user(&self, selector: Option<&str>) -> Result<hypersdk::Address, anyhow::Error> {
        match selector {
            Some(selector) => self.resolve_protocol_user(selector),
            None => Ok(self.resolve_signer()?.query_address()),
        }
    }

    pub fn resolve_protocol_user(
        &self,
        selector: &str,
    ) -> Result<hypersdk::Address, anyhow::Error> {
        Ok(crate::resolvers::resolve_protocol_user_address(
            selector,
            self.remote()?.settings(),
            &self.mode,
        )?
        .address())
    }

    pub fn resolve_acting_account(
        &self,
        selector: &str,
    ) -> Result<hypersdk::Address, anyhow::Error> {
        Ok(crate::resolvers::resolve_acting_account_address(
            selector,
            self.remote()?.settings(),
            &self.mode,
        )?
        .address())
    }

    /// Asset metadata through the per-run cache.
    pub async fn asset_resolver(&self) -> Result<AssetResolver, CliError> {
        let metadata = self.metadata.get_or_load(self.remote()?.api()).await?;
        Ok(AssetResolver::new(metadata))
    }

    /// Like [`Self::asset_resolver`], falling back to the raw exchange metadata
    /// when the SDK cannot decode the universe.
    pub async fn trading_resolver(&self) -> Result<AssetResolver, CliError> {
        match self.asset_resolver().await {
            Ok(resolver) => Ok(resolver),
            Err(_) => AssetResolver::load_raw(self.remote()?.api()).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_spec::{CliCommand, NoArgs};
    use crate::output::JsonRenderOptions;
    use crate::output::terminal::OutputEnv;

    struct Probe;

    impl CliCommand for Probe {
        type Args = NoArgs;

        const SPEC: CommandSpec = CommandSpec::read(&["probe", "run"], "system", "Probe", "Probe.");

        async fn run(_inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
            Ok(())
        }
    }

    fn invocation(flags: InvocationFlags) -> Invocation {
        let env = OutputEnv::default();
        let session = Session::new(OutputFormat::Json, JsonRenderOptions::default(), env);
        let mode = Mode::new(
            flags.non_interactive,
            true,
            flags.payload_from_stdin(),
            &env,
        );
        Invocation::new(session, mode, flags, &Probe::SPEC)
    }

    #[test]
    fn invocation_carries_the_command_it_runs() {
        let inv = invocation(InvocationFlags::default());
        assert_eq!(inv.command_key(), "probe run");
        assert_eq!(inv.command().path, ["probe", "run"]);
    }

    #[test]
    fn payload_is_read_once_when_loaded() {
        let inv = invocation(InvocationFlags {
            payload_json: Some(r#"{"a":1}"#.to_string()),
            ..InvocationFlags::default()
        });
        assert!(
            inv.payload().is_none(),
            "nothing is read before load_payload"
        );
        inv.load_payload().unwrap();
        assert_eq!(inv.payload(), Some(&serde_json::json!({"a": 1})));
        inv.load_payload().unwrap();
        assert_eq!(inv.payload(), Some(&serde_json::json!({"a": 1})));
    }

    #[test]
    fn no_payload_input_leaves_payload_empty() {
        let inv = invocation(InvocationFlags::default());
        inv.load_payload().unwrap();
        assert!(inv.payload().is_none());
    }

    #[test]
    fn payload_file_dash_means_stdin_carries_the_payload() {
        let flags = InvocationFlags {
            payload_file: Some("-".to_string()),
            ..InvocationFlags::default()
        };
        assert!(flags.payload_from_stdin());
        assert!(flags.has_payload());
        assert!(!InvocationFlags::default().payload_from_stdin());
    }
}
