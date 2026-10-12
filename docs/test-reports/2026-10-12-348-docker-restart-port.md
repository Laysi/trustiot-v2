# outbox 資料庫重啟測試在 Docker 上的主機埠（Laysi/trustiot-v2#348）

驗證 `a_database_restart_mid_run_loses_and_duplicates_nothing` 在 Docker 與 podman 上都通過，並記錄其他會重啟、停止再啟動或暫停容器的測試的檢查結果。

## 環境

| 項目 | Docker 執行 | podman 執行 |
| --- | --- | --- |
| 機器 | 建置主機（WSL2 上的 Docker Desktop，20 核心），經 repo 外的檢查佇列執行 | 本機工作站（Fedora 45，8 核心） |
| 容器執行環境 | Docker 29.8.2（主機上沒有 podman） | podman 6.1.3 |
| 負載（測試前後 load average） | 1.34 至 2.56 | 測試後 5.34（同時有其他工作） |
| 日期 | 2026-10-12 | 2026-10-12 |
| 測試對象 | 本分支 `fix/348-outbox-restart-port` 的工作樹（基底 `origin/main` 的 `a7f332b`，已含 Laysi/trustiot-v2#349） | 同左 |
| 設定 | `CI=1`，`--locked` | `CI=1`，`--locked` |

## 指令

建置主機（Docker），從 worktree 根目錄：

```
rcheck.sh 348 'docker --version; uptime; cargo test --locked -p outbox --test verify_271 2>&1 | tail -40; cargo fmt --all --check && echo fmt-ok; cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -15; cargo test --workspace --locked --no-fail-fast 2>&1 | grep -E "^(test result|failures:|error)|FAILED|Running|skipped"; uptime'
```

本機（podman）：

```
CI=1 CARGO_BUILD_JOBS=2 cargo test --locked -p outbox --test verify_271
```

## 變更

- `crates/outbox/tests/support/postgres.rs`：新增 `TlsPostgres::start_restartable` 與 `TlsPostgres::restart`。可重啟的 Postgres 由測試自己挑一個空閒埠，並明確發布成 `-p 127.0.0.1:<埠>:5432`；明確指定的埠在 podman 與 Docker 重啟後都會保留，臨時埠則只有 podman 會保留。`restart` 以 SIGKILL 重啟（維持原本的「崩潰後恢復」語意），並檢查重啟後主機位址不變。
- 競態處理：挑埠與發布是兩個步驟，期間別的處理程序可能佔用該埠。`run` 因埠被佔用（`already in use`、`already allocated`、`address in use`）而失敗時，先移除 Docker 留下的 created 容器，再換一個埠重試，最多 8 次；其他失敗不重試，立即以含執行環境錯誤訊息的 panic 結束。重啟期間埠短暫釋放，若剛好被佔用，`restart` 同樣的條件下每 500 ms 重試，最多 8 次，最後一次失敗的訊息會寫出來。podman 的實際訊息（`Address already in use`）已用佔住的埠確認符合判斷字串。
- 其他測試不變：`database()` 仍用臨時埠，不受影響，也不增加挑埠的競態。
- `crates/outbox/tests/support/mod.rs`：新增 `restartable_database()`，與 `database()` 共用組 URL 的程式。
- `crates/outbox/tests/verify_271.rs`：重啟測試改用 `restartable_database()` 與 `postgres.restart()`；移除靠 `ps` 輸出找容器名稱的 `container_on`。
- 未動各 helper 的 `Drop` 實作（PR Laysi/trustiot-v2#334 修改處）。

## 其他會重啟、停止或暫停容器的測試

搜尋範圍：`crates/`、`services/`、`edge/`、`tools/`、`factory/` 的 `*.rs`，關鍵字 `restart`、`stop`、`start`、`pause`、`unpause`、`kill`，並逐一檢查 helper：`crates/outbox/tests/support`、`crates/service-kit/tests/support`、`crates/testkit/src/container.rs`、`crates/protocol/tests/broker.rs`、`services/ingest/tests/common/mod.rs`、`services/identity/tests/support/mod.rs`。

