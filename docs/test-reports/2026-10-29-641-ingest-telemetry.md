# ingest 的遙測寫入流程：端到端確認、批次去重與高水位

Laysi/trustiot-v2#641（清單 I1、決策 8）。寫入流程的說明在 [services/ingest/README.md](../../services/ingest/README.md)「寫入流程」。

## 環境

- 建置主機（macOS arm64），podman 起拋棄式容器；設了 `CI=1`，找不到容器執行環境時測試失敗而不是略過。
- 分支 `feat/641-ingest-telemetry`。容器測試以 main `5dad0429` 為基礎時執行；之後改以 main `9d879e95` 為基礎（多出的 commit 只改 `edge/agent` 與文件），ingest 的程式不變。
- Rust 1.99.0（`rust-toolchain.toml`）。
- 容器映像（都以 multi-arch index 的 digest 釘選）：ClickHouse `clickhouse-server:26.8`（`services/ingest/tests/common/mod.rs`）、Postgres `postgres:18.6-alpine`（`testkit` 的 `product_postgres`，帶 TLS，套用 migration Job 的全部 Postgres migration）、Mosquitto `eclipse-mosquitto:2.1.2-alpine`（`testkit::mqtt::mosquitto`，dynamic security，`service:ingest` 的角色取自 `tools/migrate` 的定義）。
- `tests/telemetry.rs` 的 ClickHouse 先放 200,000 列別的串流的資料，用來量找殘留列的那一句讀了多少。
- 安全審查後（同日）`tests/telemetry.rs` 多了第 7、8 步，原來的第 7 步改為第 9 步；[telemetry.txt](data/2026-10-29-641-ingest-telemetry/telemetry.txt) 是修正後在建置主機（Linux）跑的輸出，分支已合入 main `fdf7bd01`（多出的 commit 只改 chart 與 ai-gateway）。下表與結論中修正前的數字（語句數、讀取列數）取自第一次在建置主機（macOS）的執行，修正後的數字列在「安全審查後的修正」。

## 指令

在 repo 根目錄：

```sh
CI=1 cargo test -p ingest --test telemetry --test startup -- --nocapture
```

## 結果

兩個測試都通過；輸出（只留測試本身印的行與結果）：

- [telemetry.txt](data/2026-10-29-641-ingest-telemetry/telemetry.txt)：`tests/telemetry.rs`（安全審查後的版本）
- [full-batch-before.txt](data/2026-10-29-641-ingest-telemetry/full-batch-before.txt)：修正前滿的批次的失敗，與找出上限的臨時探測
- [startup.txt](data/2026-10-29-641-ingest-telemetry/startup.txt)：`tests/startup.rs`

重點數據：

