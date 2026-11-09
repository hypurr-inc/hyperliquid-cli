//! The one table of what each kind of failure reports to machines.
//!
//! Every exit code, category, reason, retry policy and agent instruction lives
//! in a [`Spec`] constant here. [`CliError::spec`](super::CliError) picks one per
//! variant, the JSON envelope copies its fields, and the checked-in
//! `.claude/error-catalog.json` is rendered from [`ALL`] (a test fails when the
//! file drifts, see `catalog_file_matches_the_spec_table`).

/// Whether a machine consumer may resend the failed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Retry {
    /// Nothing is known; the envelope carries no `retryable` key.
    Unknown,
    /// Resending is never right; the envelope says `retryable: false`.
    Never,
    /// The envelope says `retryable: true` only when the error itself says so.
    /// The text names the condition.
    Sometimes(&'static str),
}

/// Who writes the JSON document on stdout for a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdoutDocument {
    /// The error envelope, printed when the process exits.
    Envelope,
    /// The command already printed its own single JSON document, which carries
    /// the envelope keys from [`partial_results_fields`](super::partial_results_fields).
    /// Printing the envelope as well would make stdout two documents.
    CommandResult,
}

/// What a failure reports to callers and machine consumers.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Spec {
    pub exit_code: i32,
    pub category: &'static str,
    pub reason: &'static str,
    pub retry: Retry,
    pub stdout: StdoutDocument,
    /// What an agent should do next.
    pub agent_action: &'static str,
}

impl Spec {
    const fn new(
        exit_code: i32,
        category: &'static str,
        reason: &'static str,
        agent_action: &'static str,
    ) -> Self {
        Self {
            exit_code,
            category,
            reason,
            retry: Retry::Unknown,
            stdout: StdoutDocument::Envelope,
            agent_action,
        }
    }

    const fn retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    const fn command_owns_stdout(mut self) -> Self {
        self.stdout = StdoutDocument::CommandResult;
        self
    }
}

