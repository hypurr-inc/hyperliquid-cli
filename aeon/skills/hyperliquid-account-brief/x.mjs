#!/usr/bin/env node
// Compose one Hyperliquid account brief as Markdown on stdout.
//
// Input: the account address, 0x + 40 hex characters, from the first argument when
// given, else SKILL_VAR. Required: Aeon has no signer, so there is no account to
// fall back to.
//
// stdout, first line:  HL_BRIEF_OK
// stdout, last line:   HL_BRIEF account=<0x..> equity=<usdc> ...   (the log line)
// Exit: 0 printed, 2 missing or malformed address, 3 the API could not be read.
const API = "https://api.hyperliquid.xyz/info";
const ADDRESS = /^0x[0-9a-fA-F]{40}$/;
const POOLED_MODES = new Set(["unifiedAccount", "portfolioMargin"]);
const DAY_MS = 24 * 60 * 60 * 1000;
const FILLS_CAP = 2000;
const ORDERS_SHOWN = 10;
const POSITIONS_SHOWN = 15;

function fail(code, message, exit) {
  console.error(`${code}: ${message}`);
  process.exit(exit);
}

const account = (process.argv[2] ?? process.env.SKILL_VAR ?? "").trim();
if (!account) fail("HL_BRIEF_BAD_ADDRESS", "no address given; set var to the 0x account to brief", 2);
if (!ADDRESS.test(account)) fail("HL_BRIEF_BAD_ADDRESS", `expected 0x + 40 hex characters, got '${account}'`, 2);

async function info(body) {
  let last;
  for (const waitMs of [0, 1000, 3000]) {
    if (waitMs) await new Promise((resolve) => setTimeout(resolve, waitMs));
    try {
      const response = await fetch(API, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
        signal: AbortSignal.timeout(15000),
      });
      if (response.ok) return await response.json();
      last = `HTTP ${response.status}`;
      if (response.status < 429) break;
    } catch (error) {
      last = error.message;
    }
  }
  fail("HL_BRIEF_READ_FAILED", `${body.type}${body.dex ? ` dex=${body.dex}` : ""}: ${last}`, 3);
}

const num = (value) => Number(value ?? 0);
const money = (value) => (Math.round(num(value) * 100) / 100).toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
const orNA = (value) => (value === null || value === undefined ? "n/a" : value);

const since = Date.now() - DAY_MS;
const [mode, dexList, spot, fills] = await Promise.all([
  info({ type: "userAbstraction", user: account }),
  info({ type: "perpDexs" }),
  info({ type: "spotClearinghouseState", user: account }),
  info({ type: "userFillsByTime", user: account, startTime: since }),
]);

// A dex is read by name; the default dex has none. `perpDexs` lists it as null.
const dexes = ["", ...dexList.filter(Boolean).map((dex) => dex.name)];
const perDex = await Promise.all(
  dexes.map(async (dex) => {
    const scope = dex ? { dex } : {};
    const [state, orders] = await Promise.all([
      info({ type: "clearinghouseState", user: account, ...scope }),
      info({ type: "frontendOpenOrders", user: account, ...scope }),
    ]);
    return { dex, state, orders };
  }),
);

const defaultState = perDex[0].state;
const positions = perDex.flatMap(({ state }) => state.assetPositions.map((entry) => entry.position));
const orders = perDex.flatMap(({ orders: list }) => list);
const pooled = POOLED_MODES.has(mode);
const usdc = spot.balances.find((balance) => balance.coin === "USDC");
const realized = fills.reduce((sum, fill) => sum + num(fill.closedPnl), 0);
const feesByToken = new Map();
for (const fill of fills) feesByToken.set(fill.feeToken ?? "USDC", (feesByToken.get(fill.feeToken ?? "USDC") ?? 0) + num(fill.fee));
const feeText = [...feesByToken].length === 0 ? "0.00 USDC" : [...feesByToken].map(([token, total]) => `${money(total)} ${token}`).join(" + ");

