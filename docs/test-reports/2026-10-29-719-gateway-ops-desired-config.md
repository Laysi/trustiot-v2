# gateway-ops：服務骨架、Postgres schema 與帶版本的 desired 設定

對應 Laysi/trustiot-v2#719。

這份報告記錄 2026-10-29 的檢查：gateway-ops 由資產服務的事件產生、存下每台閘道器帶版本的 desired 設定（新增、修改、刪除、重播、別的租戶、改綁、解綁、錯誤），跨租戶負向測試，真的執行檔的啟動與 SIGTERM，以及登記服務之後整個 workspace、腳本與 chart 的測試。

## 環境

- 程式碼：分支 `feat/719-gateway-ops-desired-config`，基於 main `5dad0429`；commit `5a4301be`（事件的 `binding_id`）、`f3d6ac9f`（服務）與 `a36e656b`（nightly.yml 的路徑）。
- 建置主機（macOS arm64）：gateway-ops 的 clippy 與測試，在 `f3d6ac9f` 之前的工作樹快照上執行（gateway-ops 的程式與測試和 `f3d6ac9f` 相同，之後只改了文件、contract 測試讀固定樣本的順序與 `scripts/tests/migrate.test.ts`）。設了 `CI=1`，容器測試以 podman 執行，沒有容器執行環境時失敗而不是略過。
- 建置主機（x86_64 Linux）：整個 workspace 的 `cargo fmt --check`、clippy、測試與 OpenAPI 檢查，在 `f3d6ac9f` 的樹上（`a36e656b` 只改 nightly.yml，Rust 不讀它）；腳本、chart 與 workflow 的檢查在 `a36e656b` 的樹上。都設了 `CI=1`。
- Postgres 是 `testkit::product` 的 TLS Postgres：migration Job 套用整個產品的 schema，服務以 `verify-full`、以 `gateway_ops_app` 與 `gateway_ops_platform` 連線；資產服務的事件以 `assets_app` 在租戶交易裡寫入，和資產服務的寫法相同。

## 指令

```sh
# gateway-ops（建置主機，macOS）
cargo clippy -p gateway-ops -p migrate -p contract -p assets --all-targets -- -D warnings
cargo test -p gateway-ops -- --nocapture

# workspace（建置主機，x86_64 Linux）
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo run --locked -p openapi -- --check

# 腳本、chart 與 workflow（建置主機）
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts
bun test deploy/chart
bun scripts/ci/check-workflows.ts
bun scripts/ci/lint-workflows.ts

# 密鑰掃描（開發者工作站）
gitleaks dir --redact <branch 的 git log -p 輸出>
git archive HEAD | tar -x -C <dir> && gitleaks dir --redact <dir>
```

## 結果

gateway-ops 測試的輸出見 [gateway-ops-tests.txt](data/2026-10-29-719-gateway-ops-desired-config/gateway-ops-tests.txt)，workspace 每個測試執行檔的結果見 [workspace-summary.txt](data/2026-10-29-719-gateway-ops-desired-config/workspace-summary.txt)。

### 新增、修改、刪除與重播（`tests/desired_config.rs`）

`device_changes_make_versions_and_replays_make_none`，一台閘道器、一個租戶：

| 步驟 | 版本 | 設備數 |
| --- | --- | --- |
| 綁定 | 1 | 0 |
| 新增沒有設備使用的型號 | 不變 | |
| 新增設備 1 | 2 | 1 |
| 新增設備 2 | 3 | 2 |
| 修改設備 1（輪詢間隔） | 4 | 2 |
| 同內容的新 revision、晚到的舊 revision | 不變 | |
| 刪除設備 2 | 5 | 1 |
| 以 `Replay::Reprocess` 重播以上 8 個事件 | 不變（仍是 5 版） | 1 |

每一版的 `payload` 都以 crates/protocol 的 `DesiredConfig::decode`（解碼並驗證，閘道器套用前跑的那一套）讀回，`payload` 裡的版本和 `version` 欄相同；第 4 版的設備 1 是最新 revision 的內容，第 5 版不再列出設備 2。

### 別的租戶與先前的綁定

`other_tenants_and_ended_bindings_stay_out`：閘道器先綁租戶 A（版本 1、2，第 2 版有 A 的設備）；租戶 B 在同一個硬體 ID 寫兩個設備（一個帶不是這台閘道器的綁定，一個不帶 `binding_id`，代表 2026-10-29 以前的事件），版本不變；閘道器改綁 B 之後第 3 版屬於 B、只有 B 不帶綁定的那個設備；A 的設備之後的修改不產生版本，B 目前綁定的新設備產生第 4 版。輸出：`versions [1, 2, 3, 4] in tenants ["A", "A", "B", "B"]`，B 另一個綁定的設備不在任何一版，A 的設備不在 B 的任何一版。

### 解綁與讀取介面

