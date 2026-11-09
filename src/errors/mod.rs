//! Structured error system for Hyperliquid CLI.
//!
//! [`CliError`] is the one error type commands return. Each variant maps to a
//! [`spec`] constant that gives it its exit code, category, reason, retry policy
//! and agent guidance; the JSON envelope and the checked-in `.claude/error-catalog.json` all read from those constants. The
//! exit-code table lives there and nowhere else.

mod classify;
mod envelope;
mod recovery;
mod refusal;
mod spec;
mod transport;

use thiserror::Error;

use crate::response_sanitization::Untrusted;

pub(crate) use classify::from_sdk;
pub use classify::{
    Decisive, Rejection, address_rate_limit_in_body, classify_rejection, from_anyhow,
    from_exchange_rejection, http_response_indicates_rate_limit, http_status_in_text,
    is_server_status,
};
pub use envelope::{ErrorEnvelope, UsageParts, partial_results_fields};
pub use recovery::NextCommand;
pub use refusal::Refusal;
pub use spec::StdoutDocument;
pub use transport::{network_error, transport_cause};

#[derive(Error, Debug)]
pub enum CliError {
    /// Wraps anyhow errors from any part of the application. Build one that
    /// quotes remote text with [`CliError::internal_with_remote`].
    #[error("{0}")]
    Internal(#[from] anyhow::Error),

    #[error("{context}: {remote}")]
    ExchangeRejected {
        context: &'static str,
        remote: Untrusted,
    },

    /// `preview` is a runnable `--dry-run` command; `approve` is the same
    /// command with `--yes`, offered only for use after a human approves.
    #[error("{headline}")]
    ConfirmationRequired {
        headline: String,
        preview: Option<String>,
        approve: Option<String>,
    },

    /// Invalid CLI configuration, config file, or environment setting.
    #[error("Configuration error: {0}")]
    Configuration(String),

    /// Required input is missing and this invocation can't prompt for it.
    #[error("{0}")]
    MissingInput(String),

    /// No wallet or private key configured.
    #[error("Authentication required.")]
    AuthRequired,

    /// Invalid key format or expired credentials.
    #[error("Invalid authentication: {0}")]
    InvalidAuth(String),

    /// A remote service rejected the credentials.
    #[error("Invalid authentication: {0}")]
    AuthRejected(Untrusted),

    /// The exchange could not find the signer: the signature recovered to an
    /// address that is neither a user nor an approved API wallet.
    #[error("Invalid authentication: {0}")]
    SignerNotRegistered(Untrusted),

    #[error("OWS wallet '{wallet}' was not found")]
    OwsWalletNotFound { wallet: String },

    /// OWS wallet has no Hyperliquid or EVM account for signer resolution.
    #[error("OWS wallet '{wallet}' has no Hyperliquid ({caip2}) or EVM (eip155:1) account")]
    OwsNoChainAccount { wallet: String, caip2: String },

    /// `retry_after` is the `Retry-After` header in seconds when present.
    /// `retryable` is true only for reads, which are safe to resend.
    /// Build the no-information form with [`CliError::rate_limited`].
    #[error("Too many requests to the Hyperliquid API. {}", rate_limit_hint(.retry_after))]
    RateLimited {
        retry_after: Option<u64>,
        retryable: bool,
    },

    /// The address spent its request budget. The exchange answers
    /// this with HTTP 200 and its own counts, which the message quotes.
    #[error(
        "Too many requests from this address: its request allowance is used up. Exchange said: {0}"
    )]
    AddressRateLimited(Untrusted),

    /// Network or API unreachable. The message is complete user-facing copy.
    #[error("{0}")]
    Unavailable(String),

    /// The server was reached, so this is not a connectivity problem. The
    /// status goes in [`CliError::detail`], not in the headline.
    #[error("{}", api_status_headline(*.status))]
    ApiStatus {
        status: u16,
        /// Canonical reason phrase, for the detail line.
        phrase: String,
        remote: Option<Untrusted>,
    },

    #[error("{0}")]
    Timeout(String),

    /// A mutation request was sent but its result is unknown. Construct with
    /// [`CliError::unknown_outcome`].
    #[error("Can't confirm whether the exchange applied this action. {0}")]
    UnknownOutcome(String),

    #[error("Unsupported input: {0}")]
    Unsupported(String),

    /// The exchange refused an order with one of its documented messages.
    /// `text` is the complete message, ending with the labelled remote text.
    #[error("Unsupported input: {text}")]
    ExchangeRefused { refusal: Refusal, text: String },

    #[error("Couldn't find asset \"{asset}\". Did you mean {suggestions}?")]
    AssetNotFound {
        asset: String,
        /// The single closest match.
        suggestions: String,
    },

    #[error("Couldn't find asset \"{asset}\".")]
    AssetNotFoundNoSuggestion { asset: String },

    /// Partial success in a batch operation.
    #[error("Partial results: {0}")]
    PartialResults(String),
}

