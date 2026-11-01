# certs 換發：退還額度的故障注入、並行名額的負載量測、被拒事件的寫入量（2026-11-01）

Laysi/trustiot-v2#846（自 Laysi/trustiot-v2#801、PR Laysi/trustiot-v2#845 延後的進階驗證）。規則與取捨在 [services/certs/README.md](../../services/certs/README.md) 的「換發」（「限流與資源上限」「事件」「規則與取捨」）；基本驗證在 [2026-10-31-801-renewal-abuse-limits.md](2026-10-31-801-renewal-abuse-limits.md)。

這份報告只量測、不改規則。量測和 README 不一致的地方寫在「結論」的「與 README 的差異」，並列為後續提案。

## 環境

- **程式碼**：分支 `test/846-certs-renewal-fault-load`，基於 main `303e60ee`（`services/certs/tests/renewal_faults.rs`、`services/certs/tests/renewal_load.rs`、`services/certs/tests/support/renewal.rs`）。
- **機器**：建置主機（x86_64 Linux）跑了故障注入與兩次負載與寫入量的量測；最後的整個 workspace 測試在另一台建置主機（arm64 Linux）上跑過。所有測試都以 `CI=1` 執行，需要容器的測試不略過：容器起 `testkit::product` 的拋棄式 TLS Postgres，以 migration Job 的程式套用整個產品的 schema，再以 `certs_app` 與 `certs_platform` 啟動真的 `certs` 執行檔。閘道器的 TPM 以軟體 P-256 金鑰代替，指標以 OTLP/HTTP 送到測試裡的 collector 替身。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust。
- **設定**：
  - 故障注入：每個硬體 ID 一次 2 個簽章驗過的兌換、之後每小時 1 個（`renewal.gateway_burst` 2、`renewal.gateway_interval` 1h），其餘預設。
  - 並行名額：`renewal.max_concurrent` 用預設的 2；`renewal.gateway_burst` 100、`renewal.global_burst` 10000、`renewal.global_interval` 1ms，讓全域限流與每個硬體 ID 的限流都不會擋住量測，只剩並行名額決定結果。
  - 寫入量：`renewal.gateway_burst` 100、全域限流同上、`renewal.refusal_flush_interval` 1s；`renewal.refusal_window` 分別是 1h、24h（最長）與 60s（最短，真實時間）。
- 量測的數字與主機的負載有關，同一組指令跑兩次不會得到同樣的數字（見「結果」的兩次執行）；測試只斷言結構。

## 指令

在 repo 根目錄：

```sh
# 退還的故障注入（一般測試，CI 會跑）
CI=1 cargo test --locked -p certs --test renewal_faults -- --nocapture

# 負載與寫入量的量測（`#[ignore]`，不在 CI；印出 `measure:` 開頭的資料行）
CI=1 cargo test --locked -p certs --test renewal_load -- --ignored --nocapture --test-threads=1

