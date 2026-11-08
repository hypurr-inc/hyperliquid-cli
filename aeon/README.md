# Hyperliquid CLI Pack

Read-only Hyperliquid monitoring for Aeon, built on the
[`hyperliquid` CLI](https://github.com/hypurr-inc/hyperliquid-cli) — one binary
that speaks the whole Hyperliquid surface (perps, spot, HIP-3 dex markets,
outcomes, orders, positions, transfers, staking, vaults, borrow/lend).

This pack is deliberately **read-only**. Aeon runs unattended on a schedule, and
a scheduled skill that could sign a trade is a foot-gun: a threshold, a stale
watchlist, or a bad `${var}` becomes a position nobody asked for. These skills
report facts and stop. Execution stays interactive, through the same CLI, with a
`--dry-run` preview and an explicit approval.

Install the pack:

```bash
bin/install-skill-pack hypurr-inc/hyperliquid-cli --path aeon
```

## Skills

| Skill | Schedule | What it does |
| --- | --- | --- |
| `hyperliquid-account-brief` | `0 13 * * *` (disabled) | One account: equity, withdrawable, margin use, every open position with uPnL and liquidation price, open orders, 24h realized PnL and fees |
| `hyperliquid-market-monitor` | `0 * * * *` (disabled) | Watched perps: 24h move, hourly funding and open interest, notifying only when a threshold is crossed |

Both are stateless and use no secrets. The account brief reads an address from
`${var}` or the configured signer; the monitor reads `${var}` as a comma-separated
watchlist and takes thresholds from `HL_MOVE_PCT` (default `5`) and
`HL_FUNDING_PCT` (default `0.01`).

## Requirements

- The `hyperliquid` binary on `PATH`. Each skill installs it if it is missing:
  ```bash
  curl -fsSLO https://raw.githubusercontent.com/hypurr-inc/hyperliquid-cli/main/install.sh
  sh install.sh
  ```
- `jq` (present on GitHub Actions runners).
- No API key, no wallet, no secret. Public reads only.

## What it will not do

- Sign, submit, cancel, transfer or stake. Any skill that writes is out of scope
  for this pack by design.
- Print or request a private key. If a signer is configured for other runs, these
  skills never read it.
- Recommend a trade. Alerts carry the number, the threshold it crossed, and the
  market context (open interest, volume).

## Files

```
aeon/
├── skills-pack.json
├── README.md
└── skills/
    ├── hyperliquid-account-brief/
    │   ├── SKILL.md
    │   └── brief.sh
    └── hyperliquid-market-monitor/
        ├── SKILL.md
        └── monitor.sh
```

`SKILL.md` is the agent-facing contract (frontmatter, steps, exit codes, output
reading). The `.sh` helpers do the reads and the formatting so a run costs three
commands and no model arithmetic.