fn rate_limit_hint(retry_after: &Option<u64>) -> String {
    match retry_after {
        Some(seconds) => format!("Try again in {seconds}s."),
        None => "Try again shortly.".to_string(),
    }
}

fn api_status_headline(status: u16) -> &'static str {
    if classify::is_server_status(status) {
        "The Hyperliquid API failed to process the request."
    } else {
        "The Hyperliquid API rejected the request."
    }
}

/// Every text in the error system that tells a person or agent to run a
/// command: agent actions, hints, recovery commands and the outcome-notation
/// advice. The registry test resolves the `hyperliquid <words>` in them.
#[cfg(test)]
pub(crate) fn command_advice() -> Vec<String> {
    let mut texts = Vec::new();
    for spec in spec::catalog_specs()
        .into_iter()
        .chain([&spec::ADDRESS_RATE_LIMITED])
    {
        texts.push(spec.agent_action.to_string());
        texts.extend(spec.hint.map(str::to_string));
    }
    let recoveries = [CliError::AuthRequired, CliError::unknown_outcome("x")];
    for recovery in recoveries.iter().filter_map(CliError::recovery) {
        texts.extend(recovery.commands.into_iter().map(|next| next.command));
    }
    texts.push(CliError::unsupported_outcome_notation("#1").to_string());
    texts
}

