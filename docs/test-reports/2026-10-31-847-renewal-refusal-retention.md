# certs：不需要金鑰的換發拒絕改為一天一筆事件（2026-10-31）

Laysi/trustiot-v2#847：換發端點不需要金鑰就能造成的拒絕（`challenge_unknown`、`challenge_mismatch`、`challenge_expired`、`signature_invalid`）原本與其他原因共用 `renewal.refusal_window`（預設 1 小時），知道已登錄硬體 ID 的人能讓每台閘道器每天寫 24 筆永久保存的 `certs.gateway_renewal_refused`。決定改在 certs 這一端：這四個原因另存 `certs.renewal_unverified_refusals`，時間窗固定一天；決策 34（平台事件紀錄不自動刪）不變。決定、寫入量與保存期限在 [services/certs/README.md](../../services/certs/README.md) 的「換發」（「事件」與「規則與取捨」的「未經驗證的流量也會寫出被拒的事件」）。

## 環境

- **程式碼**：分支 `feat/847-renewal-refusal-retention`，基於 main `e448cc02`。
- **機器**：建置主機，以 `CI=1` 執行。`services/certs/tests/renewal.rs` 的證據來自建置主機（arm64 Linux VM）；整個 workspace 的測試、OpenAPI 與前端的檢查在建置主機（x86_64 Linux）上執行。需要容器的測試不略過：podman 起 `testkit::product` 的拋棄式 TLS Postgres，以 migration Job 的程式套用整個產品的 schema（含 `0008_unverified_renewal_refusals.sql`），再以 `certs_app` 與 `certs_platform` 啟動真的 `certs` 執行檔。閘道器的 TPM 以軟體 P-256 金鑰代替。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 與 Bun。
- **設定**：`renewal_issues_for_the_registered_key_and_refuses_the_rest` 設 `renewal.refusal_window` 為 1 小時、`renewal.max_concurrent` 為 3（上限）、`renewal.refusal_flush_interval` 為 1 秒；`rate_limits_answer_429_and_bound_the_challenges` 用預設的 1 小時。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p certs --lib renewal
CI=1 cargo test --locked -p certs --test renewal -- --nocapture
cargo run --locked -p openapi -- --check
CI=1 cargo test --locked --workspace --exclude agent --no-fail-fast
bun install --cwd frontend --frozen-lockfile && bun run --cwd frontend test
```

## 結果

`services/certs/tests/renewal.rs` 的證據行（`-- --nocapture`，節錄與本 issue 有關的）：

```text
evidence: refusal windows (unverified, seconds, refusals, reasons) of rn-ok [(true, 86400, 7, "{\"challenge_expired\": 1, \"challenge_unknown\": 3, \"signature_invalid\": 1, \"challenge_mismatch\": 2}")], of rn-dec [(false, 3600, 1, "{\"decommissioned\": 1}")]
evidence: the grants (platform, platform extra, app) (true, false, false); error returned from database: new row for relation "renewal_unverified_refusals" violates check constraint "renewal_unverified_refusals_reasons_check1" at line 2076
evidence: 40 made-up redemptions of rn-burst at once: 40 times 409, the rest 503 busy; refusal windows [(true, 86400, 40, "{\"challenge_unknown\": 40}")]
evidence: refusal event {"hardware_id":"rn-ok","reasons":{"challenge_expired":1,"challenge_mismatch":2,"challenge_unknown":3,"signature_invalid":1},"refusals":7,"window_end":"2026-10-30T00:00:00Z","window_start":"2026-10-29T00:00:00Z"}
evidence: refusal event {"hardware_id":"rn-dec","reasons":{"decommissioned":1},"refusals":1,"window_end":"2026-10-29T06:00:00Z","window_start":"2026-10-29T05:00:00Z"}
evidence: refusal event {"hardware_id":"rn-burst","reasons":{"challenge_unknown":40},"refusals":40,"window_end":"2026-10-30T00:00:00Z","window_start":"2026-10-29T00:00:00Z"}
evidence: refusal windows of rl-ok [(false, 3600, 1, "{\"gateway_rate_limited\": 1}"), (true, 86400, 5, "{\"challenge_unknown\": 3, \"signature_invalid\": 2}")]
test rate_limits_answer_429_and_bound_the_challenges ... ok
test renewal_issues_for_the_registered_key_and_refuses_the_rest ... ok
test over_the_limits_a_stale_replica_logs_no_refusal_at_info ... ok
test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 18.05s
```

測試把時間窗往前移兩天讓它結束，所以事件的日期是 2026-10-29。

- **兩類分開計**：`rn-ok` 的 7 次拒絕都是不需要金鑰的原因，全在 `renewal_unverified_refusals` 的一列、時間窗 86,400 秒（一天）；`rn-dec`（除役）、`rn-never`（沒有綁定）、`rn-susp`（停權）各一次，在 `renewal_refusals`、時間窗 3,600 秒（設定的 1 小時）。`rl-ok` 同一個硬體 ID 兩類都有：簽章驗過之後的 `gateway_rate_limited` 在 1 小時的時間窗，沒有金鑰的 3 次 `challenge_unknown` 與 2 次 `signature_invalid` 在一天的時間窗。
- **事件**：每個時間窗一筆，事件的時間窗就是那一類的長度（`rn-ok` 與 `rn-burst` 一天、`rn-dec` 一小時），次數與原因和計數相同；回報之後計數的列刪掉，再過一輪不會多寫（沿用原本的檢查）。payload 的欄位不變。
- **同時大量拒絕**：同一個硬體 ID 同時 40 個捏造 nonce 的兌換（服務一次讓 3 個做資料庫工作），40 個都回 409，計數是一列 40 次，回報成一筆 `refusals: 40` 的事件：沒有第二列、沒有少算。
- **權限與限制**：`certs_platform` 對新表有 `SELECT`、`INSERT`、`UPDATE`、`DELETE`（最後的樹逐一檢查這四個），沒有 `TRUNCATE`、`REFERENCES`、`TRIGGER`；`certs_app` 什麼都沒有。以 superuser 寫入簽章驗過之後的原因（`not_bound`）被 `CHECK` 拒絕（SQLSTATE 23514）。
- **單元測試**：`only_decisions_about_the_redemption_are_counted` 確認每個計入的原因屬於哪一類，四個不需要金鑰的原因正好是 `CHECK` 允許的那四個。

workspace 的測試、`openapi -- --check` 與前端的測試：見下方「整體檢查」。

## 整體檢查

建置主機（x86_64 Linux），`CI=1`：

| 檢查 | 樹 | 結果 |
| --- | --- | --- |
| `cargo run --locked -p openapi -- --check` | `3b79db8c` | `api/openapi.json is up to date` |
| `CI=1 cargo test --locked --workspace --exclude agent --no-fail-fast` | `3b79db8c` | 全部通過：各測試執行檔合計 2,078 個測試，0 失敗 |
| `bun run --cwd frontend test` | `3b79db8c` | 252 pass、0 fail |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`openapi -- --check`、workspace 測試 | `27bb7c43`（最後的程式碼） | 全部通過：fmt、clippy 沒有警告，`api/openapi.json is up to date`，workspace 合計 2,078 個測試、0 失敗 |

`3b79db8c` 之後的提交只改了文件與 `UNVERIFIED_GRANTS` 這個測試查詢（安全審查 L2：`has_table_privilege` 一次列多個權限時有其一就是 true，所以平台角色必須有的四個權限改成逐一檢查），所以最後的樹再跑一次 workspace 測試確認。

## 結論

- 驗收條件 1（決定寫進 README，寫明最壞情況的寫入量與保存期限）：通過。README「換發」的「規則與取捨」寫明決定、每台與 1 萬台每天與每年的最壞寫入量（沒有金鑰：每台每天 1 筆、1 萬台每年約 365 萬筆；簽章驗過之後：預設每台每天 24 筆）與保存期限（永久，決策 34）。
- 驗收條件 2（改變事件的保存規則時同步更新決策 34 與身分服務 README）：不適用。保存規則沒有改，決策 34 與 `services/identity/README.md` 不變。
- 驗收條件 3（改程式時有測試，`CI=1 cargo test -p certs` 通過；沒有動到身分服務）：通過，見上方結果與整體檢查。
- 待測（延後的進階驗證）：真實流量下的寫入量量測（Laysi/trustiot-v2#846）；以前一版服務的執行檔對套用了 0008 的資料庫實際跑一次換版與退版（目前以前一版的 SQL 不碰新表、主鍵不變推論相容）。
