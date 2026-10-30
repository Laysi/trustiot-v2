# 閘道器在線狀態與閘道器列表（2026-10-30）

Laysi/trustiot-v2#112（清單 C3）：gateway-ops 從閘道器的 `up/status`（含 MQTT last will）與 `up/heartbeat` 記錄每個綁定的在線狀態、最後回報時間、agent 版本與已套用的設定版本，心跳停了之後以逾時標成離線，並以 `GET /api/gateway-ops/v1/gateways` 讓租戶的 viewer 列出閘道器。這份報告是驗收條件的證據：全部由自動化測試以 `CI=1` 實際執行，需要容器的測試不略過。

## 環境

- **程式碼**：分支 `issue-112-gateway-presence`，基於 main `e4994a25`（Laysi/trustiot-v2#768）。下面的結果是安全審查與 commit 閘門的修正（每台閘道器的等待佇列、心跳柵欄、不共享的心跳訂閱、retained 重播不建列、寫入前再確認綁定、API 的同時請求上限、新 session 清掉舊的心跳欄位）都套用之後的樹。
- **機器**：建置主機（arm64 Linux VM），經檢查佇列執行。容器測試由 podman 起 `testkit::product` 的拋棄式 TLS Postgres（以 migration Job 的程式套用整個產品的 schema）與 `testkit::mqtt` 的 Mosquitto（`eclipse-mosquitto:2.1.2-alpine`，以 digest 固定；帳號與 ACL 取自 migration Job 的 `migrate::mqtt_accounts`）。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2。
- **設定**：`CI=1`。在線狀態的測試以 `RawGateway`（`services/gateway-ops/tests/support/broker.rs`）直接送 MQTT 5 封包代替閘道器，自己決定 keepalive、last will 與何時斷線；agent 的設定是 keepalive 60 秒、心跳 60 秒（決策 6）。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -p openapi -- --check
CI=1 cargo test --locked --workspace --exclude agent
CI=1 cargo test --locked -p gateway-ops -- --nocapture
cargo deny --locked check
bun test deploy/chart
bun install --cwd scripts --frozen-lockfile && bun test scripts
bun install --cwd frontend --frozen-lockfile && bun run --cwd frontend api:check
```

驗收條件 1 的半開連線以 agent 真正的 keepalive（60 秒）量一次：在建置主機的工作副本把 `tests/presence.rs` 半開那一段的 `let keep_alive: u16 = 2;` 改成 `AGENT_KEEP_ALIVE`，跑 `CI=1 cargo test --locked -p gateway-ops --test presence last_wills -- --nocapture`。測試本身用 2 秒的 keepalive，檢查的是同一條 broker 規則，不讓 CI 每次等 90 秒（CLAUDE.md「Test time limits」）。

## 結果

`tests/presence.rs` 印出的量測（建置主機，最後一次執行）：

| 情境 | 設定 | 量到的時間 |
| --- | --- | --- |
| 連線直接斷掉（沒有 DISCONNECT），broker 發布 last will | keepalive 60 秒 | 66 ms 後標成 `offline`（`connection_lost`）；前幾次執行 60 ms、64 ms |
| 半開連線（不送封包也不關），broker 的 keepalive 檢查發布 last will | keepalive 60 秒（一次性執行） | 91.1 秒後標成 `offline`（`connection_lost`） |
| 同上 | keepalive 2 秒（CI 的測試） | 3.1 至 3.6 秒（1.5 倍是 3 秒） |
| 心跳停了，連線還在 | 宣告間隔 1 秒、寬限 1 秒，逾時 2 × 1 + 1 = 3 秒 | 3.3 秒後標成 `offline`（`heartbeat_timeout`），不早於 3 秒 |
| 一台閘道器連送 2000 則心跳，接著另一台斷線 | 每台閘道器的佇列 16 則 | 另一台 68 ms 後標成 `offline`；那時只套用了 34 則，丟掉 1935 則（都是灌訊息那台的） |

`cargo test --locked -p gateway-ops`：單元測試 48 個、整合測試 8 個檔案共 22 個，全部通過。其中與這次有關的：

- `src/presence/rules.rs`：每條規則；心跳柵欄（重複、較舊的心跳不寫，新 session 從沒有柵欄開始）；`replicas_in_any_interleaving_end_in_the_state_of_one` 列舉兩個 replica 處理同一串訊息的所有交錯順序（三串訊息，各 3432 至 12870 種），結果的狀態與 session 都和單一 replica 相同。
- `src/presence/backlog.rs`：閘道器輪流、各自保持順序、每台與總數的上限、最舊一則的時間。
- `tests/presence.rs`：上表；舊 session 晚到的 last will 不理；未綁定的閘道器不記錄；壞掉的狀態被拒；逾時後同一 session 的心跳讓它回到在線；訂閱前就在 broker 上的 retained `online` 不建立列，下一則即時心跳才建立；寫入前綁定已經結束時不寫；新連線不沿用上一次連線的心跳欄位。
- API（`tests/presence.rs`、`tests/startup.rs`）：第一則狀態前列 `unknown` 與綁定的最新版本；之後列 `online`、`last_seen_at`、`presence_changed_at`、`agent_version`，`applied_config_version` 等於心跳宣告的版本、`desired_config_version` 是綁定最新的版本；逾時後列 `offline`／`heartbeat_timeout`；另一個租戶的列表是空的；沒有 token 是 401；回應都帶 `no-store`。
- `tests/permissions.rs`：真的 binary 回應權限矩陣的探測（viewer 以上通過，沒有租戶、沒有租戶的平台管理員、不認得的角色得 403，沒有或偽造的 token 得 401），探測前後綁定與在線狀態的筆數不變。
- `tests/cross_tenant.rs`：服務角色讀不到也改不了別的租戶的 `gateway_presence`，API 的列表只列呼叫者租戶的閘道器。
- `tests/startup.rs`：broker 停掉時 `/readyz` 是 503，失敗的檢查是 `mqtt` 與 `presence`；恢復後就緒。

整個 workspace 與其他檢查（建置主機，最後的程式變更之後）：

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過，沒有警告 |
| `cargo run --locked -p openapi -- --check` | `api/openapi.json is up to date` |
| `cargo deny --locked check` | advisories、bans、licenses、sources 都 ok |
| `CI=1 cargo test --locked --workspace --exclude agent` | 1985 個通過、0 失敗、1 個 ignored |
| `bun test deploy/chart` | 711 個通過、0 失敗（連線預算 220 = 202 + 18） |
| `bun test scripts`、`bun run --cwd scripts typecheck` | 1711 個通過、9 個略過、0 失敗；型別檢查通過 |
| `bun run --cwd frontend api:check`、`typecheck`、`lint`、`test` | 全部通過；218 個測試 |
| `bun scripts/ci/check-workflows.ts`、`bun scripts/ci/check-secret-handling.ts` | 沒有違規 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 斷線後 5 秒內由 LWT 標成離線（測試） | 通過：60 至 66 ms |
| 半開連線 90 秒內（keepalive 60 × 1.5）標成離線（測試） | **未達成字面的「90 秒內」**：以 agent 的 60 秒 keepalive 量到 91.1 秒，超過 1.1 秒。原因：離線由 broker 的 keepalive 規則（1.5 倍）觸發，Mosquitto 以 1 秒為單位檢查 keepalive，90 秒的期限過後，下一次檢查才發布 last will；服務收到後幾十毫秒內寫入。是否接受這個結果，或修改驗收條件（例如「約 90 秒，broker 檢查粒度 1 秒」），由審查者決定。CI 的測試以 2 秒的 keepalive 檢查同一條規則（3.1 至 3.6 秒）。心跳逾時（預設 2 × 60 + 30 = 150 秒）是後備 |
| 列表 API 顯示已套用的設定版本與最後回報時間（測試） | 通過：`applied_config_version`、`desired_config_version`、`last_seen_at` |
| 每次狀態改變發出事件 | 以 INFO 日誌 `gateway presence changed` 與計數器 `gateway_ops.presence.changes` 代替 outbox 事件：migration Job 要求每個 outbox 生產者至少有一個消費端，目前沒有服務要跟著在線狀態走（見 PR） |
