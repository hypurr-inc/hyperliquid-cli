#!/usr/bin/env node
// Watch Hyperliquid perps and print only what newly crossed a threshold.
//
// Input: "<coins> [move=<pct>] [funding=<pct>]", from the arguments when given, else SKILL_VAR
//   coins    comma-separated perps, `dex:COIN` for HIP-3 markets. Default BTC,ETH,SOL,HYPE
//   move     absolute 24h move in percent that counts as notable. Default 5
//   funding  absolute hourly funding rate in percent. Default 0.01 (about 88%/yr)
//
// stdout, first line:  HL_WATCH_ALERT | HL_WATCH_REPEAT | HL_WATCH_QUIET
// stdout, last line:   HL_WATCH ... hits=<key,...>   (the log line; the next run reads it)
// Exit: 0 printed, 2 bad input, 3 the API could not be read.
import { existsSync, readFileSync, readdirSync } from "node:fs";

const API = "https://api.hyperliquid.xyz/info";
const DEFAULTS = { coins: ["BTC", "ETH", "SOL", "HYPE"], move: 5, funding: 0.01 };
const LOG_DIR = "memory/logs";

function fail(code, message, exit) {
  console.error(`${code}: ${message}`);
  process.exit(exit);
}

const key = (name) => {
  const [dex, coin] = name.includes(":") ? name.split(":") : ["", name];
  return dex ? `${dex.toLowerCase()}:${coin.toUpperCase()}` : coin.toUpperCase();
};

function parseVar(raw) {
  const out = { coins: [], move: DEFAULTS.move, funding: DEFAULTS.funding };
  for (const token of raw.trim().split(/\s+/).filter(Boolean)) {
    const option = token.match(/^(move|funding)=(.*)$/i);
    if (option) {
      const value = Number(option[2]);
      if (!Number.isFinite(value) || value <= 0) {
        fail("HL_WATCH_BAD_INPUT", `${option[1]} must be a positive number, got '${option[2]}'`, 2);
      }
      out[option[1].toLowerCase()] = value;
    } else if (/^[A-Za-z0-9:_.\-/,]+$/.test(token)) {
      out.coins.push(...token.split(",").filter(Boolean));
    } else {
      fail("HL_WATCH_BAD_INPUT", `unrecognised token '${token}'`, 2);
    }
  }
  const seen = new Set();
  out.coins = (out.coins.length ? out.coins : DEFAULTS.coins)
    .map((coin) => key(coin))
    .filter((coin) => !seen.has(coin) && seen.add(coin));
  return out;
}

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
  fail("HL_READ_FAILED", `${body.type}${body.dex ? ` dex=${body.dex}` : ""}: ${last}`, 3);
}

// The keys of the hits the previous run logged, so a move that persists is
// reported once and again only after it clears and returns.
function previousHits() {
  if (!existsSync(LOG_DIR)) return new Set();
  const files = readdirSync(LOG_DIR).filter((name) => /^\d{4}-\d{2}-\d{2}\.md$/.test(name)).sort();
  for (const name of files.reverse().slice(0, 2)) {
    const lines = readFileSync(`${LOG_DIR}/${name}`, "utf8").split("\n").reverse();
    for (const line of lines) {
      const match = line.match(/\bHL_WATCH\b.*?\bhits=(\S*)/);
      if (match) return new Set(match[1] === "none" ? [] : match[1].split(","));
    }
  }
  return new Set();
}

const round = (value, places) => Math.round(value * 10 ** places) / 10 ** places;
const usd = (value) => Math.round(value).toLocaleString("en-US");