| 項目 | 結果 |
| --- | --- |
| 同一批送兩次 | `Committed { records: 20 }`，再送 `Duplicate { records: 20 }`，列數兩次都是 20，完成紀錄一筆 |
| 批次內容帶別的租戶 | 列與完成紀錄的租戶都是綁定表的，payload 宣稱的租戶被忽略 |
| 未綁定的閘道器 | `Retry { after_ms: 30000 }`，0 列、沒有完成紀錄 |
| 設備 ID 不是 UUID | `rejected`（`invalid_value`，`series[0].device`），0 列 |
| 標頭讀得出、內文壞掉 | `rejected`（`decode_failed`），串流與序號取自 `peek_header` |
| 連標頭都沒有 | 不回確認，WARN `dropped a telemetry message without a readable batch header` |
| 完成表拒絕寫入（收回 `INSERT`，ClickHouse 錯誤 497） | `Retry { after_ms: 5000 }`、沒有完成紀錄；授權回來後重送 `committed`，列仍是 20 |
| 兩次嘗試之間改綁 | `committed`，20 列全在新租戶下，WARN 記下舊租戶的列被刪除 |
| 直接訂閱時 broker 重播 retained 訊息 | 不回確認、列不變，WARN `dropped a retained telemetry message` |
| 列已寫入、完成紀錄未寫時結束 | 20 列、沒有完成紀錄、沒有確認；以新的寫入流程重送（同樣的 `received_at`，重寫的區塊與刪掉的完全相同）：停在刪除之後與完成紀錄之前都沒有確認，回 `committed` 時完成紀錄已在，列 20 |
| 兩個副本、同一串流 | 一個副本持鎖時，重送在兩邊都回 `Retry { after_ms: 2000 }`；之後序號 1 重送得到 `duplicate`，序號 2–8 各只有一個 `committed`，每批 20 列 |
| 彙總與原始資料 | `telemetry_1m`、`telemetry_1h` 各 24 格，與原始列的差異 0 格 |
| `committed_at` − `received_at` | 13 批中最小 45 ms（不小於 0） |
| `stream_settings` | 服務帳號的語句：Delete 3/3、Insert 61/61、Select 34/34 都帶串流的 query ID、`replace_running_query = 1`、`max_execution_time = 20`、`async_insert = 0` |
| 找殘留列那一句讀的列數 | 16 句中最多 100 列（3,694 bytes），表中共 200,260 列 |
| DELETE 讀的列數 | `system.query_log` 對 lightweight DELETE 的 `read_rows` 記 0，量不到；它的條件與找殘留列那一句相同（`store.rs` 的單元測試比對兩者），所以以後者的讀取量為準 |
| `telemetry.tenant_id` 欄位說明 | `Tenant the gateway was bound to when the batch was written, from the ingest service's binding table, never from the payload` |
| broker 停下 | `/readyz` 503，日誌 `readiness check failed`、`check = mqtt`；broker 回來後 200 |
| 經 binary 送一批 | `Committed { records: 20 }`，再送 `Duplicate { records: 20 }`，兩次都是 20 列 |
| 日誌 | 不含 ClickHouse 與 broker 的密碼，也不含讀值 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 同一批重送兩次，列數與第一次相同，第二次回 `duplicate` | 通過（`telemetry.rs` 第 1 步、`startup.rs` 第 6 步） |
| 「列已寫入、完成紀錄未寫」時結束，重啟後重送：完整、沒有重複，回 `committed` | 通過（`telemetry.rs` 第 5 步，以測試掛鉤在 `RowsWritten` 結束） |
| 完成紀錄落盤之前不送 `committed`；ClickHouse 失敗時回 `retry` | 通過（第 5 步停在 `BeforeCommitRecord` 時沒有確認；第 3 步完成表拒絕時回 `retry`） |
| 租戶只取自對照表，批次內容的租戶被忽略；未綁定回 `retry`、不寫入 | 通過（第 1、2 步） |
| 兩個副本同時處理同一串流時沒有重複資料 | 通過（第 6 步） |
| retain 旗標為 1 的批次被丟棄並記錄；解不開的 payload 以 `peek_header` 回 `rejected` | 通過（第 4、2 步） |
| broker 連不上時 readiness 不是 200，連上後恢復 | 通過（`startup.rs` 第 5 步） |
| 重寫之後兩張彙總表與原始資料一致；刪除之前寫過相同內容的批次也完整寫入 | 通過（第 9 步；第 5 步重寫的是相同的區塊） |
| 每一句都帶 `stream_settings`，client 請求逾時不短於 `CLIENT_TIMEOUT_SECONDS` | 通過（第 9 步讀 `system.query_log`；逾時由 `src/clickhouse.rs` 的單元測試檢查） |
| `cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings` 通過；`CI` 設定時不略過 | 通過，分兩台建置主機跑完（見下方「全 workspace 的檢查」） |

### 全 workspace 的檢查

都設了 `CI=1`，2026-10-29：

- 建置主機（macOS）：`cargo fmt --all --check`；`cargo clippy --workspace --all-targets --locked --exclude agent -- -D warnings`；`cargo test --workspace --locked --exclude agent --no-fail-fast`；`cargo deny --locked check`（advisories、bans、licenses、sources 都 ok）。測試中 219 個 test binary 通過，另有 5 個 binary 在 macOS 上失敗，都不是這次改的 crate，也都與 macOS 有關（容器裡的 Linux 執行檔、連接埠與 accept queue 的行為）：`ai-clients` 的 `anthropic_http`、`migrate` 的 `mqtt_broker`、`service-kit` 的 `container` 與 `verify_container`、`testkit` 的 lib 測試。
- 建置主機（Linux）：上面這 5 個 binary 全部通過（14、6、1、1、41 個測試）；`cargo clippy -p agent --all-targets --locked -- -D warnings` 與 `cargo test -p agent --locked --no-fail-fast` 通過。

