---
name: hyperliquid-account-brief
description: Report one Hyperliquid account as a daily brief - equity, withdrawable balance, margin use, every open position with uPnL and liquidation price, open orders, and 24h realized PnL and fees. Read-only, never signs.
metadata:
  title: Hyperliquid Account Brief
  mode: read-only
  category: crypto
  var: "A 0x account to read (public data, no key). Leave empty to read the configured signer's account."
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

> **${var}** - A Hyperliquid account to brief.
> - `0x…` reads that account from public data. No key is used, and nothing is signed.
> - Empty reads the account of the configured signer (`hyperliquid wallet show` names it).
>
> If the address is not `0x` + 40 hex characters, log `HL_BRIEF_BAD_ADDRESS` and exit without notifying.

Today is ${today}. Read one Hyperliquid account and report what it holds now.

Hyperliquid is a perp and spot DEX where the account is the address: balances,
positions and open orders are public, so a brief needs no key and cannot move
anything. That makes it safe to run unattended. The `hyperliquid` CLI does the
reads and returns one JSON document per command; this skill composes them into a
short Markdown brief and sends it through `./notify`.

## Steps

1. **Check the CLI.** The brief shell helper fails closed when the binary is missing:
   ```bash
   hyperliquid --version
   ```
   If that errors, install it once (SHA-256 verified, Linux/macOS):
   ```bash
   curl -fsSLO https://raw.githubusercontent.com/hypurr-inc/hyperliquid-cli/main/install.sh
   ```
   ```bash
   sh install.sh
   ```
   Then re-run `hyperliquid --version` before continuing. If it still fails, log `HL_BRIEF_CLI_MISSING` and exit without notifying.

2. **Write the brief.**
   ```bash
   bash skills/hyperliquid-account-brief/brief.sh "${var}" > output/hyperliquid-brief.md
   ```
   - Exit `2` (`HL_CLI_MISSING` / `HL_BAD_ADDRESS`): log the code from stderr and stop without notifying.
   - Exit `3` (`HL_READ_FAILED`): the API was unreachable. Log it and stop without notifying; the next scheduled run retries.
   - Exit `0`: `output/hyperliquid-brief.md` holds the brief.

3. **Read the result.** The file starts with `## Hyperliquid brief`, then equity and
   withdrawable in USDC, margin used as a percent, position and balance counts,
   one line per open position (`entry`, `mark`, `uPnL`, `liq`, leverage, margin),
   up to ten open orders, and the last 24 hours of fills with realized PnL and fees.
   Fields with no value print `n/a`; an empty section prints `- none`.
   Every number is reported as returned by Hyperliquid — the brief does not model,
   forecast, or recommend a trade.

4. **Log** to `memory/logs/${today}.md` under `### hyperliquid-account-brief`:
   ```
   HL_BRIEF <account|signer> equity=<usdc> withdrawable=<usdc> positions=<n> orders=<n> pnl24h=<usdc>
   ```
   Use `HL_BRIEF_OK` with the same line on success, `HL_BRIEF_BAD_ADDRESS`,
   `HL_BRIEF_CLI_MISSING` or `HL_BRIEF_READ_FAILED` on the failure paths.

5. **Notify** with `./notify -f output/hyperliquid-brief.md`. Send every successful
   run: a daily brief is the point, and a silent day would look like a broken run.
   The brief is well under 4000 characters; order lines are capped at ten with a
   pointer to `hyperliquid orders open` for the rest. Never call a channel API
   directly.

## Reading the numbers

- **Equity** is the account value the clearinghouse reports, in USDC. **Withdrawable**
  is what could leave the account now, so the gap between them is margin in use.
- **Margin used** is the percentage of equity consumed by margin. A jump without a
  new position usually means mark price moved.
- **liq** is the liquidation price the exchange reports for that position, not a
  calculation of ours. Cross-margin positions can share margin, so `liq` on one line
  is not the whole story.
- **uPnL** is unrealized and moves with mark price; **realized PnL** in the 24h
  section comes from fills, so it is final.
- A stale timestamp or an empty account means the reads succeeded and the account is
  quiet — not that the skill failed. Only exit `3` means the API could not be read.

## Hard limits

- Read-only. This skill never signs, never submits, and never asks for a key.
- It reads one account per run. For a watchlist of markets use
  `hyperliquid-market-monitor`; for execution use the `hyperliquid` CLI interactively
  with a `--dry-run` preview and an explicit approval.
- Public reads are third-party HTTP calls to `api.hyperliquid.xyz`; they are
  unauthenticated, so no secret is involved and `./secretcurl` is not used.
