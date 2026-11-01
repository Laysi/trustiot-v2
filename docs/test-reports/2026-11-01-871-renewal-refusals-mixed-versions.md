# certs 換發拒絕的每日計數：新舊版服務換版、退版與並存（2026-11-01）

對應 issue：Laysi/trustiot-v2#871（來源：Laysi/trustiot-v2#847 延後的進階驗證，PR Laysi/trustiot-v2#868）。這份報告記錄以 certs 服務的兩個真實執行檔，在同一個已套用 `0008_unverified_renewal_refusals.sql` 的資料庫上實測：前一版啟動並照舊計數與回報、新版留下的每日計數列在退版期間不動、再升級後回報為一筆事件、兩版並存時各寫自己的表。

結果先講：全部通過，沒有發現與 `services/certs/README.md`「換發」的「事件」與「規則與取捨」不一致的地方。前一版（PR Laysi/trustiot-v2#868 合併前的 main）在 schema 已有 0008 的資料庫上正常啟動，把不需要金鑰的原因照舊計進 `renewal_refusals`、依 `renewal.refusal_window` 回報，完全不碰 `renewal_unverified_refusals`，連已經到期的列也不回報。再升級後，新版把那一列回報成一筆事件（一天的時間窗，次數 3，原因 `challenge_unknown` 2、`signature_invalid` 1）。兩版同時執行、各被不需要金鑰的請求打 10 次時，每版寫自己那張表的 1 列，共 2 筆事件，不是 20 筆。

## 環境

- 建置主機（x86_64 Linux）；Postgres 是測試用的拋棄式容器，schema 由本分支的 migration Job（`tools/migrate`）套用，含 0008。不需要 Kubernetes。
- 新版：本分支的 certs 執行檔（以 main `6651a296` 為基底，沒有改動服務程式，只增加測試）。
- 前一版：PR Laysi/trustiot-v2#868 合併前的 main，commit `778f417a`（`git log` 中 `b227fe8e` 的上一個 main commit），以 `git archive` 取出它自己的原始碼目錄單獨建置，執行檔不是由本分支建出來的。
- 兩版用同一份設定：`renewal.refusal_window` 1 小時（所以前一版的時間窗與新表的一天看得出差別）、`renewal.refusal_flush_interval` 1 秒、寬鬆的限流。
- 時間：「一天過去」以資料庫的 `UPDATE` 把那一列的時間窗往前移 2 天（`renewal_refusals` 的列往前移 3 小時）代替，讓它在下一輪回報時已到期。列本身、計數與回報都是服務真實執行的。

## 指令

```sh
# repo 根目錄。先取出前一版的原始碼，單獨建置它的執行檔
PREVIOUS="$(mktemp -d)"   # outside the repository
git archive 778f417a | tar -x -C "$PREVIOUS"
(cd "$PREVIOUS" && CARGO_TARGET_DIR="$PREVIOUS/target" cargo build --locked -p certs --bin certs)

# 測試預設被 #[ignore]，因為需要兩個執行檔；TRUSTIOT_PREVIOUS_CERTS 指向前一版的 certs
TRUSTIOT_PREVIOUS_CERTS="$PREVIOUS/target/debug/certs" \
  CI=1 cargo test --locked -p certs --test renewal_mixed_versions -- --ignored --nocapture
```

前一版的原始碼與建置輸出放在 repo 外的暫存目錄 `$PREVIOUS`，跑完刪除（本次實際是放在建置主機上的工作目錄、未進 repo，跑完已刪除）。測試是 `services/certs/tests/renewal_mixed_versions.rs` 的 `the_previous_version_and_this_one_share_the_refusal_counts`，沿用 `tests/renewal.rs` 的做法（真實 certs 執行檔、`testkit::product` 的 schema、經出廠登錄 API 登錄的閘道器），閘道器的金鑰是軟體 P-256 金鑰。

測試的步驟（每步的「新」是本分支的執行檔，「舊」是前一版）：

1. 新：`mixed-a` 被 2 次 `challenge_unknown`（409）與 1 次 `signature_invalid`（401）拒絕。
2. 停掉新版，把 `mixed-a` 在新表的列移到 2 天前（已到期）。啟動舊版：它在有 0008 的 schema 上啟動並就緒；`mixed-a` 再被 3 次 `challenge_unknown`、`mixed-b`（簽章驗過、未綁定）被 1 次 `not_bound` 拒絕。把舊版寫的列移到 3 小時前，等它回報。
3. 停掉舊版，啟動新版：等新表那一列被回報。
4. 新舊兩版同時執行，各對 `mixed-c` 送 10 次 `challenge_unknown`；把兩張表的列移到過去，等回報。

## 結果

