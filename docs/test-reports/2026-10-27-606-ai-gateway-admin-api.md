# ai-gateway：模型角色的管理 API，放在獨立的管理埠（2026-10-27）

Laysi/trustiot-v2#606：ai-gateway 在自己的管理埠（`admin.listen_addr`，預設 `127.0.0.1:8082`）提供 `GET /internal/admin/v1/roles`、`PUT` 與 `DELETE /internal/admin/v1/roles/{role}`，以 Laysi/trustiot-v2#605 的 `Store` 更換與重設模型角色；線上格式在新的 `crates/ai-gateway-api`。這份報告是驗收條件的證據：全部由下列自動化測試以 `CI=1` 實際執行（需要容器的測試不略過）。每個測試的名稱見 [ai-gateway-tests.txt](data/2026-10-27-606-ai-gateway-admin-api/ai-gateway-tests.txt)。

## 環境

- **程式碼**：分支 `feat/606-ai-gateway-admin-api`：功能 commit `49b67e62`、文件 commit `b7cd4a90`，merge main `304de010` 與 `509c3f1f`（Laysi/trustiot-v2#574）之後的樹（merge commit `f5ac6342`）。本報告的 commit 只加文件與測試輸出。
- **機器**：建置主機（x86_64 Linux），經檢查佇列執行；容器測試由 podman 起 service-kit 的拋棄式 TLS Postgres（`crates/service-kit/tests/support/postgres.rs`，每個測試自己的資料庫，套用 ai-gateway 的 migration，與 migration Job 的順序相同）。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2、cargo-deny。
- **設定**：`CI=1`。不連真正的供應商：供應商是 loopback 上的腳本化 HTTP/1.1 伺服器。程序測試（`tests/process.rs`）以 `AI_GATEWAY__ADMIN__LISTEN_ADDR=127.0.0.1:0` 啟動真正的 `ai-gateway` 執行檔。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -p openapi -- --check
CI=1 cargo test --locked --no-fail-fast -p ai-gateway -p ai-gateway-api -p migrate -p sql-check -p repo-check
cargo tree --locked -p ai-gateway --prefix none | grep -c '^contract '       # 0
cargo tree --locked -p ai-gateway-api --prefix none | grep -c '^contract '   # 0
cargo deny --locked check
bun install --cwd scripts --frozen-lockfile
bun test scripts/nightly
bun test scripts/ci/changes.test.ts
bun scripts/ci/check-workflows.ts
```

驗收證據可以單獨重跑：`CI=1 cargo test -p ai-gateway --test admin --test process`。

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo run -p openapi -- --check` | 通過（管理 API 不在對外的規格裡，`api/openapi.json` 沒有變） |
| `cargo test -p ai-gateway` | 單元 26、`admin` 4、`chart_connections` 1、`decisions` 4、`fallback` 9、`messages` 7、`no_contract` 2、`process` 8、`roles_store` 9，共 70 個全部通過 |
| `cargo test -p ai-gateway-api` | 單元 5、`no_contract` 2，共 7 個全部通過 |
| `cargo test -p migrate` | 170 個全部通過（含容器的 Postgres 與 ClickHouse 測試） |
| `cargo test -p sql-check` | 26 個全部通過 |
| `cargo test -p repo-check` | 26 個全部通過 |
| `cargo tree`：`contract` 的出現次數 | ai-gateway 0、ai-gateway-api 0；ai-gateway-api 的一般相依只有 serde |
| `cargo deny --locked check` | advisories ok, bans ok, licenses ok, sources ok |
| `bun test scripts/nightly` | 381 通過、0 失敗（`nightly.yml` 的 PR 路徑加了 `crates/ai-gateway-api/**`） |
| `bun test scripts/ci/changes.test.ts` | 283 通過、0 失敗 |
| `bun scripts/ci/check-workflows.ts` | 沒有違規 |

## 結論

