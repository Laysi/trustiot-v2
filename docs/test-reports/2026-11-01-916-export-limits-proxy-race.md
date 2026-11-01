# lab-dataset export_limits：變動測試的 proxy 把讀列的次序算錯（2026-11-01）

對應 issue：Laysi/trustiot-v2#916。

`tools/lab-dataset/tests/export_limits.rs` 的 `a_row_written_into_a_window_before_its_read_is_refused` 在 PR Laysi/trustiot-v2#915 的 CI（2026-11-01）失敗一次：匯出成功、`rows: 240`，沒有拒絕。本報告記錄根因的證據與修正後的重複執行。

## 環境

- 建置主機（arm64 Linux）；Rust 1.99.0（`rust-toolchain.toml`）；ClickHouse 測試容器 `clickhouse/clickhouse-server:26.8`（`services/ingest/tests/common/mod.rs` 釘住的 digest）。
- 修正前：main `afef49fe` 加上暫時的紀錄與延遲（[instrument-before-fix.patch](data/2026-11-01-916-export-limits-proxy-race/instrument-before-fix.patch)，不提交）。
- 修正後：分支 `fix/916-export-limits-flake`；注入延遲的那一段另外套 [inject-after-fix.patch](data/2026-11-01-916-export-limits-proxy-race/inject-after-fix.patch)（不提交）。
- 每次執行都設 `CI=1`（沒有容器時測試失敗而不是略過）。

## 指令

修正前（從 repo 根目錄，先套 `instrument-before-fix.patch`）：[before-fix.sh](data/2026-11-01-916-export-limits-proxy-race/before-fix.sh)。

1. `LAB_PROXY_DELAY_READ=2`：第 2 次讀列的回應整段送到 client 之後，proxy 的那個 task 晚 500 ms 才記錄這個回應，模擬負載高時排程器暫時沒有執行那個 task；只跑失敗的那個測試。
2. 不注入延遲：整個 `export_limits` 測試檔，10 輪、每輪同時 2 份，每次讀列都印出 proxy 給它的編號與它實際是第幾次。

修正後（從 repo 根目錄，`inject-after-fix.patch` 複製成根目錄的 `inject-916.patch`）：[after-fix.sh](data/2026-11-01-916-export-limits-proxy-race/after-fix.sh)。

1. `cargo fmt --all --check`、`cargo clippy --locked -p lab-dataset --all-targets -- -D warnings`。
2. `CI=1 cargo test --locked -p lab-dataset --test export_limits` 連續 20 次。
3. 同一個測試執行檔 10 輪、每輪同時 3 份（自己製造負載）。
4. 套上注入：第 2 次讀列的 task 在 client 拿到回應之後被拖 500 ms，跑失敗的那個測試與整個測試檔。

## 結果

### 根因

proxy 在轉送第 N 次讀列之前執行變動，N 是用「已經記錄的回應數 + 1」算的；而回應是在整段轉給 client、再等到 ClickHouse 關閉連線之後才記錄。HTTP client（`tools/migrate/src/http.rs`，hyper）依回應的長度或 chunk 結尾就拿到完整回應並回傳，不等 proxy 那一端結束；匯出接著讀這批的完成紀錄、再送下一次讀列。所以 proxy 記錄第 2 個回應的 task 只要晚一點被排程，第 3 次讀列抵達時算出的編號就是 2：變動沒有在第 3 次讀列之前執行，而是在第 4 次之前執行，寫進已經讀過的第 3 個視窗。依 `datasets/lab/README.md` 處理表的最後一列，已讀過的視窗之後才發生的變動讀不到，所以匯出成功、240 列，正是 CI 的失敗。

注入延遲重現了 CI 的輸出，內容一字不差（[before-fix.txt](data/2026-11-01-916-export-limits-proxy-race/before-fix.txt)）：

```text
PROXY rows request arrived as read 1 numbered 1
PROXY rows request arrived as read 2 numbered 2
PROXY delaying the record of read 2's answer
PROXY rows request arrived as read 3 numbered 2 MISNUMBER
PROXY rows request arrived as read 4 numbered 3 MISNUMBER
PROXY rows request arrived as read 5 numbered 4 MISNUMBER
PROXY rows request arrived as read 6 numbered 5 MISNUMBER
thread 'a_row_written_into_a_window_before_its_read_is_refused' (202673) panicked at tools/lab-dataset/tests/export_limits.rs:624:6:
rows that changed during the export are refused: Exported { dataset: "limits", ..., segments: 1, rows: 240, batches: 6, uncommitted_batches: 0, repeated_rows_dropped: 0, start: "2026-10-30T02:59:59.993Z", end: "2026-10-30T03:05:00.000Z", windows: 6 }
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 6 filtered out; finished in 3.57s
```

