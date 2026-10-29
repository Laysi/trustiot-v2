# gateway-ops 的 chart 部署：broker 帳號、資料庫角色、連線預算與 nightly smoke

Laysi/trustiot-v2#721（清單 E1、決策 7）。部署的說明在 [deploy/chart/README.md](../../deploy/chart/README.md)「gateway-ops」，服務本身在 [services/gateway-ops/README.md](../../services/gateway-ops/README.md)「部署」。

## 環境

- 建置主機（x86_64 Linux）與建置主機（macOS），podman 起拋棄式容器；`bun run --cwd scripts test` 與 cargo 在建置主機（x86_64 Linux）跑，`bun test deploy/chart` 在建置主機（macOS）跑。
- 分支 `feat/721-gateway-ops-chart`，以 Laysi/trustiot-v2#642 的分支 `feat/642-ingest-chart`（`1ea3aa5c`，PR Laysi/trustiot-v2#747，尚未合併）為基礎，再合併 main。
- Rust 1.99.0（`rust-toolchain.toml`）；Bun、Helm、kubeconform、actionlint、kubectl、flux 依 `.prototools`。
- 沒有 Kubernetes：chart 只以 `helm template` 與 kubeconform 檢查；真正的部署由 nightly 在 k3d 上驗證（見「結論」）。

## 指令

在 repo 根目錄：

```sh
bun test deploy/chart
bun test deploy/flux
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun run --cwd scripts test
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/check-workflows.ts
bun scripts/ci/lint-workflows.ts
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p gateway-ops -p testkit -p repo-check
```

`bun run --cwd scripts test` 跑的檔案與 `bun test scripts` 相同，包含 `scripts/nightly`（含 `diagnostics.test.ts` 的「nightly 不讀日誌」規則）、`scripts/release` 與 `scripts/ci/changes.test.ts`。Rust 只多了 `services/gateway-ops/tests/chart_connections.rs` 與 `crates/testkit` 的一行清單，所以 cargo test 只跑這三個套件；clippy 跑整個 workspace。

## 結果

輸出（只留測試本身的行與結果）：[checks.txt](data/2026-10-29-721-gateway-ops-deploy/checks.txt)。

| 檢查 | 結果 |
| --- | --- |
| `bun test deploy/chart` | 711 pass、0 fail |
| `bun test deploy/flux` | 78 pass、0 fail |
| `bun run --cwd scripts test` | 1711 pass、9 skip、0 fail |
| typecheck、secret handling、workflow policy、actionlint | 都沒有錯誤 |
| `cargo fmt --check`、clippy `-D warnings` | 通過 |
| `cargo test -p gateway-ops -p testkit -p repo-check` | 全部 `ok`，含 `chart_connections` 與 `reads_every_budget_of_the_chart`（在合併 main `ea5f27c7` 之前跑；那次合併只動到 `edge/agent`、`crates/protocol` 與文件） |

`bun test deploy/chart` 中與本 issue 相關的檢查：

- gateway-ops 經 `trustiot.mqttBroker.serviceCredentials`（Laysi/trustiot-v2#634 的 helper）拿 broker 帳號：`TRUSTIOT__MQTT__HOST`（`trustiot.mqttBroker.serviceHost`）、`_PORT` 1883、`_USERNAME` `service:gateway-ops`、`_PASSWORD_FILE` `/etc/trustiot/mqtt-broker-account/gateway-ops`；只投射 accounts Secret 的 `gateway-ops` 這個 key，`fsGroup` 65530（與 identity、certs、ingest 都不同）；其他 pod 都沒有這個 key。這四個變數 custom env 設不了（連同 `--skip-schema-validation` 時模板自己的拒絕）。
- mesh 只放行 gateway-ops 連 broker 的服務埠 1883，不放行閘道器埠 8883；沒有任何呼叫端連得到 gateway-ops（它的 AuthorizationPolicy 沒有 principals）。`mesh.calls.gateway-ops` 不列 `mqtt-broker` 時不給 env 也不給 key。README 的呼叫矩陣與渲染結果相符（`mqtt-broker` 的呼叫端是 `certs`、`gateway-ops`、`ingest`、`mqtt-bridge`）。
- 開啟 gateway-ops 而 broker 沒開時渲染失敗，訊息說明要開 `mqttBroker.enabled`。
- migration Job 的 `TRUSTIOT__PASSWORDS__POSTGRES` 含 `gateway_ops_app` 與 `gateway_ops_platform`，密碼在 `<release>-gateway-ops-password`（宣告為空，由 Job 填入）；只有 gateway-ops 的 Deployment 讀它，別的服務以 custom env 讀它時渲染失敗；bundled Postgres 的 NetworkPolicy 放行 gateway-ops 的 pod。
- 連線預算：gateway-ops 每個 pod 8（`maxConnections` 4 + 平台 4），整版滾動時 16；八個服務全開時連線池合計 192，加上保留的 18 條共 210 ≤ `max_connections` 220（由 200 調高）。`deploy/flux` 的對照組改為多開三個 identity 副本才超出（243 > 220）。