| 驗收條件 | 證據 | 結果 |
| --- | --- | --- |
| 兩個程序共用一個資料庫：經 A 的管理埠 `PUT` 之後，B 的下一個轉送請求用新的模型 | `process.rs` 的 `a_change_through_the_admin_port_reaches_every_process_and_outlives_a_restart`：A、B 先都用 `first/model-one`；A 的管理埠回 200、版本 1、`source: database`；B 的下一個請求用 `second/model-two` | 通過 |
| 重啟之後用更換後的模型 | 同一個測試：停掉 A、B 再起一個新程序，第一個請求用 `second/model-two` | 通過 |
| 422 `protocol_mismatch` 之後 `GET` 與轉送都不變；`unknown_provider` 與 `invalid_models` 也涵蓋 | `admin.rs` 的 `refused_requests_answer_their_code_and_change_nothing`：三種協定不符（兩個方向、混用）、未設定的供應商、五種不合格清單（空、9 個、重複、沒有供應商、含空白）；每次之後三張表的筆數都是 (0, 0, 0)，最後 `GET` 仍是設定的模型、版本 0，轉送仍用 `first/model-one` | 通過 |
| 舊版本回 409 且什麼都不改；`DELETE` 之後 `GET` 是設定的模型、`source: configuration` | `admin.rs` 的 `a_change_and_a_reset_through_the_admin_api`：舊版本的 `PUT` 與 `DELETE` 都回 409 `version_conflict`、`current_version: 1`，筆數不變；`DELETE ?expected_version=1` 回版本 2、`source: configuration`，`GET` 與轉送都回到 `first/model-one`；再重設一次什麼都不寫 | 通過 |
| 缺少或不合格的操作者回 400 且什麼都不改；操作者出現在變更紀錄與日誌 | `refused_requests_answer_their_code_and_change_nothing`：沒有、空的、兩個、129 字元、非 ASCII、含 tab 的 `ai-gateway-actor`，對 `PUT`、`DELETE`、`GET` 都回 400 `invalid_actor`，筆數不變。`a_change_and_a_reset_through_the_admin_api` 的變更紀錄兩筆的 `changed_by` 都是操作者；程序測試的 INFO 日誌 `model role changed` 帶 `role`、`actor`、`action`、`version`、`models`，變更紀錄是 `(1, set, <操作者>)` | 通過 |
| 兩個埠互不提供對方的路由（404） | `process.rs` 的 `each_port_serves_only_its_own_routes`（真正的程序、兩個埠）：管理路由在 API 埠回 404；轉送路由、`/healthz`、`/readyz` 在管理埠回 404。`admin.rs` 的 `the_admin_and_forwarding_routers_share_no_route` 在 router 層再確認一次 | 通過 |
| `cargo tree` 兩個 crate 都沒有 `contract` | 上表 `cargo tree`；`tests/no_contract.rs` 兩個 crate 各一份 | 通過 |
| README 寫明管理 API、驗證方式與操作者標頭 | `services/ai-gateway/README.md` 的「管理 API」與「驗證方式」；`crates/ai-gateway-api/README.md` | 通過 |
| `cargo test -p ai-gateway` 與 workspace clippy 通過 | 上表 | 通過 |

其他安全相關的證據：資料庫連不上時三個路由都回 503 `unavailable`、不呼叫供應商（`the_admin_api_answers_503_while_the_database_cannot_be_reached`）；超過 16 KiB 的本文回 413 `request_too_large`；每個錯誤回應都帶 `cache-control: no-store`，訊息不引用請求的內容（`assert_refused` 檢查每一個拒絕）。

待測：多個管理請求同時更換同一個角色的壓力測試（進階驗證，依組織規範延後到之後的 P2 issue）。管理埠在叢集裡只開給身分服務的授權實測，屬於 Laysi/trustiot-v2#607（chart 的 Service 與授權），在那之前管理埠只綁 `127.0.0.1`。