## 安全審查後的修正（2026-10-29）

審查（分支 `c872335c`）沒有 Critical；以下四項在同一個分支修正。

| 項目 | 修正 | 證據 |
| --- | --- | --- |
| H1：滿的批次永遠寫不進去 | 先重現：以 agent 的 `BatchBuilder` 做出 262,037 bytes、1,845 個 series（每台設備 4 個資料點、名稱 64 字元）的批次，第一句（找殘留列）就失敗、回 `Retry { after_ms: 5000 }`，高水位不前進。原因在服務端的 HTTP 用戶端：整個 `param_series` 放在 URL，而 `http` crate 不接受 64 KiB 以上的 URI，請求根本沒送出（臨時探測：63 KiB 的參數成功、64 KiB 起一律 `uri too long`）。改成依編碼後的長度把 series 切成每段不超過 32 KiB（`MAX_SERIES_PARAM_BYTES`；先試過 64 KiB 一段，整個 URI 仍超過上限而失敗），找殘留列、DELETE、數列與兩句對帳每段各跑一次 | 第 7 步：同樣大小的批次一次就 `Committed { records: 1845 }`；收回完成表的 `INSERT` 後回 `retry`、列已寫入，授權回來重送 `Committed { records: 1845 }`、列 1,845（找殘留列與 DELETE 都分段點名每個 series）。單元測試：切段後每段不超過上限、合起來與原本相同（`rollup.rs`），長度的計算與實際的編碼器一致、32 KiB 一段加上其他參數仍是合法的 URI（`transport.rs`） |
| M1：沒有閘道器之間的公平性 | 每台閘道器一個批次桶（預設每分鐘 600 批、最多存 60 批，`TRUSTIOT__TELEMETRY_LIMITS__*`）與處理中批次的上限（同時處理數的一半），在查綁定之前檢查；超過的只讀標頭、回 `Retry { after_ms: 10000 }`，不查綁定、不拿鎖、不下 ClickHouse 語句、不佔處理名額。逐則的日誌每台閘道器先給 5 行、之後每 10 秒 1 行，略過的行數記在下一行（`suppressed_logs`） | 第 8 步（桶設為 5 批、每分鐘 1 批）：200 則重播得到 3 個 `duplicate`、1 個 `Retry { after_ms: 2000 }`（兩則被放行的重播搶同一個鎖）、178 個 `Retry { after_ms: 10000 }`、18 則沒有回覆（同時送出的限額確認已滿）；這個串流在 ClickHouse 只有 4 次讀高水位（第一次寫入 1 次加 3 次 `duplicate`）；關於它的日誌 5 行；洗版之後送出的另一台閘道器的批次 `Committed`。單元測試：桶的補充與上限、處理中上限、被拒的訊息不扣額度、日誌額度與略過計數、閒置閘道器的清除（`limits.rs`） |
| L1：完成紀錄之前數列 | 寫完成紀錄之前以與 DELETE 相同的條件（不分租戶）數這批的列，不等於筆數就回 `retry`（`step` 是 `row_count`）。這縮小、但沒有關閉鎖遺失的窗口：要關閉得讓寫入帶 fencing | 每次 `committed` 之前都經過這一句：第 9 步的 `query_log` 有 41 句數列，最多讀 5,497 列（表中共 203,990 列）；三句（找殘留列、數列、DELETE）的條件相同由 `store.rs` 的單元測試比對 |
| L3：readiness 一定拿得到連線 | 同時處理的批次改為 `MAX_CONNECTIONS` − 2，下限改為 3（一條給消費端、一條給 readiness） | `telemetry::concurrency` 與 `config.rs` 的單元測試 |

修正後在建置主機（Linux）：`cargo clippy -p ingest -p migrate --all-targets -- -D warnings` 通過，`cargo test -p ingest --test telemetry` 通過（41 秒）；第 9 步兩張彙總表各 3,730 與 3,722 格、差異 0 格，Delete 11/11、Insert 122/122、Select 109/109 句都帶串流設定。

待測：真的斷電重設後的落盤（Laysi/trustiot-v2#643，需要可以強制斷電的環境）；吞吐量與積壓的量測（還沒有 issue，見 PR 的後續提議）。
