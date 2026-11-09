---
name: hyperliquid-market-monitor
description: Watch chosen Hyperliquid perps and notify only when a 24h move or hourly funding rate newly crosses a threshold. Read-only, needs no key.
metadata:
  title: Hyperliquid Market Monitor
  mode: read-only
  category: crypto
  var: "BTC,ETH,SOL,HYPE move=5 funding=0.01"
  tags:
    - crypto
    - hyperliquid
    - perps
    - monitoring
  requires: []
  capabilities:
    - read_only
    - external_api
    - sends_notifications
---

> **${var}** - What to watch and how sensitive to be: `<coins> [move=<pct>] [funding=<pct>]`.
> - `<coins>` - comma-separated perps (`BTC,ETH`); `dex:COIN` for a HIP-3 market (`xyz:XYZ100`). Empty watches `BTC,ETH,SOL,HYPE`.
> - `move=<pct>` - absolute 24h move in percent that counts as notable. Default `5`.
> - `funding=<pct>` - absolute hourly funding rate in percent. Default `0.01` (about 88%/yr).
>
> Example: `BTC,ETH,SOL move=4 funding=0.02`. Unknown coins are reported, not dropped.

Today is ${today}. Report only the watched Hyperliquid perps that newly crossed a threshold, not a full market dump.

Hyperliquid publishes one document per venue with everything this check needs per market: mid price, previous-day price, hourly funding, open interest and 24h volume. The helper reads it with plain `fetch` (no CLI, no key), filters it, and compares the result with the hits the previous run logged, so a move that persists is reported once rather than every run.

## Steps

1. **Run the helper.**
   ```bash
   node skills/hyperliquid-market-monitor/x.mjs
   ```
   `${var}` is already exported in the environment as `SKILL_VAR` on Aeon's runner and the helper reads it itself, so run that exact line: do not prefix it with a variable assignment or `env`, and do not add a pipe or redirect. On a runner that does not export `SKILL_VAR` (a local or MCP run), pass the watch string as the arguments instead: `node skills/hyperliquid-market-monitor/x.mjs BTC,ETH move=5`. Arguments win over any ambient `SKILL_VAR`. It prints to stdout. The first line is the verdict, the last line is the log line.
   - Exit `2` (`HL_WATCH_BAD_INPUT`): the variable could not be read. Send one notification naming the bad token (`./notify --severity warn`), then stop.
   - Exit `3` (`HL_READ_FAILED`): the API could not be read. Stop without notifying; the next run retries.

2. **Act on the first line.**
   - `HL_WATCH_QUIET`: nothing crossed a threshold and every coin resolved. Do not notify.
   - `HL_WATCH_REPEAT`: markets are still past a threshold but every one was already reported by the previous run. Do not notify.
   - `HL_WATCH_ALERT`: send the Markdown block (from `## Hyperliquid watch` to the `Thresholds:` line, verbatim) with:
     ```bash
     ./notify -f - <<'EOF'
     <the block>
     EOF
     ```
     Never call a channel API directly.

3. **Final output.** Read-only runs do not write `memory/logs/`; the workflow records your final output. End with the verdict and the log line exactly as printed (it starts `HL_WATCH ` and carries `hits=`; the next run reads it from the log to tell new hits from repeats), plus whether a notification was sent.

## Reading the numbers

- **24h move** is `mid / prevDayPx - 1`, from Hyperliquid's own previous-day price. It is not a close-to-close return, so a wick shows up here.
- **funding** is the hourly rate in percent. Hyperliquid charges it hourly; `0.01%/h` is roughly 88% a year, which is why the default threshold is that high.
- **OI** is open interest in coin units multiplied by mid price, in USDC.
- **24h vol** is the exchange's day notional volume, in USDC. It is context for a move: the same percent on thin volume is a different event.
- A hit key is `COIN:move+`, `COIN:move-`, `COIN:fund+` or `COIN:fund-`. A coin with missing or non-numeric market data is reported as `no usable market data` rather than skipped. A malformed API response exits `3` (`HL_READ_FAILED`). A key leaves the log line when the market drops back under the threshold, so the next crossing alerts again.

## Hard limits

- Read-only. This skill never signs, never submits an order and never asks for a key.
- It reports thresholds, not advice: no entry, stop, size or direction call.
- For one account's state use `hyperliquid-account-brief`.
- Reads are unauthenticated HTTP calls to `api.hyperliquid.xyz`, so no secret is involved and `./secretcurl` is not used.
