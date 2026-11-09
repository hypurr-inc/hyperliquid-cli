//! Everything one run of the CLI needs, built once in `main` and passed by
//! reference to every handler.
//!
//! The output session, the prompting mode, the selected command, the dry-run
//! and payload inputs and the market cache are known at startup. The
//! network, API endpoint, HTTP client and signer depend on the environment and
//! config file, so they resolve on first use ([`Invocation::remote`]) and
//! local-only commands such as `wallet list` never read them.

use std::sync::OnceLock;
use std::time::Duration;

use hypersdk::hypercore::Chain;
use hypersdk::hypercore::UserRole;

use crate::assets::{AssetMetadata, AssetResolver, DexArg, MarketCache, PerpDex, PerpDexList};
use crate::auth::{SignerFlags, SignerSelection};
use crate::command_spec::CommandSpec;
use crate::errors::CliError;
use crate::http_api::ApiClient;
use crate::input_hardening::{FilePolicy, parse_json_text, read_json_file};
use crate::mode::Mode;
use crate::network::Network;
use crate::output::{Notice, Session, TableData};
use crate::settings::Settings;
use crate::signing::nonce::NonceStore;
use crate::signing::{SelectedSigner, SignerRole};
use crate::terminal::OutputFormat;
use crate::wire::InfoRequest;

#[derive(Debug, Clone, Default)]
pub struct InvocationFlags {
    pub testnet: bool,
    pub dry_run: bool,
    pub non_interactive: bool,
    pub payload_json: Option<String>,
    /// `--payload-file`: a path, or `-` for stdin.
    pub payload_file: Option<String>,
    pub signer: SignerFlags,
    /// `--expires-after`: how long after signing an L1 action the exchange
    /// still accepts it.
    pub expires_after: Option<Duration>,
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
    /// What the exchange says the selected signer is, for a signer no stored
    /// account describes; [`SignerRole::Unverified`] when the role could not be
    /// read.
    signer_role: OnceLock<SignerRole>,
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

#[derive(Debug)]
pub struct Invocation {
    settings: Settings,
    session: Session,
    mode: Mode,
    flags: InvocationFlags,
    command: &'static CommandSpec,
    payload: OnceLock<Option<serde_json::Value>>,
    markets: MarketCache,
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
            markets: MarketCache::new(),
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

    /// Whether this run must be approved before it acts: the command prompts on
    /// the resolved network and `yes` (`--yes`) was not given. The one rule the
    /// pipeline, the commands and the order submit path share; see
    /// [`crate::consent`] for asking.
    pub fn requires_approval(&self, yes: bool) -> Result<bool, CliError> {
        if yes {
            return Ok(false);
        }
        self.command
            .prompts_on(|| self.remote().map(Remote::network))
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

    /// The `expiresAfter` timestamp (milliseconds) an L1 action signed now
    /// would carry under `--expires-after`.
    #[must_use]
    pub fn expires_after_ms(&self) -> Option<u64> {
        self.flags
            .expires_after
            .map(|lifetime| crate::clock::expires_at_ms(crate::clock::now_ms(), lifetime))
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
            signer_role: OnceLock::new(),
        };
        Ok(self.remote.get_or_init(|| remote))
    }

