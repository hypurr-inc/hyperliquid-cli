# Changelog

## Unreleased

- HIP-3 reads: `book`, `candles`, `spread`, `funding`, and `subscribe trades|orderbook|candles` accept a DEX-qualified market (`xyz:XYZ100`) by looking the market up on its dex, so a HIP-3 book, tape and funding rate no longer fail with `asset not found`.
- `mids`, `positions list`, `orders open`, and `account portfolio` take `--dex <NAME>` to scope the read to a builder-deployed perp DEX. `account portfolio --dex` reports that dex's perp account only; spot balances and vault equities stay on the default-dex view.
- A `--dex` name is validated against `perpDexs` before the read: an unknown DEX exits `13` with `Unknown DEX` instead of returning an empty account or empty mid map, and the canonical spelling is used in every request. A blank `--dex` behaves like no flag.
- Order prices and sizes are truncated to the exchange tick and lot rules for the asset (5 significant figures and `6 - szDecimals` perp or `8 - szDecimals` spot/outcome decimals for prices, `szDecimals` for sizes) on `orders create`, `batch-create`, `scale`, `tpsl`, `bracket`, `chase`, and `trailing-stop`, so an over-precise value is placed at the representable value instead of being rejected on submission. A value that truncates to zero exits `2` naming the asset.
- `orders modify` keeps a trigger order's existing trigger price when `--trigger-price` is omitted instead of overwriting it with the order's limit price, and aborts before submitting when the replacement requires `--always-place` (a trigger replacement or an IOC TIF), matching the exchange's `a: false` pre-validation.
- `orders twap-create` rejects durations above the exchange's 7-day maximum.
- Bankr now refuses every user-signed action it does not cover (withdrawals, spot/asset sends, staking, builder approvals, `setAbstraction`) with exit `13` before a request is sent, on the typed-data path as well as the L1 path; only trading actions and spot-to-perp USDC moves reach Bankr, as documented.
- A signature whose recovery byte names no public key is refused as `<provider>_malformed_response` (exit `10`) instead of panicking, for Bankr and OWS alike.
- `--keystore` is no longer overridden by an ambient `HYPERLIQUID_PRIVATE_KEY` or a legacy config key; a private key given on the command line still wins.
- Account selectors fail with the vault's own error when the vault can't be read or a name is ambiguous, for data lookups (`--user`, `--account`) as for signing, instead of reporting the account as not found. An address selector now matches a pre-vault stored account in any letter case, as it does a vault wallet.
- `HYPERLIQUID_API_BASE_URL` (and its network variants) accept an IPv6 loopback host such as `http://[::1]:8080`, like the Bankr endpoint does.
- `wallet show --account <vault wallet>` describes the wallet by name like the default wallet does. `account ls` no longer prints the always-empty Master, Agent Name and Expires At columns (JSON is unchanged).
- `Config keys written` in wallet and setup receipts lists the keys the command changed, read from the config diff, and the root help lists every environment variable the CLI reads.

## 0.17.0 - 2026-11-09

Highlights:

- Added Bankr hosted-wallet login: `hyperliquid wallet login` validates a `bk_...` key against `GET /wallet/me`, stores it owner-only with the linked EVM address, and `hyperliquid --bankr <command>` signs with that wallet. `hyperliquid wallet logout` forgets it.
- `BANKR_API_KEY`, `BANKR_API_URL`, and an existing Bankr CLI login in `~/.bankr/config.json` are read, so `--bankr` works without repeating `wallet login`.
- Bankr signs trading actions as EIP-712 typed data (`POST /wallet/sign`, `eth_signTypedData_v4`). Actions whose signature hypersdk derives from a per-action schema (withdrawals, third-party transfers, staking, multisig) fail with exit `13` before any request is sent, and every returned signature is recovered locally against the selected address.
- `AuthRequired` now suggests `hyperliquid wallet login` beside `hyperliquid setup`, and the help lists `BANKR_API_KEY`/`BANKR_API_URL`.
- Added `HYPERLIQUID_HOME_DIR`, `HYPERLIQUID_CONFIG_DIR`, and `HYPERLIQUID_DATA_DIR` to pin the home, config, and data directories instead of using platform discovery. They are listed in the root help.

Command-line behavior:

- `wallet logout` reports `confirmation: prompt` in its schema, matching the prompt it already asked for.
- `BANKR_API_KEY` and `BANKR_API_URL` are read trimmed, so a whitespace-only value no longer shadows the stored config or the Bankr CLI credentials.
- A Bankr request never follows an HTTP redirect, and the cached wallet address is reused only for the endpoint `wallet login` recorded.

