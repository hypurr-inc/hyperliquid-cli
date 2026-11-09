# Changelog

## Unreleased

- `perps list` and `perps get` add `margin_mode` next to `isolated_margin` (unchanged): `cross_or_isolated`, `no_cross` (isolated only, margin removable), `strict_isolated` (isolated only, margin not removable) or `isolated_only` (only the deprecated `onlyIsolated` flag is set). The table view gains a `Margin Mode` column.
- `positions update-leverage` refuses cross margin (the default without `--isolated`) on an isolated-only market with exit `13`, naming the market and the exact `positions update-leverage ... --isolated` command, in `--dry-run` as live.
- `positions update-leverage` and `positions update-margin` take `--on-behalf-of` (`vaultAddress`) like the orders commands; `--dry-run` reports `acting_as`/`vault_address` and an `on_behalf_of` arg (`null` when absent).
- `positions update-margin` takes a signed `--amount`: positive adds isolated margin, negative removes it (`ntli` is sent signed), and zero exits `2`. Removal on a `strictIsolated` market exits `13` locally, before any account lookup; `noCross` and legacy `onlyIsolated` markets allow it. `--dry-run` adds `direction` (`add`|`remove`) to its args.
- `positions update-margin` checks, on a live run, that the acting account has an open isolated position on the market (dex-qualified name for HIP-3) and otherwise exits `13` with the `positions update-leverage ... --isolated` fix, instead of sending an action the exchange rejects.
- Orders `--margin-mode` (`create`, `buy`, `sell`, `scale`, `tpsl`, `twap-create`) is verified against the account's current mode for the market from `activeAssetData`, so `--margin-mode isolated` works before any position is open (it used to require one). A mismatch still exits `2` with the `positions update-leverage` fix; its message now reads `the current <coin> margin mode is <mode>` instead of naming a position.
- Orders `--margin-mode cross` on an isolated-only market exits `13` before any account lookup, in `--dry-run` as live; `--margin-mode isolated` there needs no lookup.
- The `--margin-mode` help and schema text no longer says "Defaults to cross": the default is the market's current mode, isolated-only markets always use isolated margin, and the mode is set with `positions update-leverage`.
- Order dry-run previews report `margin_mode` as the requested mode, else the account's current mode when a signer or `--on-behalf-of` is known and `activeAssetData` answers, else the market rule's default (`isolated` on isolated-only markets, `cross` otherwise); it was always `cross` without the flag.
- Transfers check the balance the account really spends: `transfer send`, `withdraw` and `perp-to-spot` compare the amount with the spot USDC available (total less hold) when the account is unified or portfolio-margin, where the perp `withdrawable` is not the balance and used to refuse valid transfers with a false `insufficient balance`; standard accounts keep the perp `withdrawable` check.
- `account portfolio` adds `summary.account_mode` and, for unified and portfolio-margin accounts, `summary.perp_state_is_partial: true`; pretty output notes that the balances live in the spot balance. The perp figures are unchanged.
- `watch risk` measures `margin_usage` by the account's mode: a unified account by the documented unified-account ratio (maintenance margin over collateral less isolated margin, worst collateral token), a portfolio-margin account by the `health` of `borrowLendUserState` (alert when it is not `healthy`). Their alert payloads carry a `basis` key; standard and dex-abstraction accounts are unchanged.
- `borrowlend user` reads `borrowLendUserState` instead of reinterpreting spot balances, whose numbers were wrong for unified accounts. Positions carry `supplied`, `borrowed`, `supply_basis` and `borrow_basis`; the output adds `health` and `health_factor`. The `hold` key is gone: it described spot holds, not borrow/lend.
- Orders on a HIP-3 dex under dex abstraction no longer get a helper `sendAsset`: the exchange moves the collateral itself in that mode, as it does for unified and portfolio-margin accounts.
- `account abstraction set` help and schema describe the protocol cautions: builder-code addresses and HIP-3/HIP-4 deployers must stay in standard mode, and unified and portfolio-margin accounts are limited to 50k actions per day.
- `outcomes list` and `outcomes get` resolve `template:<id>` outcome and question names and `template:<side>` side names (from `outcomeTemplates` and the outcome's description), so live rows read `PHX`/`SAS` and `NBA Regular Season: Phoenix Suns v San Antonio Spurs` instead of `template:{shortNameA}` and `template:sportsContestWinner`. Each row gains `quote_token` (live outcomes mix USDC and USDH), `venue`, `deployer_fee_scale`, `template`, `question`, `question_name`, `question_role` (`named` or `fallback`), `settled` and `warnings`; the existing keys are unchanged and the table view appends `Quote`, `Venue`, `Question`, `Role` and `Warnings` columns. A name that cannot be resolved (unavailable or unlisted template) keeps its raw text and says why in `warnings`.
- `outcomes list` now lists the newest outcome first (highest outcome id) instead of the lowest, so the default `--limit 100` shows current markets, not the oldest of several hundred; `--oldest-first` restores the previous order. Scripts that relied on the old order and `--limit` must pass `--oldest-first`.
- One odd `outcomeMeta` entry no longer fails the whole outcome listing (or the outcome lookups `asset decode|search` share): sides beyond the second are ignored with a per-row warning, an outcome without sides or with an unencodable id is named on stderr, and a malformed entry is reported instead of aborting the read.
- New `outcomes questions` lists questions with their `named_outcomes`, `fallback_outcome` and `settled_named_outcomes` newest first; new `outcomes settled <OUTCOME>` shows `settledOutcome` (`settle_fraction`, per-side payouts, details, question state; exit `13` when the outcome has not settled); new `outcomes templates` lists `outcomeTemplates`.
- New `outcomes split|merge|merge-question|negate` send the HIP-4 `userOutcome` L1 action (`splitOutcome`, `mergeOutcome`, `mergeQuestion`, `negateOutcome`; `--max` sends `amount: null`). They check the outcome or question against `outcomeMeta` first (exit `13` otherwise), support `--dry-run`, prompt on mainnet unless `--yes` (testnet never prompts) and can be signed by Bankr, local keys and OWS like orders. Settlement itself needs no action: settled shares convert to quote tokens automatically.

## 0.18.0 - 2026-11-09

Highlights:

- Order prices and sizes follow the exchange tick and lot rules on every placing command and dry-run; `orders modify` keeps trigger prices; `twap-create` enforces the 7-day maximum.
- HIP-3 reads (`book`, `candles`, `spread`, `funding`, `subscribe`) and the `--dex` scope on `mids`, `positions list`, `orders open` and `account portfolio`; `positions update-leverage|update-margin` accept HIP-3 markets; HIP-3 positions are found by their dex-qualified name.
- A signer that cannot sign a command is refused on `--dry-run` exactly as on a live run; every signing dry-run preview carries `signer`, `acting_as` and `vault_address`.
- Error envelopes, exit codes and `.claude/error-catalog.json` come from one table; catalog entries carry `retry` instead of `retryable`.
- Internal refactor of orders, signing, output and command modules with unchanged JSON shapes and exit codes except where listed below.

Command-line behavior:

- `feedback --open` and `report-incident --open` open the browser only for a person at a terminal (not under an agent, CI or JSON output), and `opened_in_browser` is `true` only when the opener exits successfully.
- `account ls`, `wallet show`, `wallet address`, and `wallet list` no longer print a `Notes:` block claiming they change state or "run as soon as you invoke it" in `--help`.
- Watch views (`--watch`) never draw the full-screen TUI under `HYPERLIQUID_AGENT`, `CI`, or `--non-interactive`, even on a terminal: a bounded run streams NDJSON and an unbounded one is refused with exit `13`.
- The lost-connection notice of `subscribe` streams prints only for a person who could be prompted on a stderr terminal (not for JSON, agent, CI or `--non-interactive` runs).
- An invalid `HYPERLIQUID_SUBSCRIBE_MAX_EVENTS` / `HYPERLIQUID_WATCH_MAX_TICKS` is rejected only when it is consulted (no flag bounds the stream), consistently for both variables.
- The update notice and the automatic update attempt throttle independently (`last_attempted_at` joins `last_notified_at` in the version cache; old caches still load), and a timestamp in the future never suppresses either.
- `report-incident` reads the recorded last error from the run's configured directory; the rest of the usage-error envelope is unchanged.
- HIP-3 reads: `book`, `candles`, `spread`, `funding`, and `subscribe trades|orderbook|candles` accept a DEX-qualified market (`xyz:XYZ100`) by looking the market up on its dex, so a HIP-3 book, tape and funding rate no longer fail with `asset not found`.
- `mids`, `positions list`, `orders open`, and `account portfolio` take `--dex <NAME>` to scope the read to a builder-deployed perp DEX. `account portfolio --dex` reports that dex's perp account only; spot balances and vault equities stay on the default-dex view.
- A `--dex` name is validated against `perpDexs` before the read: an unknown DEX exits `13` with `Unknown DEX` instead of returning an empty account or empty mid map, and the canonical spelling is used in every request. A blank `--dex` behaves like no flag.
- Order prices and sizes are truncated to the exchange tick and lot rules for the asset (5 significant figures and `6 - szDecimals` perp or `8 - szDecimals` spot/outcome decimals for prices, `szDecimals` for sizes) on `orders create`, `batch-create`, `scale`, `tpsl`, `bracket`, `chase`, and `trailing-stop`, so an over-precise value is placed at the representable value instead of being rejected on submission. A value that truncates to zero exits `2` naming the asset.
- `orders modify` keeps a trigger order's existing trigger price when `--trigger-price` is omitted instead of overwriting it with the order's limit price, and aborts before submitting when the replacement requires `--always-place` (a trigger replacement or an IOC TIF), matching the exchange's `a: false` pre-validation.
- `orders twap-create` rejects durations above the exchange's 7-day maximum.
- A non-interactive run that stops at the confirmation check no longer repeats a secret argument in its suggested `--dry-run`/`--yes` commands or in the saved last-error report: `wallet import <PRIVATE_KEY>`, `wallet import-mnemonic <MNEMONIC>`, `account add` and `wallet login --api-key` values are dropped, as `--private-key` and `--keystore-password` already were.
- `HYPERLIQUID_NETWORK=testnet` now skips the mainnet-only confirmation check in the run pipeline exactly like `--testnet`; before, a non-interactive run without `--yes` was rejected there although the command itself would not have prompted.
- `watch risk` declares its stream bound like `subscribe`: in JSON or agent contexts it needs `--max-events` or `--idle-timeout-ms` (checked before the run, exit `13` like `subscribe` and `mids --watch`), and `hyperliquid schema watch risk` reports `stream_bounds` and `bounded_ndjson_stream`. An explicit `--format pretty` on a pipe no longer forces the bound; an explicit `--format json` on a terminal now does.
- `feedback`, `mcp` and `report-incident` no longer carry a `Notes:` block saying they change live exchange state; they change no state.
- The watch view refreshes only on its timer, a socket message, or `r`; other keys no longer trigger a refresh.
- Bankr now refuses every user-signed action it does not cover (withdrawals, spot/asset sends, staking, builder approvals, `setAbstraction`) with exit `13` before a request is sent, on the typed-data path as well as the L1 path; only trading actions and spot-to-perp USDC moves reach Bankr, as documented.
- A signature whose recovery byte names no public key is refused as `<provider>_malformed_response` (exit `10`) instead of panicking, for Bankr and OWS alike.
- `--keystore` is no longer overridden by an ambient `HYPERLIQUID_PRIVATE_KEY` or a legacy config key, and `--private-key` together with `--keystore` is a usage error (exit `2`) instead of silently dropping the keystore.
- Account selectors fail with the vault's own error when the vault can't be read or a name is ambiguous, for data lookups (`--user`, `--account`) as for signing, instead of reporting the account as not found. An address selector now matches a pre-vault stored account in any letter case, as it does a vault wallet.
- `HYPERLIQUID_API_BASE_URL` (and its network variants) accept an IPv6 loopback host such as `http://[::1]:8080`, like the Bankr endpoint does.
- `wallet show --account <vault wallet>` describes the wallet by name like the default wallet does. `account ls` no longer prints the always-empty Master, Agent Name and Expires At columns (JSON is unchanged).
- `Config keys written` in wallet and setup receipts lists the keys the command changed, read from the config diff, and the root help lists every environment variable the CLI reads.
- A live run refuses a signer that cannot sign the command before any prompt or network lookup, with the same exit `13` error as before: Bankr and address-only `--ows-signer` for user-signed commands (`transfer send|spot-send|send-asset|withdraw`, `staking`, `builder approve`, `account abstraction set`, `api-wallet create|approve|revoke`), and an address-only `--ows-signer` for every signing command, not only orders and `prio bid`. `--dry-run` still previews with an address-only `--ows-signer` or no signer at all (a `null` signer with `signer_warning`).
- `hyperliquid schema` and the registry report `bankr_signer` (`supported`, `unsupported`, `not_required`) in each command's `x-hyperliquid` policy, so Bankr support is readable without running the command.
- Every signing command's `--dry-run` preview carries `signer`, `acting_as` and `vault_address`; `transfer spot-to-perp|perp-to-spot|send|withdraw` and `prio bid` previews gained `acting_as` and `vault_address`, and `account abstraction set` reports `acting_as` as the signer's account instead of `null`. A `null` signer always comes with `signer_warning`.
- `account abstraction set --dry-run` with no resolvable signer previews `user` as `null` instead of the placeholder address `0x…0001`.
- `--dry-run` of `account abstraction set`, `builder approve` and `api-wallet create|approve|revoke` refuses a stored API wallet with exit `13`, as the live run does.
- The dry-run `command` of `buy`, `sell`, `outcome-buy` and `outcome-sell` names the command typed instead of `orders create`.
- `asset decode` and `asset search` print a warning on stderr when asset or outcome metadata can't be loaded (the JSON answer is unchanged: formula-only rows), and `asset decode` shows an unknown `Cli Input` as an empty table cell instead of `null`.
- `report-incident` URL-encodes the issue labels like `feedback` does (`labels=bug%2Cagent-incident`); GitHub reads both spellings alike.
- The exit-15 result of `orders batch-create`, `scale`, `cancel-all` and `orders bracket` now carries the standard partial-result keys: `category` is `partial_results` (was `partial_failure`, which is now the `reason`) and the commands to run are in `next` (`[{"command": "..."}]`), replacing `next_command` and `next_commands`.
- An order, cancel, modify, TWAP, TP/SL, trailing-stop, chase or bracket status that carries a rate-limit text now exits `11`, and one that carries a credential refusal exits `10`; every other rejection keeps exit `13` and its wording.
- `orders chase`, `orders bracket` and `orders cancel-all --coin ... --dex ...` read a HIP-3 market's own dex: a resting chase quote or bracket entry is no longer treated as filled because the default dex's open orders do not list it, and `cancel-all` finds the dex's orders.
- `orders chase` resolves its market once, accepts perpetual markets only, and no longer reads the default builder configuration (it never sent a builder fee). `--dry-run` now fails like the live command when the market or its mid price is unavailable (exit `13`/`12`) instead of previewing a null `first_price`.
- `orders bracket --entry limit` refuses `--amount` with a bracket-specific message (exit `2`).
- `account orders` shows a missing TIF as a dimmed `n/a` in pretty output, like `orders open`.
- `orders history` retries the raw order list only when the response shape does not match, not when the body is malformed JSON.
- `--dry-run` is refused with the live run's exit `13` when the selected signer could not do the action: `--bankr --dry-run withdraw|transfer send|staking …|builder approve|api-wallet …` no longer preview what the live run rejects. `api-wallet create|approve|revoke --dry-run` names `master_address` when a signer resolves.
- Naming a wallet the vault does not hold is `wallet_not_found` (exit `10`) for `wallet rename|export|delete|list`, `account set-default|remove` (before: exit `1` for rename/export/list, `13` for delete/set-default/remove); other vault failures keep their own exit codes instead of exit `1`. `account set-default|remove` accept the wallet's address, as their help says, and refuse an ambiguous name instead of taking the first match.
- `account set-default` refuses a wallet with no Hyperliquid account (exit `10`, `wallet_account_missing`) before writing the config, instead of saving an unusable default and then failing.
- `wallet import-mnemonic --alias " "` takes the next free `imported` name, like `account add` with a blank alias, instead of failing on an empty name.
- A config file that can't be written exits `2` (`Configuration`) instead of `1`, and `wallet reset|logout` report a config file they can't inspect as an error instead of "nothing to reset". An unreadable or invalid `~/.bankr/config.json` is a configuration error (exit `2`) for `--bankr` instead of "authentication required", and a Bankr reply whose body can't be read is a network error.
- A command that never signs no longer reads the private key from the environment or the config file (and no longer prints the plaintext-key warning or fails on an unreadable `private_key`) when the network comes from `HYPERLIQUID_NETWORK` or `--testnet`.
- A rate limit, credential or HTTP-status failure loading asset metadata for an order is reported as such instead of being retried against the raw `meta` endpoint; only a reply the SDK cannot decode falls back. `wallet create|import|import-mnemonic` print a warning when a packaged builder or referral default is invalid and skipped.
- A bad build-time builder default is named by its variable (`HYPERLIQUID_DEFAULT_BUILDER_ADDRESS|FEE_RATE`) in the validation error.
- `orders create` (and `buy`, `sell`, `bracket` entries) fails with the "exchange returned N statuses for 1 order" error, exit `1`, when the exchange answers a single order with anything but exactly one status; before, zero statuses printed an empty table and exited `0`.
- `orders bracket --entry limit` protects the whole entry when it fills while resting: an entry that vanished from the open orders was reported as `entry_unfilled` ("nothing filled") unless a partial fill had been seen first, leaving the new position without TP/SL. `--entry-timeout` now counts from the moment the entry is resting, not from the start of the command, and `orders chase --timeout` counts from the first quote, not from the market lookups before it.
- `orders chase --dry-run` previews the first quote as the live chase sends it: the price truncated to the market's tick grid, and a `--size` below one lot is refused (exit `2`) like the live command.
- `orders cancel-all --coin` fails (exit `13`) for a coin or HIP-3 market the exchange does not list instead of listing nothing to cancel, and validates `--dex` against `perpDexs` like `orders open`.
- `orders create --grouping normal-tpsl --builder ...` (or a configured default builder) sends the builder fee with the grouped batch; before, the fee was shown in the preview and dropped from the signed action.
- The HIP-3 helper margin transfer (`orders create`, `orders bracket` entry) moves the dex's own collateral token instead of always `USDC`; `orders create --dry-run` lists it under `side_effects`, as before.
- `orders cancel-all` reports a cancel status other than `success` or an error as failed instead of cancelled.
- `positions update-leverage` and `positions update-margin` accept a HIP-3 market (`xyz:XYZ100`) and send its protocol asset id, as orders do; before they failed with `asset not found`.
- `prio bid` counts `--max` and the current gas in HyperCore wei (10^8 per HYPE, as the exchange documents) without a token-list lookup; the fallback scale was 10^18, which sent a cap 10^10 times too large when the token list did not name HYPE. A `--max` finer than 8 decimals now exits `2` also under `--dry-run`.
- `staking link initiate|finalize` pretty output highlights the Warning column, not the Network column.
- Responses that failed to decode no longer print as empty: `referral status` fails on a field of the wrong type, `vault list|get|search` on an unreadable apr/deposit/account-value, `meta` on a response without a `universe`, `outcomes list` on a side that cannot be encoded, and `watch risk` on a failed fill-history read at start. A HIP-3 dex whose metadata fails to load is reported (`asset search|decode` warn; resolving one of its markets exits `12` instead of `asset not found`), and a failed `perpDexs` listing fails the raw metadata load.
- Missing table cells read `n/a` in `referral status`, `prio status`, `vault list` and `staking validators`; `transfer send-asset --dest dex:dex:x` keeps the name `dex:x`; subaccount transfer consent previews print the network as `Testnet`/`Mainnet`; the borrow/lend unknown-token error reads `spot token "X" not found`; over-precise USDC amounts for `subaccount transfer` and `positions update-margin` use the shared amount wording.
- `orders tpsl`, `orders bracket` and the margin-mode check find an open HIP-3 position by its dex-qualified name (`xyz:XYZ100`, as the exchange reports it); before, they looked for the bare token and reported no position.
- `CI` and `FORCE_COLOR` set to `0`, `false`, `no` or `off` are read as off instead of on.
- Table and pretty output escape every Unicode format character (category Cf, such as bidi overrides and zero-width joiners) in untrusted cells, not only ASCII controls.
- `candles` and `spread` accept spot pairs (`PURR/USDC`) like `book` does; before they exited `13` with `asset_not_found`.
- `orders scale` rejects a ladder whose size is lost to lot rounding or whose prices collapse onto one tick, instead of placing fewer orders than asked.
- `.claude/error-catalog.json` (`agents/error-catalog.json` links to it) is generated from the same table as the error envelope; entries carry `retry` instead of `retryable`.

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