nightly（`scripts/nightly`）的改動以單元測試驗證：`NIGHTLY_IMAGES` 加上 `gateway-ops`，快取 key 的輸入涵蓋 `services/gateway-ops/**` 與 `deploy/images/gateway-ops/**`（`image-cache.test.ts`）；API 檢查的 values 啟用 gateway-ops，渲染出的 Deployment 帶上述 env、密碼 Secret 由 Job 填、沒有缺少的 Secret（`api.test.ts`）；`checkGatewayOps` 在沒有就緒副本或任一角色沒有連線時失敗。

## 設計上的取捨

- broker 連線只以 gateway-ops 的 readiness 證明（Laysi/trustiot-v2#720：連著 broker，且套用回報的共享訂閱以 QoS 1 獲准），與 ingest 相同。曾試過讀 broker 的日誌找 `service:gateway-ops` 的登入紀錄，但 nightly 的程式不得讀任何 pod 的日誌（`scripts/nightly/diagnostics.test.ts`：日誌印出應用程式持有的內容），所以拿掉。
- `services.gateway-ops.database.maxConnections` 預設 4：bundled Postgres 原本只剩 6 條餘裕（Laysi/trustiot-v2#747 審查 L5），即使 4 + 4 也要調高 `max_connections`，調到 220 留 10 條；**升級到這一版會讓叢集內的 Postgres 重啟一次**（chart README 已寫明）。記憶體估計見 chart README「叢集內的 Postgres」的表（220 那欄是依斜率推估，未實測）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 啟用 gateway-ops 的 Deployment 渲染出所需變數，broker 密碼只掛 gateway-ops 自己的 key（`bun test deploy/chart`） | 通過 |
| mesh 只允許 gateway-ops → broker 服務埠，呼叫矩陣文件與 chart 測試一致 | 通過 |
| 帶 `smoke` 標籤的 PR 的 nightly 以兩份 values 部署，gateway-ops 就緒且連上 broker | 待測：nightly 只在 CI 跑（工作站不跑 Kubernetes），PR 開出並加上 `smoke` 標籤後由 CI 驗證；新的檢查是 `scripts/nightly/smoke.ts` 的「gateway-ops is ready」。前提是 Laysi/trustiot-v2#720 已合併：在那之前 gateway-ops 的設定拒絕 chart 給的 `TRUSTIOT__MQTT__*`（`deny_unknown_fields`），不會就緒 |
| `bun test deploy/chart`、`bun test scripts`、`cargo test -p repo-check`、`bun scripts/ci/check-secret-handling.ts` 通過 | 通過 |

## 安全審查後的修正

同日的安全審查（head `8181be21`）沒有 Critical、High、Medium。修正如下：

| 項目 | 處理 | 驗證 |
| --- | --- | --- |
| L3 ingest、gateway-ops 不列 broker | 開啟時 `mesh.calls.<名稱>` 沒列 `mqtt-broker` 就渲染失敗並說明原因（沒有帳號、mesh 擋連線，pod 永遠不就緒） | chart 測試：兩個服務各以「只列 Postgres（ingest 加 ClickHouse）」與空清單渲染，都失敗並帶訊息；gateway-ops 關閉時同樣的清單可以渲染；helper 副本的「不列 broker 就沒有帳號」改以 certs 驗證 |
| L2 展示叢集的換版 | chart README「gateway-ops」改為建議分兩次換版：先換 chart（Postgres 因 `max_connections` 重啟），再開 gateway-ops（broker 重啟、閘道器與 ingest 重連、migration Job 重跑、用 Postgres 的服務滾動更新） | 文件 |
| I3、I4 | `_postgres.tpl` 的註解對齊；本報告的「環境」只寫建置主機 | 文件 |
| I2 `max_connections` 220 的記憶體 | 不改：表中是推算值，實測延後到另開的 P2 issue | — |

修正後的檢查（合併 Laysi/trustiot-v2#747 的 `f2722e72` 之前，那次合併只動到 `tools/migrate/src/clickhouse.rs`）：

- `bun test deploy/chart`：711 pass、0 fail（建置主機（macOS））。
- `bun test scripts/nightly scripts/release/chart-images.test.ts`、`bun test deploy/flux`、`cargo fmt --all --check`：通過（本機）。
- 全 workspace 的 `cargo test --locked --workspace --exclude agent` 與 clippy：合併 `f2722e72` 之前在建置主機（x86_64 Linux）跑過一次，clippy 通過，測試只有 `tools/sql-check` 的 `the_workspace_has_no_string_built_sql` 失敗，那是 #747 的 `tools/migrate/src/clickhouse.rs`，由 `f2722e72` 修正；合併之後的重跑因建置主機的磁碟空間不足沒有執行，待補。