pub(super) const INTERNAL: Spec = Spec::new(
    1,
    "internal",
    "internal_error",
    "Stop and inspect the message.",
);
pub(super) const EXCHANGE_REJECTED: Spec = Spec::new(
    1,
    "internal",
    "exchange_rejected",
    "The exchange answered and refused the action. Read the labelled remote message and change the request before trying again.",
);
pub(super) const INVALID_CONFIG: Spec = Spec::new(
    2,
    "configuration",
    "invalid_config",
    "Fix the configuration file, environment variable, or option value named in the message.",
);
pub(super) const MISSING_INPUT: Spec = Spec::new(
    2,
    "missing_input",
    "missing_input",
    "Pass the named option or argument; this run can't prompt for it.",
);
pub(super) const INVALID_ARGUMENTS: Spec = Spec::new(
    2,
    "usage",
    "invalid_arguments",
    "Fix the arguments using `data.usage`; `next` holds the help command.",
);
pub(super) const UNKNOWN_COMMAND: Spec = Spec::new(
    2,
    "usage",
    "unknown_command",
    "Use a command from `hyperliquid --help`; `data.suggestion` is a hint, not a command to run.",
);
pub(super) const AUTH_REQUIRED: Spec = Spec::new(
    10,
    "auth",
    "auth_required",
    "Configure a wallet, private key, or keystore; `next` lists the setup commands.",
);
pub(super) const INVALID_AUTH: Spec = Spec::new(
    10,
    "auth",
    "invalid_auth",
    "Check the key format, keystore password, or session; a remote service rejected the credentials.",
);
pub(super) const WALLET_NOT_FOUND: Spec = Spec::new(
    10,
    "auth",
    "wallet_not_found",
    "Pick a wallet that exists in the OWS vault.",
);
pub(super) const WALLET_ACCOUNT_MISSING: Spec = Spec::new(
    10,
    "auth",
    "wallet_account_missing",
    "Use an OWS wallet with a Hyperliquid or EVM account.",
);
pub(super) const SIGNER_NOT_REGISTERED: Spec = Spec::new(
    10,
    "auth",
    "signer_not_registered",
    "The signature recovered to an address the exchange doesn't know as a user or API wallet (the address is in the remote message). Likely causes: the API wallet was never approved, expired or was pruned (approving a new unnamed API wallet replaces the old one), or the wrong key signed. Check `hyperliquid api-wallet list`; the master account approves a wallet with `hyperliquid api-wallet approve`. If the named address is not your signer's, the signed payload differs from what the exchange hashed.",
);
pub(super) const RATE_LIMITED: Spec = Spec::new(
    11,
    "rate_limited",
    "rate_limited",
    "Back off for `retry_after` seconds when present; prefer streaming over REST polling. The address-based limit (`Too many cumulative requests`) only grows with trading volume or the `reserveRequestWeight` action; meanwhile the address may send one request every 10 seconds. `hyperliquid account rate-limit` shows its usage.",
)
.retry(Retry::Sometimes(
    "reads only; a rate-limited mutation is never marked retryable",
));
pub(super) const NETWORK_ERROR: Spec = Spec::new(
    12,
    "unavailable",
    "network_error",
    "The API was not reached; retry with bounded exponential backoff.",
);
pub(super) const API_ERROR: Spec = Spec::new(
    12,
    "unavailable",
    "api_error",
    "The API answered with a failure status; see `data.http_status`. Retry only when `retryable` is true.",
)
.retry(Retry::Sometimes("HTTP 5xx statuses"));
pub(super) const TIMEOUT: Spec = Spec::new(
    12,
    "unavailable",
    "timeout",
    "The operation exceeded the requested bound; retry with a longer bound or fewer items.",
);
pub(super) const UNKNOWN_OUTCOME: Spec = Spec::new(
    12,
    "unavailable",
    "unknown_outcome",
    "Never resend. Run the `next` commands to see whether the action applied.",
)
.retry(Retry::Never);
pub(super) const UNSUPPORTED_INPUT: Spec = Spec::new(
    13,
    "unsupported",
    "unsupported_input",
    "Correct the asset, DEX, address, or value the message names.",
);
pub(super) const MIN_ORDER_VALUE: Spec = Spec::new(
    13,
    "unsupported",
    "order_below_minimum_value",
    "Raise the size or price until the order's notional value is at least $10 (spot: 10 units of the quote token).",
);
pub(super) const REDUCE_ONLY_VIOLATION: Spec = Spec::new(
    13,
    "unsupported",
    "reduce_only_violation",
    "A reduce-only order may only shrink an existing position on the same market. Check the side and size in `hyperliquid positions list`, or drop --reduce-only.",
);
pub(super) const POST_ONLY_WOULD_CROSS: Spec = Spec::new(
    13,
    "unsupported",
    "post_only_would_cross",
    "A post-only (--tif alo) order may not match on entry. Price it behind the best bid or ask (`hyperliquid book`), or use --tif gtc.",
);
pub(super) const IOC_NO_MATCH: Spec = Spec::new(
    13,
    "unsupported",
    "ioc_no_match",
    "An IOC order found nothing to match and was canceled. Move the limit price across the book (`hyperliquid book`) or reduce the size.",
);
pub(super) const BAD_PRICE: Spec = Spec::new(
    13,
    "unsupported",
    "bad_price",
    "Round the price to the market's tick: at most 5 significant figures and at most (6 for perps, 8 for spot) minus szDecimals decimal places; integer prices are always allowed.",
);
pub(super) const PRICE_OUT_OF_BAND: Spec = Spec::new(
    13,
    "unsupported",
    "price_out_of_band",
    "The price is too far from the oracle or reference price. Price closer to the current mark (`hyperliquid book`, `hyperliquid perps get`).",
);
pub(super) const ZERO_SIZE: Spec = Spec::new(
    13,
    "unsupported",
    "zero_size",
    "The size is zero after rounding to the market's size decimals. Use a larger --size.",
);
pub(super) const OPEN_INTEREST_CAP: Spec = Spec::new(
    13,
    "unsupported",
    "open_interest_cap",
    "The market is at or growing toward its open-interest cap, so orders that increase open interest are refused. Reduce or close a position, trade another market, or retry later.",
);
pub(super) const INVALID_TRIGGER_PRICE: Spec = Spec::new(
    13,
    "unsupported",
    "invalid_trigger_price",
    "The TP/SL trigger price is not valid for this order against the current market (for example a trigger on the wrong side of the mark price). Check the trigger price and side.",
);
pub(super) const NO_MARKET_LIQUIDITY: Spec = Spec::new(
    13,
    "unsupported",
    "no_market_liquidity",
    "The book has nothing to fill a market order against. Place a limit order, or retry when the book has depth (`hyperliquid book`).",
);
pub(super) const CONFIRMATION_REQUIRED: Spec = Spec::new(
    13,
    "unsupported",
    "confirmation_required",
    "Run the `next` preview. Run the approve command only after a human approves.",
);
pub(super) const ASSET_NOT_FOUND: Spec = Spec::new(
    13,
    "unsupported",
    "asset_not_found",
    "Correct the asset name; `data.suggestion` is a hint for a human, never a command to run.",
);
pub(super) const PARTIAL_FAILURE: Spec = Spec::new(
    15,
    "partial_results",
    "partial_failure",
    "Inspect the per-item results in the single stdout document, then check `next` before acting again.",
)
.command_owns_stdout();

