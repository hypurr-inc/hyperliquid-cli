---
name: hyperliquid-market-monitor
description: Watch chosen Hyperliquid perps and notify only when a 24h move, hourly funding rate or open interest crosses a threshold. Read-only, needs no key.
metadata:
  title: Hyperliquid Market Monitor
  mode: read-only
  category: crypto
  var: "Comma-separated coins to watch, e.g. BTC,ETH,SOL,HYPE. Empty uses BTC,ETH,SOL,HYPE."
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

> **${var}** - The perps to watch, comma-separated (`BTC,ETH`). Empty watches `BTC,ETH,SOL,HYPE`.
>
> Thresholds come from the environment, not from the variable:
> - `HL_MOVE_PCT` - absolute 24h move in percent that counts as notable. Default `5`.
> - `HL_FUNDING_PCT` - absolute hourly funding rate in percent. Default `0.01` (0.01%/h ≈ 88%/yr).
>
> Unknown coins are reported rather than silently dropped. A run where nothing
> crosses prints `HL QUIET` and does not notify.

Today is ${today}. Report only the watched Hyperliquid perps that moved, not a
full market dump.

Hyperliquid publishes one document with everything this skill needs per market:
mid price, previous-day price, hourly funding, open interest and 24h volume.
That makes the check stateless — no bookmark to keep, no history to replay, and
the first run reports the same way as the hundredth. The `hyperliquid` CLI reads
that document and this skill filters it, so an alert is a fact about the market
followed by a threshold, nothing more.

## Steps

1. **Check the CLI.** If `hyperliquid --version` fails, install it once:
   ```bash
   curl -fsSLO https://raw.githubusercontent.com/hypurr-inc/hyperliquid-cli/main/install.sh
   ```
   ```bash
   sh install.sh
   ```
   Re-run `hyperliquid --version` before continuing. If it still fails, log
   `HL_WATCH_CLI_MISSING` and exit without notifying.

2. **Run the monitor.**
   ```bash
   bash skills/hyperliquid-market-monitor/monitor.sh "${var}" > output/hyperliquid-watch.md
   ```
   - Exit `2` (`HL_CLI_MISSING`): log it and stop without notifying.
   - Exit `3` (`HL_READ_FAILED`): `meta` could not be read. Log it and stop without
     notifying; the next run retries.
   - Exit `0`: read the file.

3. **Read the result.**
   - `HL QUIET` on the first line means no watched market crossed either threshold
     and every coin resolved. Log `HL_WATCH_QUIET` and **do not notify**.
   - `## Hyperliquid watch` means at least one line follows: one per crossed market
     with price, 24h move, hourly funding, open interest in USDC and 24h volume.
     A `not listed on Hyperliquid` line means a coin in the watchlist does not exist
     on the perp venue — report it so the operator fixes the list.
   - Nothing crossed but a coin was unknown also notifies: a silent typo would hide
     the mistake forever.

4. **Log** to `memory/logs/${today}.md` under `### hyperliquid-market-monitor`:
   ```
   HL_WATCH <coins> hits=<n> unknown=<n> move>=<pct> funding>=<pct>/h
   ```
   Use `HL_WATCH_QUIET` instead when the run printed `HL QUIET`.

5. **Notify** with `./notify -f output/hyperliquid-watch.md` when the file starts with
   `## Hyperliquid watch`. Keep the thresholds in the message (the script writes
   them), so a reader knows why the line appeared. Never call a channel API directly.

## Reading the numbers

- **24h move** is `mid / prevDayPx - 1`, from Hyperliquid's own previous-day price.
  It is not a close-to-close return, so a wick shows up here.
- **funding** is the hourly rate in percent. Hyperliquid charges it hourly; `0.01%/h`
  is roughly 88% a year, which is why the default threshold is that high.
- **OI** is open interest in coin units multiplied by mid price, in USDC. OI grows
  when positions open and does not fall when they are merely marked up.
- **24h vol** is the exchange's day notional volume, in USDC. It is context for a
  move: the same percent on thin volume is a different event.
- A quiet run is a successful run. Only exit `3` means the read failed.

## Hard limits

- Read-only. This skill never signs, never submits an order and never asks for a key.
- It reports thresholds, not advice: no entry, stop, size or direction call.
- For account state use `hyperliquid-account-brief`; for execution use the
  `hyperliquid` CLI interactively with `--dry-run` and an explicit approval.
- Reads are unauthenticated HTTP calls to `api.hyperliquid.xyz`, so no secret is
  involved and `./secretcurl` is not used.