不注入延遲也會自然發生：同時 2 份 × 10 輪（2,100 次讀列）裡有 1 次 `read 2 numbered 1 MISNUMBER`，也就是第 1 個回應還沒記錄，第 2 次讀列就抵達了。那一次剛好沒有變動放在受影響的讀列之前，所以測試通過；同樣的延遲落在 `a_row_written_into_a_window_before_its_read_is_refused` 的第 2 個回應上，就是 CI 的失敗。（第一次跑同一個腳本時 2,100 次讀列沒有出現，所以這是低機率的競爭，和 CI 上只看到一次相符。）

排除的其他可能：

| 可能 | 結論 |
| --- | --- |
| INSERT 還沒生效就送出查詢 | 不是。proxy 等管理者的 INSERT 的 HTTP 回應完整回來才轉送讀列，測試沒有設定 `async_insert`。修正後注入延遲的那次，第 3 個視窗規劃 40 列、讀到 41 列，證明那列在讀之前就看得到。 |
| 多寫的那一列不在第 3 個視窗 | 不是。那列的 `received_at` 是第 3 批的接收時間（`start_of_data()` 加 2 分鐘，同一個 `OnceLock`），讀取時間是接收時間減 2 ms，在規劃的 `ts_min`～`ts_max` 之內；修正後的訊息 `40 rows were received from 2026-10-30T03:02:00.000Z ... 41 when it read them` 指的正是這個視窗。 |
| 匯出的偵測有破口 | 不是。變動在讀之前發生時，視窗的列數檢查會拒絕（上一列）；變動發生在已讀過的視窗是已知、已記錄的界線，測試 `a_change_in_a_window_already_read_is_not_seen` 釘住它。 |

### 修正

只改測試的 proxy（`tools/lab-dataset/tests/export_limits.rs`），匯出功能不變：

- 讀列與規劃的編號在請求抵達時給：匯出一次只送一個查詢、等到回應才送下一個，抵達順序就是匯出的順序。
- proxy 先把 ClickHouse 的回應整段讀完並記錄，才轉給 client：client 拿到回應之前，記錄已經完成，`answers()` 在匯出回傳時也一定完整（`an_hour_over_the_answer_limit_is_read_in_windows` 比對回應數與視窗數，原本也有同樣的競爭）。
- proxy 記下每個變動是在哪個查詢之前執行、那個查詢讀的是哪個接收時間（`from_ms`）；`refused_after` 與 `a_change_in_a_window_already_read_is_not_seen` 斷言每個變動都執行一次，而且放在「第 N 次讀列之前」的變動確實在第 N 個視窗（第 N 批的接收時間）被讀之前執行。編號再算錯時，測試會直接指出變動放錯位置，而不是誤報匯出沒有拒絕。

### 修正後

[after-fix.txt](data/2026-11-01-916-export-limits-proxy-race/after-fix.txt)（腳本加上結束碼檢查之後的那次；之前不檢查結束碼的版本跑過一次，結果相同：20/20、30/30、注入後拒絕）：

| 項目 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked -p lab-dataset --all-targets -- -D warnings` | 通過 |
| `CI=1 cargo test --locked -p lab-dataset --test export_limits` 連續 20 次 | 20/20 通過，每次 7 passed |
| 同一個執行檔同時 3 份 × 10 輪 | 30/30 通過 |
| 注入：第 2 次讀列的 task 在 client 拿到回應後被拖 500 ms | 第 3 次讀列編號 3，匯出被拒絕：`40 rows were received from 2026-10-30T03:02:00.000Z up to 2026-10-30T03:02:00.001Z when the export planned its reads, 41 when it read them`；整個測試檔 7 passed |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 根因寫在 PR 裡，有證據 | 通過：測試 proxy 的讀列編號依賴「已記錄的回應數」，與 client 收到回應的時間沒有先後保證；注入一個 task 的延遲即穩定重現 CI 的失敗輸出，不注入時也自然出現 1 次編號錯（上面「根因」）。 |
| 修正後 `CI=1 cargo test --locked -p lab-dataset --test export_limits` 在建置主機上重複執行至少 20 次都通過 | 通過：連續 20/20，另有同時 3 份 × 10 輪 30/30。 |
| 如果根因在匯出功能：修正功能並補測試、更新 `datasets/lab/README.md` | 不適用：根因在測試，匯出功能與 README 的處理表不變。 |
| CI 通過 | 待 PR 的 CI。 |

x86_64 建置主機上的同樣重複執行沒有做：機制與架構無關，屬於延後的進階驗證（跨平台重複驗證）。
