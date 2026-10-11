# 測試容器不留下匿名 volume（Laysi/trustiot-v2#334）

## 環境

- 機器：本機（Fedora，8 核，podman 6.1.3，crun），與其他 agent 共用；所有重型檢查都經 `rcheck.sh` 佇列，一次一個工作。
- 工具：bun 1.4.2、Rust 工具鏈依 `rust-toolchain.toml`、其餘為 `.prototools` 釘選版本。
- 測試對象：分支 `fix/334-test-container-volumes`（基於 `1ad700f`）。檢查跑在提交前的同一份工作樹，提交內容與它相同，只多了本報告與 `data/` 目錄。
- 映像（宣告的 `VOLUME`）：Postgres 18.6-alpine 為 `/var/lib/postgresql`；ClickHouse 為 `/var/lib/clickhouse`；Mosquitto 為 `/mosquitto/data` 與 `/mosquitto/log`。
- 這台機器開始時有 12 個不屬於本專案的 dangling volume，沒有動。

## 指令

```sh
# 在 worktree 根目錄；rcheck.sh 設 CI=1、CARGO_BUILD_JOBS=6、使用 worktree 自己的 ./target
OUT=<暫存目錄> CLEAN=1 bash rcheck.sh 334 'bash baseline.sh && bash run-all.sh'   # 對照，再跑全部檢查
bash semantics.sh   # podman 行為實驗（只建立與刪除 trustiot-334-* 物件）
```

