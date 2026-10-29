# certs：HTTPS 憑證換發端點（2026-10-29）

Laysi/trustiot-v2#81：閘道器以 TPM 金鑰簽一次性挑戰，經公開路徑 `POST /certs/renew` 換發閘道器憑證（清單 D2、決策 12；流程與規則見 [services/certs/README.md](../../services/certs/README.md) 的「換發」）。這份報告是驗收條件的證據：全部由下列自動化測試以 `CI=1` 實際執行（需要容器的測試不略過），閘道器的 TPM 以軟體 P-256 金鑰代替。

## 環境

- **程式碼**：分支 `feat/81-cert-renewal`。功能 commit 先是 `8f49bbad`（基於 main `5dad0429`），之後 rebase 到 main `9d879e95`（Laysi/trustiot-v2#722，只改閘道器 agent 與 `crates/protocol/README.md`，沒有衝突）成為 `21218ce1`，內容相同。本報告的 commit 只加文件與測試輸出。安全審查之後的修正是 `d3dc6c20`（見「安全審查後的修正」），報告的更新在它之後的 commit。之後合併 main `dcc603bf`（`6544fa82`），並把換發接上 broker 權限同步（見「合併 main 與 broker 權限同步」）。
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

驗收證據可以單獨重跑：`CI=1 cargo test -p certs --test renewal --test renewal_lag -- --nocapture`；換發接上 broker 權限同步的證據：`CI=1 cargo test -p certs --test dynsec -- --nocapture`。

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

## 安全審查後的修正

安全審查（`87b44b1d`）沒有 Critical 或 High，修正了兩項：

- **M1**：原本只有一道全域限流，任何人以捏造的硬體 ID 每秒送超過 20 個取挑戰請求，就讓每台閘道器的換發都得到 429，包括只能經這個端點恢復的閘道器。`d3dc6c20` 讓取挑戰與兌換各有一道全域限流（同樣的設定），每個硬體 ID 的限流仍是兩步共用。還剩下的風險（洪水期間其他閘道器取不到挑戰）與入口以來源 IP 限流的前提條件寫在 services/certs/README.md「換發」的「規則與取捨」與 docs/runbooks/demo-environment.md 的「換發端點上線」。
- **L1**：`bindings_stale` 與 `ca_not_ready` 的 503 原本在限流之前，每個請求一行 INFO。限流移到本文檢查之後、lag 與 CA 的檢查之前。

| 指令 | 在哪裡、哪個 commit | 結果 |
| --- | --- | --- |
| `cargo fmt --all --check`，`cargo clippy --locked -p certs --all-targets -- -D warnings`，`cargo test --locked -p repo-check`，`cargo test --locked -p certs`，`cargo test --locked -p certs --test renewal --test renewal_lag -- --nocapture` | 建置主機（macOS，`CI=1`），`d3dc6c20` 的工作樹 | 全部通過：certs 的單元測試 29 個（限流的 5 個在內），`renewal` 3 個、`renewal_lag` 1 個，其他測試檔都通過；repo-check 通過 |

證據行在 [security-review-fixes.txt](data/2026-10-29-81-cert-renewal/security-review-fixes.txt)：

- M1（`rate_limits_answer_429_and_bound_the_challenges`）：已登錄、已綁定的 `rl-ok` 先拿到挑戰；接著取挑戰的全域限流用完（每個硬體 ID 的 429 之後，50 個新硬體 ID 都 429，只存下 6 個挑戰），`rl-ok` 兌換仍是 201。捏造的兌換 5 個 409（兌換那一道的 6 個，換發用掉 1 個），之後 15 個 429，取挑戰仍是 429。
- L1（`over_the_limits_a_stale_replica_logs_no_refusal_at_info`）：平台角色登不進去（lag 不明）的副本收到 30 個新硬體 ID 的 60 個請求（每個一次取挑戰、一次兌換），12 個（每一步 6 個）回 503 `bindings_stale`、各一行 INFO，48 個回 429、沒有 INFO 行。
- 測試等 lag 的方式：限流改在 lag 的檢查之前之後，503 也扣限流，所以 `tests/renewal.rs` 改看匯出的 `outbox.consumer.lag`，不再輪詢端點；`tests/renewal_lag.rs` 每次輪詢用新的硬體 ID，證據行改取兌換那一步的拒絕（之前取到的是啟動時、lag 還不明的那一行）。
- 前面「證據行」的其他結果（換發、拒絕、挑戰、拒絕的指標、時間窗、lag）在修正後重跑時相同，只有序號與 ID 不同，`certs.renewal.challenges` 從 10 變成 9（少了原本輪詢等 lag 時取的那一個挑戰）。

## 合併 main 與 broker 權限同步

Laysi/trustiot-v2#633 的 broker 權限同步合併到 main 之後，`6544fa82` 把 main `dcc603bf` 合併進這個分支（merge，不 rebase）。`services/certs` 的 `README.md`、`src/config.rs`、`src/lib.rs` 有衝突，兩邊都保留。合併之後的 commit 把換發接上同步：簽發交易 commit 之後呼叫 `BrokerSync::gateway`（和出廠登錄相同），只比對這一台。唯一的憑證過期或被撤銷之後，閘道器在 dynsec 的 client 會被完整比對刪掉；接上之後，這樣的閘道器換發完就立刻拿回 client，不必等下一次完整比對。測試是 `tests/dynsec.rs` 最後一段：真的 Mosquitto、真的服務，服務以 `dynsec.interval` 1 小時重啟，所以啟動時的那次之後，測試期間不會再有完整比對。

