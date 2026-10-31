# lab-dataset export：匯出範圍的記憶體、一次查詢的回應上限與讀取量（2026-10-31）

Laysi/trustiot-v2#837：處理 Laysi/trustiot-v2#821 量測發現的三個界線。改之前的量測在 [2026-10-31-821-lab-dataset-export-scale.md](2026-10-31-821-lab-dataset-export-scale.md)。

1. 記憶體隨匯出範圍線性增加（整個範圍讀進記憶體）。
2. 每小時一次查詢，一次回應的上限是 16 MiB（`tools/migrate/src/http.rs` 的 `MAX_BODY_BYTES`）。
3. 每小時查詢的 `read_rows` 遠多於回傳的列數，原因未確認。

## 環境

- **程式碼**：分支 `perf/837-lab-dataset-export-limits`，基於 main `46ced0bf`。改之前的對照是同一個 main commit 以 `git archive` 取出、另一個 cargo target 建置的 `lab-dataset`（`--release`），以環境變數 `LAB_DATASET_BASELINE` 交給測試。
- **機器**：建置主機（arm64 Linux VM），與 Laysi/trustiot-v2#821 的量測同一台。
- **工具與服務**：`.prototools` 釘選的 Rust；ClickHouse 26.8.22.13（`services/ingest/tests/common/mod.rs` 釘選的映像，拋棄式容器，套用產品的全部 ClickHouse migration）。匯出行程是 `lab-dataset` 指令本身。
- **資料**：與 Laysi/trustiot-v2#821 相同的產生方式（測試行程在拋棄式 ClickHouse 裡以 `INSERT ... SELECT` 產生，不進 repo）：一個租戶、一台閘道器、一個串流，30 天、每分鐘一批，兩台設備各五個資料點，432,000 列、43,200 批與完成紀錄，跨兩個月分區。產生後有 5 個 active part（每個 part 的接收時間範圍與列數在 [export-scale-report.json](data/2026-10-31-837-lab-dataset-export-limits/export-scale-report.json) 的 `data.active_parts`）。
- **小資料集**：合成 fixture（`datasets/lab/synthetic`）載入拋棄式 ClickHouse，`tests/export.rs`。

## 指令

在 repo 根目錄執行（建置主機上）。

```sh
# 改之前的執行檔：main 46ced0bf 取出到 <worktree>/.baseline-main/（不進 repo），另一個 target 建置
CARGO_TARGET_DIR=<另一個 target> cargo build --release --locked --manifest-path .baseline-main/Cargo.toml -p lab-dataset
export LAB_DATASET_BASELINE=<另一個 target>/release/lab-dataset

# 小資料集：lab-dataset 的全部測試（CI=1：沒有容器時失敗而不是略過），含與改之前的逐位元組比對
CI=1 cargo test --locked -p lab-dataset -- --nocapture

# 大量資料：7、15、30 天 × 兩種分段大小 × 改之前與改之後，加上 30 天的 https
CI=1 cargo test --release --locked -p lab-dataset --test export_scale -- --ignored --nocapture
```

不設 `LAB_DATASET_BASELINE` 時（例如 CI）兩個測試都只跑改之後的部分。

量測的做法（`tools/lab-dataset/tests/export_scale.rs` 的模組說明）：

- 記憶體峰值：子行程每 10 ms 讀一次 `/proc/<pid>/status` 的 `VmHWM`，與 Laysi/trustiot-v2#821 相同。
- 查詢：每次匯出後 `SYSTEM FLUSH LOGS`，從 `system.query_log` 取讀取帳號在這次匯出的時間內的查詢，依開頭的字分成五種（讀取視窗或每小時的列、普查、重複讀值、規劃、完成紀錄），記次數、回傳列數與 `read_rows`。
- 回應本文：30 天那次的每個讀取視窗（改之前是每小時）的敘述從查詢記錄取出再送一次，量本文的位元組數，列數加總必須等於 432,000、每個都小於 16 MiB。
- `EXPLAIN indexes = 1`：30 天那次範圍中間的那個讀取敘述。
- 逐位元組比對：同一份資料、同一個容器，改之前與改之後的執行檔各匯出一次，逐檔比較，印出每個檔案的 SHA-256。

## 結果