| 位置 | 動作 | 重啟後是否再連同一主機埠 | 處理 |
| --- | --- | --- | --- |
| `crates/outbox/tests/verify_271.rs` `a_database_restart_mid_run_loses_and_duplicates_nothing` | `restart --time 0` 資料庫容器，再用原連線設定寫入與消費 | 是（本 issue 的錯誤） | 已修正（固定埠） |
| `crates/service-kit/tests/container.rs` | `stop --time <寬限>` 服務容器，驗證 drain | 否，停止後只讀日誌與結束碼 | 不需修改 |
| `crates/service-kit/tests/verify_container.rs` | `kill --signal TERM` 服務容器 | 否 | 不需修改 |
| `crates/outbox/tests/crash.rs`、`crates/outbox/tests/support/mod.rs`、`crates/service-kit/tests/support/mod.rs`、`services/identity/tests/support/mod.rs` | 對測試啟動的處理程序（非容器）送 `kill` | 不適用（不是容器） | 不需修改 |
| `crates/protocol/tests/broker.rs`、`services/ingest/tests/common/mod.rs`、`crates/testkit/src/container.rs`、`crates/service-kit/tests/support/postgres.rs`、`crates/testkit/src/postgres.rs` | 只啟動與移除容器；沒有 `restart`、`stop`、`pause` | 不適用 | 不需修改 |
| `crates/protocol/tests/compat.rs`、`adversarial.rs` | 含 `stop`、`pause` 字樣，但是協定動作名稱，不是容器操作 | 不適用 | 不需修改 |

`services/identity/tests/adversarial.rs` 的 `rotation_across_restarts_...` 是在測試處理程序內重建服務，不重啟容器。

## 結果

- Docker（建置主機）：`verify_271` 13 通過、0 忽略、0 失敗（變更前的基底 `0623e5a` 上也跑過一次，12 通過、1 忽略，`f5` 當時仍被忽略），其中 `a_database_restart_mid_run_loses_and_duplicates_nothing ... ok`。
- podman（本機）：同樣 13 通過、0 忽略、0 失敗。
- 建置主機：`cargo fmt --all --check` 通過（`fmt-ok`）；`cargo clippy --workspace --all-targets --locked -- -D warnings` 無警告；`CI=1 cargo test --workspace --locked --no-fail-fast` 每個測試程式都是 `ok`，沒有失敗，容器型測試未被略過。
- 原始輸出：[`data/2026-10-12-348-docker-restart-port/docker-build-host.txt`](data/2026-10-12-348-docker-restart-port/docker-build-host.txt)、[`data/2026-10-12-348-docker-restart-port/podman-verify_271.txt`](data/2026-10-12-348-docker-restart-port/podman-verify_271.txt)。輸出已去除主機名稱與路徑。

### Postgres 容器啟動逾時

Issue 提到建置主機上每批測試有一次 Postgres 容器在 90 秒內沒有啟動。本次在建置主機跑了兩輪（變更前的基底與 rebase 後各一輪，每輪含 `verify_271` 與完整 workspace 測試，每個測試各啟動自己的 Postgres 容器），本機跑了三次 `verify_271`（其中一次只跑重啟測試），沒有出現任何啟動逾時；建置主機測試期間 load average 為 0.45 至 2.56。沒有觀察到問題，所以不另開 issue，也沒有放寬任何逾時。

## 結論

- 驗收 1（只有 Docker 的主機上 `verify_271` 全過，podman 也過，輸出存檔）：通過。
- 驗收 2（其他會重啟容器的測試已檢查，需修正的已修正並列出）：通過；只有一個測試會重啟容器，已修正，其餘見上表。
- 驗收 3（建置主機 Docker 上 clippy 與 `CI=1 cargo test --workspace`）：通過。
