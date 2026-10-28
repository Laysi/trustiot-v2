# certs：HTTPS 憑證換發端點（2026-10-29）

Laysi/trustiot-v2#81：閘道器以 TPM 金鑰簽一次性挑戰，經公開路徑 `POST /certs/renew` 換發閘道器憑證（清單 D2、決策 12；流程與規則見 [services/certs/README.md](../../services/certs/README.md) 的「換發」）。這份報告是驗收條件的證據：全部由下列自動化測試以 `CI=1` 實際執行（需要容器的測試不略過），閘道器的 TPM 以軟體 P-256 金鑰代替。

## 環境

- **程式碼**：分支 `feat/81-cert-renewal`。功能 commit 先是 `8f49bbad`（基於 main `5dad0429`），之後 rebase 到 main `9d879e95`（Laysi/trustiot-v2#722，只改閘道器 agent 與 `crates/protocol/README.md`，沒有衝突）成為 `21218ce1`，內容相同。本報告的 commit 只加文件與測試輸出。
- **機器**：建置主機，經檢查佇列執行：`certs` 的換發測試與證據行在建置主機（macOS，aarch64，`CI=1`）；整個 workspace 的檢查在建置主機（x86_64 Linux）。容器測試由 podman 起 `testkit::product` 的拋棄式 TLS Postgres，以 migration Job 的程式套用整個產品的 schema，再以 `certs_app` 與 `certs_platform` 啟動真的 `certs` 執行檔。`tests/renewal_lag.rs` 是自己的測試執行檔、自己的資料庫。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2、cargo-deny。
- **設定**：`CI=1`。換發的設定以環境變數給（各測試檔開頭）：情境測試放寬限流、時間窗 1 天、每秒回報；限流測試每個硬體 ID 2 個、每小時一個，全域 6 個、每分鐘一個；lag 測試 `TRUSTIOT__RENEWAL__MAX_CONSUMER_LAG=1s`。指標以 OTLP/HTTP 送到測試裡的 collector 替身。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -p openapi -- --check
CI=1 cargo test --locked --workspace --exclude agent
CI=1 cargo test --locked -p agent
cargo deny --locked check
cargo build --locked --target aarch64-unknown-linux-gnu -p protocol
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend test
bun test deploy/chart
```

驗收證據可以單獨重跑：`CI=1 cargo test -p certs --test renewal --test renewal_lag -- --nocapture`。

## 結果

| 指令 | 在哪裡、哪個 commit | 結果 |
| --- | --- | --- |
| `cargo clippy --locked -p certs -p protocol -p contract --all-targets -- -D warnings`，`cargo test --locked -p protocol -p contract`，`cargo test --locked -p certs -- --nocapture` | 建置主機（macOS），`8f49bbad` 的工作樹 | clippy 通過；protocol、contract 與 certs 的測試（含 `renewal` 2 個）都通過，只有第一版的 `renewal_lag` 失敗：它輪詢時撞到每個硬體 ID 的限流（429）。測試改為放寬該限流、把 429 當作「還沒」之後通過（下一列） |
| `cargo test --locked -p certs --test renewal_lag -- --nocapture` | 建置主機（macOS），`8f49bbad` 的工作樹 | 1 個通過（17.55 秒） |
| `cargo fmt --all --check`，`cargo clippy --locked --workspace --all-targets -- -D warnings`，`cargo run --locked -p openapi -- --check`，`cargo test --locked --workspace --exclude agent`，`cargo test --locked -p agent`，`cargo deny --locked check` | 建置主機（x86_64 Linux），`8f49bbad` | 全部通過：`api/openapi.json is up to date`；workspace 1766 個測試、agent 291 個測試通過，0 失敗（唯一的 ignored 是 assistant 既有的 doctest）；`renewal` 2 個與 `renewal_lag` 1 個也在其中；cargo-deny `advisories ok, bans ok, licenses ok, sources ok` |
| `cargo build --locked --target aarch64-unknown-linux-gnu -p protocol`，`bun install --cwd frontend --frozen-lockfile && bun run --cwd frontend test`，`bun test deploy/chart` | 建置主機（x86_64 Linux），`21218ce1` | 全部通過：aarch64 建置完成；前端 218 個測試通過（含每個 Rust 訊息鍵都有 zh-TW 與 en 翻譯的檢查）；chart 659 個測試通過 |

證據行（`--nocapture`）在 [renewal-tests.txt](data/2026-10-29-81-cert-renewal/renewal-tests.txt)：

- 換發：`rn-ok` 唯一的憑證撤銷之後換發成功，序號 `43808b9e...`、CN `rn-ok`、登錄的公鑰、鏈到根；簽發紀錄 1 → 2；一筆租戶事件，它的交易 ID（900）與簽發紀錄那一列相同；讀取的日誌帶 `hardware_id`、`tenant_id`、`binding_id`。
- 拒絕：除役 403 `decommissioned`、沒綁定過與綁定已結束 403 `not_bound`、租戶停權 403 `tenant_suspended`，四台的簽發紀錄都還是 1、沒有事件。
- 挑戰：重送 409、別的金鑰簽 401 後同一個挑戰 409、別台的挑戰 403 後那一台 409、別的用途 403、過期 410 後 409；沒登錄的硬體 ID 拿到挑戰、兌換 401；格式錯 422。
- 指標：`certs.renewal.refused` 依原因 `challenge_unknown` 4、`signature_invalid` 2、`challenge_mismatch` 2、`challenge_expired` 1、`decommissioned` 1、`not_bound` 2、`tenant_suspended` 1、`invalid_request` 1；`certs.renewal.issued` 1。
- 時間窗：`rn-ok` 的 7 次拒絕合併成一筆全平台事件 `{"refusals":7,"reasons":{"challenge_expired":1,"challenge_mismatch":2,"challenge_unknown":3,"signature_invalid":1}}`，回報後列刪除、3 秒後仍只有一筆；沒登錄的 `rn-ghost` 沒有列、沒有事件。
- 限流：每個硬體 ID 超過後 429、`Retry-After: 3600`；全域超過後 50 個新硬體 ID 都 429，資料庫只存下限流放行的 6 個挑戰。
- lag：平台角色登不進去（lag 不明）的副本兩步都 503 `bindings_stale`、`Retry-After: 30`，挑戰沒有作廢、正常的副本之後兌換成功；消費端停在不認得的綁定事件、lag 超過 1 秒後兩步都 503，兌換沒有作廢挑戰。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 已登錄、已綁定、租戶正常的閘道器以正確的簽章換到新憑證：CN 是硬體 ID、公鑰是登錄的那一把、鏈到 `root_certificate`；目前憑證已過期或已撤銷時同樣成功 | 通過：`tests/renewal.rs` 撤銷閘道器唯一的憑證後換發，CN、公鑰、鏈與 rustls 的用戶端憑證驗證都通過。已過期的情況沒有另外測：端點不讀閘道器現有的憑證（請求不帶憑證、不經 mTLS），撤銷與過期走的是同一條路徑 |
| 除役、未綁定、租戶停權時拒絕，不簽憑證、不寫簽發紀錄 | 通過：除役、沒綁定過、綁定已結束、停權（測試替身）都 403，簽發紀錄不變、沒有事件 |
| 重送 409、竄改的簽章 401、別台設備或別的用途的挑戰 403、過期 410，失敗的兌換讓挑戰作廢 | 通過：每種失敗之後再用同一個挑戰都是 409 |
| 超過每個硬體 ID 與全域的限流回 429 與 `Retry-After`；大量取挑戰不讓資料庫無上限成長 | 通過：`tests/renewal.rs` 的限流測試；取挑戰時刪掉過期的挑戰也有測試。上限的推導見 README「限流與資源上限」 |
| 成功換發的租戶事件與簽發紀錄在同一個交易；每次拒絕有結構化日誌與依原因的指標；已登錄的硬體 ID 在一個時間窗內被拒多次只寫一筆事件（帶次數），未登錄的不寫事件 | 通過：交易 ID 相同；每次拒絕的日誌與 OTLP 指標；7 次拒絕一筆事件；未登錄沒有事件 |
| aarch64 建置、`cargo test --workspace`、clippy、`openapi --check`、`bun test deploy/chart`、`bun run --cwd frontend test` | 通過：見「結果」的表 |
| 消費端 lag 不明或超過上限時回 503，不簽憑證 | 通過：`tests/renewal.rs`（不明）與 `tests/renewal_lag.rs`（超過上限） |

待測（之後另開的 P2，見 issue「不包含」）：經真實入口（Cloudflare 與 Istio）的換發、限流在負載下的量測、以真實 TPM 的換發（Laysi/trustiot-v2#83 的實機驗證）。
