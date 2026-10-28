# ingest 的遙測寫入流程：端到端確認、批次去重與高水位

Laysi/trustiot-v2#641（清單 I1、決策 8）。寫入流程的說明在 [services/ingest/README.md](../../services/ingest/README.md)「寫入流程」。

## 環境

- 建置主機（macOS arm64），podman 起拋棄式容器；設了 `CI=1`，找不到容器執行環境時測試失敗而不是略過。
- 分支 `feat/641-ingest-telemetry`。容器測試以 main `5dad0429` 為基礎時執行；之後改以 main `9d879e95` 為基礎（多出的 commit 只改 `edge/agent` 與文件），ingest 的程式不變。
- Rust 1.99.0（`rust-toolchain.toml`）。
- 容器映像（都以 multi-arch index 的 digest 釘選）：ClickHouse `clickhouse-server:26.8`（`services/ingest/tests/common/mod.rs`）、Postgres `postgres:18.6-alpine`（`testkit` 的 `product_postgres`，帶 TLS，套用 migration Job 的全部 Postgres migration）、Mosquitto `eclipse-mosquitto:2.1.2-alpine`（`testkit::mqtt::mosquitto`，dynamic security，`service:ingest` 的角色取自 `tools/migrate` 的定義）。
- `tests/telemetry.rs` 的 ClickHouse 先放 200,000 列別的串流的資料，用來量找殘留列的那一句讀了多少。

## 指令

在 repo 根目錄：

```sh
CI=1 cargo test -p ingest --test telemetry --test startup -- --nocapture
```

## 結果

兩個測試都通過；輸出（只留測試本身印的行與結果）：

- [telemetry.txt](data/2026-10-29-641-ingest-telemetry/telemetry.txt)：`tests/telemetry.rs`
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
| 重寫之後兩張彙總表與原始資料一致；刪除之前寫過相同內容的批次也完整寫入 | 通過（第 7 步；第 5 步重寫的是相同的區塊） |
| 每一句都帶 `stream_settings`，client 請求逾時不短於 `CLIENT_TIMEOUT_SECONDS` | 通過（第 7 步讀 `system.query_log`；逾時由 `src/clickhouse.rs` 的單元測試檢查） |
| `cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings` 通過；`CI` 設定時不略過 | 通過，分兩台建置主機跑完（見下方「全 workspace 的檢查」） |

### 全 workspace 的檢查

都設了 `CI=1`，2026-10-29：

- 建置主機（macOS）：`cargo fmt --all --check`；`cargo clippy --workspace --all-targets --locked --exclude agent -- -D warnings`；`cargo test --workspace --locked --exclude agent --no-fail-fast`；`cargo deny --locked check`（advisories、bans、licenses、sources 都 ok）。測試中 219 個 test binary 通過，另有 5 個 binary 在 macOS 上失敗，都不是這次改的 crate，也都與 macOS 有關（容器裡的 Linux 執行檔、連接埠與 accept queue 的行為）：`ai-clients` 的 `anthropic_http`、`migrate` 的 `mqtt_broker`、`service-kit` 的 `container` 與 `verify_container`、`testkit` 的 lib 測試。
- 建置主機（Linux）：上面這 5 個 binary 全部通過（14、6、1、1、41 個測試）；`cargo clippy -p agent --all-targets --locked -- -D warnings` 與 `cargo test -p agent --locked --no-fail-fast` 通過。

待測：真的斷電重設後的落盤（Laysi/trustiot-v2#643，需要可以強制斷電的環境）；吞吐量與積壓的量測（還沒有 issue，見 PR 的後續提議）。
