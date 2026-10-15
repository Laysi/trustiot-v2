# 服務在資料庫缺少 migration 時不 ready（Laysi/trustiot-v2#402）

對應 Laysi/trustiot-v2#402：service-kit 的 `migrations` readiness 檢查、migration Job 在每個服務 schema 寫的紀錄 `<schema>.trustiot_migrate_applied`，以及身分與資產服務的接線。設計見 [部署與可搬移性](../architecture/deployment.md)的「服務自己確認 migration 已套用」。

## 環境

- 建置主機（x86_64 Linux；需要容器的測試用它的 rootless podman），經協調者的檢查佇列（`rcheck.sh`）執行，設了 `CI=1`，所以需要容器的測試不會略過。
- 分支 `feat/402-migration-readiness`，以 `origin/main` 的 `86b52f7` 為基礎。
- 工具版本照 `.prototools` 與 `rust-toolchain.toml`；Postgres 是測試支援程式以 digest 鎖定的 `postgres:18.6-alpine`（帶 TLS）。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
cargo test --locked -p service-kit --lib migrations
cargo test --locked -p migrate --test service_records --test fresh_database --test outbox_and_tenant_roles --test file_escapes --test registry
cargo test --locked -p identity --test migrations --test cross_tenant
cargo test --locked -p assets
cargo test --locked --workspace
cargo test --locked -p service-kit --no-default-features
cargo run --locked -p openapi -- --check
bun scripts/ci/check-secret-handling.ts
bun test scripts/ci/changes.test.ts
```

## 結果

| 測試 | 驗證 | 結果 |
| --- | --- | --- |
| `service-kit` 單元測試 `migrations::tests`（6 個） | 清單比對：缺少、SHA-256 不同、資料庫較新（rollback）、同名檔案在別的 component；producer 的清單先列兩個 outbox 範本；錯誤文字寫出每個檔案與「run the migration Job of this release」，`no_records` 時說明紀錄表不存在或沒有授權；ERROR 日誌的 `missing`、`changed`、`no_records` 欄位，同樣的失敗只記一次、改變時再記；通過後不再查詢（pool 指向不存在的主機也通過） | 通過 |
| `tools/migrate/tests/service_records.rs` | 真的 Job、測試用的服務清單（服務名稱帶 `-`、outbox producer 與它的消費端）：副本與 `trustiot_migrate.applied` 相同，表屬於 `record_keeper_owner`，ACL 只有 owner 與 `record_keeper_app=r`；下一版的映像（多一個檔案）在上一版 Job 的資料庫上 `MigrationCheck` 失敗並寫出 `record-keeper/0002_second.sql`，上一版映像通過；下一版的 Job 跑完後同一個檢查通過；上一版映像在較新的資料庫上通過；失敗的檔案兩份紀錄都不留；第二次執行沒有變更（catalog 快照相同）；拿掉紀錄表（#402 之前的資料庫）時 `no_records` 且 4 個檔案都缺，下一次執行只有「建立」與「補齊 4 筆」兩項變更、檢查通過，再下一次沒有變更；手動加一筆、改一筆 hash 時，下一次執行改回並在報告寫出；服務角色讀得到自己的 4 筆、讀不到 `trustiot_migrate.applied`、不能新增修改刪除；另一個服務的服務角色、能進這個 schema 的消費端平台角色都讀不到；各角色仍通過 service-kit 的連線檢查 | 通過 |
| `services/identity/tests/migrations.rs` | 真的 binary、Job 套用的產品 schema：副本等於 Job 的紀錄、等於 `identity::schema::SCHEMA.expected()`（兩個 outbox 範本與兩個檔案）；ready 並記 INFO；superuser 拿掉 `0002_tenants.sql` 的紀錄後，新的行程 `/readyz` 503，ERROR 的 `missing` 是 `identity/0002_tenants.sql`、`changed` 是 `none`；已經通過的行程維持 200；紀錄補回後同一個新行程轉為 200；`outbox-aggregate.sql` 的 hash 改成 64 個 0 時 503、`changed` 寫出它與兩個 hash；`identity_app` 讀不到 `trustiot_migrate.applied`、`assets.trustiot_migrate_applied`，不能改自己的紀錄 | 通過 |
| `services/assets/tests/migrations.rs` | 同上的情境（`assets/0001_assets.sql`）；資料庫多一筆較新版本的紀錄時照常 ready，INFO 的 `newer` 寫出它；`assets_app` 讀不到 `identity` 的紀錄；`assets_platform` 有 `identity` schema 的 USAGE（讀事件），仍讀不到 `identity` 與 `assets` 的紀錄；`identity_app` 讀不到 `assets` 的紀錄 | 通過 |
| `services/{identity,assets}/tests/cross_tenant.rs` | 跨租戶負向測試（決策 29），紀錄表以「不是租戶資料」豁免 | 通過 |
| `migrate` 其他整合測試、`identity` 的 `ext_authz.rs`、`adversarial.rs`、`tenants.rs`、`assets` 的 `tenant_created.rs`、`outbox`、`testkit` | 既有行為不變（`fresh_database.rs` 的身分 schema 表數由 6 改為 7；`outbox_and_tenant_roles.rs` 模擬 `outbox-aggregate.sql` 之前的資料庫時一併刪掉副本的那一筆；身分服務手動建 schema 的兩個測試檔以 `record_release_migrations` 代替 Job 寫紀錄，並給 `identity_app` 設定 Job 會設的 `search_path`） | 通過 |
| `cargo test --workspace`、`service-kit` 不帶預設 feature、`openapi --check`、clippy 兩種、fmt | 全部 | 見下方 |
| `check-secret-handling.ts`、`changes.test.ts` | 沒有違規；171 個測試通過 | 通過 |

全 workspace 的結果（2026-10-15 03:11，建置主機，exit 0）：fmt 與兩種 clippy 沒有輸出警告；`migrations` 單元測試連跑 5 次都是 6 個通過；`cargo test --workspace` 每個測試執行檔都是 `ok`、沒有 `FAILED`（`service-kit` 的 lib 95 個通過）；`service-kit --no-default-features` 全部通過（lib 68 個）；`api/openapi.json is up to date`。各執行檔的 `test result` 行與次數在 [data/2026-10-15-402-migration-readiness/full-run.txt](data/2026-10-15-402-migration-readiness/full-run.txt)。

`service-kit` 的單元測試 `a_failure_is_logged_once_with_each_file_and_a_success_is_kept` 起初也數成功時的 INFO 行；整個 `--lib` 一起跑時，4 次有 1 次執行緒區域的 capture 裡少了那一行（ERROR 行都在），所以改為只在服務的整合測試裡從真的 binary 日誌確認 INFO 行。

## 結論

- migration 不完整的資料庫上服務不 ready、日誌寫明缺少的檔案；完整時 ready；兩者都有測試：通過（身分、資產服務的 `tests/migrations.rs`，`tools/migrate/tests/service_records.rs`）。SHA-256 不同也不 ready，日誌寫出檔案與兩個 hash：通過。
- 服務角色只讀得到自己 schema 的紀錄，有跨服務的負面測試：通過（含能進別的服務 schema 的消費端平台角色）。紀錄不是租戶資料、不在租戶交易裡讀，所以沒有另外的跨租戶情境；跨租戶套件照常通過。
- 資料庫比映像新（rollback）時 ready：通過。
- 文件：`docs/architecture/deployment.md`、`docs/runbooks/migrations.md`、`tools/migrate/README.md`、`crates/service-kit/README.md`、`crates/testkit/README.md`、兩個服務的 README。
- 待測：podman 的開發環境 e2e（`TRUSTIOT_DEV_E2E=1 bun test scripts/tests/e2e.test.ts`）與 nightly smoke（k3d、兩組 values）由協調者執行；預期 `identity` 與 `assets` 在 Job 之後照常 ready。
