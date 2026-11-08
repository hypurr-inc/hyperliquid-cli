#!/usr/bin/env bash
# Compose one Hyperliquid account brief as Markdown on stdout.
#
# Usage: brief.sh [ADDRESS]
#   ADDRESS  a 0x account to read (public data, no key needed). Omit to read the
#            account of the configured signer.
#
# Exit codes:
#   0  brief written
#   2  the CLI is missing or the address is malformed
#   3  the Hyperliquid API could not be read
set -euo pipefail

lookup() { command -v hyperliquid >/dev/null 2>&1; }

if ! lookup; then
  echo "HL_CLI_MISSING: the hyperliquid CLI is not installed" >&2
  exit 2
fi

addr="${1:-}"
if [ -n "$addr" ]; then
  case "$addr" in
    0x[0-9a-fA-F]*) : ;;
    *) echo "HL_BAD_ADDRESS: expected 0x + 40 hex characters, got '$addr'" >&2; exit 2 ;;
  esac
fi

# `--start` needs a full RFC3339 timestamp, not a bare date.
since="$(date -u -v-24H +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '24 hours ago' +%Y-%m-%dT%H:%M:%SZ)"

read_or_fail() {
  # $1 = label, rest = command
  local label="$1"
  shift
  local body
  if ! body="$("$@" 2>/dev/null)"; then
    echo "HL_READ_FAILED: $label" >&2
    exit 3
  fi
  printf '%s' "$body"
}

if [ -n "$addr" ]; then
  portfolio="$(read_or_fail portfolio hyperliquid --format json account portfolio "$addr")"
  positions="$(read_or_fail positions hyperliquid --format json --ows-signer "$addr" positions list)"
  orders="$(read_or_fail orders hyperliquid --format json --ows-signer "$addr" orders open)"
  fills="$(read_or_fail fills hyperliquid --format json account fills "$addr" --start "$since")"
  account_line="Account: $addr"
else
  portfolio="$(read_or_fail portfolio hyperliquid --format json account portfolio)"
  positions="$(read_or_fail positions hyperliquid --format json positions list)"
  orders="$(read_or_fail orders hyperliquid --format json orders open)"
  fills="$(read_or_fail fills hyperliquid --format json account fills --start "$since")"
  account_line="Account: the configured signer"
fi

jq -nr \
  --argjson pf "$portfolio" \
  --argjson pos "$positions" \
  --argjson ord "$orders" \
  --argjson fills "$fills" \
  --arg account "$account_line" \
  --arg since "$since" '
  def usd: (tonumber | (.*100|round) / 100);
  [ $fills[].closed_pnl | tonumber ] as $pnl
  | [ $fills[].fee | tonumber ] as $fees
  | (($pnl | add) // 0) as $realized
  | (($fees | add) // 0) as $fee_total
  | "## Hyperliquid brief",
    "",
    $account,
    "",
    "**Equity** \($pf.summary.account_value) USDC · **withdrawable** \($pf.summary.withdrawable) USDC · **margin used** \($pf.summary.margin_utilization_pct)%",
    "Notional \($pf.summary.total_notional_position) USDC · positions \($pf.summary.positions_count) · spot balances \($pf.summary.spot_balances_count) · vault equities \($pf.summary.vault_equities_count)",
    "",
    "### Positions",
    (if ($pos | length) == 0 then "- none open" else
      ($pos[] | "- \(.coin) \(.side) \(.size) · entry \(.entry // "n/a") · mark \(.mark) · uPnL \(.unrealized_pnl) · liq \(.liquidation_price // "n/a") · \(.leverage) · margin \(.margin_used)")
    end),
    "",
    "### Open orders",
    (if ($ord | length) == 0 then "- none" else
      ($ord[0:10][] | "- \(.coin) \(.side) \(.size)/\(.original_size) @ \(.limit_price) · \(.order_type)\(if .tif then " " + .tif else "" end)\(if .reduce_only then " · reduce-only" else "" end) · oid \(.oid)")
    end),
    (if ($ord | length) > 10 then "- … and \(($ord | length) - 10) more (hyperliquid orders open)" else empty end),
    "",
    "### Last 24h (since \($since))",
    "- fills \($fills | length) · realized PnL \(($realized*100|round)/100) USDC · fees \(($fee_total*100|round)/100) USDC",
    "",
    "Read-only. Every number comes from the Hyperliquid API; the account is unchanged."
  '
