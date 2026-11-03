# repo-check

以 `cargo test` 檢查整個 repo 的規則，不產生執行檔。它也是 workspace 的第一個成員：Cargo 的虛擬 workspace 沒有成員時，`cargo test --workspace` 等指令會直接失敗。

執行：`cargo test -p repo-check`（`cargo test --workspace` 也會跑到）。

| 規則 | 理由 | 失敗時怎麼修 |
| --- | --- | --- |
| `.prototools` 的 `rust` 與 `rust-toolchain.toml` 的 `toolchain.channel` 是同一個確切版本（`MAJOR.MINOR.PATCH`），`.prototools` 的 `bun` 也是確切版本 | proto 透過 rustup 安裝 Rust、不為 cargo 建 shim，實際執行哪個版本由 rustup 讀 `rust-toolchain.toml` 決定；兩處不一致時，`proto install` 裝的版本與 cargo 用的版本不同 | 兩個檔案一起改成同一個版本，再跑 `proto install` |
| 根目錄 `Cargo.toml` 的 `workspace.package.rust-version` 不高於鎖定的工具鏈 | 否則鎖定的工具鏈會拒絕建置 | 調低 `rust-version`，或升級工具鏈 |
| 每個 workspace 成員都有 `edition.workspace = true` 與 `[lints] workspace = true` | 少了 `[lints] workspace = true` 的 crate 會靜默略過共用 lints | 在該成員的 `Cargo.toml` 補上 |
| `services/*/src` 的 Rust 程式碼沒有改 schema 的 SQL：`CREATE`、`ALTER`、`DROP`、`RENAME`、`TRUNCATE`、`ATTACH`、`DETACH`、`EXCHANGE`、`UNDROP` 接 `TABLE`、`TABLES`、`DATABASE`、`VIEW`、`DICTIONARY`（不分大小寫、可跨行，中間可夾 `OR REPLACE`、`IF NOT EXISTS`、`TEMPORARY`、`MATERIALIZED` 等；略過 `//` 開頭的註解行） | 執行期不建表、不改表（決策 14、[Laysi/trustiot#103](https://github.com/Laysi/trustiot/issues/103)）；schema 只由各服務的 `migrations/` 經 migration Job 套用 | 把語句移到該服務的 `migrations/`；刪除資料列用 `DELETE FROM`，不用 `ALTER TABLE ... DELETE` |
| `DEV_ONLY_CRATES`（目前是 `crates/testkit`）只出現在其他成員的 `[dev-dependencies]` | 測試輔助 crate 不能進服務的執行檔；`tools/sql-check` 把它們的原始碼當測試程式檢查（`TEST_SUPPORT`），前提就是它們只在測試裡執行 | 把該相依移到 `[dev-dependencies]`；正式程式需要的功能放進 service-kit 等一般 crate |
| `crates`、`services`、`edge`、`factory`、`tools`（不含 `repo-check` 自己）的 Rust 程式碼中，以 `"rm"` 開頭、含 `"--force"`（或 `"-f"`）的參數列，一定要有 `"--volumes"`（或 `"-v"`） | Postgres、ClickHouse、Mosquitto 映像都宣告 `VOLUME`，測試容器因此都有匿名 volume；只用 `rm --force` 移除會留下它們，累積到 podman 的 lock 用完（`exceeded num_locks (2048)`），所有容器測試都失敗（[Laysi/trustiot-v2#334](https://github.com/Laysi/trustiot-v2/issues/334)） | 在該參數列加上 `"--volumes"`（開發環境的 TypeScript 由 `scripts/tests/e2e.test.ts` 檢查）；`volume rm`、`network rm` 等不以 `"rm"` 開頭的不受影響 |
| repo 內沒有 `AGENTS.md`（不分大小寫，含 symlink；略過 `target`、`node_modules` 與 `.` 開頭的目錄） | agent 規範只放 `CLAUDE.md`（[專案目錄](../../docs/architecture/repository-layout.md)） | 把內容併進 `CLAUDE.md` 後刪除該檔 |
| 入口例外路徑的 plan 表（`deploy/chart/templates/_ingress.tpl` 的 `trustiot.ingress.publicPathTable`）每一列的方法等於服務在那個路徑提供的方法：`login` 對照 `api/openapi.json` 與 `services/identity/src` 的 `.route(LOGIN_PATH, ...)`，`healthCheck` 對照 `crates/service-kit/src` 與 `services/identity/src` 的 `.route(LIVENESS_PATH, ...)`，`certRenewal`、`rootCrl` 對照 `services/certs/src` 的 `.route(RENEWAL_PATH, ...)`、`.route(ROOT_CRL_PATH, ...)`（axum 的 `get` 算 `GET` 與 `HEAD`），`mqtt` 對照 `services/mqtt-bridge/src/handshake.rs` 唯一的 `method != Method::X`（且 mqtt-bridge 不綁任何路徑、全部交給 `.fallback(upgrade)`）；除了 `mqtt`，每一列的 `fixed` 等於服務的路徑常數；表上只有這五列。路徑常數可寫成完整路徑（`a::CONST`）；讀不懂的寫法（`any`、`on`、該路徑的 `.route_service`、可能通到該路徑的 `.nest`、多個方法比對、mqtt-bridge 在 handshake 以外讀方法、表上未知的欄位）一律失敗；文字看不出路徑的寫法（別名常數、`Router::route(r, ...)`、`format!`）讀不到 | 入口只放行表上的方法（[Laysi/trustiot-v2#859](https://github.com/Laysi/trustiot-v2/issues/859)）；服務新增方法而表沒改時，入口會拒絕它卻沒有測試失敗，表多列了方法時則多放行一個不需要 cookie 的方法（[Laysi/trustiot-v2#904](https://github.com/Laysi/trustiot-v2/issues/904)） | 表與服務一起改（`deploy/chart/README.md`「入口 gateway」），新的例外路徑或新的路由寫法在 `tests/repository.rs` 與 `src/public_paths.rs` 加上讀法 |

每條規則都有單元測試證明違反時會失敗（`src/lib.rs`、`src/public_paths.rs`），`tests/repository.rs` 則把規則套用到 repo 本身。
