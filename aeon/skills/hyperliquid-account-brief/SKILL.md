---
name: hyperliquid-account-brief
description: Report one Hyperliquid account as a daily brief - equity, withdrawable balance, margin use, every open position with uPnL and liquidation price, open orders, and 24h realized PnL and fees. Read-only, never signs.
metadata:
  title: Hyperliquid Account Brief
  mode: read-only
  category: crypto
  var: ""
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

> **${var}** - The Hyperliquid account to brief: `0x` plus 40 hex characters. Required. Balances, positions and orders are public, so no key is used and nothing is signed. Aeon has no signer, so there is no account to fall back to.

Today is ${today}. Read one Hyperliquid account and report what it holds now.

Hyperliquid is a perp and spot DEX where the account is the address: balances, positions and open orders are public, so a brief needs no key and cannot move anything. The helper reads them with plain `fetch` (no CLI, no install), across the default perp dex and every HIP-3 dex, and handles unified and portfolio-margin accounts, whose collateral lives in the spot state.

## Steps

1. **Run the helper.**
   ```bash
   node skills/hyperliquid-account-brief/x.mjs
   ```
   It reads `SKILL_VAR` itself and prints to stdout. The first line is `HL_BRIEF_OK`, then the brief, then a log line starting `HL_BRIEF `.
   - Exit `2` (`HL_BRIEF_BAD_ADDRESS`): the variable is empty or not `0x` + 40 hex characters. Send one short notification (`./notify --severity warn`) saying the address in `var` is missing or malformed, then stop. Do not retry.
   - Exit `3` (`HL_BRIEF_READ_FAILED`): the API could not be read. Stop without notifying; the next run retries.

2. **Notify** the brief (from `## Hyperliquid brief` to the closing `Read-only.` line, verbatim) with:
   ```bash
   ./notify -f - <<'EOF'
   <the brief>
   EOF
   ```
   Send every successful run: a daily brief is the point, and a silent day would look like a broken run. Never call a channel API directly.

3. **Final output.** Read-only runs do not write `memory/logs/`; the workflow records your final output. End with the log line exactly as printed and whether a notification was sent.

## Reading the numbers

- **Equity** is the account value the clearinghouse reports, in USDC. **Withdrawable** is what could leave the account now, so the gap between them is margin in use.
- **Pooled accounts** (`unifiedAccount`, `portfolioMargin`) keep balances in the spot state; the brief then shows the spot USDC balance and says the perp account value is partial.
- **liq** is the liquidation price the exchange reports for that position, not a calculation of ours. Cross-margin positions can share margin, so `liq` on one line is not the whole story.
- **uPnL** is unrealized and moves with mark price; **realized PnL** in the 24h section comes from fills, so it is final.
- Positions are the fifteen largest by value, orders the first ten; the rest are counted. If the fills read hits the API's 2000-fill limit the brief says so.
- An empty account means the reads succeeded and the account is quiet, not that the skill failed. Only exit `3` means the API could not be read.

## Hard limits

- Read-only. This skill never signs, never submits, and never asks for a key.
- It reads one account per run. For a watchlist of markets use `hyperliquid-market-monitor`.
- Reads are unauthenticated HTTP calls to `api.hyperliquid.xyz`, so no secret is involved and `./secretcurl` is not used.
