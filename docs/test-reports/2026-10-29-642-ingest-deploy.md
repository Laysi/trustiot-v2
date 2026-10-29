# ingest 的 chart 部署：ClickHouse 寫入帳號、broker 帳號、資料庫與連線預算

Laysi/trustiot-v2#642（清單 I1、決策 8）。部署的說明在 [deploy/chart/README.md](../../deploy/chart/README.md)「ingest」，帳號的稽核與停下時的處理在 [docs/runbooks/migrations.md](../runbooks/migrations.md)。

## 環境

- 建置主機（x86_64 Linux），podman 起拋棄式容器。
- 分支 `feat/642-ingest-chart`，以 main `24271deb` 為基礎。
- Rust 1.99.0（`rust-toolchain.toml`）；Bun、Helm、kubeconform、actionlint 依 `.prototools`。
- `tests/clickhouse_ingest.rs` 的容器：ClickHouse `clickhouse-server:26.8` 與 Postgres，映像與 `tools/migrate` 其他 ClickHouse 測試相同（以 digest 釘選）。
- 沒有 Kubernetes：chart 只以 `helm template` 與 kubeconform 檢查；真正的部署由 nightly 在 k3d 上驗證（見「結論」）。

## 指令

在 repo 根目錄：

```sh
cargo test --locked -p migrate --test clickhouse_ingest
cargo test --locked -p migrate --lib ingest
cargo test --locked -p ingest --test chart_connections
cargo test --locked -p testkit --all-targets budget
bun test deploy/chart
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun run --cwd scripts test
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/check-workflows.ts
bun scripts/ci/lint-workflows.ts
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --exclude agent
```

`bun run --cwd scripts test` 跑的檔案與 `bun test scripts` 相同，包含 `scripts/release`、`scripts/nightly` 與 `scripts/ci/changes.test.ts`。`edge/agent` 沒有改動，所以 `cargo test -p agent` 沒跑。

## 結果

全部通過；輸出（只留測試本身的行與結果）：[checks.txt](data/2026-10-29-642-ingest-deploy/checks.txt)。

| 檢查 | 結果 |
| --- | --- |
| `tests/clickhouse_ingest.rs`（真的 ClickHouse） | `ingest_account ... ok`（24.90 s） |
| `migrate` 單元測試（`ingest` 篩選） | 2 passed，含 `the_ingest_account_is_fixed_and_its_own` |
| `services/ingest/tests/chart_connections.rs` | `the_chart_budgets_every_pool_the_service_opens ... ok` |
| `testkit` 的 chart 預算 | `reads_every_budget_of_the_chart ... ok`（清單加上 ingest） |
| `bun test deploy/chart` | 708 pass、0 fail |
| `bun run --cwd scripts test` | 1708 pass、9 skip、0 fail |
| typecheck、secret handling、workflow policy、actionlint | 都沒有錯誤 |
| `cargo fmt --check`、clippy `-D warnings` | 通過 |
| `cargo test --workspace --exclude agent` | 通過（229 個測試執行檔與 doc test 全為 `ok`） |

`tests/clickhouse_ingest.rs` 驗的事（以 migration Job 的程式碼對拋棄式 ClickHouse 跑）：

1. ingest 沒部署時不建帳號。
2. 部署後，Job 產生密碼寫入 ingest 的 Secret，以它建立 `trustiot_ingest_writer`，唯一且預設的角色是 `trustiot_ingest`；再跑一次沒有變動。
3. 這個帳號能讀寫 `telemetry`、`stream_commits`、`telemetry_1m`、`telemetry_1h`；以下全被 ClickHouse 拒絕：讀 `batch_commits`、`tenant_telemetry`、`trustiot_logs.otel_logs`、`trustiot_migrate.applied`，`ALTER ... MODIFY SETTING`、`ALTER ... DELETE`（`stream_commits`）、`DROP`、`TRUNCATE`，建帳號、改自己的 `HOST`、授予自己別的角色、建角色。
4. 輪替：新的 generation 設新密碼，舊的不再能登入。
5. 竄改後 Job 撤銷登入（`HOST NONE`）並停下，錯誤說明哪裡不對；修好後再跑恢復登入。竄改方式：帳號多一個權限、多一個角色、預設角色改為 NONE、角色帶 ADMIN OPTION、帳號帶 settings；角色 `trustiot_ingest` 多出 `tenant_telemetry` 或 `batch_commits` 的讀取、`ALTER UPDATE`、`GRANT OPTION`、少了 `ALTER DELETE`、多一個角色；別的帳號也持有 `trustiot_ingest`。
6. ingest 關掉後帳號留著但不能登入；再開啟後以 Secret 的密碼登入。
7. 帳號還沒建立時，若已有別人持有 `trustiot_ingest`，Job 停下，不建帳號。

`bun test deploy/chart` 中與本 issue 相關的檢查：

- ingest 在 `values-saas.yaml` 與 `values-onprem.yaml` 都能渲染並通過 kubeconform。
- ingest 經 `trustiot.mqttBroker.serviceCredentials`（Laysi/trustiot-v2#634 的 helper）拿 broker 帳號：只掛 accounts Secret 的 `ingest` 這個 key，`fsGroup` 65531，`service:ingest` 由 chart 設定、custom env 改不了；其他 pod 都沒有這個 key。broker 關掉或 `mesh.calls.ingest` 不列 `mqtt-broker` 時都不給。
- 提高 `mqttBroker.passwordGeneration` 只重啟 broker，ingest 的 pod template 不變。
- ClickHouse 帳號與兩個 Postgres 角色的密碼都在 `<release>-ingest-password`（宣告為空，由 Job 填入）；只有 ingest 的 Deployment 讀它，別的服務以 custom env 讀它時渲染失敗。
- 連線預算：ingest 每個 pod 14（MAX 10 + platform 4），整版滾動時兩個 pod 28；七個服務全開時連線池合計 176，加上保留的 18 條共 194 ≤ `max_connections` 200。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| `bun test deploy/chart` 通過：ingest 在兩個 values 檔都渲染；pod 只掛 `ingest` 這個 key（Laysi/trustiot-v2#634 的 helper）；其他 pod 沒有；提高 passwordGeneration 時 ingest 的 pod template 不變 | 通過 |
| ingest 的連線池列入預算，`services/ingest/tests/chart_connections.rs` 通過 | 通過 |
| migration Job 建立的 ingest ClickHouse 帳號只有 `trustiot_ingest` 角色，多出的權限讓 Job 報錯（`cargo test -p migrate`，真的 ClickHouse） | 通過 |
| 帶 `smoke` 標籤的 PR 的 nightly 顯示 ingest pod ready | 待測：nightly 只在 CI 跑（工作站不跑 Kubernetes），PR 開出並加上 `smoke` 標籤後由 CI 驗證；新的檢查是 `scripts/nightly/smoke.ts` 的 「ingest is ready」 |
| `bun test scripts/release`、`bun test scripts/nightly`、`bun test scripts/ci/changes.test.ts`、`bun scripts/ci/check-secret-handling.ts`、`cargo test --workspace` 通過 | 通過（`scripts` 的三組包含在 `bun run --cwd scripts test`；`agent` 沒有改動未跑） |