# 其餘檢查
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=1 cargo test --locked --workspace --exclude agent
```

## 結果

### 1. 簽發時資料庫失敗：退還額度

原始資料：[refund-fault-injection.txt](data/2026-11-01-846-renewal-faults-load-volume/refund-fault-injection.txt)。

做法：簽章驗過之後，以超級使用者暫時拿掉 `certs_app` 的權限，使簽發交易失敗；權限還回去之後同一台閘道器再換發。每個故障 3 次兌換（超過 burst 2），第二次兌換之前等 `Retry-After` 指定的 5 秒（像閘道器做的）。

| 故障 | 兌換的回應 | 簽發紀錄 | 租戶事件 |
| --- | --- | --- | --- |
| 拿掉 `issued_certificates` 的 `INSERT`：第一個寫入的語句被拒 | 3 次都是 503、`code` `unavailable`、`Retry-After: 5`、`no-store` | 1 列，前後不變 | 0 |
| 拿掉 `outbox_event` 的 `INSERT`：簽發紀錄已經在交易裡寫入，事件寫不進去 | 同上 | 前後不變；資料庫的 `n_tup_ins` 顯示 3 次 INSERT 發生過、整個交易 rollback | 0 |

- 6 次失敗之後，權限還回去：同一台閘道器換發 2 次（burst）都是 201，簽發紀錄多 2 列、租戶事件 2 筆（各與簽發紀錄在同一個交易）；第 3 次才是 429（`Retry-After` 3600，`gateway_rate_limited`）。所以 6 次失敗都全額退還，而且退還沒有超過 burst：沒有退還的話，權限一回來第 1 次換發就是 429；退過頭的話，第 3 次會成功。
- 日誌與指標：6 行 `unavailable`、1 行 `gateway_rate_limited`；`certs.renewal.refused{reason="unavailable"}` 是 6、`{reason="gateway_rate_limited"}` 是 1。`unavailable` 不計入被拒的計數：`renewal_refusals` 只有 `{"gateway_rate_limited": 1}`，事件的 `reasons` 沒有 `unavailable`。
- 故障之後，不帶金鑰的請求（捏造的 nonce、別的金鑰簽的兌換）仍照原本的規則回 409、401；它們不用硬體 ID 自己的額度，由 `tests/renewal.rs` 的 `rl-ok` 案例負責。

### 2. 負載下的並行名額（`renewal.max_concurrent` 預設 2）

原始資料：[concurrency-run2.jsonl](data/2026-11-01-846-renewal-faults-load-volume/concurrency-run2.jsonl)（最後的一次，5 個等級）、[concurrency-run1.jsonl](data/2026-11-01-846-renewal-faults-load-volume/concurrency-run1.jsonl)（第一次，4 個等級）、[concurrency-run2-busy-metric.jsonl](data/2026-11-01-846-renewal-faults-load-volume/concurrency-run2-busy-metric.jsonl)。

做法：每個等級先為一批已登錄、已綁定、租戶正常的閘道器各取好挑戰（每台最多 50 個），再**同時**送出全部的簽章兌換（每個都做完整的資料庫工作：讀登錄、驗簽章、簽發、寫紀錄與事件）；同時有一台「真正的閘道器」照 `Retry-After` 從取挑戰開始重試，直到拿到憑證。一台閘道器單獨連續換發 10 次的回應時間是中位數 18 ms、最大 25 ms。

最後一次（第二次）執行：

| 同時送出 | 201 | 503 `busy` | 全部回應完成 | 201 的回應時間 中位數 / p95 / 最大 | 真正的閘道器 |
| --- | --- | --- | --- | --- | --- |
| 20 | 20 | 0 | 108 ms | 58 / 100 / 106 ms | 第一次就 201，109 ms |
| 100 | 100 | 0 | 496 ms | 263 / 472 / 493 ms | 第一次就 201，469 ms |
| 300 | 300 | 0 | 1.39 s | 697 / 1265 / 1370 ms | 第一次就 201，1.37 s |
| 450 | 450 | 0 | 2.03 s | 1025 / 1916 / 2014 ms | 第一次就 201，2.02 s |
| 600 | 591 | 9（1.5%） | 2.67 s | 1318 / 2512 / 2659 ms | 第一次就 201，2.62 s |

- 完成簽發的速度是每秒約 185 到 222 個（兩個名額輪流做，每個兌換佔用一個名額約 9 到 14 ms（兩次執行合計））。
- 輪不到名額的請求等到 2 秒才得到 503：`busy` 的回應時間中位數 2011 ms、最大 2052 ms；全部帶 `Retry-After: 1`。
- 第一次執行的最高等級（同時 600）：450 個 201、150 個 503（25%）；`busy` 的回應時間中位數 2050 ms、最大 2078 ms；真正的閘道器的**取挑戰**得到 503（`busy`），1 秒後重試成功，從開始到拿到憑證 3.03 s。這是 README「佔滿名額的效果是整個副本」的實測：`busy` 不分請求種類，連取挑戰也等不到名額；閘道器照 `Retry-After` 重試就恢復。其餘等級（20 到 300）第一次執行也沒有 `busy`。
- 兩次執行的 `busy` 比例差很多（25% 與 1.5%），因為門檻取決於當時的處理速度：同時排隊的請求超過「2 秒乘以每秒完成的數量」（約 300 到 450 個）才有人等滿 2 秒。
- 檢查：每個回應不是 201 就是 `Retry-After: 1` 的 503；每個等級簽發紀錄的新列數等於 201 的數量加真正閘道器的一張，`busy` 沒有留下任何列；`renewal_refusals` 與 `renewal_unverified_refusals` 沒有這些硬體 ID 的列（`busy` 不計入被拒的計數）；`certs.renewal.refused{reason="busy"}` 的值等於客戶端看到的 503（兌換那一步 9，取挑戰那一步 0；測試只斷言兩步中較大的那個數字，取挑戰那一步為 0 是由客戶端看到的回應推算）。

### 3. 被拒事件的寫入量

原始資料：[refusal-volume.jsonl](data/2026-11-01-846-renewal-faults-load-volume/refusal-volume.jsonl)（最後一次）、[refusal-volume-run1.jsonl](data/2026-11-01-846-renewal-faults-load-volume/refusal-volume-run1.jsonl)（第一次）。

做法：20 個已登錄的硬體 ID 綁定到一個停權的租戶。每一輪，每個硬體 ID 都被 7 個請求拒絕：3 個捏造的 nonce（`challenge_unknown`）與 1 個別的金鑰簽的兌換（`signature_invalid`），都不需要金鑰；3 個以自己金鑰簽的兌換，簽章驗過之後租戶是停權的（`tenant_suspended`）。三個 `renewal.refusal_window`：1h 與 24h 各 3 輪（每個硬體 ID 21 次拒絕、共 420 個請求），60s 做 6 輪、每輪相隔 15 秒（840 個請求，跨過至少兩個時間窗，用真實的時間等事件出現）。1h 與 24h 的時間窗以「把列的時間窗往前移兩天」結束（`tests/renewal.rs` 同樣的做法，事件立刻出現）；不需要金鑰的那一類時間窗是 UTC 的一天，三種設定都以這個方式結束。

| `renewal.refusal_window` | 請求 | `renewal_refusals` 的列（簽章驗過之後） | `renewal_unverified_refusals` 的列（不需要金鑰，一天） | 事件 `certs.gateway_renewal_refused` | 事件的次數加總 | 同一個硬體 ID、類別、時間窗的事件數最大值 |
| --- | --- | --- | --- | --- | --- | --- |
| 1h | 420 | 20（1 個時間窗） | 20（1 個時間窗） | 20 + 20 = 40 | 420（等於請求數） | 1 |
| 24h | 420 | 20（1 個時間窗） | 20（1 個時間窗） | 20 + 20 = 40 | 420 | 1 |
| 60s（第二次） | 840 | 最多同時 40（2 個時間窗），共 40 | 20（1 個時間窗） | 40 + 20 = 60 | 840 | 1 |
| 60s（第一次） | 840 | 最多同時 40，共 60（3 個時間窗） | 20（1 個時間窗） | 60 + 20 = 80 | 840 | 1 |

- 事件的時間窗長度：簽章驗過之後的是 `renewal.refusal_window`（3600、86400、60 秒），不需要金鑰的一律 86400 秒。
- 寫出事件之後兩張表裡這些硬體 ID 的列都被刪掉（沒有留下列、沒有第二個事件）。60s 的時間窗結束後約 60 秒的回報延遲再加一輪 `renewal.refusal_flush_interval`：第二次執行最後一輪之後 76 秒、第一次執行 116 秒所有事件都出現。
- 一筆事件的 envelope 平均約 415 bytes（414 到 418）。依此推算：一把登錄金鑰在預設 1 小時的時間窗內被拒，每天最多 24 筆、約 10 KB；沒有金鑰的人讓同一台每天最多 1 筆、約 0.4 KB；`renewal.refusal_window` 24h 時簽章驗過之後的那一類也是每天 1 筆。
- 60s 時間窗的列數：第二次執行最多同時 40 列，與 README「列數最多是已登錄的閘道器數乘以還沒回報的時間窗數」一致（結束的時間窗在回報前還要等 60 秒，所以與下一個時間窗的列同時存在）；第一次執行跨了 3 個時間窗，總共 60 筆事件。時間窗從 2000-01-01T00:00:00Z 起對齊，一次執行跨幾個時間窗取決於開始的時間。
- 每個請求（共 420 或 840 個）都被拒絕並計數，沒有 503 或 429：客戶端看到的被拒次數等於兩張表的列與事件加起來的次數，沒有少算。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 簽發時資料庫失敗的換發回 503；閘道器照 `Retry-After` 重試沒有得到 `gateway_rate_limited`；資料庫恢復後換發成功，簽發紀錄與事件沒有只寫了一半（測試，證據在 `docs/test-reports/`） | 通過：自動化測試 `tests/renewal_faults.rs`（會在 CI 執行）。兩種故障各 3 次、共 6 次 503（`Retry-After: 5`）都沒有 429；簽發紀錄與租戶事件都沒有留下（第二個故障的簽發紀錄 INSERT 發生過並隨交易 rollback）；權限恢復後換發 2 次成功、第 3 次才 429，退還不超過 burst |
| 佔滿 `renewal.max_concurrent` 時，其他請求得到 503（`busy`）的比例、等待時間與真正閘道器的換發結果記在 `docs/test-reports/` | 通過（量測）：上面的「2.」。全部回應 201 或 `busy`；`busy` 等 2 秒（中位數 2.01 到 2.05 s）；真正的閘道器在 `busy` 時照 `Retry-After` 重試成功 |
| 被拒事件的實際寫入量與 README 的上限一致（預設時間窗與調長後各一次），記在 `docs/test-reports/` | 通過（量測）：上面的「3.」。每個已登錄的硬體 ID、類別與時間窗最多一列、一筆事件；事件的次數加總等於請求數；調長 `renewal.refusal_window` 到 24h 後簽章驗過之後的那一類也是每個硬體 ID 一筆；不需要金鑰的那一類不受 `renewal.refusal_window` 影響，固定一天 |

### 與 README 的差異

量測支持 README 的描述，只有一點需要補充，沒有改規則：

- **「在全域速率以內送會做資料庫工作的請求、佔滿 `renewal.max_concurrent` 的名額」在預設的限流下做不到（本主機）。** README「規則與取捨」的「還剩下什麼」寫這樣做的效果是整個副本：其他請求等 2 秒後得到 503（`busy`）。效果（整個副本、等 2 秒、`Retry-After: 1`）量到了，但要出現 `busy`，同時等待的請求要超過約「2 秒乘以每秒完成的數量」（本主機約 300 到 450 個）。預設的全域限流每一步各一份（取挑戰與兌換都要名額）：每一步一次 100 個、之後每秒 20 個，所以兩步合計同時最多 200 個進得來、持續每秒 40 個。同時 100 個的量測是中位數 263 到 328 ms、最大 493 到 695 ms，每個兌換佔用名額約 9 到 14 ms；以每秒 139 到 222 個的完成速度，200 個同時約 0.9 到 1.4 秒做完，低於 2 秒的等待上限（這是由量測推算的，沒有另外跑兩步洪水同時來的情況；取挑戰比兌換便宜），持續每秒 40 個所需的佔用時間門檻是 50 ms（2 個名額除以每秒 40 個），遠高於 9 到 14 ms。所以預設限流下，`busy` 只會在每個請求的資料庫工作明顯變慢時出現（資料庫負載、等鎖：平台唯讀交易的 `lock_timeout` 是 2 秒、`statement_timeout` 是 5 秒，名額最多被佔住那麼久），或全域限流被調高時（本測試調高到 10000）。上面的量測以調高全域限流模擬「資料庫工作很慢」的結果；預設限流下的結論是由同時 100 個沒有 `busy` 的量測與預設值推算的，沒有另外跑預設限流的洪水。

### 後續提案（不在本 issue 的範圍）

- README「還剩下什麼」的那一句改為「在全域速率以內，每個請求的資料庫工作慢到超過 `max_concurrent` 除以兩步全域速率的和（預設 50 ms）時，名額會被佔滿」，並引用本報告；或補一個真正拖慢資料庫工作（例如另一個交易拿住鎖）的測試，量預設限流下的 `busy`。這是文字與驗證的修正，不是規則的改變。
- 若要把 `busy` 的告警（Laysi/trustiot-v2#183）設成「整個副本被佔滿」的訊號：`certs.renewal.refused{reason="busy"}` 的 `step` 屬性分得出是取挑戰還是兌換；本量測中閘道器的取挑戰也會被 `busy` 擋下。
