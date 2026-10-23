# chart：Postgres 連線預算的檢查與 `max_connections` 160（2026-10-23）

Laysi/trustiot-v2#575：整版滾動更新時，所有用 Postgres 的服務新舊副本並存，連線池加總要放得進叢集內 Postgres 的 `max_connections`。chart 以 values 算出需要的連線，超過就渲染失敗；預設的 `max_connections` 從 100 調到 160。算法、每個服務的數字與記憶體估算在 [deploy/chart/README.md](../../deploy/chart/README.md) 的「連線預算」。這份報告是驗收條件的證據；在 lab 量記憶體延後到 Laysi/trustiot-v2#596。

## 環境

- **程式碼**：分支 `feat/575-postgres-connection-budget`，從 main `994ffae7` 建立。
- **本機檢查**：開發者工作站。需要容器的 Rust 測試設了 `CI=1`，不略過。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、bun 1.4.2、helm 4.3.0。
- **共享記憶體的計算**：`docker.io/library/postgres:18.6-alpine`，以 `crates/service-kit/tests/support/start.rs` 釘選的 digest。
- **日期**：2026-10-23。

## 指令

在 repo 根目錄：

```sh
bun test deploy/chart
proto install kubectl && proto install kubeconform && proto install helm && proto install flux
bun test deploy/flux
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts/ci/changes.test.ts
bun scripts/ci/check-secret-handling.ts
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p service-kit --no-default-features --all-targets -- -D warnings
CI=1 cargo test --workspace --no-fail-fast
cargo run -p openapi -- --check
cargo deny --locked check
```

各種安裝的渲染結果以 `helm template trustiot deploy/chart --namespace trustiot <values>` 取得，完整指令在 [renders.txt](data/2026-10-23-575-postgres-connection-budget/renders.txt)。

突變：每次只改一處、跑會抓到它的檢查、記下結束碼與失敗訊息，再把檔案改回原樣。指令與輸出在 data 目錄的 `m1-*.txt` 到 `m8-*.txt`。

## 結果

### 各種安裝的檢查結果

| 安裝 | 需要的連線 | 結果 |
| --- | --- | --- |
| SaaS 型態，預設（沒有開服務） | 18 | 通過 |
| SaaS 型態，四個用 Postgres 的服務 | 146 | 通過 |
| 地端型態，四個用 Postgres 的服務 | 146 | 通過 |
| 四個服務，`max_connections=146` | 146 | 通過（剛好） |
| 四個服務，`max_connections=145` | 146 | 失敗：`max_connections is 145, but 146 connections are needed: ... (per replica x pods: assets 12 x 2, certs 9 x 2, identity 31 x 2, telemetry-query 12 x 2), and 18 stay reserved ...` |
| 四個服務，原本的 `max_connections=100` | 146 | 失敗 |
| 展示環境的 overlay（目前開 identity、assets） | 104 | 通過 |
| 展示環境的 overlay，四個服務全開 | 146 | 通過 |
| 展示環境的 overlay，四個服務、identity 兩個副本 | 177 | 失敗：`identity 31 x 3` |

chart 測試另外驗證：全部規劃中的服務都開時兩種型態都通過；identity 的連線池小於 3、任何服務大於 100、用 Postgres 卻沒有 `database.maxConnections`、以自訂 `env` 設定 `TRUSTIOT__DATABASE__MAX_CONNECTIONS`、在 `serviceDefaults` 設定 `database` 都渲染失敗；smoke 替身列出 `postgres` 時因為沒有預算而失敗；`external` 模式不檢查加總、仍檢查每個連線池的範圍；Deployment 的 `maxSurge` 是 1。

### 記憶體估算的輸入

PostgreSQL 18.6 以 chart 的參數與 CloudNativePG 的預設算出的共享記憶體（[shared-memory.txt](data/2026-10-23-575-postgres-connection-budget/shared-memory.txt)）：