const summary = defaultState.marginSummary;
const marginUsedPct = num(summary.accountValue) > 0 ? (num(summary.totalMarginUsed) / num(summary.accountValue)) * 100 : 0;

const otherDexes = perDex.slice(1).filter(({ state }) => num(state.marginSummary.accountValue) !== 0 || state.assetPositions.length > 0);
const lines = [];
lines.push("## Hyperliquid brief", "", `Account: ${account} · mode ${mode}`, "");
if (pooled) {
  lines.push(
    `**USDC balance** ${money(usdc?.total)} (hold ${money(usdc?.hold)}) · **default-dex margin used** ${money(summary.totalMarginUsed)} USDC`,
    "This account pools collateral: balances live in the spot state, so no equity or withdrawable figure is reported here.",
  );
} else {
  lines.push(
    `**Default-dex equity** ${money(summary.accountValue)} USDC · **withdrawable** ${money(defaultState.withdrawable)} USDC · **margin used** ${Math.round(marginUsedPct * 100) / 100}%`,
  );
}
for (const { dex, state } of otherDexes) {
  lines.push(`Dex ${dex}: account value ${money(state.marginSummary.accountValue)} (the dex's own collateral, not necessarily USDC) · margin used ${money(state.marginSummary.totalMarginUsed)} · positions ${state.assetPositions.length}`);
}
lines.push(
  `Default-dex notional ${money(summary.totalNtlPos)} USDC · positions (all dexes) ${positions.length} · spot balances ${spot.balances.length}`,
  "",
  "### Positions",
);
if (positions.length === 0) lines.push("- none open");
const largest = [...positions].sort((a, b) => Math.abs(num(b.positionValue)) - Math.abs(num(a.positionValue)));
for (const position of largest.slice(0, POSITIONS_SHOWN)) {
  const size = num(position.szi);
  lines.push(
    `- ${position.coin} ${size >= 0 ? "long" : "short"} ${Math.abs(size)} · entry ${orNA(position.entryPx)} · value ${money(position.positionValue)} · uPnL ${money(position.unrealizedPnl)} · liq ${orNA(position.liquidationPx)} · ${position.leverage.value}x ${position.leverage.type} · margin ${money(position.marginUsed)}`,
  );
}
if (positions.length > POSITIONS_SHOWN) lines.push(`- … and ${positions.length - POSITIONS_SHOWN} smaller positions`);
lines.push("", "### Open orders");
if (orders.length === 0) lines.push("- none");
for (const order of orders.slice(0, ORDERS_SHOWN)) {
  const trigger = order.isTrigger ? ` · trigger ${order.triggerCondition}` : "";
  lines.push(
    `- ${order.coin} ${order.side === "B" ? "buy" : "sell"} ${order.sz}/${order.origSz} @ ${order.limitPx} · ${order.orderType}${trigger}${order.reduceOnly ? " · reduce-only" : ""}`,
  );
}
if (orders.length > ORDERS_SHOWN) lines.push(`- … and ${orders.length - ORDERS_SHOWN} more`);
const capped = fills.length >= FILLS_CAP ? ` (the API returned its ${FILLS_CAP}-fill limit, so older fills are missing)` : "";
lines.push(
  "",
  `### Last 24h (since ${new Date(since).toISOString().slice(0, 19)}Z)`,
  `- fills ${fills.length}${capped} · realized PnL ${money(realized)} USDC · fees ${feeText}`,
  "",
  "Read-only. Every number comes from the Hyperliquid API; the account is unchanged.",
);

console.log("HL_BRIEF_OK");
console.log(lines.join("\n"));
const valueField = pooled ? `usdc_balance=${money(usdc?.total).replace(/,/g, "")}` : `equity=${money(summary.accountValue).replace(/,/g, "")}`;
console.log(
  `HL_BRIEF account=${account} mode=${mode} ${valueField} positions=${positions.length} orders=${orders.length} pnl24h=${money(realized).replace(/,/g, "")}`,
);