原始資料：[export-scale-run.log](data/2026-10-31-837-lab-dataset-export-limits/export-scale-run.log)（大量資料那次的完整輸出，含每個檔案的 SHA-256）、[export-scale-report.json](data/2026-10-31-837-lab-dataset-export-limits/export-scale-report.json)（全部數字）、[explain-before.txt](data/2026-10-31-837-lab-dataset-export-limits/explain-before.txt) 與 [explain-after.txt](data/2026-10-31-837-lab-dataset-export-limits/explain-after.txt)（`EXPLAIN indexes = 1`）、[explain-probe.log](data/2026-10-31-837-lab-dataset-export-limits/explain-probe.log)（改之前先做的一次性探測，見第 3 點）、[export-test.log](data/2026-10-31-837-lab-dataset-export-limits/export-test.log)（`tests/export.rs` 的輸出）。

### 3. `read_rows` 的原因（先確認，再改）

`EXPLAIN indexes = 1`（[explain-before.txt](data/2026-10-31-837-lab-dataset-export-limits/explain-before.txt)，改之前的一小時查詢，範圍中間）：

| 索引 | 用到的鍵 | 剩下的 part | 剩下的 granule |
| --- | --- | --- | --- |
| Min-Max | `received_at` | 1/5 | 38/55 |
| Partition | `toYYYYMM(received_at)` | 1/1 | 38/38 |
| PrimaryKey | 只有 `tenant_id`（binary search） | 1/1 | 38/38 |

原因確認：遙測表的主鍵是 `(tenant_id, device_id, point, ts)`，不含 `received_at`。依接收時間的條件只能靠月分區與每個 part 的 `received_at` 最小、最大值剪掉 part；留下的 part 裡主鍵只用得到 `tenant_id`，所以讀這個租戶在那個 part 的每一列（38 個 granule，約 30 萬列），一小時只回傳 600 列。#821 的推測（讀月分區內的列）大致正確，更精確地說是讀「接收時間範圍重疊的 part」裡該租戶的全部列。

一次性探測（[explain-probe.log](data/2026-10-31-837-lab-dataset-export-limits/explain-probe.log)，同樣的 30 天資料，在決定做法之前跑；探測程式沒有進 repo）比較了加上讀值時間 `ts` 的條件、與把視窗放大的效果：

| 查詢 | 回傳列數 | `read_rows` |
| --- | --- | --- |
| 一小時（範圍中間） | 600 | 303,600 |
| 一小時加 `ts` 條件 | 600 | 155,648 |
| 一天 | 14,400 | 303,600 |
| 一天加 `ts` 條件 | 14,400 | 163,840 |
| 53 小時 | 31,800 | 303,600 |
| 53 小時加 `ts` 條件 | 31,800 | 180,224 |
| 整個 30 天 | 432,000 | 432,000 |

加上 `ts` 條件後主鍵用到 `ts`（generic exclusion search），只讀讀值時間重疊的 granule；但每個設備與資料點至少要讀一到兩個 granule（每個 8,192 列，這份資料一個 granule 約是一個資料點 5.7 天的讀值），所以一次查詢的讀取量有下限，視窗越大，多讀的比例越小。只改查詢、不改 schema，做不到「每次讀取接近回傳的列數」。

改之後（[explain-after.txt](data/2026-10-31-837-lab-dataset-export-limits/explain-after.txt)，範圍中間的一個讀取視窗，約 47 小時）：Min-Max 剩 1/5 個 part、38/55 個 granule，主鍵用到 `tenant_id` 與 `ts`，剩 22/38 個 granule。

`ts` 條件不會漏列：它的值是規劃查詢對這個視窗的列算出的讀值時間最小、最大值，再加上讀到的列數必須等於規劃的、每個資料點的列數最後必須等於普查的兩道檢查（`src/export.rs` 的模組說明）。

### 讀取量（`read_rows`），改之前與改之後

