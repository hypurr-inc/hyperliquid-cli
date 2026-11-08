#!/usr/bin/env bash
# Watch Hyperliquid perps and print only what crossed a threshold.
#
# Usage: monitor.sh [BTC,ETH,...]
#   Comma-separated coins. Default: BTC,ETH,SOL,HYPE
#
# Environment:
#   HL_MOVE_PCT      absolute 24h move that counts as notable (default 5)
#   HL_FUNDING_PCT   absolute hourly funding rate in percent (default 0.01)
#
# Prints "HL QUIET" when nothing crossed, otherwise a Markdown block.
# Exit codes: 0 printed, 2 the CLI is missing, 3 the API could not be read.
set -euo pipefail

if ! command -v hyperliquid >/dev/null 2>&1; then
  echo "HL_CLI_MISSING: the hyperliquid CLI is not installed" >&2
  exit 2
fi

watchlist="${1:-BTC,ETH,SOL,HYPE}"
move="${HL_MOVE_PCT:-5}"
funding="${HL_FUNDING_PCT:-0.01}"

if ! meta="$(hyperliquid --format json meta 2>/dev/null)"; then
  echo "HL_READ_FAILED: meta" >&2
  exit 3
fi

jq -nr \
  --argjson meta "$meta" \
  --arg list "$watchlist" \
  --argjson move "$move" \
  --argjson funding "$funding" '
  def round2: (.*100 | round) / 100;
  def round4: (.*10000 | round) / 10000;
  ($list | split(",") | map(gsub("^\\s+|\\s+$"; "") | ascii_upcase) | map(select(length > 0))) as $want
  | [ $meta.asset_contexts[] | select(.name as $n | $want | index($n)) ] as $rows
  | [ $want[] | select(. as $n | [ $rows[].name ] | index($n) | not) ] as $unknown
  | [ $rows[]
      | . as $r
      | (($r.midPx // $r.markPx) | tonumber) as $mid
      | ($r.prevDayPx | tonumber) as $prev
      | (if $prev > 0 then (($mid - $prev) / $prev * 100) else 0 end) as $chg
      | (($r.funding | tonumber) * 100) as $fund_pct
      | (($r.openInterest | tonumber) * $mid) as $oi_usd
      | select(($chg | fabs) >= $move or ($fund_pct | fabs) >= $funding)
      | "- \($r.name) \($mid) · 24h \($chg | round2)% · funding \($fund_pct | round4)%/h · OI \($oi_usd | round) USDC · 24h vol \(($r.dayNtlVlm | tonumber) | round) USDC"
    ] as $hits
  | if ($hits | length) == 0 and ($unknown | length) == 0 then
      "HL QUIET"
    else
      ( "## Hyperliquid watch" ),
      "",
      (if ($hits | length) == 0 then "- no watched market crossed the threshold" else $hits[] end),
      (if ($unknown | length) > 0 then "", "- not listed on Hyperliquid: \($unknown | join(", "))" else empty end),
      "",
      "Thresholds: 24h move ≥ \($move)% or hourly funding ≥ \($funding)%. Read-only."
    end
  '
