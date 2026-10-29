# identity：平台管理者經身分服務更換 AI 模型角色（2026-10-29）

Laysi/trustiot-v2#608：身分服務的 `GET`/`PUT`/`DELETE /api/identity/v1/platform/ai/model-roles[/{role}]` 只讓平台管理者本人使用，檢查跨站請求與請求的形狀後，以專用的 HTTP client 轉給 AI 閘道的管理 API，並寫出全平台事件 `identity.ai_model_role_changed`（規則見 [services/identity/README.md](../../services/identity/README.md) 的「AI 模型角色」）。這份報告是驗收條件的證據：全部由下列自動化測試以 `CI=1` 實際執行（需要容器的測試不略過）。

## 環境

- **程式碼**：分支 `feat/608-identity-model-roles`，以 main `dcc603bf` 為基礎。功能 commit `5114a33b`；之後的 commit 改兩個測試訊息的字（`tools/sql-check` 把以 `DELETE` 開頭的 `format!` 字串當成組出來的 SQL）、加上這份報告、合併 main，再依安全審查的 M1 把結果不明的變更從 503 改成 504（見下方「安全審查之後」）。
- **機器**：經檢查佇列執行。證據、identity 與 contract 的測試、fmt、openapi、cargo-deny、前端的檢查在建置主機（macOS，arm64）；workspace 的 clippy、`cargo test --workspace` 與 `bun test scripts` 在建置主機（x86_64 Linux）。容器測試以 podman 起 `testkit::product` 的拋棄式 TLS Postgres（migration Job 套用整個產品的 schema）。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun。
- **AI 閘道**：測試自己在 loopback 起的假管理 API（`tests/ai_model_roles.rs` 的 `StandIn`），記下收到的每個請求（方法、路徑、query、標頭、body），照 [AI 閘道的管理 API](../../services/ai-gateway/README.md#管理-api) 的規則回答（角色 `main` 是 `anthropic-messages`、`router` 是 `openai-decisions`），或依步驟回指定的狀態與 body，或一直不回。型別用與 AI 閘道相同的 `crates/ai-gateway-api`。
- **服務**：真的 `identity` 執行檔，`TRUSTIOT_AI_GATEWAY_ADMIN_URL` 指向假管理 API；另外兩次啟動分別沒有這個變數、指向沒有人聽的埠。session 經服務自己的 ext_authz 換成內部身分 token；帶 `act` 的 token 由 `testkit::identity::TestIssuer` 簽（服務信任它的公鑰）。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -p openapi -- --check
cargo deny --locked check
CI=1 cargo test --locked --no-fail-fast --workspace
CI=1 cargo test --locked -p identity --test ai_model_roles -- --nocapture
bun install --cwd scripts --frozen-lockfile && bun test scripts
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
```

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過（x86_64 Linux；macOS 上以 `--exclude agent` 也通過） |
| `cargo run --locked -p openapi -- --check` | 通過（`api/openapi.json is up to date`；新增三條路由與 `ModelRoles`、`ModelRole`、`ModelProvider`、`IgnoredModels`、`ModelSource`、`SetRoleModels`） |
| `cargo deny --locked check` | `advisories ok, bans ok, licenses ok, sources ok`（新的依賴 `hyper`、`hyper-util`、`http-body-util` 已在 workspace 裡） |
| `CI=1 cargo test --locked --no-fail-fast --workspace`（x86_64 Linux） | 245 個測試執行檔全部 `ok`：通過 2110、失敗 0、略過（`#[ignore]`）1。同一份程式在 macOS 以 `--exclude agent` 跑時，`tools/sql-check` 抓到兩個以 `DELETE` 開頭的測試訊息（已改字，見「環境」），另有與這個變更無關、只在 macOS 失敗的測試（`ai-clients` 的連線逾時、`migrate` 的 `mqtt_broker`、`service-kit` 的 PID 1 容器），所以以 Linux 的結果為準 |
| `CI=1 cargo test -p identity` 與 `-p contract`（macOS） | 全部通過；`identity` 的單元測試 126 個（其中 AI 模型角色 13 個），`tests/ai_model_roles.rs` 2 個，`contract` 的 `tests/events.rs` 18 個 |
| `CI=1 cargo test -p identity --test ai_model_roles -- --nocapture` | 2 個通過（14.15 秒），證據行見下 |
| `bun test scripts`（x86_64 Linux） | 1704 pass、0 fail |
| `bun run --cwd frontend api:check`、`typecheck`、`lint`、`test` | 都通過；`test` 218 pass、0 fail（含每個伺服器訊息 key 都有翻譯的檢查） |

`tests/ai_model_roles.rs` 的證據行全文：[ai-model-roles-evidence.txt](data/2026-10-29-608-model-role-api/ai-model-roles-evidence.txt)（在安全審查的修正之後執行；使用者 ID 是測試隨機產生的）。摘要：

- **權限**：沒有 token 或 token 不合格 401；租戶 `admin`、沒有租戶的使用者、平台管理者經其他服務換發（帶 `act`）的 token 在三條路由都 403；假管理 API 收到 0 個請求。
- **轉送**：請求另外帶了偽造的 `ai-gateway-actor`、cookie、token 與 `Authorization`，假管理 API 收到的標頭只有 `accept`、`ai-gateway-actor`（值是 `user:<token 的使用者>`）與 `host`，PUT 另有 `content-type`；PUT 的 body 只有 `models` 與 `expected_version`（請求 body 另帶的 `role`、`actor` 沒有轉送）。
- **AI 閘道的拒絕**：協定不符 422 `/models` `ai_model_role.protocol_mismatch`，未知的供應商與重複的模型 422 各帶自己的 key，角色沒有改變；舊版本的 PUT 與 DELETE 409 `ai_model_role.version_conflict`、`details.current_version` 1；沒有的角色 404 `ai_model_role.not_found`。
- **事件**：PUT 回 200 時 outbox 已經有一筆沒有租戶、actor 是平台管理者的 `identity.ai_model_role_changed`（`set`、版本 1、生效的模型）；重設寫 `reset`、版本 2；再重設一次（沒有存模型）版本不變、不寫事件；後台的 `GET /api/identity/v1/platform/event-log` 列出這兩筆（`aggregate_type` `ai_model_role`、`aggregate_id` `main`）。撤銷 `identity_platform` 對 `identity.outbox_event` 的 `INSERT` 後，PUT 仍回 200（版本 3）、沒有事件，日誌有 ERROR `could not write identity.ai_model_role_changed`（`error_kind` 是 SQLSTATE 42501，不含資料）。
- **送出前拒絕**：跨站的 PUT 與 DELETE（`Sec-Fetch-Site: cross-site` 或 `same-site`、外站的 `Origin`）在 ext_authz 與 handler 都 403 `security.cross_site_request`；沒有或不是 JSON 的 `Content-Type` 400；0 或 9 個模型 422；格式不對、負數、小數、超過 16 KiB 的 body 與沒有合格 `expected_version` 的 DELETE 400；不可能存在的角色名稱 404；假管理 API 都沒有收到。
- **結果不明**：假管理 API 回 500、504、502、503 `unavailable`、帶未知錯誤碼的 400 與 409、不是 JSON 的 418、不是角色的 200 時，PUT 與 DELETE 都 504 `timeout` `ai_model_role.outcome_unknown`（測試另外斷言不是 503），每個剛好送一次，GET 503 `error.unavailable`，都沒有寫事件。
- **AI 閘道卡住**：4 個 PUT 到了假管理 API 後沒有回應；第 5 個立刻 503 `error.unavailable`、沒有送出；這段期間兩個使用者的 ext_authz 檢查都回 200（在 4 個請求回應之前）；4 個請求在 client 的 10 秒上限之後回 504 `ai_model_role.outcome_unknown`，各只送一次。
- **沒有設定或連不上**：沒有 `TRUSTIOT_AI_GATEWAY_ADMIN_URL` 與指向沒有人聽的埠時，平台管理者的 GET、PUT、DELETE 都 503 `error.unavailable`，其他使用者照樣 403；日誌分別有 WARN（沒有設定）與 ERROR（連不上）。
- **日誌**：有 `changed an AI model role`，沒有 session cookie、token、偽造的 actor 或 `Authorization`。

## 安全審查之後

安全審查（head `45b81895`）的 M1：入口會重試 503，所以結果不明的修改與重設改回 504 `timeout`（仍帶 `ai_model_role.outcome_unknown`）；讀取與「什麼都沒送」的情況仍是 503。L1、L2 只改文件。修正之後重跑：

| 指令 | 結果 |
| --- | --- |
| `CI=1 cargo test --locked -p identity -p contract -p repo-check`（macOS） | 25 個測試執行檔、313 個測試全部通過 |
| `CI=1 cargo test --locked -p identity --test ai_model_roles -- --nocapture`（macOS） | 2 個通過（13.90 秒）；上面連結的證據行是這一次的 |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo run --locked -p openapi -- --check`（x86_64 Linux） | 都通過 |
| `CI=1 cargo test --locked -p identity -p repo-check`（x86_64 Linux） | 18 個測試執行檔、213 個測試全部通過 |
| `bun run --cwd frontend api:check`、`typecheck`、`test`（x86_64 Linux） | 都通過，`test` 218 pass、0 fail |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 401／403 時假管理 API 什麼都沒收到 | 通過 |
| 協定不符的 PUT 422 帶 key，假管理 API 沒有改變 | 通過 |
| 成功的 PUT、DELETE、GET 回假管理 API 的內容，帶 actor、沒有 token 或 cookie | 通過 |
| 409 與 404 | 通過 |
| 連不上 503（什麼都沒送）；逾時與 5xx：讀取 503、變更 504（不是入口會重試的 503）；沒有設定 URL 時三條路由都 503；假管理 API 卡住時 ext_authz 照常回答 | 通過 |
| 偽造的 actor 標頭被換掉 | 通過 |
| 讀不懂的非 2xx：變更 504（不是 503），只寫一次 | 通過 |
| 沒有 JSON `Content-Type` 或跨站的 PUT、DELETE 被拒，什麼都沒送 | 通過 |
| 成功後寫事件、actor 是管理者；事件寫不進去仍成功並記 ERROR | 通過 |
| 日誌與回應沒有 token 或 cookie；openapi 檢查、前端 `api:check` 與 `test` | 通過 |
| `cargo test -p identity`、`cargo test -p contract`、workspace 的 clippy | 通過 |

待測（延後到進階驗證）：在真的叢集裡經 chart 的 `<release>-ai-gateway-admin` Service 與真的 AI 閘道走一次完整流程（目前由假管理 API 與 Laysi/trustiot-v2#606、Laysi/trustiot-v2#607 各自的測試分別證明）。