| 範圍、分段 | 改之前：查詢次數 | 改之前：`read_rows` | 改之後：讀取視窗 | 改之後：視窗的 `read_rows` | 改之後：全部查詢的 `read_rows` | 回傳列數 |
| --- | --- | --- | --- | --- | --- | --- |
| 7 天、64 MiB | 168 + 1 | 12,801,600 | 4 | 347,328 | 815,168 | 100,800 |
| 15 天、64 MiB | 360 + 1 | 70,568,960 | 8 | 1,011,872 | 2,441,904 | 216,000 |
| 30 天、64 MiB | 720 + 1 | 124,070,128 | 16 | 2,171,200 | 3,969,296 | 432,000 |
| 7 天、4 MiB | 168 + 1 | 12,269,120 | 4 | 347,328 | 815,168 | 100,800 |
| 15 天、4 MiB | 360 + 1 | 28,051,824 | 8 | 856,224 | 1,999,536 | 216,000 |
| 30 天、4 MiB | 720 + 1 | 54,602,896 | 16 | 1,671,488 | 3,469,584 | 432,000 |

- 「+ 1」是完成表的查詢。改之後的「全部查詢」含 1 次普查、10 次重複讀值（每個資料點一次）、1 次規劃、每個視窗一次完成表的查詢；30 天、64 MiB 那次分別是 432,000、775,216、432,000、158,880 列。
- 兩輪（64 MiB、4 MiB 分段）之間 ClickHouse 在背景合併了 part，所以同一個範圍在兩輪的讀取量不同；要比較的是同一輪裡的改之前與改之後。
- 30 天：讀取視窗的 `read_rows` 是回傳列數的 5.0 倍（第二輪 3.9 倍），改之前是 287 倍（第二輪 126 倍）；整次匯出讀的列數降為改之前的 3.2%（第二輪 6.4%）。

### 2. 一次查詢的回應大小

| 30 天、64 MiB | 查詢次數 | 回應本文（最小 / 中位數 / 最大，bytes） | 最大是上限的 |
| --- | --- | --- | --- |
| 改之前（每小時） | 720 | 150,221 / 152,359 / 152,494 | 0.9% |
| 改之後（讀取視窗） | 16 | 495,546 / 7,281,960 / 7,282,024 | 43% |

- 兩者的回應本文加總都是 109,583,425 bytes（一列 253.7 bytes），列數加總都是 432,000。
- 視窗依回應長度的上界合併（預設 12 MiB，上界約是實際長度的 1.7 倍），所以一小時再多列也不會碰到上限；只有同一毫秒收到的列就超過上限時才會失敗。
- 縮小上限的測試（`tests/export.rs`，合成 fixture，16 個接收時間）：上限 1 byte 時切成 16 個視窗（每個接收時間一個），64 KiB 時 3 個，預設時 1 個；三者寫出的 2 個檔案逐位元組相同。注入重複列的資料以 1 byte 的上限匯出，同樣只留一筆，檔案與一次讀完的相同。

### 1. 記憶體

| 範圍 | 列數 | 64 MiB 分段：改之前 | 64 MiB 分段：改之後 | 4 MiB 分段：改之前 | 4 MiB 分段：改之後 |
| --- | --- | --- | --- | --- | --- |
| 7 天 | 100,800 | 76.4 MiB | 53.9 MiB | 44.1 MiB | 26.7 MiB |
| 15 天 | 216,000 | 120.2 MiB | 102.5 MiB | 78.3 MiB | 27.5 MiB |
| 30 天 | 432,000 | 226.3 MiB | 177.6 MiB | 145.4 MiB | 36.8 MiB |

- 改之後讀取時只放一個視窗與跨視窗的狀態。峰值在最後的 `Dataset::open`：它一次讀一個分段檔（上限 `segment_bytes`）先驗雜湊再解析，並記住每筆讀值以檢查跨分段的重複。
  - 64 MiB 分段時，7、15 天的資料只有一個分段（18 MB、39 MB），30 天的第一個分段是 64 MiB 滿檔，所以峰值隨分段檔的大小增加到約 178 MiB 為止；範圍再大，分段檔不再變大，峰值只多每筆讀值的集合。
  - 4 MiB 分段時 7、15、30 天是 26.7、27.5、36.8 MiB：30 天比 7 天多 10.1 MiB，是多出的 331,200 筆讀值的集合（每筆約 32 bytes：16 bytes 的鍵加雜湊表的空間）。測試的上限是 7 天的峰值加每多一筆 64 bytes 再加 16 MiB（64,416 KiB），30 天是 37,644 KiB，通過。
  - 改之前同樣是 4 MiB 分段，30 天是 7 天的 3.3 倍（每多一筆約 0.31 KiB）。
