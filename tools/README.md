# tools

部署與開發用的 Rust 工具，子目錄都是 workspace 成員（根目錄 `Cargo.toml` 的 `tools/*`）。

- `migrate/`：部署時的 migration Job：依序跑各服務的 migration 與 ClickHouse DDL，並冪等重建每租戶的資料庫角色（M8）。
- `openapi/`：把各服務由程式碼產生的 OpenAPI 規格與 M1 錯誤 schema 合併成 `api/openapi.json`；`cargo run -p openapi` 重新產生，見它的 [README](openapi/README.md)。
- `repo-check/`：repo 規則檢查，隨 `cargo test --workspace` 執行，規則見它的 [README](repo-check/README.md)。
