# Hyperliquid CLI Pack

Read-only Hyperliquid monitoring for Aeon, from the team behind the
[`hyperliquid` CLI](https://github.com/hypurr-inc/hyperliquid-cli).

Each skill is a `node` helper that runs the pinned `hyperliquid` CLI release and
formats its JSON. A read-only Aeon run may not call `hyperliquid` or `bash` itself but
may run `node`, so the helper downloads the release binary once (SHA-256 verified,
cached in the temp dir) and starts it as a subprocess with an empty environment: no
key, no install step for the operator. The CLI stays the tool for execution too, with a
`--dry-run` preview and an explicit approval.

This pack is deliberately **read-only**. Aeon runs unattended on a schedule, and a
scheduled skill that could sign a trade is a foot-gun: a threshold, a stale
watchlist, or a bad `${var}` becomes a position nobody asked for.

```bash
bin/install-skill-pack hypurr-inc/hyperliquid-cli --path aeon
```

## Skills

| Skill | Schedule | `var` | What it does |
| --- | --- | --- | --- |
| `hyperliquid-account-brief` | `0 13 * * *` (disabled) | `0x…` account (required) | Default-dex equity, withdrawable and margin use, open positions across dexes with uPnL and liquidation price, open orders, 24h realized PnL and fees by token. For unified and portfolio-margin accounts it reports the spot USDC balance and margin used instead of equity and withdrawable |
| `hyperliquid-market-monitor` | `0 */4 * * *` (disabled) | `BTC,ETH move=5 funding=0.01` (default-dex perps) | Watched perps by 24h move and hourly funding. Notifies only for crossings the previous run did not already report |

```yaml
# aeon.yml
hyperliquid-account-brief: { enabled: true, schedule: "0 13 * * *", var: "0xYourAddress" } # replace the placeholder with a real account: 0x followed by 40 hex characters; the skill notifies once and stops otherwise
hyperliquid-market-monitor: { enabled: true, schedule: "0 */4 * * *", var: "BTC,ETH,SOL,HYPE move=5 funding=0.01" }
```

Both are stateless apart from the monitor reading its own previous log line, and use
no secrets.

## Behavior to know

- **Quiet is normal.** The monitor prints `HL_WATCH_QUIET` or `HL_WATCH_REPEAT` and does
  not notify when nothing new crossed a threshold. A move that persists is reported once;
  it alerts again after it clears and returns.
- **Failures are loud once.** A missing or malformed account address, or a bad `var` for
  the monitor, sends one notification and stops. An unreachable API stays silent and the
  next run retries.
- **Unknown coins are reported**, so a typo in the watchlist cannot hide.
- **First run downloads the CLI** (about 10 MB from the GitHub release `PIN` in `hl.mjs`); later runs on the same runner reuse it. The helper exits `3` (`HL_CLI_UNAVAILABLE`) if the download or its checksum fails.

## What it will not do

- Sign, submit, cancel, transfer or stake.
- Print or request a private key.
- Recommend a trade. Alerts carry the number, the threshold it crossed, and the market
  context (open interest, volume).

## Files

```
aeon/
├── skills-pack.json
├── README.md
└── skills/
    ├── hyperliquid-account-brief/
    │   ├── SKILL.md
    │   ├── hl.mjs
    │   └── x.mjs
    └── hyperliquid-market-monitor/
        ├── SKILL.md
        ├── hl.mjs
        └── x.mjs
```
