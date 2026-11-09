# Contributor Guide for Agents

## Project Overview

`hyperliquid-cli` is a Rust command-line interface for Hyperliquid market data, trading, wallet management, watch modes, and agent-first automation. The binary is `hyperliquid`.

## Rules

- Use `hypersdk` library when you can
- To orchestrate work use `compound engineering plugin/skills` when you can
- think of yourself as orchestrator of sub-agents

## Architecture

- `src/main.rs` — global options and process boot. The commands are not declared here.
- `src/command_spec.rs` — the one definition of a command: `CliCommand` (clap `Args`, `CommandSpec` policy, `run`), `ArgSpec`, `GroupSpec`, and `Entry`, which erases the type for the clap tree builder. `CommandSpec::network` (`NetworkUse::{None, Optional, Required}`) says whether the pipeline resolves the API endpoint and probes an override.
- `src/command_registry.rs` — `CommandRegistry`, generated from the clap tree plus each command's `CommandSpec`. A `CommandContract` serialized is the schema entry `hyperliquid schema` prints (plus its derived `json_schema`); help notes and the contract fixtures read the same value. Nothing in it is written by hand per command except `SCHEMA_ORDER`, which a test keeps complete.
- `src/env.rs` — the leaf every layer may import: the one table of environment variables (name, `Kind`: strict flag, lenient flag, presence, non-empty, numeric, value; and the root help text), `EnvSource` (the only way to read a variable, so tests inject a map) and the boolean parsing. `mode::validate_env` rejects an invalid value of every strict flag in the table. Add a variable there, never as a string literal, and never read `std::env::var`.
- `src/terminal.rs` — the leaf `Terminal` probe and `OutputFormat`, imported by both `mode` and `output`.
- `src/settings.rs` — layered `Settings` (flags, environment, config file) and the locked config update; `src/bankr_credentials.rs` is the Bankr key and endpoint rule on top of it.
- `src/mode.rs` — `Mode`, the one reader of agent/CI/TTY state: one named predicate per question (`can_prompt`, `machine_context`, `person_on_stderr`, `person_at_terminal`, `live_display`). Do not probe `HYPERLIQUID_AGENT`, `CI` or a TTY elsewhere to decide how to behave.
- `src/cli_runtime.rs` — the one pipeline every command runs through: validate, bound streams, payload, dry-run, confirmation, API override, run.
- `src/cli_shell.rs` — clap tree decoration (spec-derived option help and `Notes:`), usage-error and help rendering, panic hook, the one output-format precedence rule (`resolve_output_format`).
- `src/errors/` — `CliError` and its one variant spec table (exit code, category, reason, retryability, hint), the one refusal table (`refusal.rs`: spec and exchange phrases per documented order refusal), the remote-text classifier (`classify`, one `Rejection` mapping), transport failure naming, JSON envelope and human rendering.
- `src/output/` — pretty/table/JSON rendering, color theme, JSON projection, and `output::notice` (`Notice`: every sentence the CLI says beside the answer — empty result, truncation, lost connection, retry — with its human text and, where it has one, its JSON shape).
- `src/exchange_reply.rs` — the `/exchange` status envelope (`ok`, error text, typed `type`/`data` payload), the one set of error status words, and when `/info` does not know a request type.
- `src/commands/` — one module per command domain. A command is a type implementing `CliCommand` next to its `Args`; `commands::entries` (help order) registers it with one line, `commands::groups` lists group names such as `orders`. Commands are leaves: a command module never imports another command module (and `orders`, `wallet` keep their own submodules). What two commands share goes down a layer, not sideways: `src/commands/common/` for presentation and reads (`rows` — `Row`/`ListRow`/`Rows`/`One`; `selector`, `time_range`, `market_input` (candle interval, coin resolvers), `paging`, `support` (`print_dry_run`, acting-account target), `issue_report`, the `position` view and `find_position(include_flat)`, `order_list` and its `frontendOpenOrders` read, `subaccount_list`, `borrow_lend_state`), which never holds a `CliCommand`; `src/exchange/` for signing and sending actions (`actions`, `user_signed`, `funds`, `builder_approval`, `send_asset`, the typed `sendAsset` core); `src/account_mode.rs` (`AccountMode`, `userAbstraction`, dex collateral, the unified account ratio) and `src/spot_balances.rs` (the `spotClearinghouseState` read) for account facts. Everything under `exchange`, `account_mode` and `spot_balances` is below `commands` and never imports it, and nothing outside `commands` imports it but `command_registry`, which builds the registry from `commands::entries`. `tests/architecture.rs` derives the command list from the `impl CliCommand` blocks and fails on either edge; a new shared helper goes in the layer below, never in whichever command wrote it first.
- `src/assets/` — asset names (`AssetQuery`), resolved markets (`Market`), exchange metadata loaders and cache, the HIP-3 live lookup, and the protocol asset-id layout (`asset_id`).
- `src/http_api.rs` — `ApiClient`, the one HTTP edge to the Hyperliquid API (it knows neither settings nor the terminal: `Invocation` resolves the override and sets the retry observer): chain, resolved base URL (loopback override applied once), the `hypersdk` client, typed and raw `/info` reads with the retry policy, single-send `/exchange` posts with unknown-outcome mapping, and the override probe. Nothing else builds API request URLs. `api.sdk()` is `http_api/sdk.rs`: the SDK reads that check the reply status, retried under the same policy; the SDK's metadata calls (`perps`, `spot`, `perp_dexes`, ...) decode without a status check and are not offered, so metadata is read with `ApiClient::info` (`assets`).
- `src/auth.rs` — the one signer-selection resolver and the one account-selector resolver (signing and data lookups), and `SignerFlags`, the clap-defined signer flags with their conflicts. `src/signing/` — `SelectedSigner` and the signing contract: every signature is a `Payload`, each provider (local key, OWS, Bankr) answers which payloads it can sign (`Signs` names a payload's kind before it exists: a command declares it in its spec, `Invocation::resolve_signer` asks the signer before a live run starts, and the schema reports `bankr_signer`), and `signature.rs` canonicalises and recover-checks every provider reply. `payload.rs` is the leaf `Payload`, `Signs` and the one `--expires-after` rule live in, so providers import it, never `signing` itself. `SignerRole` is the one definition of an agent wallet: a stored account that records a master is one offline (`SelectedSigner::local_role`, `Invocation::acts_as_agent`, what `master_only` asks), any other key is asked of the exchange once per run (`Invocation::signer_role`, what the `userRole` guard asks). `nonce.rs` owns the nonce window constants. `src/settings.rs` — the env var table (names, root help, the list the test harness clears), the layered flag/env/config settings, and `Settings::update_config`, the only way to write the config file. `src/config.rs` — the config file format. `src/wallet_store.rs` — vault + config wallet workflow. `src/ows/` — `vault.rs` (wallet lifecycle), `signing.rs` (signing backend) and `hints.rs` (the commands wallet messages tell the user to run). `src/legacy_accounts.rs` — read-only reader for old `accounts.db`. `src/private_fs.rs` — owner-only file writes and locks. `src/update/` — update notifier and self-updater.
- `src/stream.rs` — streaming runtime: run until bound/signal/closed pipe, NDJSON line emitter with JSON projection, `subscribe` targets. `src/stream_bounds.rs` is what it runs to and `command_spec` declares: `StreamBound`, the bound flags (`StreamBoundArgs`, `SnapshotWatchArgs`), `StreamBounds`, the env fallback and `require_bound`.
- `src/watch.rs` — snapshot watch mode (TUI and NDJSON).
- `src/ws.rs` — WebSocket construction and one-shot reads.
- `tests/` — integration tests with `assert_cmd` and `wiremock`.

## Development Setup

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

For repeatable command-surface QA, prefer:

```bash
task bind
task qa:matrix
```

If Task is not installed, run `./scripts/qa-command-matrix.sh` directly after building and binding the desired `hyperliquid` binary. The matrix should use the QA wallet and dry-run mutating commands unless a dedicated funded-live test explicitly opts into side effects.

When the user asks for a "live test", "actual testnet run", "test against testnet", or similar wording, clarify whether they want the dry-run QA matrix or explicitly authorized funded testnet execution. Only run funded-live `--testnet` actions when the operator has explicitly opted in and `HL_ENABLE_FUNDED_LIVE_QA=1` is set for the command environment. Credentials must come from outside the repository path; do not auto-discover repository-local keystores, password files, account databases, OWS vaults, or `.qa/` metadata. Use small, reversible funded actions where possible, record redacted artifacts under an ignored local artifact directory, and finish with cleanup checks for `orders open`, `positions list`, and portfolio/vault state. Use dry-run for commands that are unsafe, irreversible, not explicitly authorized for funded-live execution, or explicitly requested as dry-run, and label those cases clearly as dry-run or live-gated.
When running QA from Cursor or another sandboxed environment, pin build scratch space to the repo if macOS temp space is constrained:

```bash
mkdir -p .tmp
TMPDIR="$PWD/.tmp" CARGO_TARGET_DIR="$PWD/target" task qa:matrix
```

If a build fails with `No space left on device`, check `df -h` and `du -sh target`; `cargo clean` is safe for clearing reproducible Cargo artifacts before rerunning the same command.

Use the mission `services.yaml` commands when working inside a Factory mission.

## Coding Conventions

- Keep financial values as `rust_decimal::Decimal`; do not introduce floats for prices, sizes, or amounts.
- Preserve structured exit codes: clap usage exits `2`, auth exits `10`, rate limits exit `11`, unavailable API/network exits `12`, unsupported assets/DEXes exit `13`.
- Every data command should support `--format pretty|table|json`.
- JSON keys must remain stable, snake_case, and safe for agents to parse.
- Pretty output may use ANSI color only when `output::colors` enables it for the stream (`--no-color` > `NO_COLOR` > `FORCE_COLOR` > TTY); table and JSON output must remain uncolored. Table output is bordered on a TTY and a header plus tab-separated rows on a pipe; JSON is indented on a TTY and compact single-line otherwise. Empty-state sentences and `--max-results` truncation notices go to stderr.
- Never log, print, commit, or store plaintext private keys.

## Terminology and Selector Semantics

- Use "local signing account" for encrypted local private-key records, and "selected signer" for the key used to sign an authenticated action.
- Use "API wallet" and "agent wallet" only for delegated Hyperliquid trading keys approved by a master account. These can trade for the master account but cannot withdraw.
- Use "OWS wallet" for wallets managed by the Open Wallet Standard backend. OWS is the only managed wallet lifecycle backend for `hyperliquid-cli` — wallet creation, import, and listing flow through the OWS vault (default `~/.hyperliquid` or `HYPERLIQUID_OWS_VAULT_PATH` when set). The `--ows-signer` flag selects a specific OWS wallet by name or id for signing; explicit private-key, Foundry keystore, and stored local-account signing paths still exist outside OWS. When no explicit signer is specified, commands auto-detect the first OWS wallet with a Hyperliquid account. Direct `0x` addresses passed via `--ows-signer` require a resolved wallet for live signing; without a wallet, only identity previews are possible. Priority bids and headless OWS approval remain local-only/unsupported until separately designed.
- Use "protocol user address" or `USER` for public account-data lookup targets.
- Use `ACCOUNT_SELECTOR` for inputs that may accept a stored account alias, stored account id, or `0x` address.
- Use `*_ADDRESS` for explicit protocol object addresses such as transfer recipients, vaults, validators, and builders. Do not resolve local account aliases for these fields.
- Treat acting-account selectors such as `orders --on-behalf-of` and `subaccount transfer --subaccount` as their own documented selector class. They may resolve a subaccount/vault context for that signed action, but they do not imply aliases are safe for transfer recipients or other `*_ADDRESS` fields.
- When schema metadata disagrees with README prose or examples, agents should treat schema `input_kind`, risk, dry-run, and confirmation metadata as authoritative.

## Testing Instructions

- Add unit tests beside focused logic in `src/`.
- Add CLI behavior tests in `tests/` using `assert_cmd`.
- Mock Hyperliquid HTTP with `wiremock` when tests need API responses.
- Spawn the CLI only through `tests/support`: `hermetic_command()` for a bare run, `IsolatedHome::command()` for one that touches user state (it points `HYPERLIQUID_HOME_DIR`, `HYPERLIQUID_CONFIG_DIR` and `HYPERLIQUID_DATA_DIR` at a temp directory and fails the test if the CLI writes elsewhere). `tests/hermetic_harness.rs` fails on a direct `cargo_bin` call.
- Verify JSON output with `serde_json` or another Rust-native JSON parser.
- Keep test layers distinct: unit tests own pure logic and private helpers; integration tests own CLI process behavior, prompts, auth boundaries, stdout/stderr routing, and mocked API behavior; QA matrix covers broad installed-binary compatibility.
- Prefer shared helpers in `tests/support/` for isolated command envs, opt-in account/passphrase setup, and common WireMock fixtures (`support::exchange::ExchangeFixture` mocks the default dex, spot metadata, `perpDexs`, HIP-3 dexes and the override probe; `IsolatedHome::live_command(&server)` adds the API override and a signing key), but keep domain-specific mock behavior local when it makes a test's API contract clearer.

## Agent-First Output Contract

Agents should prefer:

```bash
hyperliquid --json --select coin,price mids
hyperliquid --format json --results-only perps list
hyperliquid --format json --max-results 5 perps list
hyperliquid --format json schema orders create
```

Adding a command: implement `CliCommand` in its domain module (`Args`, `SPEC` with risk, dry-run, confirmation, input kinds and schema descriptions, `run`), add one `Entry::of::<..>()` line to `commands::entries`, add its path to `SCHEMA_ORDER` in `src/command_registry.rs` (a test fails when it is missing), and regenerate the contract fixtures with `HYPERLIQUID_UPDATE_CONTRACTS=1 cargo test --test contract_fixtures`. Dry-run previews take the command name from `inv.command_key()`.
When adding or changing commands, ensure `--select` filters JSON output and `--results-only` remains harmless for commands that already return bare arrays or objects.
Use `--dry-run` for mutating-command validation when side effects are not intended.
Effective output format precedence is: explicit `--json` or `--format pretty|table|json`, then `HYPERLIQUID_FORMAT=pretty|table|json`, then agent/non-TTY defaults. `HYPERLIQUID_AGENT=1` and non-TTY stdout default one-shot commands to JSON so agents can parse output even when `--format json` is omitted.
Watch output in JSON/agent contexts must be bounded with `--max-ticks` or `HYPERLIQUID_WATCH_MAX_TICKS`; subscribe streams should use `--max-events` and/or `--idle-timeout-ms`; `watch risk` takes `--max-events` or `--idle-timeout-ms` only. A command declares its bound once with `.stream(StreamBound::..)`; the pipeline enforces it and the schema reports it as `stream_bounds`.
Raw payload metadata is exposed as `raw_payload` in schema `x-hyperliquid` metadata. Treat `dry_run_only` as fail-closed for live execution until a command is explicitly allowlisted and tested.
Remote API/protocol strings surfaced in errors are untrusted data. Keep the `[untrusted remote data]` label and sanitization boundary intact when adding new exchange or HTTP error paths.