const watch = parseVar(process.argv.slice(2).join(" ") || (process.env.SKILL_VAR ?? ""));
const dexes = [...new Set(watch.coins.map((coin) => (coin.includes(":") ? coin.split(":")[0] : "")))];
const lookup = new Map();
for (const dex of dexes) {
  const label = dex || "default";
  const reply = await info(dex ? { type: "metaAndAssetCtxs", dex } : { type: "metaAndAssetCtxs" });
  const [meta, contexts] = Array.isArray(reply) ? reply : [];
  if (!Array.isArray(meta?.universe) || !Array.isArray(contexts) || contexts.length < meta.universe.length) {
    fail("HL_READ_FAILED", `metaAndAssetCtxs dex=${label}: unexpected response shape`, 3);
  }
  meta.universe.forEach((asset, index) => {
    if (asset.isDelisted) return;
    if (!contexts[index] || typeof contexts[index] !== "object") {
      fail("HL_READ_FAILED", `metaAndAssetCtxs dex=${label}: no context for ${asset.name}`, 3);
    }
    lookup.set(key(asset.name), contexts[index]);
  });
}

const hits = [];
const unknown = [];
const unreadable = [];
const finite = (value) => (value === null || value === undefined || value === "" ? NaN : Number(value));
for (const coin of watch.coins) {
  const context = lookup.get(coin);
  if (!context) {
    unknown.push(coin);
    continue;
  }
  const mid = finite(context.midPx ?? context.markPx);
  const previous = finite(context.prevDayPx);
  const fundingPct = finite(context.funding) * 100;
  const openInterest = finite(context.openInterest);
  const volume = finite(context.dayNtlVlm);
  if (![mid, previous, fundingPct, openInterest, volume].every(Number.isFinite) || mid <= 0 || previous <= 0) {
    unreadable.push(coin);
    continue;
  }
  const change = ((mid - previous) / previous) * 100;
  const kinds = [];
  if (Math.abs(change) >= watch.move) kinds.push(`move${change > 0 ? "+" : "-"}`);
  if (Math.abs(fundingPct) >= watch.funding) kinds.push(`fund${fundingPct > 0 ? "+" : "-"}`);
  if (kinds.length === 0) continue;
  hits.push({
    coin,
    keys: kinds.map((kind) => `${coin}:${kind}`),
    line: `- ${coin} ${mid} · 24h ${round(change, 2)}% · funding ${round(fundingPct, 4)}%/h · OI ${usd(openInterest * mid)} USDC · 24h vol ${usd(volume)} USDC`,
  });
}

const allKeys = hits.flatMap((hit) => hit.keys);
const seenBefore = previousHits();
const fresh = hits.filter((hit) => hit.keys.some((key) => !seenBefore.has(key)));
const freshUnknown = unknown.filter((coin) => !seenBefore.has(`${coin}:unknown`));
const freshUnreadable = unreadable.filter((coin) => !seenBefore.has(`${coin}:unreadable`));
const logged = [...allKeys, ...unknown.map((coin) => `${coin}:unknown`), ...unreadable.map((coin) => `${coin}:unreadable`)];
const logLine = `HL_WATCH coins=${watch.coins.join(",")} hits=${logged.length ? logged.join(",") : "none"} new=${fresh.length} unknown=${unknown.length} unreadable=${unreadable.length} move>=${watch.move} funding>=${watch.funding}/h`;

if (fresh.length === 0 && freshUnknown.length === 0 && freshUnreadable.length === 0) {
  console.log(logged.length ? "HL_WATCH_REPEAT" : "HL_WATCH_QUIET");
  console.log(logLine);
  process.exit(0);
}

console.log("HL_WATCH_ALERT");
console.log("## Hyperliquid watch");
console.log("");
for (const hit of fresh) console.log(hit.line);
if (freshUnknown.length) console.log(`${fresh.length ? "\n" : ""}- not listed on Hyperliquid: ${freshUnknown.join(", ")}`);
if (freshUnreadable.length) console.log(`${fresh.length || freshUnknown.length ? "\n" : ""}- no usable market data for: ${freshUnreadable.join(", ")}`);
console.log("");
console.log(`Thresholds: 24h move ≥ ${watch.move}% or hourly funding ≥ ${watch.funding}%. Read-only.`);
console.log(logLine);