/// Every spec a failure can report, in exit-code order. The catalog lists these.
#[cfg(test)]
pub(super) const ALL: &[&Spec] = &[
    &INTERNAL,
    &EXCHANGE_REJECTED,
    &INVALID_CONFIG,
    &MISSING_INPUT,
    &INVALID_ARGUMENTS,
    &UNKNOWN_COMMAND,
    &AUTH_REQUIRED,
    &INVALID_AUTH,
    &WALLET_NOT_FOUND,
    &WALLET_ACCOUNT_MISSING,
    &SIGNER_NOT_REGISTERED,
    &RATE_LIMITED,
    &NETWORK_ERROR,
    &API_ERROR,
    &TIMEOUT,
    &UNKNOWN_OUTCOME,
    &UNSUPPORTED_INPUT,
    &MIN_ORDER_VALUE,
    &REDUCE_ONLY_VIOLATION,
    &POST_ONLY_WOULD_CROSS,
    &IOC_NO_MATCH,
    &BAD_PRICE,
    &PRICE_OUT_OF_BAND,
    &ZERO_SIZE,
    &OPEN_INTEREST_CAP,
    &INVALID_TRIGGER_PRICE,
    &NO_MARKET_LIQUIDITY,
    &CONFIRMATION_REQUIRED,
    &ASSET_NOT_FOUND,
    &PARTIAL_FAILURE,
];

/// The agent-facing catalog of every exit code, category and reason: the exact
/// contents of `.claude/error-catalog.json`.
#[cfg(test)]
pub(super) fn catalog_json() -> String {
    #[derive(serde::Serialize)]
    struct Entry {
        exit_code: i32,
        category: &'static str,
        reason: &'static str,
        retry: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_when: Option<&'static str>,
        stdout: &'static str,
        agent_action: &'static str,
    }
    #[derive(serde::Serialize)]
    struct Catalog {
        version: u32,
        errors: Vec<Entry>,
    }

    let errors = ALL
        .iter()
        .map(|spec| Entry {
            exit_code: spec.exit_code,
            category: spec.category,
            reason: spec.reason,
            retry: match spec.retry {
                Retry::Unknown => "unknown",
                Retry::Never => "never",
                Retry::Sometimes(_) => "sometimes",
            },
            retry_when: match spec.retry {
                Retry::Sometimes(when) => Some(when),
                Retry::Unknown | Retry::Never => None,
            },
            stdout: match spec.stdout {
                StdoutDocument::Envelope => "envelope",
                StdoutDocument::CommandResult => "command_result",
            },
            agent_action: spec.agent_action,
        })
        .collect();
    let mut json =
        serde_json::to_string_pretty(&Catalog { version: 2, errors }).expect("catalog serializes");
    json.push('\n');
    json
}
