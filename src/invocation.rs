//! Everything one run of the CLI needs, built once in `main` and passed by
//! reference to every handler.
//!
//! The output session, the prompting mode, the selected command, the dry-run
//! and payload inputs and the asset metadata cache are known at startup. The
//! network, API endpoint, HTTP client and signer depend on the environment and
//! config file, so they resolve on first use ([`Invocation::remote`]) and
//! local-only commands such as `wallet list` never read them.

use std::sync::OnceLock;

use hypersdk::hypercore::Chain;

use crate::assets::{AssetResolver, MetadataCache};
use crate::auth::{SignerFlags, SignerSelection};
use crate::command_spec::CommandSpec;
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::input_hardening::{FilePolicy, parse_json_text, read_json_file};
use crate::mode::Mode;
use crate::network::Network;
use crate::output::{OutputFormat, Session, TableData};
use crate::settings::Settings;
use crate::signing::SelectedSigner;

/// The parsed global flags that shape an invocation.
#[derive(Debug, Clone, Default)]
pub struct InvocationFlags {
    pub testnet: bool,
    pub dry_run: bool,
    pub non_interactive: bool,
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
    selection: SignerSelection,
    network: Network,
    api: ApiClient,
    signer: OnceLock<SelectedSigner>,
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
    #[must_use]
    pub fn network(&self) -> Network {
        self.network
    }

    #[must_use]
    pub fn chain(&self) -> Chain {
        self.network.chain()
    }

    #[must_use]
    pub fn api(&self) -> &ApiClient {
        &self.api
    }
}

/// The context of one CLI run.
#[derive(Debug)]
pub struct Invocation {
    settings: Settings,
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
        settings: Settings,
        session: Session,
        mode: Mode,
        flags: InvocationFlags,
        command: &'static CommandSpec,
    ) -> Self {
        Self {
            settings,
            session,
            mode,
            flags,
            command,
            payload: OnceLock::new(),
            metadata: MetadataCache::new(),
            remote: OnceLock::new(),
        }
    }

    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

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

    pub fn print_untimed(&self, data: &dyn TableData) {
        self.session.print(data);
    }

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

    /// The network facts, resolved on first use. A failed resolution is not cached.
    pub fn remote(&self) -> Result<&Remote, CliError> {
        if let Some(remote) = self.remote.get() {
            return Ok(remote);
        }
        let network = self.settings.network(self.flags.testnet)?;
        let remote = Remote {
            selection: SignerSelection::from_flags(&self.settings, &self.flags.signer)?,
            network,
            api: self.api_client_for(network)?,
            signer: OnceLock::new(),
        };
        Ok(self.remote.get_or_init(|| remote))
    }

    /// An API client for `network` with this run's endpoint override. `setup`
    /// may pick another network than the run resolved. A person at a terminal
    /// is told when a read is retried.
    pub fn api_client_for(&self, network: Network) -> Result<ApiClient, CliError> {
        let api = ApiClient::new(network, self.settings.api_base_url_override(network)?);
        Ok(if self.mode.person_on_stderr() {
            api.on_retry(|| eprintln!("Retrying automatically…"))
        } else {
            api
        })
    }

    /// The selected signer, resolved once per run. A failed resolution is not cached.
    ///
    /// A live run of a command that signs ([`CommandSpec::signs`]) also asks the
    /// signer whether it can, so an unsupported provider fails here, before any
    /// prompt or network lookup. A dry run signs nothing and only needs the identity.
    pub fn resolve_signer(&self) -> Result<SelectedSigner, anyhow::Error> {
        let remote = self.remote()?;
        let signer = match remote.signer.get() {
            Some(signer) => signer,
            None => {
                let signer =
                    crate::auth::resolve_signer(&remote.selection, &self.settings, &self.mode)?;
                remote.signer.get_or_init(|| signer)
            }
        };
        if !self.dry_run()
            && let Some(signs) = self.command.signs
        {
            signer.ensure_can_sign(signs)?;
        }
        Ok(signer.clone())
    }

    /// The account whose data a read command queries: the explicit selector, else
    /// the selected signer's query address.
    pub fn resolve_user(&self, selector: Option<&str>) -> Result<hypersdk::Address, anyhow::Error> {
        match selector {
            Some(selector) => self.resolve_account(selector),
            None => Ok(self.resolve_signer()?.query_address()),
        }
    }

    /// The address an account selector names, whether a command reads its data
    /// or acts on its behalf: a 0x address, a vault wallet or a stored account.
    pub fn resolve_account(&self, selector: &str) -> Result<hypersdk::Address, anyhow::Error> {
        crate::auth::lookup_account_address(selector, &self.settings, &self.mode)
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

    struct Probe;

    impl CliCommand for Probe {
        type Args = NoArgs;

        const SPEC: CommandSpec = CommandSpec::read(&["probe", "run"], "system", "Probe", "Probe.");

        async fn run(_inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
            Ok(())
        }
    }

    fn invocation(flags: InvocationFlags) -> Invocation {
        let mode = Mode::default().running(
            OutputFormat::Json,
            flags.non_interactive,
            flags.payload_from_stdin(),
        );
        let session = Session::new(JsonRenderOptions::default(), &mode);
        let settings =
            Settings::detached(crate::settings::EnvSource::fixed::<&str, &str>([]), None);
        Invocation::new(settings, session, mode, flags, &Probe::SPEC)
    }

    #[test]
    fn payload_is_absent_until_loaded_and_read_only_once() {
        let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let file = dir.path().join("payload.json");
        std::fs::write(&file, r#"{"a":1}"#).unwrap();
        let relative = file.strip_prefix(std::env::current_dir().unwrap()).unwrap();
        let inv = invocation(InvocationFlags {
            payload_file: Some(relative.to_str().unwrap().to_string()),
            ..InvocationFlags::default()
        });
        assert!(
            inv.payload().is_none(),
            "nothing is read before load_payload"
        );
        inv.load_payload().unwrap();
        std::fs::remove_file(&file).unwrap();
        inv.load_payload().unwrap();
        assert_eq!(inv.payload(), Some(&serde_json::json!({"a": 1})));
    }

    #[test]
    fn inline_payload_json_is_parsed_and_no_payload_stays_empty() {
        let inv = invocation(InvocationFlags {
            payload_json: Some(r#"{"a":1}"#.to_string()),
            ..InvocationFlags::default()
        });
        inv.load_payload().unwrap();
        assert_eq!(inv.payload(), Some(&serde_json::json!({"a": 1})));

        let none = invocation(InvocationFlags::default());
        none.load_payload().unwrap();
        assert!(none.payload().is_none());
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
