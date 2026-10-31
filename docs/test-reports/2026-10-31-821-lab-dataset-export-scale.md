# lab-dataset export：大量資料下的記憶體與耗時、經 https 的匯出（2026-10-31）

Laysi/trustiot-v2#821，延後自 Laysi/trustiot-v2#816（PR Laysi/trustiot-v2#818）的進階驗證：`lab-dataset export` 在 30 天、每分鐘一筆、多個資料點的資料上，行程的記憶體峰值、耗時與每小時查詢的回應大小，以及經 `https://` 與 `CLICKHOUSE_CA_FILE` 的匯出是否與經 http 的相同。

## 環境

- **程式碼**：分支 `test/821-lab-dataset-export-scale`，基於 main `55aa6349`；量測用的測試是 `tools/lab-dataset/tests/export_scale.rs`，https 的自動測試在 `tools/lab-dataset/tests/export.rs`。
- **機器**：建置主機（arm64 Linux VM）。最後一次完整量測與 https 的自動測試都在這台跑；同一個指令的前兩次（見「結果」）執行時主機另有負載。
- **工具與服務**：`.prototools` 釘選的 Rust，`--release` 建置；ClickHouse 26.8.22.13（`services/ingest/tests/common/mod.rs` 釘選的映像，拋棄式容器，記憶體上限 1500 MB，套用產品的全部 ClickHouse migration）；匯出行程是 `lab-dataset` 指令本身，不是在測試行程內呼叫 library。
- **資料**：測試行程在拋棄式 ClickHouse 裡以伺服器端的 `INSERT ... SELECT` 產生，每天一次，不進 repo。
  - 一個租戶、一台閘道器、一個串流；每分鐘一批（43,200 批），一批 10 列，每批有一筆完成紀錄。
  - 兩台設備，每台五個資料點（三個浮點的瞬間值、一個 32 位元計數器、一個布林狀態），每個資料點每分鐘一筆，共 432,000 列。
  - 資料的最後一批在量測時間的兩天前（保存期限 400 天內），跨兩個月分區；讀值時間比接收時間早 400 ms。
  - 讀取帳號只有兩個表的 `SELECT` 與被 row policy 點名（與 `datasets/lab/README.md`「匯出」相同）。
  - 與正式環境不同的地方：資料是每天一次 INSERT，不是每批一次，所以 part 的排列（5 個 active part）與 ingest 長期寫入的不同。
- **https**：憑證由測試行程用 `testkit::postgres::TestCa` 產生的 CA 簽發，只含 `127.0.0.1`，CA 與金鑰不進 repo，也不進映像；伺服器的 HTTPS port 以容器啟動時寫入的設定檔開啟（`start_throwaway_with_https`）。

## 指令

在 repo 根目錄執行（建置主機上）。

```sh
# 大量資料的量測與 30 天的 https 比對（ignored：要幾分鐘與幾百 MB 磁碟，只在 Linux 上跑）
cargo test --release --locked -p lab-dataset --test export_scale -- --ignored --nocapture

# https 的自動測試（隨 `cargo test -p lab-dataset` 一起跑，CI 也跑）
cargo test --locked -p lab-dataset --test export -- --nocapture
```

量測的做法：

- 記憶體峰值：匯出的子行程每 10 ms 讀一次 `/proc/<pid>/status` 的 `VmHWM`，取行程結束前的最後一個值（只在 `Name` 是 `lab-dataset` 時才採，避免讀到 fork 後、exec 前還帶著測試行程記憶體的子行程）。
- 耗時：從啟動子行程到它結束，包含連線、每小時的查詢、分組與檢查、寫檔與最後的 `Dataset::open`。
- 每小時查詢：`SYSTEM FLUSH LOGS` 後從 `system.query_log` 取讀取帳號在 30 天那次匯出的時間窗內、以 `SELECT stream_id, batch_seq` 開頭的查詢：次數、`result_rows`、`result_bytes`（ClickHouse 的結果大小，不是 JSON 文字的長度）、`read_rows`、`query_duration_ms`、`memory_usage`。
- 回應本文的長度：HTTP client 的上限算的是本文長度（`tools/migrate/src/http.rs` 的 `MAX_BODY_BYTES`）。查詢記錄裡的敘述已經把參數值寫進去（AST 重新格式化後的文字，語意與匯出送的相同），把第一個視窗的兩個時間換成每個視窗自己的，再各送一次，量 720 個回應本文的位元組數，列數（換行數）加總必須等於 432,000。
- https：同一份資料、同一個容器的 HTTPS port，`CLICKHOUSE_URL` 是 `https://127.0.0.1:<port>`，`CLICKHOUSE_CA_FILE` 指向測試產生的 CA，匯出 30 天；與 http 那次的資料集逐檔逐位元組比較。