| `max_connections` | 100 | 150 | 160 | 180 | 200 |
| --- | --- | --- | --- | --- | --- |
| `shared_memory_size`（MB） | 152 | 156 | 157 | 157 | 159 |

同一個資料目錄的預設值：`autovacuum_max_workers` 3、`autovacuum_work_mem` -1、`superuser_reserved_connections` 3、`reserved_connections` 0。估算的公式與結果（160 時 753 MB 固定部分，剩 271 MB 給 `work_mem`）在 chart README 的「連線預算」。

### 突變

| 突變 | 改了什麼 | 抓到它的檢查 | 結果 |
| --- | --- | --- | --- |
| m1 | 拿掉 `values-check.yaml` 呼叫的 `trustiot.postgres.checkConnections` | `bun test deploy/chart -t "Postgres connection budget"` | 3 個測試失敗（145 該失敗卻通過、identity 第二個副本與較大的 assets 連線池該失敗卻通過） |
| m2 | assets 的 `EVENT_READER_CONNECTIONS` 2 → 3 | `cargo test -p assets --test chart_connections` | 失敗：`the chart's otherPools for assets`，left 2、right 3 |
| m3 | certs 的 `REGISTRATION_CONNECTIONS` 4 → 6 | `cargo test -p certs --test chart_connections` | 失敗：otherPools 4 對 6 |
| m4 | telemetry-query 的 `start` 多開一個連線池 | `cargo test -p telemetry-query --test chart_connections` | 失敗：`.max_connections(...)` 多了一處 |
| m5 | identity 的 `login::SEND_POOL_CONNECTIONS` 2 → 3 | `cargo test -p identity --test chart_connections` | 失敗：otherPools 21 對 22 |
| m6 | identity 的事件紀錄多消費一個產生端（`schema::CONSUMES`） | 同上 | 失敗：otherPools 21 對 22 |
| m7 | chart 的 identity 預算 `otherPools` 21 → 20 | 同上 | 失敗：20 對 21 |
| m8 | 從 `scripts/ci/changes.ts` 拿掉 `deploy/chart/templates/_postgres.tpl` | `bun test scripts/ci/changes.test.ts` | 失敗：`every repository path a Rust source names runs rust` |

### 檢查

| 指令 | 結果 |
| --- | --- |
| `bun test deploy/chart` | 628 pass、0 fail |
| `bun test deploy/flux` | `flux.test.ts` 全部通過，含新的「with all four Postgres services on …」；`readme-commands.test.ts` 的 3 個測試在這台工作站失敗（展示機 age 金鑰指令的 bash 區塊回 127：測試把 `PATH` 設為 `/usr/bin:/bin`，這台工作站的 `sha256sum` 不在其中），main 上相同。它只讀 `deploy/flux/README.md`，這次沒有改，CI 在 Linux 上執行它 |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun test scripts/ci/changes.test.ts` | 282 pass |
| `bun scripts/ci/check-secret-handling.ts` | 沒有違規 |
| `cargo fmt --all --check`、兩個 `cargo clippy` | 通過 |
| `CI=1 cargo test --workspace --no-fail-fast` | 1301 pass、4 fail。新的四個 `chart_connections`、`testkit` 的 `chart` 單元測試、`repo-check` 與 `service-kit` 的 `tenant_db`（以 `TRUSTIOT__DATABASE__MAX_CONNECTIONS=10` 跑 example-service）都通過。失敗的 4 個與這次的變更無關，都是這台工作站的環境：`service-kit` 的 `container` 與 `verify_container` 把主機建的 example-service 放進 Linux 容器，這台工作站建出的不是 Linux 執行檔，容器起不來；`ai-clients` 的 `an_unanswered_connection_attempt_is_a_connect_timeout` 在這台工作站填不滿 accept 佇列；`migrate` 的 `mqtt_broker` 在 Mosquitto 重新載入後容器停止，重跑一次結果相同。這次沒有改 `service-kit` 的函式庫、`tools/migrate` 或 `deploy/chart/files`，CI 在 Linux 上執行這些測試 |
| `cargo run -p openapi -- --check` | `api/openapi.json is up to date` |
| `cargo deny --locked check` | advisories、bans、licenses、sources 都 ok |

### 合併 main 之後

2026-10-23 把 main `dbe476c`（Laysi/trustiot-v2#589 nightly 部署 certs、Laysi/trustiot-v2#593 成員角色）合併進本分支：`docs/roadmap/milestones.md` 的「拆分」段兩邊各加一句，兩句都保留；其餘自動合併。合併後在 `git archive` 解出的乾淨目錄重跑：

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check`、兩個 `cargo clippy`、`cargo run -p openapi -- --check` | 通過 |
| `CI=1 cargo test -p assets -p certs -p identity -p telemetry-query -p testkit -p repo-check -p contract`、`CI=1 cargo test -p service-kit --test tenant_db` | 全部通過（含四個 `chart_connections` 與 identity 新的 `members`） |
| `bun test deploy/chart` | 628 pass、0 fail |
| `bun test deploy/flux` | 74 pass、3 fail，失敗的是上面同樣 3 個 `readme-commands.test.ts`，main 上相同 |
| `bun test scripts` | 1621 pass、26 fail；main `dbe476c` 上同一台工作站跑出完全相同的 26 個失敗（`scripts/flux`、`scripts/nightly/certs.test.ts` 與 `scripts/dev` 的憑證測試，工作站的 openssl、`sha256sum` 與 sudo 環境），這次只改了 `scripts/ci/changes.ts` |
| `bun run --cwd scripts typecheck`、`bun scripts/ci/check-secret-handling.ts`、`cargo deny --locked check` | 通過 |