`data/run-all.sh` 依序執行並在量測點前後記錄 `podman volume ls --filter dangling=true -q`：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo clippy -p service-kit --no-default-features --all-targets --locked -- -D warnings`、`cargo test --workspace --locked`（量測）、`cargo test -p service-kit --no-default-features --locked`、`cargo run --locked -p openapi -- --check`、`cargo deny --locked check`、`bun install --cwd scripts --frozen-lockfile`、`bun run --cwd scripts typecheck`、`bun test scripts`（量測）、`bun test deploy/chart`、`bun scripts/ci/check-workflows.ts`、`bun scripts/ci/check-secret-handling.ts`，最後是 `TRUSTIOT_DEV_E2E=1 bun test scripts/tests/e2e.test.ts`（量測，開發環境的 podman 端到端測試）。`data/baseline.sh` 暫時把 service-kit 測試支援的 `Container::drop` 還原成修正前的 `rm --force`，跑 `tenant_db` 與 `job_lock`，再還原。腳本預設只列出洩漏的 volume，不刪除；`CLEAN=1` 才會刪除跑期間變成 dangling 的 volume，只能在佇列的獨占時段使用（說明在腳本開頭）。

## 結果

### 驗收條件 1：跑完後 dangling volume 的數量不變

「重現」的步驟是 `CI=1 cargo test --workspace` 與 `bun test scripts`。這台機器上其他 agent 的工作也會經過同一個佇列，所以我記錄了每個量測點的 volume 名稱（前 12 碼，見 `data/dangling-*.txt`），並比對集合，而不只比對數量。

| 量測點（最終樹，22:41–22:49） | dangling volume 數 | 與跑前的名稱差異 |
| --- | --- | --- |
| 跑前（`dangling-before.txt`） | 44 | 無 |
| `CI=1 cargo test --workspace --locked` 之後 | 44 | 0 |
| `bun test scripts` 之後 | 44 | 0 |
| 開發環境 e2e（`up`、seed、重啟、`down`、`purge`）之後 | 44 | 0 |

- 跑前的 44 個含最初的 12 個其他專案的 volume，加上 32 個在我的佇列工作開始前，由前一個工作（另一個 agent 在修正前的程式碼上跑的測試）留下的；它們在我的工作期間一個都沒有增減。
- 較早的一次完整跑（修正內容相同，只有 repo-check 的搜尋範圍不同，`data/run1/`，22:30–22:38）在安靜的時刻量測：12、12、12，也是不變。那次 `bun test scripts` 有一項失敗（`scripts/ci/changes.test.ts`：repo-check 的測試檔提到了 CI 路徑過濾沒有涵蓋的路徑），已把搜尋範圍縮為 Rust 原始碼修正，最終樹的 `bun test scripts` 為 518 通過、1 略過（e2e，另行執行）、0 失敗。
- 沒有新增的測試容器：`containers-*.txt` 在各量測點都是同樣的 10 個（其他專案的容器）。

### 修正前的對照

`data/baseline.sh` 把 `crates/service-kit/tests/support/mod.rs` 還原成修正前的樣子，只跑 `tenant_db` 與 `job_lock`（兩個測試檔共 21 個測試，10 個與 11 個）：

- dangling volume 從 44 增加到 55，共 11 個（`leaked-by-baseline.txt`）。
- 這次對照在佇列的獨占時段執行（跑期間沒有其他容器測試，見下方「跑前」的說明），名稱差異正好是這 11 個；我以當時版本的腳本刪除它們（現在提交的腳本要 `CLEAN=1` 才會刪），回到 44，再把修正還原。

### 驗收條件 2：新測試證明容器移除後 volume 都已刪除

`crates/testkit/tests/container_volumes.rs`（`cargo test --workspace` 內，`CI=1` 時不會略過）：

- `dropping_a_container_removes_its_volumes`：啟動 Postgres 映像的容器，讀出 `inspect` 的 mounts 取得 volume 名稱，確認它們存在（至少一個，否則測試無意義），放掉 `Container`，確認這些 volume 都不存在。
- `removing_a_container_without_volumes_leaves_them_behind`：對照組，用修正前的 `rm --force` 移除，確認同樣的 volume 還在（之後刪除它們），證明上一個測試的檢查會失敗。

另外兩個保護：

- `tools/repo-check`：`test_containers_are_removed_together_with_their_volumes` 掃描 `crates`、`services`、`edge`、`factory`、`tools` 的 Rust 程式碼，任何以 `"rm"` 開頭、含 `"--force"` 卻沒有 `"--volumes"` 的參數列都讓 `cargo test` 失敗，所以各 crate 重複的 `Container` 輔助程式（service-kit、outbox、protocol、ingest 各有一份）再被複製或改回去時會被擋下。
- `scripts/tests/e2e.test.ts`：`down` 之前記下實例容器掛載的匿名 volume（64 位十六進位名稱，且至少一個），`down` 之後確認都不存在。這一項在最終樹通過（`data/step-e2e.log`，63 個 expect）。

### podman 行為（`data/semantics.txt`）

Mosquitto 映像宣告兩個 `VOLUME`，實驗把具名 volume 掛在 `/mosquitto/data`，`/mosquitto/log` 由 podman 建匿名 volume：

- A：`rm --force` 之後匿名 volume 還在。
- B：`rm --force --volumes` 之後匿名 volume 已刪除，具名 volume 還在。所以開發環境的 `down` 不會誤刪資料 volume（`down` 本來就保留它們）。
- C：`run --rm` 的容器結束後容器不存在（`writeVolumeFile` 的一次性容器本來就用 `--rm`，不需要改）。

### 其他檢查（最終樹，`data/exit-codes.txt` 全為 0）

fmt、兩次 clippy、`cargo test --workspace --locked`、`cargo test -p service-kit --no-default-features --locked`、openapi、deny、scripts 型別檢查與測試、chart（250 通過）、workflow 政策、secret handling、開發環境 e2e。

### 2026-10-12：合併 main `dddd9ed` 之後

main 在這段期間加入了身分服務（PR Laysi/trustiot-v2#305）。它的測試輔助程式 `services/identity/tests/support/mod.rs` 也以 `rm --force` 移除 Postgres 容器，沒有 `--volumes`。

- 第一次在合併後的樹上跑：`tools/repo-check` 的 `test_containers_are_removed_together_with_their_volumes` 失敗，指出的就是這個檔案。這證明上面的保護有作用。
  - `cargo test` 停在這個失敗的測試檔。
  - 這個工作結束時，佇列清掉了 2 個跑期間變成 dangling 的 volume。
  - 輸出在 `data/2026-10-11-334-test-container-volumes/merged-main-dddd9ed-first-run.txt`。
- 修正：在那一處加上 `--volumes`，寫法與其他輔助程式相同。
- 修正後重跑，全部通過：
  - 環境：建置主機 ruisi-wsl（20 核，Docker），經 `rcheck.sh`，`CI=1`。
  - 結果：
    - `cargo fmt --all --check` 通過；
    - `cargo clippy --workspace --all-targets --locked -- -D warnings` 通過；
    - `cargo test --workspace --locked --no-fail-fast`：76 個測試執行檔，588 個測試通過，0 失敗；
    - `bun run --cwd scripts typecheck` 通過；
    - `bun test scripts`：527 個測試，1 個略過（podman e2e），0 失敗。
  - volume：佇列在這個工作結束時檢查，跑期間變成 dangling 的 volume 是 0 個。
  - 輸出在 `data/2026-10-11-334-test-container-volumes/merged-main-dddd9ed-checks.txt`。第一行 `ef67bd8` 是佇列在建置主機上為工作樹拍的快照 commit，不是本 repo 的 commit。

## 結論

- 驗收 1（`CI=1 cargo test --workspace` 與 `bun test scripts` 之後 dangling volume 不變）：通過。兩次量測，名稱集合都沒有差異；修正前的對照則增加 11 個。
- 驗收 2（新測試證明容器移除後 volume 都已刪除）：通過，見上。
- 驗收 3（列出的位置都已修正，`CLAUDE.md` 已更新）：通過。已修正的位置：`crates/service-kit/tests/support/mod.rs`、`crates/testkit/src/container.rs`、`crates/outbox/tests/support/mod.rs`、`crates/protocol/tests/broker.rs`、`services/ingest/tests/common/mod.rs`、`services/identity/tests/support/mod.rs`（2026-10-12 合併 main 後補上）、`scripts/lib/commands.ts`（`down`、`purge`、`up` 重啟共用的 `removeContainers`）與 `scripts/tests/e2e.test.ts` 的探針容器。`crates/service-kit/tests/{container,verify_container}.rs` 的 `Image::drop` 用 `rmi`，不涉及容器的 volume。