## 0.16.0 - 2026-11-08

Highlights:

- Added `hyperliquid mcp`: a stdio JSON-RPC 2.0 Model Context Protocol server that exposes every registered command as an agent tool with its schema. Mutating tools require `HYPERLIQUID_MCP_ALLOW_UNSAFE=1`, and every tool call is bounded by `HYPERLIQUID_MCP_TIMEOUT_MS`.
- Added in-place background self-update with `HYPERLIQUID_AUTO_UPDATE=1`. Standalone installs verify the release SHA-256 before replacing the binary; Homebrew and Cargo installs are never touched. A failed attempt is throttled for 24 hours instead of retrying on every command.
- Replaced the hosted feedback service with `hyperliquid feedback` and `hyperliquid report-incident`, which open pre-filled GitHub issues on `hypurr-inc/hyperliquid-cli`. The Cloudflare Worker, its D1 migrations, and its build-time secrets are gone.
- Every failing command now records a sanitized envelope in the last-error file, including the command path and the `category`/`reason`/`exit_code` triple, so `report-incident` can attach diagnostics without leaking arguments.
- Release packaging now covers Windows ARM64 (`hyperliquid-windows-aarch64.zip`), and `install.sh` supports it.

Command-line behavior:

- `hyperliquid feedback` URL-encodes the issue title, body, and labels, so categories containing spaces, `&`, `#`, or `=` no longer truncate the generated URL.
- `hyperliquid feedback --open` and `hyperliquid report-incident --open` hand the URL to `rundll32 url.dll,FileProtocolHandler` on Windows instead of `cmd /C start`, so the URL is never parsed by a shell.
- Fixed nested `if` and formatting lints so the CI quality gates pass with `-D warnings`.

## 0.15.0 - 2026-11-08

Highlights:

