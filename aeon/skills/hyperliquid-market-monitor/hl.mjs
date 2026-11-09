import { execFile, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

export const PIN = "v0.19.0";
const REPO = "hypurr-inc/hyperliquid-cli";
const ASSETS = {
  "linux-x64": "hyperliquid-linux-x86_64.tar.gz",
  "linux-arm64": "hyperliquid-linux-aarch64.tar.gz",
  "darwin-x64": "hyperliquid-macos-x86_64.tar.gz",
  "darwin-arm64": "hyperliquid-macos-aarch64.tar.gz",
};

function unavailable(message) {
  console.error(`HL_CLI_UNAVAILABLE: ${message}`);
  process.exit(3);
}

async function download(url) {
  const response = await fetch(url, { redirect: "follow", signal: AbortSignal.timeout(60000) });
  if (!response.ok) unavailable(`${url} answered HTTP ${response.status}`);
  return Buffer.from(await response.arrayBuffer());
}

const root = join(tmpdir(), `hyperliquid-cli-${PIN}`);
const binary = join(root, "hyperliquid");

async function ensureCli() {
  if (existsSync(binary)) return;
  const asset = ASSETS[`${process.platform}-${process.arch}`];
  if (!asset) unavailable(`no ${PIN} release binary for ${process.platform}-${process.arch}`);
  const base = `https://github.com/${REPO}/releases/download/${PIN}/${asset}`;
  const [archive, checksum] = await Promise.all([download(base), download(`${base}.sha256`)]);
  const expected = checksum.toString("utf8").trim().split(/\s+/)[0];
  const actual = createHash("sha256").update(archive).digest("hex");
  if (actual !== expected) unavailable(`${asset} failed its SHA-256 check`);
  mkdirSync(root, { recursive: true });
  const tarball = join(root, asset);
  writeFileSync(tarball, archive);
  const unpacked = spawnSync("tar", ["-xzf", tarball, "-C", root], { encoding: "utf8" });
  if (unpacked.status !== 0) unavailable(`could not unpack ${asset}: ${unpacked.stderr}`);
  chmodSync(binary, 0o755);
}

const env = {
  PATH: process.env.PATH ?? "",
  HYPERLIQUID_AGENT: "1",
  HYPERLIQUID_NO_UPDATE_CHECK: "1",
  NO_UPDATE_NOTIFIER: "1",
  HYPERLIQUID_HOME_DIR: join(root, "home"),
  HYPERLIQUID_CONFIG_DIR: join(root, "home", "config"),
  HYPERLIQUID_DATA_DIR: join(root, "home", "data"),
};

const RETRY_WAITS_MS = [2000, 5000, 10000];

async function once(args) {
  const run = await new Promise((resolve) => {
    execFile(binary, ["--format", "json", ...args], { env, maxBuffer: 64 * 1024 * 1024 }, (error, stdout, stderr) => resolve({ error, stdout, stderr }));
  });
  if (run.error && run.error.code === "ENOENT") unavailable(run.error.message);
  let body;
  try {
    body = JSON.parse(run.stdout);
  } catch {
    body = undefined;
  }
  if (run.error || body === undefined) {
    const reason = body?.error ?? (run.stderr.trim() || run.error?.message);
    const error = new Error(`hyperliquid ${args.join(" ")}: ${reason}`);
    error.retryable = /Too many requests|invalid type: null/.test(String(reason));
    throw error;
  }
  return body;
}

export async function hyperliquid(args) {
  await ensureCli();
  for (const wait of RETRY_WAITS_MS) {
    try {
      return await once(args);
    } catch (error) {
      if (!error.retryable) throw error;
      await new Promise((resolve) => setTimeout(resolve, wait));
    }
  }
  return once(args);
}
