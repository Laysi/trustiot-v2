# repo-check

以 `cargo test` 檢查整個 repo 的規則，不產生執行檔。它也是 workspace 的第一個成員：Cargo 的虛擬 workspace 沒有成員時，`cargo test --workspace` 等指令會直接失敗。

執行：`cargo test -p repo-check`（`cargo test --workspace` 也會跑到）。

| 規則 | 理由 | 失敗時怎麼修 |
| --- | --- | --- |
| `.prototools` 的 `rust` 與 `rust-toolchain.toml` 的 `toolchain.channel` 是同一個確切版本（`MAJOR.MINOR.PATCH`），`.prototools` 的 `bun` 也是確切版本 | proto 透過 rustup 安裝 Rust、不為 cargo 建 shim，實際執行哪個版本由 rustup 讀 `rust-toolchain.toml` 決定；兩處不一致時，`proto install` 裝的版本與 cargo 用的版本不同 | 兩個檔案一起改成同一個版本，再跑 `proto install` |
| 根目錄 `Cargo.toml` 的 `workspace.package.rust-version` 不高於鎖定的工具鏈 | 否則鎖定的工具鏈會拒絕建置 | 調低 `rust-version`，或升級工具鏈 |
| 每個 workspace 成員都有 `edition.workspace = true` 與 `[lints] workspace = true` | 少了 `[lints] workspace = true` 的 crate 會靜默略過共用 lints | 在該成員的 `Cargo.toml` 補上 |
| `services/*/src` 的 Rust 程式碼沒有改 schema 的 SQL：`CREATE`、`ALTER`、`DROP`、`RENAME`、`TRUNCATE`、`ATTACH`、`DETACH`、`EXCHANGE`、`UNDROP` 接 `TABLE`、`TABLES`、`DATABASE`、`VIEW`、`DICTIONARY`（不分大小寫、可跨行，中間可夾 `OR REPLACE`、`IF NOT EXISTS`、`TEMPORARY`、`MATERIALIZED` 等；略過 `//` 開頭的註解行） | 執行期不建表、不改表（決策 14、[Laysi/trustiot#103](https://github.com/Laysi/trustiot/issues/103)）；schema 只由各服務的 `migrations/` 經 migration Job 套用 | 把語句移到該服務的 `migrations/`；刪除資料列用 `DELETE FROM`，不用 `ALTER TABLE ... DELETE` |
| repo 內沒有 `AGENTS.md`（不分大小寫，含 symlink；略過 `target`、`node_modules` 與 `.` 開頭的目錄） | agent 規範只放 `CLAUDE.md`（[專案目錄](../../docs/architecture/repository-layout.md)） | 把內容併進 `CLAUDE.md` 後刪除該檔 |

每條規則都有單元測試證明違反時會失敗（`src/lib.rs`），`tests/repository.rs` 則把規則套用到 repo 本身。