| 指令 | 在哪裡、哪個 commit | 結果 |
| --- | --- | --- |
| `cargo fmt --all --check`，`cargo clippy --locked -p certs --all-targets -- -D warnings`，`cargo test --locked -p repo-check`，`cargo test --locked -p certs`，`cargo test --locked -p certs --test dynsec -- --nocapture` | 建置主機（macOS，`CI=1`），接上同步那個 commit 的工作樹 | 全部通過：certs 的單元測試 43 個（含 #633 的），每個測試檔都通過（`dynsec` 1 個、`renewal` 3 個、`renewal_lag` 1 個）；repo-check 通過 |
| 同上的 `dynsec` 測試，但把 `src/renewal.rs` 的 `self.broker_sync.gateway(hardware_id)` 換成什麼都不做（突變檢查，之後還原） | 建置主機（macOS，`CI=1`） | 如預期失敗：`the renewed gateway's broker client: not within 120s`。所以只有換發的呼叫能讓 client 在測試期間回來 |
| `bun test scripts/ci/changes.test.ts` | 本機（Linux 的建置主機磁碟不足，工作沒有執行，見下） | 283 個通過 |

證據行在 [broker-sync.txt](data/2026-10-29-81-cert-renewal/broker-sync.txt)：

- 撤銷 `gw-633-a` 唯一的憑證，服務以 `dynsec.interval` 1 小時重啟。連線時的完整比對刪掉它的 client：`changed a gateway's broker permissions` action=removed，一行 `brought the broker's gateway permissions in line with postgres`。測試的 broker CRL 沒有列出它，所以舊憑證連得上，但發布得到 PUBACK 0x87。
- 換發 201（序號 `7420dcb5...`）。服務的 stdout 第 18 行是 `renewed a gateway certificate`（commit 之後才記），第 21 行是 `changed a gateway's broker permissions` action=added。`brought ... in line` 仍只有啟動時那一行，表示這個 client 不是完整比對加的。以換發的憑證連線，發布得到 PUBACK 0x10。
- 呼叫放在 commit 與它的 INFO 行之後，而且只有 commit 成功才會執行到（`tx.commit().await?` 失敗就回 503）。單台比對以另一個交易讀 Postgres，所以它能把 client 加回來，就表示它讀到的是已 commit 的新憑證。測試也確認比對的日誌行在換發的日誌行之後。
- `bun test scripts/ci/changes.test.ts` 經檢查佇列時被派到 Linux 的建置主機（`bun test scripts` 只在那台跑），那台的可用磁碟 39 GB，低於一個工作需要的 40 GB，所以工作沒有執行。這個測試只有一個檔案、不需要容器，改在本機執行。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 已登錄、已綁定、租戶正常的閘道器以正確的簽章換到新憑證：CN 是硬體 ID、公鑰是登錄的那一把、鏈到 `root_certificate`；目前憑證已過期或已撤銷時同樣成功 | 通過：`tests/renewal.rs` 撤銷閘道器唯一的憑證後換發，CN、公鑰、鏈與 rustls 的用戶端憑證驗證都通過。已過期的情況沒有另外測：端點不讀閘道器現有的憑證（請求不帶憑證、不經 mTLS），撤銷與過期走的是同一條路徑 |
| 除役、未綁定、租戶停權時拒絕，不簽憑證、不寫簽發紀錄 | 通過：除役、沒綁定過、綁定已結束、停權（測試替身）都 403，簽發紀錄不變、沒有事件 |
| 重送 409、竄改的簽章 401、別台設備或別的用途的挑戰 403、過期 410，失敗的兌換讓挑戰作廢 | 通過：每種失敗之後再用同一個挑戰都是 409 |
| 超過每個硬體 ID 與全域的限流回 429 與 `Retry-After`；大量取挑戰不讓資料庫無上限成長 | 通過：`tests/renewal.rs` 的限流測試；取挑戰時刪掉過期的挑戰也有測試。上限的推導見 README「限流與資源上限」。安全審查後取挑戰與兌換各有一道全域限流，取挑戰的洪水之後已經拿到挑戰的閘道器仍然換發成功；超過限流的請求在 lag 不明時也只回 429、不記 INFO |
| 成功換發的租戶事件與簽發紀錄在同一個交易；每次拒絕有結構化日誌與依原因的指標；已登錄的硬體 ID 在一個時間窗內被拒多次只寫一筆事件（帶次數），未登錄的不寫事件 | 通過：交易 ID 相同；每次拒絕的日誌與 OTLP 指標；7 次拒絕一筆事件；未登錄沒有事件 |
| aarch64 建置、`cargo test --workspace`、clippy、`openapi --check`、`bun test deploy/chart`、`bun run --cwd frontend test` | 通過：見「結果」的表 |
| 消費端 lag 不明或超過上限時回 503，不簽憑證 | 通過：`tests/renewal.rs`（不明）與 `tests/renewal_lag.rs`（超過上限） |

另外（不在 issue 的驗收條件裡，協調者在 Laysi/trustiot-v2#633 合併後要求）：換發 commit 之後請 broker 權限的同步比對這一台，唯一的憑證剛換發的閘道器不必等完整比對就拿回 dynsec 的 client。結果：通過，見「合併 main 與 broker 權限同步」。

待測（之後另開的 P2，見 issue「不包含」）：經真實入口（Cloudflare 與 Istio）的換發、限流在負載下的量測、以真實 TPM 的換發（Laysi/trustiot-v2#83 的實機驗證）。