原始輸出：[`data/2026-11-01-871-renewal-refusals-mixed-versions/test-output.txt`](data/2026-11-01-871-renewal-refusals-mixed-versions/test-output.txt)。表中的「窗」是時間窗的秒數，「新表」是 `renewal_unverified_refusals`，「舊表」是 `renewal_refusals`。

| 步驟 | 檢查 | 結果 |
| --- | --- | --- |
| 1 | `mixed-a` 的計數列（新） | 新表 1 列，窗 86400，次數 3，原因 `{"challenge_unknown": 2, "signature_invalid": 1}`；舊表沒有列 |
| 2 | 舊版在已有 0008 的 schema 上 | 啟動、`/readyz` 200、對 `mixed-ghost`（沒登錄）的請求有決定（不是 503） |
| 2 | `mixed-a` 的計數列（舊） | 舊表新增 1 列：窗 3600，次數 3，原因 `{"challenge_unknown": 3}`（不需要金鑰的原因照舊計進舊表）；新表那一列不變 |
| 2 | `mixed-b` 的計數列（舊） | 舊表 1 列：窗 3600，次數 1，`{"not_bound": 1}` |
| 2 | 舊版的回報（兩列移到過去之後） | `mixed-a` 1 筆事件（窗 3600、次數 3、`challenge_unknown` 3）、`mixed-b` 1 筆事件（窗 3600、次數 1、`not_bound` 1） |
| 2 | 新表那一列（窗已到期 2 天，舊版的回報已跑過） | 與回報前完全相同：時間窗、次數、原因與 `xmin` 都沒變，所以舊版沒有改寫或刪除它（它到期卻沒有被回報，也表示舊版的回報沒有取走它） |
| 2 | 舊版的 `could not count`、`could not report` 日誌 | 0 行 |
| 3 | 新版啟動後 `mixed-a` 的事件 | 2 筆：舊版寫的那筆，加上新表那一列的一筆（窗 86400、次數 3、`{"challenge_unknown": 2, "signature_invalid": 1}`）；新表沒有列了；`mixed-b` 仍是 1 筆 |
| 4 | 兩版並存後 `mixed-c` 的計數列 | 新表 1 列（窗 86400、次數 10），舊表 1 列（窗 3600、次數 10）；各版只寫自己的表，沒有一張表有 20。測試容許跨整點或跨 UTC 午夜時每張表最多 2 列，這次沒有跨 |
| 4 | 兩列移到過去之後 `mixed-c` 的事件 | 2 筆：窗 86400 次數 10、窗 3600 次數 10（事件數等於列數）；兩張表都清空；兩版的失敗日誌 0 行 |

### 兩版並存的上限

README 寫的上限是「新舊並存時預設每台每天最多 25 筆」：舊版把不需要金鑰的原因與簽章驗過之後的原因一起計進 `renewal_refusals`，每個 `renewal.refusal_window`（預設 1 小時）一列，一天最多 24 列；新版另有 `renewal_unverified_refusals` 每天 1 列。步驟 4 實測了其中兩版各自寫自己的表、同一台閘道器 20 次被拒只有 2 筆事件（舊表一個小時窗、新表一個一天窗）。

「最多 24 加 1」本身沒有另外跑滿 24 個小時窗：每一列由 `(hardware_id, window_start)` 的主鍵與固定長度的時間窗決定，舊表的列數是時間窗數、新表的列數是天數，與請求數無關，所以這個上限來自兩張表的主鍵，不是程式的分支。要實際看到 24 列，得連續跑多個時間窗（例如把 `renewal.refusal_window` 調到最小的 1 分鐘並等待），那是重複的壓力測試，已列為延後。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 前一版的 certs 在已套用 0008 的資料庫上啟動，換發的拒絕照常計數與回報（實測） | 通過（步驟 2） |
| 新版留下的每日計數列在退版期間保留，再升級後回報為一筆事件，次數與原因正確（實測） | 通過（步驟 2、3）。限制：「一天過去」是把時間窗往前移，不是真的等一天 |
| 兩版並存時的事件數符合 `services/certs/README.md` 寫的上限（實測，或寫明為什麼不需要） | 通過（步驟 4：各寫自己的表，20 次拒絕 2 筆事件）；24 加 1 的上限由兩張表的主鍵決定，沒有跑滿 24 個小時窗，理由見上 |
| 指令與結果寫進 `docs/test-reports/` | 本報告 |

沒有發現 README 與實際行為不一致。

## 沒做的事

- 用 `renewal.refusal_window` 的最小值連跑多個時間窗，數出舊表一天最多 24 列：重複的壓力測試，延後。
- 這個測試是 `#[ignore]`，不在 CI 跑：它需要前一版的執行檔，而「前一版」固定是 PR Laysi/trustiot-v2#868 合併前的 commit。之後的換版若要同樣驗證，要改指向那時的前一版。
