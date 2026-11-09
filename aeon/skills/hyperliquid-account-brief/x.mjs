#!/usr/bin/env node
import { hyperliquid } from "./hl.mjs";

const ADDRESS = /^0x[0-9a-fA-F]{40}$/;
const POOLED_MODES = new Set(["unified-account", "portfolio-margin"]);
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

const num = (value) => Number(value ?? 0);
const money = (value) => (Math.round(num(value) * 100) / 100).toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
const plain = (value) => money(value).replace(/,/g, "");
const orNA = (value) => (value === null || value === undefined ? "n/a" : value);

const since = Date.now() - DAY_MS;
let defaultDex;
let dexes;
let fills;
try {
  [defaultDex, dexes, fills] = await Promise.all([
    hyperliquid(["account", "portfolio", account]),
    hyperliquid(["perps", "dexs"]),
    hyperliquid(["account", "fills", account, "--start", new Date(since).toISOString()]),
  ]);
} catch (error) {
  fail("HL_BRIEF_READ_FAILED", error.message, 3);
}

const names = dexes.map((dex) => dex.name);
const unreadableDexes = [];
const readDex = async (command, name) => {
  try {
    return await hyperliquid([...command, ...(name ? ["--dex", name] : [])]);
  } catch (error) {
    if (!name) throw error;
    unreadableDexes.push(name);
    return undefined;
  }
};
const others = [];
const orders = [];
try {
  orders.push(await hyperliquid(["account", "orders", account]));
} catch (error) {
  fail("HL_BRIEF_READ_FAILED", error.message, 3);
}
for (const name of names) {
  const portfolio = await readDex(["account", "portfolio", account], name);
  if (portfolio) others.push({ dex: name, portfolio });
  const list = await readDex(["account", "orders", account], name);
  if (list) orders.push(list);
}

const summary = defaultDex.summary;
const mode = summary.account_mode ?? "unknown";
const pooled = POOLED_MODES.has(mode);
const usdc = defaultDex.spot_balances.find((balance) => balance.coin === "USDC");
const active = others.filter(({ portfolio }) => num(portfolio.summary.account_value) !== 0 || portfolio.positions.length > 0);
const positions = [...defaultDex.positions, ...active.flatMap(({ portfolio }) => portfolio.positions)];
const openOrders = orders.flat();
const realized = fills.reduce((sum, fill) => sum + num(fill.closed_pnl), 0);
const feesByToken = new Map();
for (const fill of fills) feesByToken.set(fill.fee_token ?? "USDC", (feesByToken.get(fill.fee_token ?? "USDC") ?? 0) + num(fill.fee));
const feeText = feesByToken.size === 0 ? "0.00 USDC" : [...feesByToken].map(([token, total]) => `${money(total)} ${token}`).join(" + ");
const marginUsedPct = num(summary.account_value) > 0 ? (num(summary.total_margin_used) / num(summary.account_value)) * 100 : 0;

const lines = ["## Hyperliquid brief", "", `Account: ${account} · mode ${mode}`, ""];
if (pooled) {
  lines.push(
    `**USDC balance** ${money(usdc?.total)} (hold ${money(usdc?.hold)}) · **default-dex margin used** ${money(summary.total_margin_used)} USDC`,
    "This account pools collateral: balances live in the spot state, so no equity or withdrawable figure is reported here.",
  );
} else {
  lines.push(`**Default-dex equity** ${money(summary.account_value)} USDC · **withdrawable** ${money(summary.withdrawable)} USDC · **margin used** ${Math.round(marginUsedPct * 100) / 100}%`);
}
for (const { dex, portfolio } of active) {
  lines.push(`Dex ${dex}: account value ${money(portfolio.summary.account_value)} (the dex's own collateral, not necessarily USDC) · margin used ${money(portfolio.summary.total_margin_used)} · positions ${portfolio.positions.length}`);
}
if (unreadableDexes.length) lines.push(`Could not read: dex ${[...new Set(unreadableDexes)].join(", ")}`);
lines.push(`Default-dex notional ${money(summary.total_notional_position)} USDC · positions (all dexes) ${positions.length} · spot balances ${defaultDex.spot_balances.length}`, "", "### Positions");
if (positions.length === 0) lines.push("- none open");
const largest = [...positions].sort((a, b) => Math.abs(num(b.position_value)) - Math.abs(num(a.position_value)));
for (const position of largest.slice(0, POSITIONS_SHOWN)) {
  lines.push(
    `- ${position.coin} ${position.side} ${Math.abs(num(position.size))} · entry ${orNA(position.entry_price)} · value ${money(position.position_value)} · uPnL ${money(position.unrealized_pnl)} · liq ${orNA(position.liquidation_price)} · ${position.leverage}`,
  );
}
if (positions.length > POSITIONS_SHOWN) lines.push(`- … and ${positions.length - POSITIONS_SHOWN} smaller positions`);
lines.push("", "### Open orders");
if (openOrders.length === 0) lines.push("- none");
for (const order of openOrders.slice(0, ORDERS_SHOWN)) {
  const trigger = order.is_trigger ? ` · trigger ${order.trigger_condition ?? ""} ${orNA(order.trigger_price)}`.trimEnd() : "";
  lines.push(`- ${order.coin} ${order.side === "Bid" ? "buy" : "sell"} ${order.size}/${order.original_size} @ ${order.limit_price} · ${order.order_type}${trigger}${order.reduce_only ? " · reduce-only" : ""}`);
}
if (openOrders.length > ORDERS_SHOWN) lines.push(`- … and ${openOrders.length - ORDERS_SHOWN} more`);
const capped = fills.length >= FILLS_CAP ? ` (the API returned its ${FILLS_CAP}-fill limit, so older fills are missing)` : "";
lines.push(
  "",
  `### Last 24h (since ${new Date(since).toISOString().slice(0, 19)}Z)`,
  `- fills ${fills.length}${capped} · realized PnL ${money(realized)} USDC · fees ${feeText}`,
  "",
  "Read-only. Every number comes from the Hyperliquid API; the account is unchanged.",
);

const valueField = pooled ? `usdc_balance=${plain(usdc?.total)}` : `equity=${plain(summary.account_value)}`;
console.log("HL_BRIEF_OK");
console.log(lines.join("\n"));
console.log(`HL_BRIEF account=${account} mode=${mode} ${valueField} positions=${positions.length} orders=${openOrders.length} pnl24h=${plain(realized)}`);
