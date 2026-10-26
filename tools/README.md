# tools

部署與開發用的 Rust 工具，子目錄都是 workspace 成員（根目錄 `Cargo.toml` 的 `tools/*`）。

- `lab-dataset/`：lab 資料集（`datasets/lab/`，M7）的索引格式與檢查、人工異常片段，把資料集照 ingest 的寫法寫進 ClickHouse 遙測窄表的載入工具，以及以 agent 的 Modbus 模組錄製資料集的工具：服務的整合測試以 library 使用，手動載入與錄製用 `lab-dataset` 指令，見它的 [README](lab-dataset/README.md)。
- `migrate/`：部署時的 migration Job：依序跑各服務的 migration 與 ClickHouse DDL、冪等建立服務的資料庫角色，並重建每租戶的資料庫角色（M8）；服務的 pod 以它的 `wait-for-job` 等 Job 完成，見它的 [README](migrate/README.md)。
- `openapi/`：把各服務由程式碼產生的 OpenAPI 規格與 M1 錯誤 schema 合併成 `api/openapi.json`；`cargo run -p openapi` 重新產生，見它的 [README](openapi/README.md)。
- `repo-check/`：repo 規則檢查，隨 `cargo test --workspace` 執行，規則見它的 [README](repo-check/README.md)。
- `sql-check/`：靜態檢查 SQL 不由字串組成、租戶只由 service-kit 設定（決策 29），隨 `cargo test --workspace` 執行，規則見它的 [README](sql-check/README.md)。
