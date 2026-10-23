# assistant 部署：映像、chart 啟用、mesh 規則、授權矩陣與 nightly

對應 Laysi/trustiot-v2#592。

這份報告記錄 2026-10-24 在開發者工作站與建置主機上的檢查。驗收條件 2 的證據是帶 `smoke` label 的 PR 觸發的 nightly run，它在 Kubernetes（k3d）上執行，工作站與建置主機都不跑 Kubernetes；那次 run 的結果在下方「nightly（待補）」補上。

## 環境

- 程式碼：分支 `feat/592-assistant-deploy`，基於 main `74216494`；之後合併 main 的 `6da9b15f` 與 `d1269a0d`（Laysi/trustiot-v2#575 的 Postgres 連線預算，PR Laysi/trustiot-v2#609），合併後重跑的結果在「合併連線預算之後」。
- 開發者工作站（x86_64 Linux），Bun 1.4.2，其餘工具依 `.prototools` 釘選（helm 4.3.0、kubeconform 0.8.0、actionlint 1.7.12）。
- 建置主機（x86_64 Linux，rootless podman 5.7.0）：Rust 的 clippy 與測試（容器測試以 podman 起 Postgres，`CI=1`，所以沒有容器執行環境時會失敗而不是略過）、`bun test deploy/chart`、`bun test scripts`，以及以發佈腳本建置 arm64 映像。每次在工作樹的快照上執行。

## 指令

```sh
# Rust（建置主機）
cargo clippy --locked -p assistant --all-targets -- -D warnings
cargo test --locked -p assistant
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p migrate -p repo-check -p openapi

# chart 與 Bun scripts（chart 與 scripts 的整套測試在建置主機）
bun test deploy/chart
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts
bun test deploy/flux

# CI 規則（工作站）
bun test scripts/ci/changes.test.ts
bun scripts/ci/check-workflows.ts
bun scripts/ci/check-secret-handling.ts
proto install actionlint
bun scripts/ci/lint-workflows.ts

# arm64 映像（建置主機）：plan 後把 plan.json 的 images 篩成 assistant 一個，再 build
work="$(mktemp -d ~/.cache/trustiot-release-592.XXXXXX)"
bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run --registry 127.0.0.1:5068/laysi/trustiot-v2 --output "$work/plan.json"
bun -e 'const f = process.argv[1]; const p = await Bun.file(f).json(); p.images = p.images.filter((i) => i.name === "assistant"); await Bun.write(f, JSON.stringify(p, null, 2));' "$work/plan.json"
bun scripts/release/build.ts --plan "$work/plan.json" --out "$work/out"

# nightly：PR 加上 smoke label 後由 .github/workflows/nightly.yml 執行
```

## 結果

| 檢查 | 在哪裡 | 結果 |
| --- | --- | --- |
| `cargo clippy -p assistant --all-targets -- -D warnings` | 建置主機 | 通過 |
| `cargo test -p assistant` | 建置主機 | 全部通過；新的 `tests/startup.rs` 1 個測試通過（4.15 秒），`config` 的單元測試含新的 `the_database_login_and_pool_are_checked` |
| `cargo clippy --workspace --all-targets -- -D warnings` | 建置主機 | 通過 |
| `cargo test -p migrate -p repo-check -p openapi` | 建置主機 | 全部通過 |
| `bun test deploy/chart` | 建置主機 | 639 pass、0 fail |
| `bun test scripts` | 建置主機 | 1662 pass、0 fail（9 個依環境略過，與 main 相同） |
| `bun run --cwd scripts typecheck` | 工作站 | 通過 |
| `bun test deploy/flux` | 工作站 | 76 pass、0 fail |
| `bun test scripts/ci/changes.test.ts` | 工作站 | 282 pass、0 fail |
| `check-workflows.ts`、`check-secret-handling.ts`、`lint-workflows.ts` | 工作站 | 都沒有違規 |
| arm64 映像（發佈腳本） | 建置主機 | 建出 `assistant`，digest `sha256:bb2db5d1d12ac667820d10d9d632a5074a67f58f7fad9024d473c3f29ac540e0`，4,236,288 位元組，SBOM 221 個套件；執行檔通過 aarch64 靜態 ELF 檢查（[data/…/image-build.txt](data/2026-10-24-592-assistant-deploy/image-build.txt)） |
| 突變測試：位址不看 `mesh.calls.assistant` | 工作站 | 驗收條件 3 的測試在兩種形態都失敗，還原後通過（[data/…/mutation.txt](data/2026-10-24-592-assistant-deploy/mutation.txt)） |

`tests/startup.rs` 以 migration Job 套用整個產品 schema 的 TLS Postgres 執行真正的 `assistant` 執行檔：以 `assistant_app` 登入時 `/readyz` 回 200、日誌記下 `assistant ready`（`tools=3`、`delegation=true`），輸出裡沒有密碼；密碼錯誤時記下 `readiness check failed`、回 503；刪掉 `0001_assistant.sql` 的套用紀錄時日誌寫出缺的檔案、回 503，補回後恢復 200。

