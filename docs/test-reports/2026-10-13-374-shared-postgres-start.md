# outbox 與 testkit 的 Postgres 容器改用共用的啟動邏輯（Laysi/trustiot-v2#374）

驗證 `crates/outbox` 與 `crates/testkit` 的測試 Postgres 容器，和 `crates/service-kit`、`services/identity` 的測試經同一份啟動邏輯啟動：等待上限沿用 Laysi/trustiot-v2#364 量測的具名常數、發佈的連接埠失效時換容器、失敗訊息附容器狀態與日誌尾端且不印環境變數。只改測試與測試輔助程式；沒有新的量測，常數與依據見 [`2026-10-12-364-job-lock-postgres-start.md`](2026-10-12-364-job-lock-postgres-start.md)。

## 環境

| 項目 | 內容 |
| --- | --- |
| 機器 | 建置主機（WSL2 上的 Docker Desktop，20 核心），經 repo 外的檢查佇列（`rcheck.sh`）執行；容器是測試固定 digest 的 `postgres:18.6-alpine` |
| 日期 | 2026-10-13 |
| 測試對象 | 分支 `test/374-shared-postgres-start`，基底 `origin/main` 的 `5fbe8ea`，提交 `7180d6a` |
| 設定 | `CI=1`（由 rcheck 設定），`--locked`，debug profile |

## 做法與依賴方向

啟動邏輯（限制常數、等待、換容器、診斷、映像與 TLS 啟動指令）從 `crates/service-kit/tests/support/postgres.rs` 搬到同目錄新的 `start.rs`，只有這一份。`crates/service-kit`、`crates/outbox`、`crates/testkit`、`services/identity` 都以 `#[path]` 引入它，`services/identity` 本來就以這個方式共用 `postgres.rs`。

不把邏輯移進 `crates/testkit`，是因為 testkit 依賴 service-kit：service-kit 的測試要用 testkit 就得在 service-kit 加 `testkit` 的 dev-dependency，成為循環；而且 testkit 以預設功能依賴 service-kit，會讓 `cargo test -p service-kit --no-default-features` 實際以預設功能建置，那組檢查就失去意義。outbox 與 testkit 都依賴 service-kit（反向的依賴不存在），所以讓 service-kit 的測試目錄放這個檔案、其他 crate 引入它，沒有循環。

- `crates/outbox/tests/support/postgres.rs`：`TlsPostgres::start_within(limits, runtime, kind, fixed_port, published_port_dead)` 建在 `start_postgres_within` 上；`start` 與 `start_restartable` 呼叫它。每個測試仍然自己一個容器；測試自己指定主機連接埠時（`start_restartable`），換上的容器也用新挑的固定連接埠，`restart()` 照常運作。
- `crates/testkit/src/postgres.rs`：`start_tls_postgres_within(limits, runtime, kind, published_port_dead)`，`tls_postgres` 呼叫它；`testkit::postgres::start` 公開共用的啟動邏輯。
- 刪掉三份重複的 `POSTGRES_IMAGE` 與 `START_WITH_TLS`、outbox 與 testkit 各自的 90 秒等待與只有最後一次錯誤的失敗訊息。

## 指令

```
bash rcheck.sh 374 'cargo fmt --all --check; cargo clippy --workspace --all-targets --locked -- -D warnings; cargo clippy -p service-kit --no-default-features --all-targets -- -D warnings; cargo test -p outbox --locked; cargo test -p testkit --locked; cargo test -p service-kit --locked; cargo test -p service-kit --no-default-features --locked; cargo test -p identity --locked; cargo test -p sql-check --locked; cargo test -p repo-check --locked'
```

（實際是每個指令前加 `echo ==<名稱>`、輸出篩出 `test result` 與錯誤行，再一次執行；rcheck 以 `CI=1` 執行。）

## 結果

提交 `7180d6a`，一次執行，結束於 2026-10-13 04:08（Asia/Taipei），全部通過：

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 無差異 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通過 |
| `cargo clippy -p service-kit --no-default-features --all-targets -- -D warnings` | 通過 |
| `cargo test -p outbox --locked` | 全部通過，含新的 `postgres_start` 3 個測試（7.0 秒） |
| `cargo test -p testkit --locked` | 全部通過，含新的 `postgres_start` 2 個測試（5.9 秒） |
| `cargo test -p service-kit --locked` | 全部通過；`postgres_start` 3 個測試仍通過 |
| `cargo test -p service-kit --no-default-features --locked` | 全部通過 |
| `cargo test -p identity --locked` | 全部通過（32、11、1、9、8 個測試） |
| `cargo test -p sql-check --locked`、`cargo test -p repo-check --locked` | 通過 |

### 新的測試

| 測試 | 斷言 |
| --- | --- |
| `crates/outbox/tests/postgres_start.rs` `a_server_that_does_not_come_up_is_reported_with_what_was_awaited_and_the_container` | 上限設為 1 毫秒（映像光是 `initdb` 就超過 1 秒，與機器快慢無關）時，訊息含 `Postgres with TLS did not accept a login within 1ms`、嘗試次數、`initdb`、`last error:`、`status=running`、`last 20 log lines:`；不含 `TEST_TLS_KEY`、`PRIVATE KEY`、`POSTGRES_PASSWORD=<密碼>`；該容器已被移除 |
| 同檔 `a_container_whose_published_port_stays_dead_is_replaced` | 第一個容器的連接埠在主機端被模擬為壞掉（伺服器本身正常），第二個容器被採用（位址不同）、以 `verify-full` 登入成功、壞的容器已移除、只剩一個，放掉後也移除 |
| 同檔 `a_replacement_on_a_port_the_test_chose_keeps_that_port_across_a_restart` | 同上但測試自己指定連接埠：換上的容器經 `restart()`（它檢查連接埠不變）重啟後仍可登入 |
| `crates/testkit/tests/postgres_start.rs` 兩個測試 | 與 outbox 前兩個相同，經 `start_tls_postgres_within` |

啟動邏輯本身（全部容器都壞時每個容器各一段訊息、`pg_isready` 的判斷、慢啟動不觸發換容器）仍由 `crates/service-kit/tests/postgres_start.rs` 測；它搬動後的程式（`start.rs`）三個測試仍通過，所以 outbox 與 testkit 的測試只驗證接線（容器、TLS 設定、登入、移除），沒有重複那些情況。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| outbox 與 testkit 的 Postgres 測試容器經同一份啟動邏輯啟動：發佈連接埠失效時換容器、失敗訊息附容器狀態與日誌，有測試斷言這兩點 | 通過：見上表的新測試 |
| `CI=1 cargo test -p outbox --locked`、`-p testkit`、`-p service-kit` 與 `cargo clippy --workspace --all-targets --locked -- -D warnings` 通過 | 通過：見上表 |

未測：沒有新的量測，也沒有在 Docker Desktop 真的出現壞連接埠時重現（約 300 個容器 1 個，Laysi/trustiot-v2#364 的報告已量過）；換容器的路徑以模擬壞連接埠的方式測。