- Migrated organization and repositories to `hypurr-inc`.
- Adopted open [Agent Plugins v1.0.0](https://agent-plugins.org/) standard with root `plugin.json` and canonical `skills/hyperliquid-cli/SKILL.md` packaging.
- Added native Claude Code plugin adapter with `.claude-plugin/plugin.json` manifest.
- Added full Hermes Agent compatibility (`metadata.hermes`, `requires_toolsets: [terminal]`) for automated tap and hub discovery.
- Enhanced Universal Agent install support via `skills.sh`, Claude Code, Cursor, and OpenClaw/ClawHub.

## 0.12.0 - 2026-11-07

Highlights:

- Untangled core architecture: consolidated errors, state, streams, assets, execution contexts, and command catalog.
- Added global `--json` flag shortcut for `--format json` across all subcommands.
- Added native L1 TWAP randomization (`--randomize`) on `orders twap-create` to jitter execution timing and suborder sizes by ±20%.
- Added native trailing stop orders (`orders trailing-stop`) tracking mark prices on L1 nodes with activation price and percent/distance retracement.
- Added atomic order modification (`orders modify --always-place`).
- Upgraded hypersdk to 0.3 with full native OrderResponseStatus handling and resting-order reconciliation.

Command-line behavior:

- `--dry-run` on a read-only command that does not preview (`wallet list`, `account ls`, `schema`) now fails with `--dry-run is only supported for mutating commands with preview support` (exit `13`), like every other such command, instead of running for real.
- `hyperliquid schema` lists `tif` as `gtc, alo, ioc` and `orders bracket --entry` as the six values clap accepts. `account set-default` reports `confirmation: none` (it never prompts), and its help notes say so.
- Errors for a bad `HYPERLIQUID_MAINNET_API_BASE_URL` or `HYPERLIQUID_TESTNET_API_BASE_URL` now name that variable. The update-check cache is written owner-only (0600). If a new wallet can't be recorded in the config and can't be removed from the vault either, the error says the wallet is still there. `wallet reset` no longer prints the plaintext-key warning for a file it is about to delete.
- A group without an action (`hyperliquid orders`) and bare `hyperliquid` print help on stdout and exit `0`. `-h` and `--help` print the same help at every depth, and a closed pipe (`| head`) ends quietly.
- `-v`/`--version` works at every depth. With JSON output it prints `{"version":"X"}`.
- Added global `--non-interactive` and `--no-color`. `CI`, `HYPERLIQUID_AGENT`, JSON output, and `--payload-file -` never prompt. Boolean `HYPERLIQUID_*` variables are validated, and an invalid value exits `2`.
- Usage errors show one suggestion, the real usage line, and `hyperliquid <path> --help` for the nearest valid command. `-fjson` and `-ftable` are honored, and an explicit `--format` wins over `HYPERLIQUID_FORMAT`.
- Help summaries are imperative and ordered by task, the root help lists environment variables, and every state-changing command ends with a `Notes:` block (risk, dry-run, confirmation, testnet behavior). Singular group names and `ls`/`list` work as hidden aliases.
- Non-interactive runs that need confirmation fail with `Can't run <command> without confirmation in non-interactive mode.` and name `--dry-run` and, when supported, `--yes`. `wallet export --yes` and `setup --yes` satisfy confirmation in machine contexts.
- A panic prints `Unexpected error in hyperliquid <command>. This is a bug in hyperliquid-cli.` with the version and report URL, and exits `1`.

Output:

- Color follows `--no-color`, `NO_COLOR`, `FORCE_COLOR`, then per-stream terminal detection (`TERM=dumb` is plain). Pretty output on a pipe has no ANSI codes.
- `--format table` prints a header and tab-separated rows on a pipe and keeps borders on a terminal. JSON is indented on a terminal and compact single-line otherwise; parsed content is unchanged.
- Empty results print the real header on stdout and a `No <items>.` sentence on stderr. `--max-results` truncation in pretty and table output prints `Showing N of M.` on stderr.
- `Completed in` timing prints only for human formats on a terminal, never in JSON, CI, or agent runs.
- Remote text in pretty and table cells is escaped (control, bidi, and zero-width characters show as visible escapes). Epoch-millisecond times show as ISO 8601 UTC in pretty and table output; JSON keeps the numbers.
- Update notices print after the result, at most once a day, never in CI, agent, JSON, or non-terminal runs, or with `NO_UPDATE_NOTIFIER`. The background check no longer delays commands. `hyperliquid update` refuses to replace Homebrew or Cargo installs and prints the matching upgrade command.

Errors:

- JSON errors keep `error`, `category`, and `exit_code` on stdout and add a stable `reason`, plus `retryable`, `retry_after`, `next`, and `data` when they apply. Usage errors move clap's usage block into `data.usage` and the help command into `next`.
- Human errors start with `✗` on a styled terminal and `Error:` otherwise, followed by recovery commands when one applies.
- Network failures name the host and cause (`Couldn't reach api.hyperliquid.xyz. Connection refused.`). A 5xx from the API is reported as an API failure, not a network problem. Rate limits say when to retry.
- Reads retry up to twice on connect errors, timeouts, 5xx, and 429, honoring `Retry-After`. Mutations are never retried. A mutation whose result is unknown (timeout, reset, or 5xx after sending) fails with `reason: "unknown_outcome"` (exit `12`) and points to `orders open` and `account fills`.
- Every Hyperliquid API read goes through one client, so `meta`, the raw `orders history` fallback and the raw spot-metadata fallback also retry transient failures and report a non-2xx reply as an API failure (`reason: "api_error"`) instead of `internal`. The `feedback` service and the release host classify a non-2xx reply the same way (`api_error`, or `rate_limited` for 429) instead of `network_error`. Retry notices (`Retrying automatically…`) appear only for a person at a terminal that may prompt, not for JSON, agent, CI, `--non-interactive` or `--payload-file -` runs. `account subaccounts` and `vaults get` read the exchange's `null` answer as an empty or missing result instead of matching decode-error text.
- Exchange rejections are no longer misclassified as network failures because their text mentions "connection" or "timeout"; they exit `1` with `reason: "exchange_rejected"`.
- Unknown assets suggest at most one close match, also in `data.suggestion`.
- Remote text is stripped of bidi and zero-width characters and capped at 300 characters.

Trading and consent:

- Per-order exchange errors in create, scale, batch, TP/SL, TWAP, cancel, and modify are labelled `[untrusted remote data]` and sanitized.
- Batch, scale, TP/SL, and cancel-all results with some failed legs print every leg in one document and exit `15`. Bracket failures after the entry is live say whether the entry is resting or filled and how to protect or cancel it.
- Mainnet order and cancel-all prompts show network, signer, acting account, builder fee, and counts before asking. Prompts and setup narration go to stderr. An empty stdin no longer accepts defaults: `setup </dev/null` exits `2` without writing anything.
- `wallet delete` and `account remove` preview the wallet and state that deletion can't be undone; on a terminal they require typing the wallet name. `wallet reset` says it deletes the config file and which defaults it clears.
- `account remove` and `account set-default` without a selector in a run that can't prompt exit `2` with `reason: "missing_input"`.
- A configured default wallet that no longer exists is an error instead of a silent fallback to another wallet.
- Config and account-store writes are atomic, owner-only (`0600`), and locked against concurrent updates. Broken config is reported instead of ignored. Read-only commands no longer create the wallet vault directory.
- `--keystore` reads `HYPERLIQUID_KEYSTORE_PASSWORD`. `--payload-file -` reads multi-line JSON from stdin.
- Dry-run envelopes add `network` and the resolved `signer` (or `signer_warning`); pretty and table dry-runs list the arguments.

Trading:

- Added `orders trailing-stop`, Hyperliquid's native perp trailing stop (live since 2026-09-21). The exchange tracks the mark price and fires a market order when it retraces from its best level by `--retracement-pct` or `--retracement-px`; `--activation-price` delays tracking and `--reduce-only` limits it to closing. It supports `--dex`, `--on-behalf-of`, `--dry-run` (`would_execute: "trailing_stop"`), and signs through every signer. Mainnet prompts before placing and `--testnet` does not; machine contexts need `--yes`. Spot and outcome markets are rejected with exit `2`. The JSON receipt carries `oid`, and `orders open` shows the stop as `TrailingStopMarket`.
- Added `orders modify --always-place`. It sends `a: true` on the modify action, which places the replacement even if the cancel fails and lets trigger orders be modified. Without it the replacement must be a non-trigger ALO order or a non-executable GTC order, and the action is unchanged. Modify dry-runs report `always_place`.
- `orders modify` on a trailing stop now points to `orders trailing-stop` instead of the Hyperliquid app.

Streams:

- `watch`, `subscribe`, and `watch risk` exit `130`, `143`, or `129` on SIGINT, SIGTERM, or SIGHUP, restore the terminal, and never leave a partial JSON line. A closed stdout ends them quietly with exit `0`.
- A dropped subscription connection prints `! Lost connection. Retrying automatically…` on a terminal and emits one `{"type":"warning","reason":"stream_gap"}` line after reconnecting.
- An explicit human format that falls back to JSON lines on a pipe says so on stderr.

Schema:

- `schema` lists every option the CLI accepts, including the previously missing `buy`, `sell`, `outcome-buy`, `outcome-sell`, `orders chase`, `orders bracket`, `orders tpsl`, `borrowlend supply`, and `watch risk` options. A test fails when the catalog and the CLI drift.

## 0.11.1 - 2026-05-26

Highlights:

- Aligned order builder-fee handling with Hyperliquid builder-code semantics: explicit builder fees are rejected for spot buys, spot sells remain supported, and packaged default builder fees are skipped where they cannot apply.
- Clarified spot builder-fee applicability in English, Chinese, Korean, Japanese, and internal contributor docs.

## 0.11.0 - 2026-05-17

Highlights:

- Hardened order lifecycle safety for `--on-behalf-of` across cancel, cancel-all, modify, TP/SL, TWAP, and scheduled cancel flows so acting-account selectors are carried into lookups, dry-run previews, and live `vaultAddress` submissions.
- Added prompt-gated safety for mainnet scheduled cancel-all actions, including `--yes` bypass support for intentional automation.
- Clarified signer versus acting-account selector semantics in public docs and machine-readable schemas.
- Preserved Hypersdk / Alloy 1 signer compatibility while allowing app-level Alloy 2 helpers and refreshed dependency pins for `hypersdk`, `alloy`, `rand`, `rpassword`, and `sha2`.
- Refreshed QA command matrix fixtures and bounded subscription output checks for agent-safe automation.
- Aligned release packaging and pre-release health checks for Linux, macOS, and Windows artifacts, plus self-update asset lookup for the supported Linux/macOS tarballs.

## 0.1.0 - 2026-05-15

Initial public release of `hyperliquid-cli`.

Highlights:

- Agent-first JSON, schema, projection, and bounded stream output contracts.
- Open Wallet Standard local encrypted wallet support for setup, import, signing, and account selection.
- Market data, perps, spot, orders, transfers, subaccounts, staking, vaults, borrow/lend, referrals, builder fee, and feedback command coverage.
- Safe-by-default dry-run previews, confirmation gates, structured errors, and stable exit codes.
- Release packaging for Linux, macOS, and Windows with checksum verification.
