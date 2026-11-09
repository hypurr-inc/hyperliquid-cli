//! Everything one run of the CLI needs, built once in `main` and passed by
//! reference to every handler.
//!
//! The output session, the prompting mode, the selected command, the dry-run
//! and payload inputs and the asset metadata cache are known at startup. The
//! network, API endpoint, HTTP client and signer depend on the environment and
//! config file, so they resolve on first use ([`Invocation::remote`]) and
//! local-only commands such as `wallet list` never read them.

use std::sync::OnceLock;

use hypersdk::Address;
use hypersdk::hypercore::Chain;
use hypersdk::hypercore::UserRole;

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
use crate::wire::InfoRequest;

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
    network: Network,
    api: ApiClient,
    signer: OnceLock<SelectedSigner>,
    /// The master account when the selected signer proved to be an API wallet:
    /// `None` for any other signer and when the role could not be read.
    signer_master: OnceLock<Option<Address>>,
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
            network,
            api: self.api_client_for(network)?,
            signer: OnceLock::new(),
            signer_master: OnceLock::new(),
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

    /// The selected signer, resolved once per run: who signs, with no check of
    /// what they are asked to sign. A failed resolution is not cached. The
    /// signer flags and the ambient key are read here, not for a command that
    /// never asks.
    fn signer_identity(&self) -> Result<&SelectedSigner, anyhow::Error> {
        let remote = self.remote()?;
        if let Some(signer) = remote.signer.get() {
            return Ok(signer);
        }
        let selection = SignerSelection::from_flags(&self.settings, &self.flags.signer)?;
        let signer = crate::auth::resolve_signer(&selection, &self.settings, &self.mode)?;
        Ok(remote.signer.get_or_init(|| signer))
    }

    /// What the command needs of `signer`, live or dry run alike, so a preview
    /// is never offered for a run the same signer would be refused: the
    /// provider must cover what the command signs ([`CommandSpec::signs`]) and
    /// a command only the master account may sign ([`CommandSpec::master_only`])
    /// refuses a stored API wallet.
    fn ensure_signer_fits(&self, signer: &SelectedSigner) -> Result<(), CliError> {
        if let Some(signs) = self.command.signs {
            signer.ensure_covers(signs)?;
        }
        if let Some(action) = self.command.master_only
            && signer.is_stored_agent()
        {
            return Err(CliError::Unsupported(format!(
                "{action} must be signed by the master account; select the master wallet, not a stored API wallet"
            )));
        }
        Ok(())
    }

    /// What a dry run knows of the signer: who it is, or why that is unknown
    /// (nothing selected, a wallet that isn't there). A signer that resolves
    /// must fit the command ([`Self::ensure_signer_fits`]) or the preview fails,
    /// as the live run would; it need not hold a key.
    pub(crate) fn preview_signer(
        &self,
    ) -> Result<Result<&SelectedSigner, anyhow::Error>, CliError> {
        match self.signer_identity() {
            Ok(signer) => self.ensure_signer_fits(signer).map(|()| Ok(signer)),
            Err(unresolved) => Ok(Err(unresolved)),
        }
    }

    /// The signer a live run signs with, checked before any prompt or network
    /// lookup: it must fit the command ([`Self::ensure_signer_fits`]) and, for a
    /// command that signs, hold a key. A dry run signs nothing, so it needs only
    /// the identity: an address-only `--ows-signer` previews, and a run with no
    /// signer previews with a null one ([`Self::signer_identity`] fails, and
    /// the preview says so).
    pub fn resolve_signer(&self) -> Result<SelectedSigner, anyhow::Error> {
        let signer = self.signer_identity()?;
        self.ensure_signer_fits(signer)?;
        if !self.dry_run() && self.command.signs.is_some() {
            signer.ensure_holds_key()?;
        }
        Ok(signer.clone())
    }

    /// The account whose data a read command queries: the explicit selector, else
    /// the selected signer's own account ([`Self::resolve_signer_account`]).
    pub async fn resolve_user(
        &self,
        selector: Option<&str>,
    ) -> Result<hypersdk::Address, anyhow::Error> {
        match selector {
            Some(selector) => self.resolve_account(selector),
            None => self.resolve_signer_account().await,
        }
    }

    /// The account a read command shows when it names none: the selected
    /// signer's. An API wallet has no account data of its own, so a bare key
    /// that turns out to be one (a stored API wallet already reads its master)
    /// is refused with the master it signs for, instead of answering with an
    /// empty result. The role is read once per run; a failed read is not a reason
    /// to refuse.
    pub async fn resolve_signer_account(&self) -> Result<hypersdk::Address, anyhow::Error> {
        let signer = self.resolve_signer()?;
        if !signer.is_stored_agent()
            && let Some(master) = self.signer_master(&signer).await
        {
            return Err(CliError::Unsupported(format!(
                "the selected signer {} is an API wallet for {master}, which has no account data of its own and would read empty; query the master account instead: pass {master} as the account argument, or select the master wallet",
                signer.address()
            ))
            .into());
        }
        Ok(signer.query_address())
    }

    /// The master `signer` is an API wallet of, if its `userRole` says so.
    async fn signer_master(&self, signer: &SelectedSigner) -> Option<Address> {
        let remote = self.remote().ok()?;
        if let Some(known) = remote.signer_master.get() {
            return *known;
        }
        let master = match remote
            .api
            .info::<UserRole>(
                &InfoRequest::user("userRole", signer.address()),
                "loading the signer's role",
            )
            .await
        {
            Ok(UserRole::Agent { user }) => Some(user),
            Ok(_) | Err(_) => None,
        };
        let _ = remote.signer_master.set(master);
        master
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
    /// when the SDK cannot decode the universe. The SDK reports a reply it
    /// cannot decode as an unclassified failure, so every classified one (the
    /// network, a rate limit, a rejected credential, an HTTP status) is the
    /// answer, not a reason to ask again.
    pub async fn trading_resolver(&self) -> Result<AssetResolver, CliError> {
        match self.asset_resolver().await {
            Err(CliError::Internal(_)) => AssetResolver::load_raw(self.remote()?.api()).await,
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_spec::{CliCommand, NoArgs};
    use crate::config::{BankrLogin, Config, ConfigFile};
    use crate::output::JsonRenderOptions;
    use crate::settings::{EnvSource, env};
    use crate::signing::Signs;
    use tempfile::TempDir;

    struct Probe;

    impl CliCommand for Probe {
        type Args = NoArgs;

        const SPEC: CommandSpec = CommandSpec::read(&["probe", "run"], "system", "Probe", "Probe.");

        async fn run(_inv: &Invocation, _args: &NoArgs) -> Result<(), anyhow::Error> {
            Ok(())
        }
    }

    fn invocation_over(
        settings: Settings,
        flags: InvocationFlags,
        command: &'static CommandSpec,
    ) -> Invocation {
        let mode = Mode::default().running(
            OutputFormat::Json,
            flags.non_interactive,
            flags.payload_from_stdin(),
        );
        let session = Session::new(JsonRenderOptions::default(), &mode);
        Invocation::new(settings, session, mode, flags, command)
    }

    fn invocation(flags: InvocationFlags) -> Invocation {
        let settings = Settings::detached(EnvSource::fixed::<&str, &str>([]), None);
        invocation_over(settings, flags, &Probe::SPEC)
    }

    const KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000009";
    const RAW_ADDRESS: &str = "0x0000000000000000000000000000000000000001";

    static WITHDRAW: CommandSpec =
        CommandSpec::signed(&["probe", "withdraw"], "system", "Probe", "Probe.")
            .signs(Signs::UserSigned("Withdraw"));
    static TRADE: CommandSpec =
        CommandSpec::signed(&["probe", "trade"], "system", "Probe", "Probe.");
    static MASTER_ONLY: CommandSpec =
        CommandSpec::signed(&["probe", "approve"], "system", "Probe", "Probe.")
            .master_only("approveAgent");

    /// `--bankr` with the login `wallet login` stores, which needs no request.
    fn bankr_invocation(command: &'static CommandSpec, dry_run: bool) -> Invocation {
        let config = Config {
            bankr: BankrLogin::new("bk_test", None, RAW_ADDRESS.parse().unwrap()),
            ..Config::default()
        };
        let settings = Settings::detached(EnvSource::fixed::<&str, &str>([]), Some(config));
        let mut flags = InvocationFlags {
            dry_run,
            ..InvocationFlags::default()
        };
        flags.signer.bankr = true;
        invocation_over(settings, flags, command)
    }

    #[test]
    fn a_run_that_never_signs_never_reads_the_signer_inputs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{"private_key": 5}"#).unwrap();
        let settings = Settings::new(
            EnvSource::fixed([(env::NETWORK, "testnet")]),
            Some(ConfigFile::at(path)),
        );
        let inv = invocation_over(settings, InvocationFlags::default(), &Probe::SPEC);

        assert!(inv.remote().is_ok(), "a public read needs no signer");
        assert!(
            inv.resolve_signer().is_err(),
            "the unreadable key is only an error for a run that signs"
        );
    }

    #[test]
    fn a_signer_must_cover_what_the_command_signs_on_a_dry_run_too() {
        for dry_run in [false, true] {
            let inv = bankr_invocation(&WITHDRAW, dry_run);
            let err = inv.resolve_signer().unwrap_err();
            assert_eq!(
                crate::errors::from_anyhow(err).exit_code(),
                13,
                "dry_run={dry_run}"
            );
            let Err(err) = inv.preview_signer() else {
                panic!("dry_run={dry_run}: a preview is refused where the live run would be");
            };
            assert_eq!(err.exit_code(), 13);

            let trade = bankr_invocation(&TRADE, dry_run);
            assert!(trade.resolve_signer().is_ok(), "dry_run={dry_run}");
            assert!(matches!(trade.preview_signer(), Ok(Ok(_))));
        }
    }

    #[test]
    fn an_address_only_signer_previews_but_cannot_sign_live() {
        let selected = |dry_run| {
            let mut flags = InvocationFlags {
                dry_run,
                ..InvocationFlags::default()
            };
            flags.signer.ows_signer = Some(RAW_ADDRESS.to_string());
            let settings = Settings::detached(EnvSource::fixed::<&str, &str>([]), None);
            invocation_over(settings, flags, &TRADE)
        };
        let live = selected(false);
        assert_eq!(
            crate::errors::from_anyhow(live.resolve_signer().unwrap_err()).exit_code(),
            13
        );
        let preview = selected(true);
        assert_eq!(
            preview
                .resolve_signer()
                .unwrap()
                .address()
                .to_string()
                .to_lowercase(),
            RAW_ADDRESS
        );
        assert!(matches!(preview.preview_signer(), Ok(Ok(_))));
    }

    #[test]
    fn a_run_with_no_signer_previews_without_one_but_fails_live() {
        let tmp = TempDir::new().unwrap();
        let vault = tmp.path().to_str().unwrap().to_string();
        let build = |dry_run| {
            let settings = Settings::detached(
                EnvSource::fixed([(env::OWS_VAULT_PATH, vault.as_str())]),
                None,
            );
            let flags = InvocationFlags {
                dry_run,
                ..InvocationFlags::default()
            };
            invocation_over(settings, flags, &TRADE)
        };
        assert!(matches!(build(true).preview_signer(), Ok(Err(_))));
        assert!(build(false).resolve_signer().is_err());
    }

    #[test]
    fn a_master_only_command_refuses_a_stored_api_wallet() {
        let inv = invocation_over(
            Settings::detached(EnvSource::fixed::<&str, &str>([]), None),
            InvocationFlags::default(),
            &MASTER_ONLY,
        );
        let key: hypersdk::hypercore::PrivateKeySigner = KEY.parse().unwrap();
        let agent = SelectedSigner::stored_account(
            key.clone(),
            "agent".to_string(),
            RAW_ADDRESS.parse().unwrap(),
        );
        let err = inv.ensure_signer_fits(&agent).unwrap_err();
        assert_eq!(err.exit_code(), 13);
        assert!(
            err.to_string()
                .contains("approveAgent must be signed by the master account")
        );
        assert!(
            inv.ensure_signer_fits(&SelectedSigner::private_key(key))
                .is_ok()
        );
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

    #[tokio::test]
    async fn a_classified_metadata_failure_is_not_retried_against_the_raw_metadata() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let settings =
            Settings::detached(EnvSource::fixed([(env::API_BASE_URL, server.uri())]), None);
        let inv = invocation_over(settings, InvocationFlags::default(), &Probe::SPEC);

        let direct = inv.asset_resolver().await.unwrap_err();
        assert!(!matches!(direct, CliError::Internal(_)), "{direct}");
        let after_direct = server.received_requests().await.unwrap().len();

        inv.trading_resolver().await.unwrap_err();
        let after_trading = server.received_requests().await.unwrap().len();
        assert_eq!(
            after_trading,
            after_direct * 2,
            "only the SDK's own load ran again, not the raw metadata fallback"
        );
    }
}