## 結果

原始資料：[export-scale-report.json](data/2026-10-31-821-lab-dataset-export-scale/export-scale-report.json)（量測的全部數字）、[export-scale-run.log](data/2026-10-31-821-lab-dataset-export-scale/export-scale-run.log)（最後一次的輸出）、[earlier-runs.txt](data/2026-10-31-821-lab-dataset-export-scale/earlier-runs.txt)（同一個測試前兩次的匯出數字）、[export-https-test.log](data/2026-10-31-821-lab-dataset-export-scale/export-https-test.log)（https 自動測試的輸出）。

### 匯出的記憶體與耗時（http，每次一個獨立的行程）

| 範圍 | 列數 | 批次 | 分段檔 | 記憶體峰值 | 耗時 |
| --- | --- | --- | --- | --- | --- |
| 7 天 | 100,800 | 10,080 | 1 | 67 MiB | 1.1 s |
| 15 天 | 216,000 | 21,600 | 1 | 122 MiB | 2.5 s |
| 30 天 | 432,000 | 43,200 | 2 | 219 MiB | 4.7 s |

- 三次都成功：筆數、批次數等於產生的資料，沒有未完成的批次與略過的重複列；30 天的結果以 `lab-dataset check` 檢查通過（兩個分段檔，78,828,821 bytes 含索引）。
- 記憶體隨範圍線性增加：每多一天約多 7 MiB（每列約 0.5 KiB），沒有固定的上限。匯出先把整個範圍的列讀進記憶體、依批次分組、再複製一份給完成紀錄檢查（`src/export.rs` 的 `Exporter::run`、`group`、`finish`），所以峰值是範圍的列數的函數，不是一個視窗的函數。以這個比例，這份資料（每分鐘 10 列）匯出一年約 2.5 GiB。
- 同一個測試前兩次（主機另有負載）：30 天的記憶體峰值 227、218 MiB，耗時 12.8、11.9 s。記憶體重現，耗時受主機負載影響約 2.5 倍，所以耗時只當量級看，不當指標。

### 每小時查詢（30 天那次，720 次）

| 項目 | 最小 | 中位數 | 最大 |
| --- | --- | --- | --- |
| 次數 | 720（等於 30 × 24） | | |
| 回傳列數 | 600 | 600 | 600 |
| 回應本文（bytes） | 150,221 | 152,359 | 152,494 |
| ClickHouse 的 `result_bytes` | 82,507 | 96,829 | 96,829 |
| 查詢耗時（ms） | 1 | 5 | 15 |
| 伺服器端記憶體（bytes） | 5,390,940 | 5,529,461 | 5,530,117 |
| 讀取列數（`read_rows`） | 600 | 86,400 | 316,200 |

