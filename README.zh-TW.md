# Hyperliquid CLI

[![Crates.io](https://img.shields.io/badge/crates.io-v0.18.0-orange.svg)](https://crates.io/crates/hyperliquid-cli)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.93%2B-blue.svg)](https://www.rust-lang.org)
[![Built on hypersdk](https://img.shields.io/badge/built%20on-hypersdk-blueviolet.svg)](https://github.com/infinitefield/hypersdk)

Languages: [English](README.md) | [繁體中文](README.zh-TW.md) | [简体中文](README.zh-CN.md) | [日本語](README.ja-JP.md) | [한국어](README.ko-KR.md)

**為你的 AI 代理提供一個用於交易 Hyperliquid 的 CLI 和錢包。**

`hyperliquid` 是一個單一二進制文件，可為你的個人代理——OpenClaw、Hermes、PicoClaw、Claude、Codex，或任何能夠調用 shell 的 LLM——提供生產級命令行和加密錢包，用於 [Hyperliquid](https://app.hyperliquid.xyz)。市場、訂單、轉賬、質押、金庫、借貸、builder 費用、WebSocket 流：每個操作都以乾淨的 JSON 命令呈現，代理可以讀取、推理並執行。

把它放進你的代理工具箱，它就能檢查價格、下單、管理倉位並流式讀取訂單簿——全部通過一個二進制文件完成，並內置 dry-run、schema 和安全閘門。

## 為什麼選擇 hyperliquid-cli

- **代理優先。** 為代理循環而構建。每個命令都支持通過 `--json` 或 `--format json` 輸出 JSON、字段投影（`--select`）、結果上限（`--max-results`）以及機器可讀的 `schema` 輸出。`HYPERLIQUID_AGENT=1`（或非 TTY stdout）會自動默認輸出 JSON。你的代理讀取穩定的 snake_case 鍵、結構化錯誤對象和定義明確的退出碼——無需抓取文本，無需猜測。
- **給代理使用的錢包。** 創建一個可以交易但永遠不能提現的 API wallet（也稱為 agent wallet）。把它交給 OpenClaw、Hermes、Claude 或任何自動化系統，它就能在受限權限內運行。OWS wallet secrets 保存在加密 vault 中；由 CLI 生成的 API wallet private key 會且只會打印一次，方便你安全保存。
- **一個工具，覆蓋廣泛協議。** 市場、perps、spot、HIP-3 DEXes、訂單、轉賬、子賬戶、金庫、質押、借貸、builder 費用、推薦、賬戶抽象以及 WebSocket 訂閱——全部在一個二進制文件後面。
- **默認安全。** 被 schema 標記為 prompt-gated 的 live mainnet 可變更操作需要確認。`--dry-run` 會在受支持的副作用發生前預覽。testnet 只需一個標誌即可使用。API wallets 按協議設計無法提現。
- **十進制正確。** 每個價格、數量和金額都使用 `rust_decimal`。沒有浮點數，也沒有意外舍入。
- **單個二進制文件。** 使用 Rust 構建，並基於 [`hypersdk`](https://github.com/infinitefield/hypersdk)。數秒安裝，可放入容器，隨處運行。

## 安裝

```bash
curl -fsSLO https://raw.githubusercontent.com/hypurr-inc/hyperliquid-cli/main/install.sh
sh install.sh
hyperliquid --version
```

安裝器會在把二進制文件複製到 `~/.local/bin` 之前驗證 SHA-256 校驗和。默認安裝最新 release；可使用 `HYPERLIQUID_CLI_REPO=OWNER/REPO`、`HYPERLIQUID_CLI_VERSION=v0.18.0` 和 `BIN_DIR=/path/to/bin` 覆蓋倉庫、固定版本或安裝目錄。

從源碼安裝：

```bash
cargo install --path . --bin hyperliquid
```

需要 Rust 1.93+。

## 快速開始

```bash
# Read the market
hyperliquid status
hyperliquid mids
hyperliquid book BTC

# Read it as JSON, projected, capped
hyperliquid --format json --select coin,price --max-results 10 mids

# Configure a signer
hyperliquid setup

# Plan an order without sending it
hyperliquid --dry-run orders create --coin BTC --side buy --price 50000 --size 0.001

# Send it on testnet
hyperliquid --testnet orders create --coin BTC --side buy --price 50000 --size 0.001

# Ask the CLI to describe its own contract
hyperliquid --format json schema orders create
```

## 錢包設置

`hyperliquid` 使用 **Open Wallet Standard (OWS)** 作為其唯一錢包後端。錢包保存在磁盤上的加密 vault 中（默認 `~/.hyperliquid`，可通過 `HYPERLIQUID_OWS_VAULT_PATH` 覆蓋）。通過隱藏提示交互式輸入的秘密不會回顯、記錄到日誌或打印；顯式的 `wallet export` 和生成 API wallet 的流程是有意例外，可能一次性顯示 secret。

### 引導式設置

新操作者的最快路徑：

```bash
hyperliquid setup
```

嚮導會引導你創建或導入錢包、選擇默認網絡、持久化任何打包的 builder fee / referral 默認值，並可選擇批准默認 builder fee 上限。對於無人值守環境，可以非交互式接受所有默認值：

```bash
hyperliquid setup -y
```

### 直接創建或導入錢包

```bash
hyperliquid wallet create               # generate a new wallet
hyperliquid wallet import               # paste a private key (hidden prompt)
hyperliquid wallet import-mnemonic      # paste a BIP-39 mnemonic (hidden prompt)
```

新創建或導入的錢包會成為默認簽名者。

### 管理多個錢包

```bash
hyperliquid wallet list                 # all wallets in the OWS vault
hyperliquid wallet show                 # current default
hyperliquid wallet address              # just the address
hyperliquid wallet rename <SELECTOR> <NEW_NAME>
hyperliquid wallet export <SELECTOR>    # reveal secret (with confirmation)
hyperliquid wallet delete <SELECTOR>
```

在不更改默認值的情況下為單個命令選擇特定錢包：

```bash
hyperliquid --account alice orders open
hyperliquid --ows-signer 0xabc... positions list
```

### API / agent wallets

API wallets（也稱為 agent wallets）是由主賬戶批准的委託 Hyperliquid 交易密鑰。它們可以交易但不能提現——非常適合把受限簽名者交給自動化系統或 AI 代理：

```bash
hyperliquid api-wallet create --name trading-agent
hyperliquid api-wallet list <MASTER_ADDRESS>
hyperliquid api-wallet revoke --name trading-agent
```

### 其他簽名者來源

如果你不想為單個命令使用 OWS vault，請顯式傳入簽名者：

```bash
hyperliquid --keystore ~/.foundry/keystores/my-wallet ...
hyperliquid --private-key 0x... ...     # avoid in shared shells / history
```

或設置環境變量：

```bash
export HYPERLIQUID_PRIVATE_KEY=0x...
export OWS_PASSPHRASE=...               # unlock encrypted OWS wallet
```

### 安全規則

- **絕不要** 提交私鑰、助記詞、keystore 文件、OWS secrets 或配置數據庫。
- 在共享環境中，優先使用 OWS wallets 或 keystores，而不是原始 `--private-key` 標誌。
- 將交易委託給腳本或代理時使用 API wallets——它們無法提取資金。
- 每當你想在上線前演練一個流程時，`--testnet` 只需一個標誌即可使用。

## 輸出格式

每個數據命令都公開相同的自動化界面：

| 標誌 | 用途 |
| --- | --- |
| `--format pretty\|table\|json` | 面向人類或機器的輸出。 |
| `--select <FIELDS>` | 將 JSON 投影到逗號分隔的字段。 |
| `--results-only` | 去除 envelope，只返回數據。 |
| `--max-results <N>` | 在客戶端限制頂層 list/map 大小。 |
| `--dry-run` | 驗證並預覽受支持的可變更操作。 |
| `--payload-json` / `--payload-file` | 將原始 JSON 輸入到 dry-run。 |

設置 `HYPERLIQUID_AGENT=1`（或在非 TTY 中運行），一次性命令會自動默認輸出 JSON。錯誤是穩定對象：

```json
{"error": "Authentication required. Run `hyperliquid setup` to configure your wallet."}
```

每個可變更命令都附帶一個代理可在行動前讀取的 `schema` 描述——包括風險等級、確認要求和 dry-run 支持。

請參閱 [`SKILL.md`](SKILL.md) 獲取代理操作指南。

## 術語和地址選擇器

| 領域 | 示例 |
| --- | --- |
| OWS wallet/account record | 由 `account add`、`account ls`、`account set-default` 及相關命令管理的 OWS wallet record。 |
| Selected signer | 用於簽署認證操作的密鑰，來自標誌、環境/配置、全局 `--account` 或 OWS selector。 |
| Protocol user address | 用於 fills、portfolio、fees 或 order status 等 info queries 的公開 Hyperliquid 用戶地址。 |
| Master account | 可以批准 API wallets 並擁有 subaccounts 的協議所有者賬戶。 |
| API wallet / agent wallet | 由 master account 批准的委託 Hyperliquid 交易密鑰。它可以為 master account 交易，但不能提現。 |
| OWS signer | 使用 `--ows-signer` 選擇的 Open Wallet Standard signer source。 |
| Subaccount | 由 master account 控制的協議 subaccount。 |
| Protocol address | 用於 recipient、vault、validator、builder 或類似對象的字面鏈上/協議地址。 |

類似地址的命令輸入分為三類安全等級：

| 類別 | 接受的值 | 用於 |
| --- | --- | --- |
| `ACCOUNT_SELECTOR` | Stored account alias、stored account id 或 `0x` address | 使用 `--account` 選擇簽名者，或管理 OWS wallet records。 |
| `USER` | `0x` user address，或有文檔說明的安全 stored-account selector | 公共查詢，例如 `account portfolio`、`orders status --user` 或 fee queries。 |
| `*_ADDRESS` | 僅顯式 `0x` protocol address | Transfer recipients、vaults、validators、builders 和其他 protocol objects。Local aliases 不會替換到這些字段。 |

對於代理，當 `hyperliquid --format json schema ...` 工具 schema 與示例或說明性文字衝突時，它們是輸入語義的權威來源。

CLI 接受的規範頂層別名：

- `api-wallets` -> `api-wallet`
- `subaccounts` -> `subaccount`
- `transfers` -> `transfer`
- `vaults` -> `vault`

## 命令參考

### 全局選項

| 選項 | 描述 |
| --- | --- |
| `-f, --format pretty\|table\|json` | 輸出格式。TTY 中有效默認值為 `pretty`，非 TTY stdout 或 `HYPERLIQUID_AGENT=1` 時為 JSON；顯式 `--format` 優先於 `HYPERLIQUID_FORMAT`。 |
| `--private-key <PRIVATE_KEY>` | 使用原始私鑰簽名。覆蓋環境和配置。 |
| `--keystore <PATH>` | 使用 Foundry 兼容 keystore 文件簽名。 |
| `--keystore-password <PASSWORD>` | Keystore 密碼。對人類用戶建議使用更安全的 secret 來源。 |
| `--account <SELECTOR>` | 用作簽名者的已存儲 wallet alias、id 或 address。與其他簽名者標誌衝突。 |
| `--ows-signer <SELECTOR>` | OWS wallet selector（name 或 id）。接受 `0x` addresses 用於身份/dry-run 管線。別名：`--wallet`。與本地簽名者標誌衝突。 |
| `--testnet` | 將 API 調用和已簽名操作路由到 Hyperliquid testnet。 |
| `--select <FIELDS>` | 將 JSON 輸出投影到逗號分隔的字段。 |
| `--results-only` | 去除常見 JSON envelopes，僅返回數據。 |
| `--max-results <N>` | 在客戶端限制頂層 list/map 結果。 |
| `--dry-run` | 驗證並預覽可變更命令，而不產生副作用。 |
| `--payload-json <JSON>` / `--payload-file <PATH\|->` | 為可變更 dry-runs 提供原始 JSON payload context。 |
| `--no-update-check` | 禁用本次調用的 release 更新檢查。 |
| `-h, --help` / `-V, --version` | 打印 help 或 version 信息。 |

### 市場數據

| 命令 | 描述 |
| --- | --- |
| `perps list [--dex <DEX>]` | 列出 perpetual markets。 |
| `perps get <COIN> [--dex <DEX>]` | 顯示一個 perpetual market。 |
| `spot list` | 列出 spot markets。 |
| `spot get <PAIR>` | 顯示一個 spot pair，例如 `PURR/USDC`。 |
| `outcomes list [--limit <N>]` | 從 `outcomeMeta` 列出活躍的 outcome market sides。 |
| `outcomes get #<ENCODING>` / `outcomes get +<ENCODING>` | 顯示 outcome side metadata 和派生的 asset ID。 |
| `book <COIN> [-w] [--max-ticks <TICKS>]` | 顯示 L2 order book snapshot 或 watch updates。 |
| `mids [-w] [--max-ticks <TICKS>]` | 顯示所有 mid prices。 |
| `candles <COIN> [--interval <INTERVAL>] [--limit <N>] [-w] [--max-ticks <TICKS>]` | 顯示 candle history。 |
| `spread <COIN>` | 顯示 bid、ask 和 spread。 |
| `funding <COIN>` | 顯示當前和預測 funding。 |
| `meta` | 顯示原始 exchange metadata。 |
| `status` | 顯示 API health 和 rate-limit context。 |

### 賬戶、錢包和設置

| 命令 | 描述 |
| --- | --- |
| `setup [-y] [--approve-builder\|--no-approve-builder]` | 運行引導式首次設置嚮導。 |
| `wallet create` | 創建並存儲新錢包。 |
| `wallet import [PRIVATE_KEY]` | 導入錢包。省略 key 可使用隱藏提示。 |
| `wallet show` | 顯示當前 wallet metadata。 |
| `wallet address` | 僅打印已配置 wallet address。 |
| `wallet import-mnemonic [MNEMONIC] [--alias <ALIAS>]` | 從 BIP-39 mnemonic phrase 導入錢包。 |
| `wallet list` | 列出 OWS vault 中的所有 wallets。 |
| `wallet rename <SELECTOR> <NEW_NAME>` | 重命名錢包。 |
| `wallet delete <SELECTOR>` | 刪除錢包。除非使用 `-y`，否則會提示。 |
| `wallet export <SELECTOR> [-y]` | 導出 wallet secret（mnemonic 或 private key）。 |
| `wallet reset [-y]` | 確認後移除 wallet configuration。 |
| `account fees [ADDRESS_OR_WALLET]` | 查詢 fee schedule 和 volume context。 |
| `account fills [ADDRESS_OR_WALLET] [--start <TIME>] [--end <TIME>] [--aggregate-by-time]` | 查詢公共 fill history，可選擇按時間。 |
| `account ledger [ADDRESS_OR_WALLET] --start <TIME> [--end <TIME>]` | 查詢 deposits、withdrawals、transfers 和其他非 funding ledger updates。 |
| `account funding [ADDRESS_OR_WALLET] --start <TIME> [--end <TIME>]` | 查詢用戶 funding payment history。 |
| `account orders [ADDRESS_OR_WALLET]` | 查詢公共 open orders。 |
| `account portfolio [ADDRESS_OR_WALLET]` | 查詢公共 portfolio summary。 |
| `account portfolio-history [ADDRESS_OR_WALLET]` | 查詢 frontend portfolio graph/history data。 |
| `account rate-limit [ADDRESS_OR_WALLET]` | 查詢用戶 rate-limit context。 |
| `account subaccounts [ADDRESS_OR_WALLET]` | 查詢公共 subaccounts。 |
| `account twap-history [ADDRESS_OR_WALLET]` | 查詢用戶 TWAP order history。 |
| `account twap-fills [ADDRESS_OR_WALLET] [--start <TIME>] [--end <TIME>] [--aggregate-by-time]` | 查詢用戶 TWAP slice fills。 |
| `account abstraction [ADDRESS]` | 讀取某地址的 account abstraction mode；若省略 `ADDRESS`，則讀取所選賬戶。 |
| `account abstraction set --mode disabled\|unified-account\|portfolio-margin` | 為已配置簽名者設置 account abstraction；除非使用 `-y`，否則會提示。 |
| `subaccount list [ADDRESS_OR_WALLET]` | 查詢 master address 的公共 subaccounts。 |
| `subaccount create --name <NAME>` | 創建由 master account 簽名的 subaccount。 |
| `account add [PRIVATE_KEY] [--alias <ALIAS>] [--type <TYPE>] [--default]` / `account ls` / `account set-default [SELECTOR]` / `account remove [SELECTOR] [-y]` | 管理已存儲 wallets。 |
| `api-wallet create [--name <NAME>] [--expires-in <DURATION>] [--agent-address <ADDRESS>] [--generate]` | 生成並批准 Hyperliquid API/agent wallet。 |
| `api-wallet approve [--name <NAME>] [--expires-in <DURATION>] (--agent-address <ADDRESS>\|--generate)` | 批准現有或生成的 agent wallet address。 |
| `api-wallet list [ACCOUNT]` | 列出由 master account 批准的 API wallets。 |
| `api-wallet revoke --name <NAME>` | 用一個短期一次性 agent 替換命名 API wallet。 |

API wallets 可以為批准它們的 master account 簽署交易操作，但不能提現。使用 master 或 subaccount address 進行 info queries。當 `api-wallet create` 生成本地 agent keypair 時，它會在為該地址提交 `approveAgent` 之前打印一次 private key；請安全保存該 key，因為 CLI 不會在之後自動恢復它。

### 交易和轉賬

| 命令 | 描述 |
| --- | --- |
| `orders open [-w] [--max-ticks <TICKS>]` | 列出 open orders。 |
| `orders history` | 列出 order history。 |
| `orders status --user <ADDRESS> (--oid <OID>\|--cloid <CLOID>)` | 查詢公共 order status。 |
| `orders create --coin <COIN> --side buy\|sell [--type limit\|market\|stop-loss\|take-profit\|stop-limit\|take-limit] [--price <PX>] [--trigger-price <PX>] [--size <SIZE>\|--amount <USDC>] [--dex <DEX>] [--reduce-only] [--on-behalf-of <ACCOUNT_SELECTOR>] [--cloid <CLOID>] [-y]` | 創建 limit、market、stop-loss、take-profit、stop-limit 或 take-limit orders。`--on-behalf-of` 是用作 `vaultAddress` 的 acting-account selector。 |
| `orders scale --coin <COIN> --side buy\|sell --start-price <PX> --end-price <PX> --total-size <SIZE> --orders <N>` | 創建一組等間距的 limit orders。 |
| `orders batch-create --orders-file <PATH>` | 從 JSON 創建一批 limit orders。 |
| `orders create --coin <COIN> --side buy\|sell [--take-profit <PX>] [--stop-loss <PX>] [--grouping normal-tpsl] ...` | 創建帶固定大小 TP/SL 子訂單的父訂單。 |
| `orders tpsl --coin <COIN> (--take-profit <PX>\|--stop-loss <PX>) [--grouping position-tpsl] [--side buy\|sell] [--size <SIZE>] [--margin-mode cross\|isolated]` | 創建附加到當前 position 的 TP/SL orders。 |
| `orders cancel (ORDER_ID\|--cloid <CLOID>)` | 按 order ID 或 client order ID 取消。 |
| `orders cancel-all [--coin <COIN>] [--dex <DEX>] [-y]` | 取消所有 open orders，可按 coin 或 DEX 過濾。 |
| `orders modify (ORDER_ID\|--cloid <CLOID>) [--price <PRICE>] [--trigger-price <PRICE>] [--size <SIZE>]` | 修改現有訂單。 |
| `orders twap-create --coin <COIN> --side buy\|sell --size <SIZE> --duration <SECONDS> [--dex <DEX>] [--margin-mode cross\|isolated] [-y]` | 創建 TWAP order。 |
| `orders twap-cancel <TWAP_ID> --coin <COIN> [--dex <DEX>]` | 取消 TWAP order。 |
| `orders schedule-cancel (--in <DURATION>\|--clear)` | 配置 dead man's switch。 |
| `positions list [-w] [--max-ticks <TICKS>]` | 列出 open positions。 |
| `positions update-leverage --coin <COIN> --leverage <N> [--isolated]` | 更新 leverage。 |
| `positions update-margin --coin <COIN> --amount <AMOUNT>` | 增加或移除 isolated margin。 |
| `transfer spot-to-perp --amount <USDC> [-y]` | 將 USDC 從 spot 移到 perp。 |
| `transfer perp-to-spot --amount <USDC> [-y]` | 將 USDC 從 perp 移到 spot。 |
| `transfer send --to <ADDRESS> --amount <USDC> [-y]` | 向另一個地址發送 USDC。 |
| `transfer spot-send --to <ADDRESS> --token <TOKEN> --amount <AMOUNT> [-y]` | 向另一個地址發送 spot token。 |
| `transfer send-asset --to <ADDRESS> --source perp\|spot\|dex:<DEX> --dest perp\|spot\|dex:<DEX> --token <TOKEN> --amount <AMOUNT> [--from-subaccount <ADDRESS>] [-y]` | 在賬戶、spot、perp 或 DEX contexts 之間發送資產。 |
| `transfer withdraw --to <ADDRESS> --amount <USDC> [-y]` | 將 USDC 提現到 Arbitrum。 |
| `subaccount transfer --subaccount <ACCOUNT_SELECTOR> --amount <USDC> --direction deposit\|withdraw [-y]` | 將 USDC 移入或移出 subaccount。subaccount 字段是 acting-account selector，不是通用 transfer recipient。 |
| `subaccount spot-transfer --subaccount <ACCOUNT_SELECTOR> --token <TOKEN> --amount <AMOUNT> --direction deposit\|withdraw [-y]` | 將 spot token 移入或移出 subaccount。subaccount 字段是 acting-account selector，不是通用 transfer recipient。 |

`api-wallets` 可作為 `api-wallet` 的別名。
`subaccounts` 可作為 `subaccount` 的別名。
`transfers` 可作為 `transfer` 的別名。

有時間範圍的賬戶歷史命令接受 RFC3339 時間戳和 epoch milliseconds。CLI 會向 Hyperliquid 發送毫秒，例如：

```bash
hyperliquid --format json account fills 0x0000000000000000000000000000000000000000 --start 2026-05-01T00:00:00Z --end 2026-05-02T00:00:00Z --aggregate-by-time
hyperliquid --format json --select time,delta account ledger 0x0000000000000000000000000000000000000000 --start 1777593600000 --end 1777680000000
hyperliquid --format json account portfolio-history 0x0000000000000000000000000000000000000000
hyperliquid --format json orders status --user 0x0000000000000000000000000000000000000000 --oid 123
hyperliquid --format json --dry-run orders scale --coin BTC --side buy --start-price 80000 --end-price 90000 --total-size 0.005 --orders 5
hyperliquid --format json --dry-run orders batch-create --orders-file tests/fixtures/orders_batch_create.json
hyperliquid --format json --dry-run account abstraction set --mode disabled
hyperliquid --format json outcomes get '#10'
```

Hyperliquid 文檔中的歷史端點會返回有界窗口，通常根據端點限制在約 500 或 2000 行。導出時，請查詢相鄰且不重疊的窗口，並保持 `--format json`；只使用全局 `--max-results` 來裁剪本地 CLI 輸出以便檢查。

Outcome market 表示法（`#N` spot coin 和 `+N` token name）可通過 `outcomes list` 和 `outcomes get` 發現。`orders create --coin '#N' --dry-run` 會發出帶有編碼 asset id 的穩定 outcome order preview；配置簽名者後，live limit-order submission 支持已驗證的 outcome notation。在下 live outcome order 之前，先使用 `--dry-run` 檢查編碼 asset id 和 signed-action preview。

### 高級命令

| 命令 | 描述 |
| --- | --- |
| `staking summary [ADDRESS]` / `staking validators` / `staking rewards [ADDRESS]` / `staking history [ADDRESS]` | 讀取 staking state 和 history。 |
| `staking delegate --validator <ADDRESS> --amount <AMOUNT>` / `staking undelegate --validator <ADDRESS> --amount <AMOUNT>` / `staking deposit --amount <AMOUNT>` / `staking withdraw --amount <AMOUNT>` / `staking claim-rewards` | 提交 staking actions。 |
| `staking link initiate --user <ADDRESS>` / `staking link finalize --user <ADDRESS>` | 關聯 trading 和 staking accounts 以進行 fee discount attribution。Dry-runs 包含永久性/控制權警告；live commands 需要確認或 `--yes`。 |
| `vault list [--kind protocol\|user\|normal\|child\|parent] [--user <ADDRESS>] [--limit <N>] [--sort tvl\|apr\|age\|name]` / `vault search <QUERY> [--user <ADDRESS>] [--limit <N>] [--sort tvl\|apr\|age\|name]` / `vault get <ADDRESS>` / `vault positions <ADDRESS>` | 發現並查詢 vault state。當 API 返回時，`--user` 包含用戶存款 context。 |
| `vault deposit --vault <ADDRESS> --amount <AMOUNT>` / `vault withdraw --vault <ADDRESS> --amount <AMOUNT>` | 提交 vault transfers。 |
| `borrowlend rates` / `borrowlend get <TOKEN>` / `borrowlend user [ADDRESS]` | 查詢 borrow/lend markets。 |
| `borrowlend supply <TOKEN> --amount <AMOUNT>` / `borrowlend withdraw <TOKEN> (--amount <AMOUNT>\|--max)` | 提交已驗證、由錢包簽名的 exchange `borrowLend` supply/withdraw actions；先使用 `--dry-run` 檢查 action。 |
| `builder max-fee --user <ADDRESS> --builder <ADDRESS>` | 查詢用戶已批准的 max builder fee。 |
| `builder approved --user <ADDRESS>` | 列出用戶批准的所有 builders 及 fee caps。 |
| `builder approve --builder <ADDRESS> --max-fee-rate <PERCENT> [-y]` | 為已配置 master signer 批准或撤銷 builder fee cap。 |
| `prio status` / `prio bid --max <HYPE> --ip <IP> [--slot <N>]` | 查詢 gossip priority auction 或出價。 |
| `referral register <CODE>` / `referral set [CODE]` / `referral status` | 註冊你自己的 referral code、設置 referrer 或檢查 referral state。 |
| `feedback (--scenario-json <JSON>\|--scenario-file <PATH\|->) [--contact <CONTACT>] [--tags <TAG>] [--url <URL>]` | 將結構化 CLI feedback 作為 scenario JSON object 發送到已配置的 feedback endpoint；在 scenario 中包含 `agent_address`、`signer_address` 或 `wallet_address` 以用於 rate-limit attribution，並使用 `--url` 覆蓋默認值。 |
| `schema [COMMAND...]` | 顯示供代理使用的機器可讀 command schemas。 |
| `subscribe trades --asset <ASSET>` / `subscribe orderbook --asset <ASSET>` / `subscribe candles --asset <ASSET> [--interval <INTERVAL>]` / `subscribe all-mids` / `subscribe order-updates` / `subscribe fills` `[--max-events <N>] [--idle-timeout-ms <MS>]` | 流式傳輸 WebSocket events。 |
| `update` | 在 Linux/macOS 上從最新 GitHub release 更新此二進制文件。Windows 用戶應重新運行 `install.sh` 安裝最新 `.zip` release。使用全局 `--dry-run` 可先預覽。 |

`vaults` 可作為 `vault` 的別名。

Builder approvals 使用類似 `0.001%` 的百分數字符串；`0%` 通過把已批准 max fee 設為零來撤銷。批准操作必須由 master account 簽名，而不是 API wallet。`orders create` 接受成對的 `--builder <ADDRESS> --builder-fee-rate <PERCENT>` 標誌，並在已簽名 order action 中包含官方 `builder: { b, f }` wire object；perp builder fees 上限為 `0.1%`，spot builder fees 上限為 `1%`，但僅適用於 spot 賣單。Hyperliquid 不會對 spot 買單應用 builder fees。Forks/distributions 可以在構建時內置默認 order builder parameters：

```bash
HYPERLIQUID_DEFAULT_BUILDER_ADDRESS=0x... \
HYPERLIQUID_DEFAULT_BUILDER_FEE_RATE=0.001% \
cargo build --release --bin hyperliquid
```

同名運行時環境變量會覆蓋構建時默認值。在 `hyperliquid setup` 期間，構建時默認值會顯示為建議的 builder/fee，按 Enter 即接受；存在運行時環境變量時，它們會成為建議值。`HYPERLIQUID_DEFAULT_REFERRAL_CODE` 對 setup 和 `referral set` 默認值遵循相同的構建時/運行時模式。發佈工作流會從同名 GitHub Actions repository variables 傳遞這些值。如果 env 和 config 都沒有提供默認值，用戶仍然可以為每個訂單傳入 `--builder` 和 `--builder-fee-rate`。

Vault discovery 可以為 vault detail 和 transfer dry-runs 提供輸入，而無需地址重寫：

```bash
hyperliquid --format json vault list --kind protocol --limit 5 --sort tvl
hyperliquid --format json vault get 0x...
hyperliquid --format json --dry-run vault deposit --vault 0x... --amount 5
```

完整參考：`hyperliquid --help` 和 `hyperliquid <command> --help`，以及 [`docs/`](docs/) 中的指南。

## Testnet

使用 `--testnet` 在觸碰 mainnet 之前演練讀取、dry-runs 和已批准的 live testnet flows。Testnet 使用相同的命令界面，但使用 testnet API endpoint 和單獨的賬戶狀態。

## 安全模型

CLI 旨在讓副作用可見：

- 只讀命令永遠不會觸碰私鑰。
- 簽名只會通過顯式的 `--account`、`--ows-signer`、`--keystore`、`--private-key` 或已存儲的 OWS wallets 發生。
- `--testnet` 會清晰地將 API 調用和已簽名操作路由到 Hyperliquid testnet。
- `--dry-run` 會驗證並預覽任何受支持的變更，而不會發送它。
- 當 schema 將其標記為 prompt-gated 時，live mainnet mutations 和破壞性本地 secret 操作需要確認；使用 `hyperliquid --format json schema ...` 查看每個命令的確認策略。支持時，`-y` / `--yes` 可跳過提示。
- 轉賬接收者和協議對象地址必須是顯式 `0x` addresses——local aliases 永遠不會被靜默替換。

## 退出碼

| 代碼 | 含義 |
| --- | --- |
| `0` | 成功 |
| `1` | 內部錯誤 |
| `2` | 用法、驗證或配置錯誤 |
| `10` | 缺失或無效認證 |
| `11` | 受到速率限制 |
| `12` | API 或網絡不可用 |
| `13` | 不支持的輸入、無效資產或未知 DEX |
| `14` | 過期數據 |
| `15` | 部分結果 |

## 配置

解析順序：CLI flags → environment variables → `~/.config/hyperliquid/config.json`。

| 變量 | 用途 |
| --- | --- |
| `HYPERLIQUID_PRIVATE_KEY` | 用於簽名的 private key（優先使用 OWS 或 keystore）。 |
| `HYPERLIQUID_NETWORK` | `mainnet` 或 `testnet`。 |
| `HYPERLIQUID_FORMAT` | 在 agent/non-TTY fallback 之前顯式設置默認輸出格式（`pretty`、`table` 或 `json`）。 |
| `HYPERLIQUID_AGENT` | 設置為 `1` 以強制代理默認值。 |
| `HYPERLIQUID_WATCH_MAX_TICKS` | snapshot watch mode 的默認 tick limit。 |
| `HYPERLIQUID_SUBSCRIBE_MAX_EVENTS` | agent contexts 中 WebSocket subscribe commands 的默認 event limit。 |
| `OWS_PASSPHRASE` | 用於解鎖加密 OWS wallet 的 passphrase。 |
| `HYPERLIQUID_OWS_VAULT_PATH` | 覆蓋 OWS vault path（默認 `~/.hyperliquid`）。 |
| `HYPERLIQUID_API_BASE_URL` / `HYPERLIQUID_MAINNET_API_BASE_URL` / `HYPERLIQUID_TESTNET_API_BASE_URL` | 覆蓋 API base URLs；所有 override 都限制為 loopback/local test endpoints。 |
| `HYPERLIQUID_DEFAULT_BUILDER_ADDRESS` / `HYPERLIQUID_DEFAULT_BUILDER_FEE_RATE` | per-order builder fee parameters 和 setup suggestions 的運行時默認值。 |
| `HYPERLIQUID_DEFAULT_REFERRAL_CODE` | setup 和 `referral set` 的運行時默認 referral code。 |
| `HYPERLIQUID_FEEDBACK_URL` | `hyperliquid feedback` 的運行時或構建時默認 endpoint。 |
| `HYPERLIQUID_NO_UPDATE_CHECK` | 為 truthy 時禁用 release 更新檢查。 |

## 開發

要為 `hyperliquid feedback` 嵌入默認 endpoint，請在構建環境中設置 `HYPERLIQUID_FEEDBACK_URL`。運行時，`--url` 優先，其次是運行時 `HYPERLIQUID_FEEDBACK_URL`，最後是嵌入的構建時默認值。

```bash
HYPERLIQUID_FEEDBACK_URL="https://<worker-subdomain>/feedback" cargo build --release
```

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

通過 Taskfile 進行可選的可重複 QA：

```bash
task bind
task qa:matrix
```

約定、測試規則和代理優先輸出契約位於 [`AGENTS.md`](AGENTS.md) 和 [`CONTRIBUTING.md`](CONTRIBUTING.md)。

## 當你需要其他東西時

- 構建完整應用或交易系統？請直接使用 [`hypersdk`](https://github.com/infinitefield/hypersdk)。
- 需要長時間運行的策略執行、回測或託管 bots？請選擇專用 bot framework。
- 想要深度歷史 tick data 或跨交易所研究？請使用 market-data platform。

當你想要一個面向 Hyperliquid 的統一操作界面，並且它對人類、腳本和代理都以相同方式工作時，請使用 `hyperliquid`。

## 致謝

基於 [`hypersdk`](https://github.com/infinitefield/hypersdk) 構建；`hypersdk` 由 Infinite Field 提供，用於 Hyperliquid HTTP、WebSocket 和 EIP-712 簽名。`hypersdk` 根據 [Mozilla Public License 2.0](https://www.mozilla.org/en-US/MPL/2.0/) 授權。

## 許可證

MIT — 見 [`LICENSE`](LICENSE)。

### 第三方許可證

`hyperliquid-cli` 是 MIT 授權，但它依賴帶有各自許可證的開源 crates：

| 依賴 | 許可證 | 備註 |
| --- | --- | --- |
| [`hypersdk`](https://github.com/infinitefield/hypersdk) | MPL-2.0 | 作為 Cargo dependency 未修改使用。 |
| [`alloy`](https://github.com/alloy-rs/alloy) family | MIT OR Apache-2.0 | EVM primitives 和 signers。 |
| [`tokio`](https://github.com/tokio-rs/tokio) | MIT | Async runtime。 |
| [`clap`](https://github.com/clap-rs/clap) | MIT OR Apache-2.0 | CLI framework。 |
| [`reqwest`](https://github.com/seanmonstar/reqwest) | MIT OR Apache-2.0 | HTTP client。 |
| [`rust_decimal`](https://github.com/paupino/rust-decimal) | MIT | Fixed-point decimal math。 |
| [`ows-lib`](https://crates.io/crates/ows-lib) | See crate metadata | OWS wallet backend。 |

由於 `hypersdk` 是作為未修改的上游 Cargo dependency 使用的，MPL-2.0 的文件級 copyleft 通過公開上游倉庫得以滿足。如果你 fork 此 CLI 並在樹內修改 `hypersdk` 源文件，這些文件必須保持 MPL-2.0，並且其修改後的源碼必須可用。`hyperliquid-cli` 的其餘部分保持 MIT。

生成完整的傳遞性許可證報告：

```bash
cargo install cargo-license
cargo license
```

## 免責聲明

本軟件按“原樣”提供，不附帶任何形式的保證。在去中心化交易所交易涉及重大虧損風險。你需獨自負責你的密鑰、已簽名操作和交易決策。本項目未與 Hyperliquid 官方關聯。