impl CliError {
    /// The spec constant for this variant: the only place a variant gets its
    /// exit code, category, reason and retry policy.
    fn spec(&self) -> &'static spec::Spec {
        match self {
            Self::Internal(_) => &spec::INTERNAL,
            Self::ExchangeRejected { .. } => &spec::EXCHANGE_REJECTED,
            Self::Configuration(_) => &spec::INVALID_CONFIG,
            Self::MissingInput(_) => &spec::MISSING_INPUT,
            Self::AuthRequired => &spec::AUTH_REQUIRED,
            Self::InvalidAuth(_) | Self::AuthRejected(_) => &spec::INVALID_AUTH,
            Self::SignerNotRegistered(_) => &spec::SIGNER_NOT_REGISTERED,
            Self::OwsWalletNotFound { .. } => &spec::WALLET_NOT_FOUND,
            Self::OwsNoChainAccount { .. } => &spec::WALLET_ACCOUNT_MISSING,
            Self::RateLimited { .. } => &spec::RATE_LIMITED,
            Self::AddressRateLimited(_) => &spec::ADDRESS_RATE_LIMITED,
            Self::Unavailable(_) => &spec::NETWORK_ERROR,
            Self::ApiStatus { .. } => &spec::API_ERROR,
            Self::Timeout(_) => &spec::TIMEOUT,
            Self::UnknownOutcome(_) => &spec::UNKNOWN_OUTCOME,
            Self::Unsupported(_) => &spec::UNSUPPORTED_INPUT,
            Self::ExchangeRefused { refusal, .. } => refusal.spec(),
            Self::ConfirmationRequired { .. } => &spec::CONFIRMATION_REQUIRED,
            Self::AssetNotFound { .. } | Self::AssetNotFoundNoSuggestion { .. } => {
                &spec::ASSET_NOT_FOUND
            }
            Self::PartialResults(_) => &spec::PARTIAL_FAILURE,
        }
    }

    #[must_use]
    pub fn exit_code(&self) -> i32 {
        self.spec().exit_code
    }

    #[must_use]
    pub fn category(&self) -> &'static str {
        self.spec().category
    }

    /// Stable snake_case reason code for JSON error envelopes.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        self.spec().reason
    }

    /// Who writes the JSON document on stdout when this error ends the run.
    #[must_use]
    pub fn stdout_document(&self) -> StdoutDocument {
        self.spec().stdout
    }

    /// Whether resending is known to be safe and useful. `None` when unknown.
    #[must_use]
    pub fn retryable(&self) -> Option<bool> {
        match self.spec().retry {
            spec::Retry::Unknown => None,
            spec::Retry::Never => Some(false),
            spec::Retry::Sometimes(_) => self.resend_is_safe().then_some(true),
        }
    }

    /// The per-error condition behind a [`spec::Retry::Sometimes`] policy.
    fn resend_is_safe(&self) -> bool {
        match self {
            Self::RateLimited { retryable, .. } => *retryable,
            Self::ApiStatus { status, .. } => classify::is_server_status(*status),
            _ => false,
        }
    }

    /// Seconds to wait before retrying: the `Retry-After` header of a rate
    /// limit, or the wait its spec names.
    #[must_use]
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after, .. } => *retry_after,
            _ => self.spec().retry_after_secs,
        }
    }

    /// A rate-limit response with no retry information.
    #[must_use]
    pub fn rate_limited() -> Self {
        Self::RateLimited {
            retry_after: None,
            retryable: false,
        }
    }

    /// Build an [`CliError::UnknownOutcome`] from a cause such as `The request timed out after 10s`.
    #[must_use]
    pub fn unknown_outcome(cause: impl AsRef<str>) -> Self {
        let cause = cause.as_ref().trim();
        let cause = if cause.ends_with('.') {
            cause.to_string()
        } else {
            format!("{cause}.")
        };
        Self::UnknownOutcome(cause)
    }

    #[must_use]
    pub fn api_status(status: reqwest::StatusCode, body: &str) -> Self {
        let remote = (!body.trim().is_empty()).then(|| Untrusted::from_remote(body));
        Self::ApiStatus {
            status: status.as_u16(),
            phrase: status.canonical_reason().unwrap_or_default().to_string(),
            remote,
        }
    }

    /// The error for a non-2xx reply from a service the CLI calls once, such as
    /// the release host: a rate limit, else the failure status.
    #[must_use]
    pub fn failed_reply(status: reqwest::StatusCode, body: &str) -> Self {
        if http_response_indicates_rate_limit(status.as_u16(), body) {
            Self::rate_limited()
        } else {
            Self::api_status(status, body)
        }
    }

    #[must_use]
    pub fn internal_with_remote(prefix: impl std::fmt::Display, remote: &Untrusted) -> Self {
        Self::Internal(anyhow::anyhow!("{prefix}{remote}"))
    }

    /// Build an [`CliError::Unsupported`] whose message ends with remote text,
    /// for a request the remote side refused because of the values sent. The
    /// text carries the [`UNTRUSTED_LABEL`](crate::response_sanitization::UNTRUSTED_LABEL).
    #[must_use]
    pub fn unsupported_with_remote(prefix: impl std::fmt::Display, remote: &Untrusted) -> Self {
        Self::Unsupported(format!("{prefix}{remote}"))
    }

    /// Build an [`CliError::ExchangeRefused`] whose message is `lead` followed
    /// by the labelled remote text.
    #[must_use]
    pub fn refused(refusal: Refusal, lead: impl std::fmt::Display, remote: &Untrusted) -> Self {
        Self::ExchangeRefused {
            refusal,
            text: format!("{lead}{remote}"),
        }
    }

    /// What to do about this failure, for the failures that have a specific
    /// answer: the refusals, an unregistered signer, the address rate limit.
    /// The JSON envelope carries it as `data.hint`.
    #[must_use]
    pub fn hint(&self) -> Option<&'static str> {
        self.spec().hint
    }

    /// Outcome asset notation given to a command that only trades or reads perp
    /// and spot markets.
    #[must_use]
    pub fn unsupported_outcome_notation(raw: &str) -> Self {
        Self::Unsupported(format!(
            "outcome asset notation '{raw}' requires an outcome-aware command path; use `{}` to inspect the encoded HIP-4 asset id. Live limit orders support outcome notation.",
            crate::hints::outcome_get_command(raw)
        ))
    }

    /// Secondary detail line shown under the headline in human output.
    #[must_use]
    pub fn detail(&self) -> Option<String> {
        match self {
            Self::ApiStatus { status, phrase, .. } => Some(if phrase.is_empty() {
                format!("HTTP {status}")
            } else {
                format!("HTTP {status} {phrase}")
            }),
            _ => self.hint().map(str::to_string),
        }
    }

    /// The single close-match suggestion carried by an asset-not-found error.
    #[must_use]
    pub fn suggestion(&self) -> Option<&str> {
        match self {
            Self::AssetNotFound { suggestions, .. } => suggestions
                .split(", ")
                .next()
                .filter(|name| !name.is_empty()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_reply_is_a_rate_limit_or_an_api_status() {
        assert_eq!(
            CliError::failed_reply(reqwest::StatusCode::TOO_MANY_REQUESTS, "").exit_code(),
            11
        );
        let err = CliError::failed_reply(reqwest::StatusCode::NOT_FOUND, "missing");
        assert!(matches!(err, CliError::ApiStatus { status: 404, .. }));
    }

    /// One error per variant, paired with the spec it must report.
    fn variant_specs() -> Vec<(CliError, &'static spec::Spec)> {
        let remote = |text: &str| Untrusted::from_remote(text);
        let mut variants = vec![
            (CliError::Internal(anyhow::anyhow!("test")), &spec::INTERNAL),
            (
                CliError::ExchangeRejected {
                    context: "order rejected",
                    remote: remote("rejected"),
                },
                &spec::EXCHANGE_REJECTED,
            ),
            (
                CliError::ConfirmationRequired {
                    headline: "needs confirmation".into(),
                    preview: None,
                    approve: None,
                },
                &spec::CONFIRMATION_REQUIRED,
            ),
            (
                CliError::Configuration("bad config".into()),
                &spec::INVALID_CONFIG,
            ),
            (
                CliError::MissingInput("missing".into()),
                &spec::MISSING_INPUT,
            ),
            (CliError::AuthRequired, &spec::AUTH_REQUIRED),
            (CliError::InvalidAuth("bad key".into()), &spec::INVALID_AUTH),
            (
                CliError::AuthRejected(remote("Unauthorized")),
                &spec::INVALID_AUTH,
            ),
            (
                CliError::SignerNotRegistered(remote("User or API Wallet 0x1 does not exist.")),
                &spec::SIGNER_NOT_REGISTERED,
            ),
            (
                CliError::OwsWalletNotFound {
                    wallet: "missing-wallet".into(),
                },
                &spec::WALLET_NOT_FOUND,
            ),
            (
                CliError::OwsNoChainAccount {
                    wallet: "wallet-without-account".into(),
                    caip2: "hl:testnet".into(),
                },
                &spec::WALLET_ACCOUNT_MISSING,
            ),
            (CliError::rate_limited(), &spec::RATE_LIMITED),
            (
                CliError::RateLimited {
                    retry_after: Some(3),
                    retryable: true,
                },
                &spec::RATE_LIMITED,
            ),
            (
                CliError::AddressRateLimited(remote("Too many cumulative requests sent")),
                &spec::ADDRESS_RATE_LIMITED,
            ),
            (
                CliError::Unavailable("timeout".into()),
                &spec::NETWORK_ERROR,
            ),
            (
                CliError::api_status(reqwest::StatusCode::BAD_GATEWAY, "body"),
                &spec::API_ERROR,
            ),
            (
                CliError::Timeout("deadline exceeded".into()),
                &spec::TIMEOUT,
            ),
            (CliError::unknown_outcome("cause"), &spec::UNKNOWN_OUTCOME),
            (
                CliError::Unsupported("bad".into()),
                &spec::UNSUPPORTED_INPUT,
            ),
            (
                CliError::AssetNotFound {
                    asset: "BT".into(),
                    suggestions: "BTC".into(),
                },
                &spec::ASSET_NOT_FOUND,
            ),
            (
                CliError::AssetNotFoundNoSuggestion { asset: "ZZ".into() },
                &spec::ASSET_NOT_FOUND,
            ),
            (
                CliError::PartialResults("half".into()),
                &spec::PARTIAL_FAILURE,
            ),
        ];
        variants.extend(Refusal::ALL.iter().map(|refusal| {
            (
                CliError::refused(*refusal, "lead: ", &remote("x")),
                refusal.spec(),
            )
        }));
        variants
    }

    fn every_variant() -> Vec<CliError> {
        variant_specs().into_iter().map(|(err, _)| err).collect()
    }

    #[test]
    fn each_variant_reports_its_spec() {
        for (err, spec) in variant_specs() {
            assert_eq!(err.spec(), spec, "{err:?}");
        }
    }

    #[test]
    fn retryability_follows_the_variant_not_a_second_table() {
        assert_eq!(CliError::unknown_outcome("x").retryable(), Some(false));
        assert_eq!(CliError::rate_limited().retryable(), None);
        assert_eq!(
            CliError::RateLimited {
                retry_after: Some(7),
                retryable: true
            }
            .retryable(),
            Some(true)
        );
        assert_eq!(
            CliError::api_status(reqwest::StatusCode::BAD_GATEWAY, "").retryable(),
            Some(true)
        );
        assert_eq!(
            CliError::api_status(reqwest::StatusCode::BAD_REQUEST, "").retryable(),
            None
        );
        assert_eq!(CliError::AuthRequired.retryable(), None);
    }

    #[test]
    fn display_rate_limited() {
        assert_eq!(
            CliError::rate_limited().to_string(),
            "Too many requests to the Hyperliquid API. Try again shortly."
        );
        let err = CliError::RateLimited {
            retry_after: Some(7),
            retryable: true,
        };
        assert_eq!(
            err.to_string(),
            "Too many requests to the Hyperliquid API. Try again in 7s."
        );
    }

    #[test]
    fn display_keeps_remote_text_labelled() {
        let remote = Untrusted::from_remote;
        let cases = [
            (
                CliError::ExchangeRejected {
                    context: "order rejected",
                    remote: remote("Order must have minimum value of $10."),
                },
                "order rejected: [untrusted remote data] Order must have minimum value of $10.",
            ),
            (
                CliError::AuthRejected(remote("Unauthorized")),
                "Invalid authentication: [untrusted remote data] Unauthorized",
            ),
            (
                CliError::refused(
                    Refusal::ReduceOnly,
                    "Couldn't place order. Exchange said: ",
                    &remote("Reduce only order would increase position."),
                ),
                "Unsupported input: Couldn't place order. Exchange said: [untrusted remote data] Reduce only order would increase position.",
            ),
            (
                CliError::SignerNotRegistered(remote("User or API Wallet 0x1 does not exist.")),
                "Invalid authentication: [untrusted remote data] User or API Wallet 0x1 does not exist.",
            ),
            (
                CliError::AddressRateLimited(remote("Too many cumulative requests sent (2 > 1)")),
                "Too many requests from this address: its request allowance is used up. Exchange said: [untrusted remote data] Too many cumulative requests sent (2 > 1)",
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected);
        }
    }

    #[test]
    fn only_the_classes_with_a_specific_answer_carry_a_hint() {
        let hinted = [
            CliError::SignerNotRegistered(Untrusted::from_remote("x")),
            CliError::AddressRateLimited(Untrusted::from_remote("x")),
        ]
        .into_iter()
        .chain(
            Refusal::ALL
                .iter()
                .map(|refusal| CliError::refused(*refusal, "", &Untrusted::from_remote("x"))),
        );
        for err in hinted {
            assert!(err.hint().is_some(), "{err:?}");
            assert_eq!(err.detail(), err.hint().map(str::to_string), "{err:?}");
        }
        for err in [
            CliError::rate_limited(),
            CliError::Unsupported("x".into()),
            CliError::AuthRejected(Untrusted::from_remote("x")),
            CliError::ExchangeRejected {
                context: "c",
                remote: Untrusted::from_remote("x"),
            },
        ] {
            assert!(err.hint().is_none(), "{err:?}");
        }
    }

    #[test]
    fn the_address_limit_is_a_rate_limit_that_names_its_wait_and_never_claims_a_resend_is_safe() {
        let plain = CliError::rate_limited();
        let err = CliError::AddressRateLimited(Untrusted::from_remote("x"));
        assert_eq!(err.reason(), plain.reason());
        assert_eq!(err.exit_code(), plain.exit_code());
        assert_eq!(err.retry_after(), Some(10));
        assert_eq!(err.retryable(), None);
    }

    #[test]
    fn every_variant_reports_a_cataloged_reason() {
        let catalog = spec::catalog_specs();
        for err in every_variant() {
            assert!(
                catalog.iter().any(|spec| spec.reason == err.reason()),
                "{err:?}"
            );
        }
    }

    #[test]
    fn catalog_lists_each_reason_once_and_every_listed_spec_is_reachable() {
        let catalog = spec::catalog_specs();
        let mut reasons = catalog.iter().map(|spec| spec.reason).collect::<Vec<_>>();
        reasons.sort_unstable();
        let listed = reasons.len();
        reasons.dedup();
        assert_eq!(
            reasons.len(),
            listed,
            "a reason appears twice in the catalog"
        );

        let usage = [&spec::INVALID_ARGUMENTS, &spec::UNKNOWN_COMMAND];
        let variants = every_variant();
        for spec in spec::catalog_specs() {
            assert!(
                usage.contains(&spec) || variants.iter().any(|err| err.spec() == spec),
                "no error reports {}",
                spec.reason
            );
        }
    }

    fn catalog_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".claude/error-catalog.json")
    }

    /// `.claude/error-catalog.json` is generated from the spec table. After
    /// changing a spec, regenerate with
    /// `HL_UPDATE_ERROR_CATALOG=1 cargo test --lib catalog_file_matches_the_spec_table`.
    #[test]
    fn catalog_file_matches_the_spec_table() {
        let expected = spec::catalog_json();
        if std::env::var_os("HL_UPDATE_ERROR_CATALOG").is_some() {
            std::fs::write(catalog_path(), &expected).unwrap();
        }
        let checked_in = std::fs::read_to_string(catalog_path()).unwrap();
        assert_eq!(
            checked_in, expected,
            "error-catalog.json drifted from errors::spec; regenerate it with HL_UPDATE_ERROR_CATALOG=1"
        );
    }

    /// The `## Error Handling` section of a skill file, up to the next heading.
    fn error_handling_section(skill: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(skill);
        let text = std::fs::read_to_string(path).unwrap();
        let start = text.find("## Error Handling").expect("section exists");
        let rest = &text[start + 1..];
        let end = rest.find("\n## ").map_or(rest.len(), |end| end + 1);
        rest[..end].to_string()
    }

    #[test]
    fn skill_files_document_the_catalog_exit_codes_and_reasons() {
        let mut exit_codes = spec::catalog_specs()
            .iter()
            .map(|spec| spec.exit_code)
            .collect::<Vec<_>>();
        exit_codes.dedup();

        for skill in ["SKILL.md", "skills/hyperliquid-cli/SKILL.md"] {
            let section = error_handling_section(skill);

            let documented = section
                .lines()
                .filter_map(|line| line.strip_prefix("- `"))
                .filter_map(|line| line.split_once("`:"))
                .filter_map(|(code, _)| code.parse::<i32>().ok())
                .collect::<Vec<_>>();
            assert_eq!(documented, exit_codes, "{skill} exit-code bullets");

            let examples = section
                .split_once("(for example ")
                .and_then(|(_, rest)| rest.split_once(')'))
                .map(|(list, _)| list)
                .expect("reason examples");
            for reason in examples.split(", ") {
                let reason = reason.trim_matches('`');
                assert!(
                    spec::catalog_specs()
                        .iter()
                        .any(|spec| spec.reason == reason),
                    "{skill} names unknown reason {reason}"
                );
            }
        }
    }

    #[test]
    fn partial_results_own_their_stdout_document() {
        assert_eq!(
            CliError::PartialResults("half".into()).stdout_document(),
            StdoutDocument::CommandResult
        );
        for err in every_variant() {
            if !matches!(err, CliError::PartialResults(_)) {
                assert_eq!(err.stdout_document(), StdoutDocument::Envelope, "{err:?}");
            }
        }
    }
}