- 720 個回應的列數加總等於 432,000，與 `system.query_log` 的 `result_rows` 加總相同。
- 一列的回應平均 253.7 bytes（109,583,425 bytes ÷ 432,000 列）。
- 最大的回應 152,494 bytes，是 16 MiB（16,777,216 bytes）上限的 0.9%。上限在 16,777,216 ÷ 253.7 ≈ 66,139 列／小時，約每分鐘 1,100 筆讀值（以這份資料的字串長度為準：設備 ID 是 36 字元的 UUID、串流 ID 是 `stream-1`；真實的串流 ID 與資料點代號較長時每列會更大，上限的列數會略少）。例如 100 台設備、每台 10 個資料點、每分鐘一筆，一小時 60,000 列、約 14.5 MiB，已是上限的 91%。一小時的回應超過上限時，匯出回報錯誤、不寫出殘缺的資料集；建議的做法是把查詢視窗縮短（每個視窗只要不切開一批，長度可以是任意值，見 `src/export.rs` 的 `WINDOW`），本次沒有改。
- `read_rows`（最小 600、中位數 86,400、最大 316,200）遠大於回傳的 600 列。推測的原因（沒有用 `EXPLAIN` 驗證）：遙測表的主鍵是 `(tenant_id, device_id, point, ts)`，不含 `received_at`，一小時的查詢只靠月分區剪掉其他月分，其餘要讀月分區內的列再過濾。這個資料量每次查詢只要幾毫秒，但如果推測成立，每小時讀的列數會隨表裡的資料增加，匯出一段範圍的總讀取量約是（小時數 × 月分區的列數）。

### 經 https 與 `CLICKHOUSE_CA_FILE` 的匯出

- 大量資料：30 天、432,000 列經 `https://` 匯出，記憶體峰值 221 MiB、耗時 4.7 s；索引與兩個分段檔共 3 個檔案、78,828,821 bytes，與 http 那次逐檔逐位元組相同。
- 小資料集（`tests/export.rs`，每次 `cargo test -p lab-dataset` 與 CI 都跑）：合成 fixture 載入後，同一個容器的 http 與 https 各匯出一次：
  - library：316 列、16 批、2 個檔案（58,675 bytes）逐位元組相同。
  - 指令（`CLICKHOUSE_URL=https://...`、`CLICKHOUSE_CA_FILE`）：`live` 第 3 到 6 批，兩個檔案與 http 的相同。
  - 另一個 CA（不是簽發伺服器憑證的那個）：匯出失敗，訊息是 `ClickHouse: TLS with 127.0.0.1: invalid peer certificate: UnknownIssuer`，不含密碼，也沒有留下任何檔案。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 至少 30 天、每分鐘一筆、數個資料點的資料匯出成功，記憶體峰值、耗時與每次查詢的回應大小記在 `docs/test-reports/` | 通過：30 天、10 個資料點（2 台設備）、432,000 列匯出成功，數字都在本報告：記憶體峰值 219 MiB、耗時 4.7 s、720 次查詢、回應最大 152,494 bytes（上限的 0.9%）。 |
| 經 https 與 `CLICKHOUSE_CA_FILE` 的匯出成功，結果與經 http 時相同（測試或測試報告） | 通過：自動測試（`tests/export.rs`，library 與指令）與 30 天的量測都逐位元組相同；不被信任的 CA 被拒絕。 |

量測發現的界線（不改匯出功能，另開 issue 處理）：

- 記憶體隨範圍線性增加（每列約 0.5 KiB），30 天這個資料量 219 MiB；範圍很大或資料點很多時，匯出的機器要有足夠的記憶體。
- 每小時一次的查詢離 16 MiB 的上限很遠（0.9%），每小時約 66,000 列才到上限，目前的資料量（每分鐘幾十筆）用不到。
- 每小時查詢讀的列數遠多於回傳的列數（主鍵不含 `received_at`，推測要讀月分區內的列），表很大時總讀取量可能隨範圍的小時數乘表的大小增加；需要先用 `EXPLAIN` 確認。

沒做、仍待測（進階驗證，依專案規則另議）：

- 在 x86_64 Linux 建置主機上重跑同一個量測：本次只在 arm64 Linux VM 跑（跨平台的重複驗證）。
- 讓一小時的回應真的超過 16 MiB，確認錯誤訊息與不留檔案（上限的行為是 HTTP client 的既有行為，數字只由本次的位元組數推算）。
- 以每批一次 INSERT、背景 merge 的方式長期寫入的資料再量一次（本次的資料是每天一次 INSERT）。
- 重複多次量測以得到耗時的分布。