- 為了讓集合不隨資料點的名稱變大，`Dataset::open` 改以設備與資料點在索引裡的位置（`u32`）加讀值時間記住讀值，不再複製兩個字串。
- 耗時只當量級看（主機另有負載時會變）：30 天、64 MiB 分段，改之前 5.9 s、改之後 1.7 s。

### 逐位元組比對（改之前與改之後）

- 大量資料：7、15、30 天 × 64 MiB、4 MiB 分段，6 組、共 44 個檔案全部相同。30 天、64 MiB 那組：

  | 檔案 | bytes | SHA-256 |
  | --- | --- | --- |
  | `index.toml` | 3,276 | `09e3ddeb35c5d70807447eb9e3332f0046513c35ac7c6f53823c039a67a8264e` |
  | `segments/part-0001.jsonl` | 67,107,875 | `cb5b356fb898b55b7e347f7b9d48b9eed12469f5236f2323839c228c9dacdbc7` |
  | `segments/part-0002.jsonl` | 11,717,670 | `162bc179da1db13a351df5e35d440082bfe7a16d2d528ba259f8a2b18d42fe5a` |

  其餘各組每個檔案的 SHA-256 在 [export-scale-run.log](data/2026-10-31-837-lab-dataset-export-limits/export-scale-run.log)（`BASELINE` 開頭的行）與 [export-scale-report.json](data/2026-10-31-837-lab-dataset-export-limits/export-scale-report.json) 的 `baseline_comparisons`。
- 小資料集（[export-test.log](data/2026-10-31-837-lab-dataset-export-limits/export-test.log)）：整份 fixture、指令匯出的一段範圍（`live` 第 3 到 6 批）、注入重複列的租戶，三組 6 個檔案全部相同。
- 30 天經 https 與 `CLICKHOUSE_CA_FILE` 匯出：3 個檔案、78,828,821 bytes，與 http 那次相同（記憶體峰值 178.8 MiB）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 第 3 點的原因以 `EXPLAIN` 確認並記錄；若改了，30 天的每小時查詢 `read_rows` 接近回傳的列數（測試報告） | 原因確認（本報告第 3 點）：主鍵不含 `received_at`，part 內只用得到 `tenant_id`。改了查詢：視窗依資料量合併、加上精確的讀值時間條件，30 天整次匯出的 `read_rows` 從 1 億 2 千萬降到 397 萬（讀取視窗本身是回傳列數的 5.0 倍，改之前 287 倍）。「接近回傳的列數」只改查詢做不到（每個資料點每次至少一到兩個 granule），要靠 schema 變更，提案與成本寫在 `datasets/lab/README.md`「匯出」的「讀取方式、記憶體與讀取量」，不在本 issue 範圍。 |
| 第 1、2 點各自決定：改了的有測試；沒改的在 `datasets/lab/README.md`「匯出」說明 | 兩點都改了。第 1 點：逐視窗讀寫，`tests/export_scale.rs` 斷言 4 MiB 分段時 30 天的峰值不超過 7 天的峰值加上限（36.8 MiB 對 26.7 MiB）；最後 `Dataset::open` 的分段檔與讀值集合的記憶體寫在 README。第 2 點：視窗依回應長度的上界切分，`tests/export.rs` 把上限縮到 1 byte 與 64 KiB，匯出成功且檔案與一次讀完的相同；30 天最大的回應是上限的 43%。 |
| `CI=1 cargo test -p lab-dataset` 通過；若改了讀取方式，匯出的檔案與改之前逐位元組相同 | 通過；大量資料 6 組 44 個檔案、小資料集 3 組 6 個檔案與改之前逐位元組相同。 |

沒做、仍待測（進階驗證，已在 Laysi/trustiot-v2#836，這裡不重複）：x86_64 Linux 建置主機上重跑、真實資料下超過 16 MiB 的回應、ingest 長期寫入的資料。另外沒測的：

- 一個接收時間（同一毫秒）的列就超過 16 MiB 的情況：視窗無法再切，查詢會失敗、不留檔案（邏輯與單元測試涵蓋，沒有以真的 16 MiB 資料實測；ingest 的一則訊息遠小於此）。
- 匯出期間範圍內有資料寫入或刪除時的拒絕：邏輯在 `src/export.rs`（視窗的列數、每個資料點的列數兩道檢查），沒有在容器裡模擬並行寫入。
