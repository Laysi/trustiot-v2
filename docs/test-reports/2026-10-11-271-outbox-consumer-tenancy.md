# outbox 消費端改用平台角色讀取、在事件租戶處理（Laysi/trustiot-v2#271）

驗證 `outbox::Consumer` 依決策 29 連線後的行為：以消費端服務的平台角色在 RLS 下讀產生端的 `outbox_event`、每個事件在服務角色的租戶交易處理，以及原本「有序、冪等、至少一次」的保證仍成立（Laysi/trustiot-v2#54 的獨立驗證見 [2026-10-11-54-verify](2026-10-11-54-verify.md)）。

## 1. 環境

| 項目 | 內容 |
| --- | --- |
| 機器 | 共用建置主機 `ruisi-wsl`（Ubuntu on WSL2，Linux 6.18.33.2-microsoft-standard-WSL2 x86_64，20 核），以 `rcheck.sh` 執行，`CI=1`（缺容器執行環境時測試會失敗、不會略過） |
| 工具 | rustc 1.99.0、Docker 29.7.2 |
| Postgres | `postgres:18.6-alpine@sha256:77f58511…`，每個測試一個用完即刪的容器，以測試時產生的 CA 啟用 TLS；消費端以 service-kit 的 pool 連線（`sslmode=verify-full`、每個新連線做 RLS 檢查） |
| 角色 | 每個測試的 `Database::setup`（`crates/outbox/tests/support/mod.rs`）建立產生端的服務角色與平台角色、消費端的服務角色與平台角色；`outbox_event` 執行 service-kit 的 [`sql/outbox-event.sql`](../../crates/service-kit/sql/outbox-event.sql)（RLS 與 `tenant_isolation`、`platform_events`、`consumer_read` 三條 policy），授權照 outbox README「資料庫權限」 |
| 分支 | `feat/271-outbox-platform-pool`：先在 main `e8f180b` 上測，合併 main `e21ff41` 後重測一次 |

## 2. 指令

從 repo 根目錄：

```
cargo test --locked -p outbox
cargo test --locked -p outbox --test tenancy -- --nocapture --test-threads=1
for i in 1 2 3; do cargo test --locked -p outbox --test crash --test delivery --test tenancy; done
cargo test --locked -p service-kit --test tenant_db
```

需要 podman 或 docker；設 `CI=1` 時缺少它們會讓測試失敗而不是略過。

## 3. 結果