- `ending_the_binding_leaves_a_configuration_without_devices`：`apply_unbinding` 呼叫兩次，只存一版（第 3 版，`bound = false`、0 個設備、原租戶與綁定）；之後的設備事件不產生版本；沒記錄過的綁定什麼都不做。
- `the_delivery_reads_the_latest_version_and_cannot_write`：`DesiredConfigs::current` 回最新一版，`payload` 和存的完全相同；沒有設定的閘道器回 `None`；`latest_versions` 回每台閘道器的最新版本；`gateway_ops_platform` 寫入 `desired_configs` 被拒（`42501`）；租戶 B 的交易讀不到 A 的設定。

### 產生不出設定（`tests/generation_errors.rs`）

| 情況 | 版本 | 日誌 |
| --- | --- | --- |
| 設備引用投影沒有的型號 | 停在 1 | ERROR，`reason=unknown_model`，`error` 寫出設備與型號 ID |
| 型號送到 | 2 | INFO `gateway configuration stored` |
| 同一個 converter 混用傳輸方式 | 停在 2，內容不變 | ERROR，`reason=invalid`，`error` 是 `mixed_transport at devices[1].transport`，沒有連線設定 |
| 刪掉違規的設備 | 停在 2（內容和第 2 版相同） | |
| 型號被刪除 | 停在 2 | ERROR，`reason=unknown_model` |
| 別的租戶的設備事件 | 停在 2 | WARN，`reason=other_tenant` |

### 執行檔、跨租戶與 workspace

- `tests/startup.rs`：執行檔以 chart 給的兩個角色啟動、`/readyz` 200，訂閱把綁定、型號與設備事件變成版本 1、2，日誌有 `gateway configuration stored`（`version` 2），SIGTERM 後以 0 結束。
- `tests/cross_tenant.rs`：服務角色對 `gateway_bindings`、`device_models`、`devices`、`desired_configs` 的跨租戶探測全數通過，服務角色碰得到的每個關聯都有宣告。
- workspace（x86_64 Linux）：`cargo fmt --check` 與 clippy 沒有輸出；246 個測試執行檔，2051 通過、0 失敗、1 忽略（既有的）；`api/openapi.json is up to date`。結束於 `exit 0 at 03:27:25 (build host)`。
- macOS 的建置主機上，`tools/migrate/tests/mqtt_broker.rs` 有 4 個測試在啟動 Mosquitto 容器時失敗（`lsetxattr ... read-only file system`：那台主機的 podman 不能替掛載的目錄重新標記 SELinux 標籤），和這次的變更無關；同一個檔案在 x86_64 Linux 的整個 workspace 執行裡通過。
- 腳本與 chart：見下方「腳本與 chart」。
- 密鑰掃描：branch 的兩個 commit 與 `git archive HEAD` 的樹都 `no leaks found`。

### 腳本與 chart

摘要見 [scripts-chart-summary.txt](data/2026-10-29-719-gateway-ops-desired-config/scripts-chart-summary.txt)。

- `bun run --cwd scripts typecheck` 沒有錯誤。
- `bun test scripts`：1704 通過、0 失敗、9 略過；略過的是要設環境變數或特定平台才跑的測試（本機開發環境的 podman 端對端測試、aarch64 執行檔、寄信 pod 的腳本）。
- 第一次在 `f3d6ac9f` 上跑時 `scripts/nightly/image-cache.test.ts` 有 1 個失敗：`services/gateway-ops/src/schema.rs`（migration Job 以 `#[path]` 編進去）不在 nightly.yml 的 `pull_request.paths`。`a36e656b` 把這個檔案加進去之後全部通過。
- 本機開發環境的端對端測試（`TRUSTIOT_DEV_E2E=1 bun test scripts/tests/e2e.test.ts`，建置主機，macOS）：`up` 跑的 migration Job 成功，`migration Job finished: 151 change(s), 0 warning(s); identity_app, …, ingest_app, ingest_platform, gateway_ops_app, gateway_ops_platform can log in`，也就是本機環境替 gateway-ops 的兩個角色綁了密碼。之後的 example-service 檢查失敗（`GET /healthz ... Unable to connect`），因為那台主機以 cargo 建出的是 macOS 的執行檔，放不進 Linux 容器執行；這個測試要在 x86_64 Linux 上跑完整流程，這次沒有再跑（與這次的變更無關的部分，待測）。
- `bun test deploy/chart`：659 通過、0 失敗（`_migrate.tpl` 的 `platformLogins` 多了 `gateway-ops`）。
- `check-workflows.ts`：4 個檔案沒有違規；`lint-workflows.ts`：3 個 workflow 檔沒有錯誤。結束於 `exit 0 at 03:57:36 (build host)`。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 1. 新增、修改設備產生該閘道器的新版本，通過 `protocol::config` 驗證；重播不多產生版本 | 通過 |
| 2. 刪除設備產生不含它的新版；解綁後的設定沒有設備 | 通過（解綁以 `apply_unbinding` 驗證；解綁事件本身是 Laysi/trustiot-v2#115） |
| 3. 事件租戶和閘道器目前綁定的租戶不同時忽略，不在任何設定裡 | 通過 |
| 4. 未知型號不產生新版、明確報錯、上一版不變 | 通過 |
| 5. 跨租戶負向測試涵蓋 gateway-ops 的表 | 通過 |
| 6. `cargo test --workspace`、clippy、`cargo run -p openapi -- --check`、`bun test scripts` | 通過 |
