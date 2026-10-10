# tools

部署與開發用的 Rust 工具，子目錄都是 workspace 成員（根目錄 `Cargo.toml` 的 `tools/*`）。

- `migrate/`：部署時的 migration Job：依序跑各服務的 migration 與 ClickHouse DDL，並冪等重建每租戶的資料庫角色（M8）。
- `release-selftest/`：release.yml 的流程自測映像，不是服務，見它的 [README](release-selftest/README.md)。
- `repo-check/`：repo 規則檢查，隨 `cargo test --workspace` 執行，規則見它的 [README](repo-check/README.md)。