    /// An API client for `network` with this run's endpoint override. `setup`
    /// may pick another network than the run resolved. A person at a terminal
    /// is told when a read is retried.
    pub fn api_client_for(&self, network: Network) -> Result<ApiClient, CliError> {
        let mut api = ApiClient::new(network, self.settings.api_base_url_override(network)?)
            .with_request_lifetime(self.flags.expires_after);
        if let Some(mark) = crate::paths::nonce_high_water_file(self.settings.env()) {
            api = api.with_nonce_store(NonceStore::recorded_at(mark, crate::output::warn));
        }
        Ok(if self.mode.person_on_stderr() {
            api.on_retry(|| Notice::Retrying.eprint(false))
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
            && matches!(signer.local_role(), SignerRole::Agent { .. })
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

    /// The signer a dry run previews with: `None` when none resolves, an error
    /// when one resolves that the live run would refuse. The one way a command's
    /// preview asks, so no preview swallows a refusal the live run would give.
    pub(crate) fn dry_run_signer(&self) -> Result<Option<&SelectedSigner>, CliError> {
        Ok(self.preview_signer()?.ok())
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
    /// empty result. The role is read once per run; a role that cannot be
    /// read ([`SignerRole::Unverified`]) is not a reason to refuse.
    pub async fn resolve_signer_account(&self) -> Result<hypersdk::Address, anyhow::Error> {
        let signer = self.resolve_signer()?;
        if signer.local_role() == SignerRole::Unverified
            && let SignerRole::Agent { master } = self.role_of(&signer).await
        {
            return Err(CliError::Unsupported(format!(
                "the selected signer {} is an API wallet for {master}, which has no account data of its own and would read empty; query the master account instead: pass {master} as the account argument, or select the master wallet",
                signer.address()
            ))
            .into());
        }
        Ok(signer.query_address())
    }

    /// What the selected signer is to the exchange: the one definition of an
    /// agent wallet. A stored account that records a master is an agent of it;
    /// any other key is asked of the exchange (`userRole`), once per run. A
    /// lookup that fails is [`SignerRole::Unverified`], cached like an answer:
    /// the role guards are best-effort and never turn a flaky request into a
    /// refusal.
    pub async fn signer_role(&self) -> Result<SignerRole, anyhow::Error> {
        let signer = self.signer_identity()?;
        Ok(self.role_of(signer).await)
    }

    /// The master the selected signer is known, without a request, to sign for
    /// as an agent wallet: what `master_only` commands and the agent form of
    /// an action ask, live or dry run, so it cannot wait for the network. A
    /// signer that does not resolve is not an agent here; the live run reports
    /// why it cannot sign.
    #[must_use]
    pub fn agent_master(&self) -> Option<hypersdk::Address> {
        match self.signer_identity().ok()?.local_role() {
            SignerRole::Agent { master } => Some(master),
            SignerRole::Master | SignerRole::Unverified => None,
        }
    }

    /// Whether the selected signer is known offline to sign as an agent
    /// wallet ([`Self::agent_master`]).
    #[must_use]
    pub fn acts_as_agent(&self) -> bool {
        self.agent_master().is_some()
    }

    async fn role_of(&self, signer: &SelectedSigner) -> SignerRole {
        let known = signer.local_role();
        if known != SignerRole::Unverified {
            return known;
        }
        let Ok(remote) = self.remote() else {
            return SignerRole::Unverified;
        };
        if let Some(role) = remote.signer_role.get() {
            return *role;
        }
        let role = match remote
            .api
            .info::<UserRole>(
                &InfoRequest::user("userRole", signer.address()),
                "loading the signer's role",
            )
            .await
        {
            Ok(UserRole::Agent { user }) => SignerRole::Agent { master: user },
            Ok(_) => SignerRole::Master,
            Err(_) => SignerRole::Unverified,
        };
        let _ = remote.signer_role.set(role);
        role
    }

    /// The address an account selector names, whether a command reads its data
    /// or acts on its behalf: a 0x address, a vault wallet or a stored account.
    pub fn resolve_account(&self, selector: &str) -> Result<hypersdk::Address, anyhow::Error> {
        crate::auth::lookup_account_address(selector, &self.settings, &self.mode)
    }

    /// The run's asset metadata, read once.
    pub async fn asset_resolver(&self) -> Result<AssetResolver, CliError> {
        self.markets.resolver(self.remote()?.api()).await
    }

    /// A resolver over metadata a command already holds; see
    /// [`MarketCache::resolver_over`].
    #[must_use]
    pub fn resolver_over(&self, metadata: AssetMetadata) -> AssetResolver {
        self.markets.resolver_over(metadata)
    }

    /// The perp dex a `--dex` value names, validated against the exchange:
    /// the one way a command turns the flag into a dex name or slot. `None`
    /// is the default dex.
    pub(crate) async fn dex(
        &self,
        requested: Option<&DexArg>,
    ) -> Result<Option<&PerpDex>, CliError> {
        self.markets
            .dexes()
            .select(self.remote()?.api(), requested)
            .await
    }

    /// Every perp dex the exchange lists, read once per run.
    pub(crate) async fn dex_list(&self) -> Result<&PerpDexList, CliError> {
        self.markets.dexes().list(self.remote()?.api()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_spec::{CliCommand, NoArgs};
    use crate::config::{BankrLogin, Config, ConfigFile};
    use crate::env::{self, EnvSource};
    use crate::output::JsonRenderOptions;
    use crate::signing::Signs;
    use hypersdk::Address;
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

    #[test]
    fn the_nonce_mark_lives_in_the_data_directory_of_the_runs_own_environment() {
        let data = TempDir::new().unwrap();
        let settings = Settings::detached(
            EnvSource::fixed([(env::DATA_DIR, data.path().to_str().unwrap())]),
            None,
        );
        let inv = invocation_over(settings, InvocationFlags::default(), &Probe::SPEC);
        let nonce = inv.remote().unwrap().api().nonces().next();
        let mark = std::fs::read_to_string(data.path().join("nonce-high-water")).unwrap();
        assert_eq!(mark.trim().parse::<u64>().unwrap(), nonce);
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

    static ASKS: CommandSpec =
        CommandSpec::read(&["probe", "asks"], "system", "Probe", "Probe.").prompt();
    static ASKS_ON_MAINNET: CommandSpec =
        CommandSpec::read(&["probe", "asks-on-mainnet"], "system", "Probe", "Probe.")
            .prompt()
            .testnet_unprompted();

    fn on_network(command: &'static CommandSpec, testnet: bool) -> Invocation {
        let settings = Settings::detached(EnvSource::fixed::<&str, &str>([]), None);
        invocation_over(
            settings,
            InvocationFlags {
                testnet,
                ..InvocationFlags::default()
            },
            command,
        )
    }

    #[test]
    fn a_prompting_command_needs_approval_unless_yes_was_given() {
        for testnet in [false, true] {
            let inv = on_network(&ASKS, testnet);
            assert!(inv.requires_approval(false).unwrap(), "testnet={testnet}");
            assert!(!inv.requires_approval(true).unwrap(), "testnet={testnet}");
        }
        let silent = on_network(&Probe::SPEC, false);
        assert!(!silent.requires_approval(false).unwrap());
    }

    #[test]
    fn a_command_that_skips_the_testnet_prompt_needs_approval_on_mainnet_only() {
        assert!(
            on_network(&ASKS_ON_MAINNET, false)
                .requires_approval(false)
                .unwrap()
        );
        assert!(
            !on_network(&ASKS_ON_MAINNET, true)
                .requires_approval(false)
                .unwrap()
        );
        assert!(
            !on_network(&ASKS_ON_MAINNET, false)
                .requires_approval(true)
                .unwrap()
        );
    }

    #[test]
    fn only_a_command_that_skips_the_testnet_prompt_reads_the_network() {
        let broken = || Settings::detached(EnvSource::fixed([(env::NETWORK, "nonsense")]), None);
        let asks = invocation_over(broken(), InvocationFlags::default(), &ASKS);
        assert!(asks.remote().is_err(), "the network setting is unusable");
        assert!(asks.requires_approval(false).unwrap());

        let skips = invocation_over(broken(), InvocationFlags::default(), &ASKS_ON_MAINNET);
        assert!(skips.requires_approval(false).is_err());
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

    /// An invocation signing with `KEY` as a bare private key against a mock
    /// exchange whose every `/info` reply is `reply`.
    async fn role_invocation(
        reply: wiremock::ResponseTemplate,
    ) -> (wiremock::MockServer, Invocation) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/info"))
            .respond_with(reply)
            .mount(&server)
            .await;
        let settings =
            Settings::detached(EnvSource::fixed([(env::API_BASE_URL, server.uri())]), None);
        let mut flags = InvocationFlags::default();
        flags.signer.private_key = Some(KEY.to_string());
        (server, invocation_over(settings, flags, &TRADE))
    }

    #[tokio::test]
    async fn a_bare_key_is_an_agent_when_the_exchange_says_so_and_is_asked_once() {
        use wiremock::ResponseTemplate;

        let (server, inv) = role_invocation(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"role": "agent", "data": {"user": RAW_ADDRESS}})),
        )
        .await;

        let master: Address = RAW_ADDRESS.parse().unwrap();
        assert_eq!(
            inv.signer_role().await.unwrap(),
            SignerRole::Agent { master }
        );
        assert_eq!(
            inv.signer_role().await.unwrap(),
            SignerRole::Agent { master }
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert!(
            !inv.acts_as_agent() && inv.agent_master().is_none(),
            "the offline answer never waits for the network: a bare key is not known to be an agent"
        );
    }

    #[tokio::test]
    async fn a_key_the_exchange_calls_a_user_is_a_master() {
        use wiremock::ResponseTemplate;

        let (_server, inv) = role_invocation(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"role": "user"})),
        )
        .await;
        assert_eq!(inv.signer_role().await.unwrap(), SignerRole::Master);
    }

    #[tokio::test]
    async fn a_role_that_cannot_be_read_is_unverified_and_not_asked_again() {
        use wiremock::ResponseTemplate;

        let (server, inv) = role_invocation(ResponseTemplate::new(404)).await;
        assert_eq!(inv.signer_role().await.unwrap(), SignerRole::Unverified);
        let asked = server.received_requests().await.unwrap().len();
        assert!(asked >= 1);

        assert_eq!(inv.signer_role().await.unwrap(), SignerRole::Unverified);
        assert_eq!(server.received_requests().await.unwrap().len(), asked);
        assert!(
            inv.resolve_signer_account().await.is_ok(),
            "an unverified role is no reason to refuse a read"
        );
    }

    #[tokio::test]
    async fn a_stored_agent_is_known_without_asking_the_exchange() {
        use wiremock::ResponseTemplate;

        let (server, inv) = role_invocation(ResponseTemplate::new(404)).await;
        let master: Address = RAW_ADDRESS.parse().unwrap();
        let agent =
            SelectedSigner::stored_account(KEY.parse().unwrap(), "agent".to_string(), master);
        assert_eq!(inv.role_of(&agent).await, SignerRole::Agent { master });
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