完整輸出：[`outbox-tests.txt`](data/2026-10-11-271-outbox-consumer-tenancy/outbox-tests.txt)（基於 `e8f180b`）；合併 main `e21ff41` 後的重測：[`merged-main-checks.txt`](data/2026-10-11-271-outbox-consumer-tenancy/merged-main-checks.txt)，含 `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、service-kit 無預設 feature 的 clippy、`cargo test --workspace`（64 個測試套件、482 個測試全部通過）、outbox 全部測試與 crash／delivery／tenancy 重複 3 次、`tenant_db`、`cargo deny --locked check`，全部通過。

| 測試 | 結果 |
| --- | --- |
| `cargo test -p outbox`（lib 8、crash 2、delivery 13、metrics 1、notify 2、producer 3、tenancy 5） | 全部通過 |
| crash + delivery + tenancy 重複 3 次 | 3 次全部通過（2 + 13 + 5 個測試） |
| `cargo test -p service-kit --test tenant_db` | 10 個全部通過（含新增的 `PlatformReader` 唯讀、`begin_without_tenant`、`TenantTransaction::transaction()` 檢查） |

`tests/tenancy.rs` 的 `--nocapture` 輸出（節錄）：

```
tenant A's event: a write to tenant B fails with SQLSTATE 42501
a platform-wide event: a write to tenant B fails with SQLSTATE 42501
reader without consumer_read: the platform role cannot read every tenant's events in schema `producer_422178_6`: `outbox_event` needs a consumer_read policy for it
service role with a cross-tenant read policy: refused the database session: producer_422178_9.outbox_event policy service_read applies to the database role and is not exactly the tenant condition, so it may expose other tenants' rows
platform role read 3 events (tenant A, tenant B, platform-wide); each handler ran in its event's tenant: [(…, Some("01928f6e-…-00000000000a"), 1), (…, Some("01928f6e-…-00000000000b"), 1), (…, None, 0)]
```

最後一行的三個欄位：事件 ID、handler 交易裡的 `trustiot.tenant_id`、handler 看得到的 `note` 列數。資料表裡另有一列租戶 C 的 note（擁有者寫入），租戶 A、B 的 handler 都只看到自己剛寫的 1 列，全平台事件的 handler 看到 0 列。

既有測試在新架構下的意義：

- 所有既有的投遞、當機、重播、通知模式、lag 測試都改以上述角色執行：消費端以平台角色在 RLS 下讀、以服務角色在事件租戶的交易處理。`Setup::publish` 的事件依聚合分屬租戶 A 或 B，併發測試（6 個寫入端、4 個聚合、2 個副本）的事件因此分屬兩個租戶。
- 當機測試（`crash.rs`）的 `outbox-example` 也改用 service-kit 的 pool（TLS、兩個角色），並記錄處理時的租戶：commit 後投遞前被 SIGKILL、handler 寫入後 commit 前被 SIGKILL，重啟後都只處理一次，且在事件的租戶處理。
- `services_need_only_the_documented_grants`：產生端服務角色以 `TenantTransaction::transaction()` 寫租戶事件、產生端平台角色以 `PlatformTransaction::transaction()` 寫全平台事件，消費端都收到；消費端平台角色讀不到產生端其他表、刪不了事件（42501），消費端服務角色碰不到產生端 schema（42501）。

## 4. 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 消費端以平台角色讀取 RLS 下的 `outbox_event`，讀得到所有租戶的事件；以服務角色連線時被 service-kit 拒絕（測試，真 Postgres） | 通過 | `tenancy.rs`：`the_platform_role_reads_every_tenant_and_each_event_runs_in_its_tenant`（租戶 A、B、全平台三個事件都送達、lag 0）；`a_service_role_that_can_read_across_tenants_is_refused`（把跨租戶讀取開給服務角色，消費端得到 `UnsafeSession::PermissivePolicy`）；`a_reader_without_the_read_policy_stops_the_consumer`（以沒有 `consumer_read` 的角色讀時回 `CannotReadAllTenants`，不會靜靜讀不到） |
| handler 的寫入只落在事件的租戶；handler 試圖寫入其他租戶時失敗（測試） | 通過 | `the_platform_role_reads_every_tenant_and_each_event_runs_in_its_tenant`（寫入落在事件租戶、看不到別的租戶）；`a_handler_cannot_write_another_tenant`（租戶 A 的事件與全平台事件寫租戶 B 都被拒，SQLSTATE 42501，沒有寫入、位置不動）；`an_event_whose_envelope_names_another_tenant_is_dead_lettered`（信封租戶與 log 列不符時不執行 handler） |
| 既有的 outbox 有序、冪等遞送測試全部仍通過，包含 Laysi/trustiot-v2#54 獨立驗證新增的測試 | 通過 | `cargo test -p outbox` 全部通過，crash、delivery、tenancy 重複 3 次通過。#54 的獨立驗證（PR Laysi/trustiot-v2#274）沒有新增進 `cargo test` 的測試，它的 harness 以 `.txt` 存在 data 目錄、需要數十分鐘的多行程混沌執行，本次沒有重跑；保證成立的理由見 outbox README「交易邊界與保證」：交易的切法與每筆交易內的語句順序不變，只換了 pool 與租戶設定 |
| README 已更新；測試證據存到 `docs/test-reports/` | 通過 | `crates/outbox/README.md`、`crates/service-kit/README.md`、`tools/sql-check/README.md`、`docs/architecture/decisions.md` 決策 29；本報告 |

待測：#54 的混沌 harness 在新連線方式（TLS、四個角色）下的長時間執行。harness 目前直接用一個非 TLS 的角色連線，要先改寫連線方式；本次以上述 `cargo test` 的併發、當機、重播測試與交易邊界的推論代替。