chart 測試（`deploy/chart/tests/chart.test.ts` 的「the assistant (decision 31, Laysi/trustiot-v2#592)」）：

- 驗收條件 1：預設 values、所有服務啟用時，放行 `trustiot-assistant` 的 AuthorizationPolicy 只有 `ai-gateway`、`assets`、`telemetry-query`、`identity-exchange`（只到 8082）與每個服務都自動連得到的 `otel-collector`；身分服務自己的政策（API 埠）不放行它。NetworkPolicy 放行它的 pod 的也只有這五個的 HBONE 15008，加上叢集內 Postgres 的 5432。README 的「呼叫矩陣」由既有的比對測試與渲染結果核對。
- 驗收條件 3：從渲染出的 assistant Deployment 取出每個 URL，經它指名的 Service 找到後面唯一的工作負載與 container 埠，再找出在那個埠放行 assistant 的 AuthorizationPolicy：服務一定在 `mesh.calls.assistant`、政策就是它自己的；token exchange 只到身分服務的 `identity-exchange` 政策；工具的服務都在身分服務的 `TRUSTIOT_ON_BEHALF_CALLS` 上，反過來清單上的每個服務都有位址。六種組合（預設、清單少了 assets、telemetry-query 關閉、只有 AI 閘道、AI 閘道關閉、空清單）× 兩種形態。
- `services.assistant.env` 與 `serviceDefaults.env` 設定四個位址變數（不分大小寫、有沒有 schema 驗證、清單空或不空）都讓渲染失敗；被呼叫端沒有 `http` 埠時渲染失敗；其他服務拿不到這些變數。

## 合併連線預算之後

Laysi/trustiot-v2#575 讓 chart 加總每個用 Postgres 的服務在整版滾動時的連線池，超過 `postgres.bundled.parameters.max_connections` 就渲染失敗；沒有預算或沒有 `database.maxConnections` 的服務也渲染失敗。這個分支合併它之後為 assistant 加上：

- `trustiot.postgres.connectionBudgets` 的 `assistant`：`database.maxConnections` 2 到 100（與 `assistant::config` 的 `MIN_DATABASE_CONNECTIONS`、`MAX_DATABASE_CONNECTIONS` 相同），沒有其他連線池；
- `services.assistant.database.maxConnections: 4`（服務程式的預設值，chart 以 `TRUSTIOT__DATABASE__MAX_CONNECTIONS` 傳入）；
- `services/assistant/tests/chart_connections.rs`：程式裡的 `.max_connections(...)` 只有 `lib.rs` 的服務角色連線池（另一個在 `store.rs` 的單元測試裡）、預算與範圍一致。

加總：assistant 每副本 4 條、整版滾動 8 條。五個服務全開時服務共 140 條，加上保留的 18 條是 158 條，預設 `max_connections` 160 只剩 2 條餘裕；下一個用 Postgres 的服務（例如 Laysi/trustiot-v2#605）要一起調高它。chart README「連線預算」的表格與 chart 測試的 `needed`（158）、合計列（70、140）與錯誤訊息的明細都跟著更新。展示環境的 overlay 沒有啟用 assistant，`bun test deploy/flux` 的數字不變。

| 檢查 | 在哪裡 | 結果 |
| --- | --- | --- |
| `cargo clippy --workspace --all-targets -- -D warnings` | 建置主機 | 通過 |
| `cargo test -p assistant -p testkit -p assets` | 建置主機 | 全部通過；assistant 的新測試 `tests/chart_connections.rs` 通過，`tests/startup.rs` 仍通過，testkit 的 `chart::tests::reads_every_budget_of_the_chart` 也讀到 assistant 的預算 |
| `bun test deploy/chart` | 建置主機 | 648 pass、0 fail（含「Postgres connection budget (Laysi/trustiot-v2#575)」：`needed` 158，`max_connections` 157 時渲染失敗、明細列出 `assistant 4 x 2`） |
| `bun test deploy/flux` | 工作站 | 77 pass、0 fail |
| `bun run --cwd scripts typecheck` | 工作站 | 通過 |
| `cargo fmt --all --check` | 工作站 | 通過 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| chart 只允許 assistant 呼叫 `ai-gateway`、`telemetry-query`、`assets` 與身分服務的 `token-exchange` 埠；授權矩陣文件與 chart 測試一致（`bun test deploy/chart`） | 通過（上方 chart 測試；`bun test deploy/chart` 639 pass，合併連線預算之後 648 pass） |
| 帶 `smoke` label 的 PR 的 nightly 以兩份 values 部署 assistant，pod 就緒，`assistant_app` 能登入（nightly run 為證據） | 待測：要等 PR 帶上 `smoke` label 後的 nightly run，見下方「nightly（待補）」 |
| chart 測試確認 assistant 設了位址的每個被呼叫端都在 `mesh.calls.assistant`；叢集內真正的工具呼叫是 Laysi/trustiot-v2#146 的前提條件 | 通過（chart 測試與突變測試）；叢集內的工具呼叫留給 Laysi/trustiot-v2#146 |
| `bun test scripts`、`bun test deploy/chart`、`bun run --cwd scripts typecheck` 通過 | 通過 |

## nightly（待補）

PR 帶上 `smoke` label 後的 nightly run：run 連結、commit、兩個形態的「every workload of the release is ready」與「the AI assistant is ready: it logged in to Postgres as assistant_app on its current schema (Laysi/trustiot-v2#592)」兩項的結果與 detail（就緒副本數與 `assistant_app` 的連線數）、job 的總時間與 `timeout-minutes` 85 的比較。