同日再合併 main `87b51f3`（Laysi/trustiot-v2#595 mqtt-bridge、Laysi/trustiot-v2#602 assistant 代表使用者呼叫其他服務、Laysi/trustiot-v2#594 第一位平台管理員的 bootstrap）：`deploy/chart/templates/_service.tpl` 的 env 說明與 mqtt-bridge 的 env 兩邊都保留。

- Laysi/trustiot-v2#594 加了操作者從 CronJob 建立的 bootstrap Job，以 owner 開一條 session（`tools/migrate/src/platform_admin.rs`），可能在換版時執行，所以保留的連線加 1 條，共 18 條；四個服務全開時需要 146 條，`max_connections` 160 還有 14 條餘裕。
- 上方「各種安裝的檢查結果」的表、[renders.txt](data/2026-10-23-575-postgres-connection-budget/renders.txt) 與突變 m1 都依合併後的程式重跑。
- mqtt-bridge 不連 Postgres（`mesh.calls` 只列 `mqtt-broker`），assistant 仍沒有在 `mesh.calls` 列出 `postgres`，所以兩者都不在預算裡。

## 結論

- 連線總數超過 `max_connections` 時檢查失敗：**通過**（上表的 145、100 與 identity 兩個副本；chart 測試；突變 m1 證明測試靠的是這個檢查）。
- 預設、`values-saas.yaml`、`values-onprem.yaml` 與展示環境四個服務全開都通過檢查：**通過**（`bun test deploy/chart`、`bun test deploy/flux` 的新測試）。
- 服務的程式改了連線池而 chart 沒跟著改時有測試失敗：**通過**（突變 m2–m7）。身分服務在 `lib.rs` 再呼叫一次既有的連線池建構函式（例如多開一個 `LoginStore`）時，這次的測試抓不到，等 Laysi/trustiot-v2#497 的 `connections_per_replica()` 接上。
- README 寫明算法、每個服務的數字與記憶體估算，新的 `max_connections` 估算在 1Gi 以內：**通過**，前提是估算以「固定部分加上可同時滿額的排序數」計算；若要求每條連線同時跑一個滿額 `work_mem` 排序，160 需要 1393 MB（原本的 100 也要 1028 MB），超過 1Gi，見 chart README。
- 在 lab 開滿一次量記憶體：**待測**，延後到 Laysi/trustiot-v2#596。
