# 閘道器取用新的根 CRL

Laysi/trustiot-v2#563：agent 經 HTTPS 取得較新的根 CRL、只往前換上、跨重啟保留且不能被刪檔或換舊檔降級、換上後下一條連線是完整交握、心跳回報根 CRL 的編號與是否過期。

## 環境

- 建置主機（x86_64 Linux），在工作樹的快照上執行，設 `CI=1`（需要容器的測試在沒有容器時失敗而不是略過），podman 提供 swtpm 與 Mosquitto 容器。
- Rust 1.99.0（`rust-toolchain.toml`），其他工具依 `.prototools`。
- 分支 `feat/563-root-crl-fetch`，已合併 2026-11-01 的 `origin/main`；commit 見 PR。

## 指令

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=true cargo test --locked -p agent
cargo test --locked -p protocol
cargo test --locked -p root-crl
cargo test --locked --workspace --exclude agent
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
bash docs/test-reports/data/2026-11-01-563-root-crl-update/run-mutations.sh
```

## 結果

| 指令 | 結束碼 | 摘要 | 原始資料 |
| --- | --- | --- | --- |
| `cargo fmt --all --check` | 0 | | [checks-agent-zigbuild.out](data/2026-11-01-563-root-crl-update/checks-agent-zigbuild.out) |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 0 | | 同上 |
| `CI=true cargo test --locked -p agent` | 0 | lib 376 通過（2 ignored，原本就有），整合測試全部通過，含新的 `root_crl_update`（4）與 `provision_install`（7） | 同上 |
| `cargo zigbuild ... -p agent`（aarch64） | 0 | | 同上 |
| `cargo test --locked -p protocol`、`-p root-crl` | 0 | protocol 的 `validation` 19、`compat` 10、`golden` 6 等全部通過 | [checks-protocol-root-crl.out](data/2026-11-01-563-root-crl-update/checks-protocol-root-crl.out) |
| `cargo test --locked --workspace --exclude agent` | 0 | 267 個測試執行檔全部 ok | [checks-workspace.out](data/2026-11-01-563-root-crl-update/checks-workspace.out) |
| `run-mutations.sh`（驗收條件 7） | 0 | 兩個變異都讓相依的 crate 編譯失敗，見下 | [run-mutations.out](data/2026-11-01-563-root-crl-update/run-mutations.out) |

protocol 與 workspace 那兩次是在處理安全審查的 Low 項目之前跑的。那些修改只動 agent（log 文字、`provision install` 的 stderr 提示）與文件，之後重跑了 fmt、workspace clippy、agent 全部測試與 aarch64 建置。

驗收條件 7 的變異（同 `docs/test-reports/data/2026-10-13-8-contract-type-changes/run-mutations.sh` 的作法）：只改 Rust 欄位名稱、以 serde rename 保留線上格式，所以 protocol 函式庫本身照樣編譯，但用到它的地方都失敗：

- `p1`（`Heartbeat.root_crl` → `root_crl_status`）：protocol 的測試、`agent`（`src/heartbeat.rs`）、`gateway-ops`（`tests/presence.rs`）編譯失敗。
- `p2`（`RootCrlStatus.expired` → `is_expired`）：protocol 的測試、`agent`（`src/heartbeat.rs`）編譯失敗。

## 結論

| 驗收條件 | 結果 | 證據（測試） |
| --- | --- | --- |
| 1. 編號不大於手上的、或不是信任錨簽的根 CRL 被拒、保留手上的 | 通過 | `revocation::tests::a_root_crl_not_newer_than_the_one_held_is_refused_and_the_one_held_kept`：較低、同編號不同內容、相同、沒有編號、50 位數、另一個信任錨的根，以及手上沒有時以檔案的根為準。`tests/root_crl_update.rs` 的 `a_newer_root_crl_is_taken_and_the_next_connection_is_a_full_handshake_checked_against_it`：較低、同名但不同金鑰（"does not verify"）、不是信任錨、不是 CRL，都拒絕，且檔案與記憶體保持 3 |
| 2. 換上新的根 CRL 後，下一條連線不用 resumption，並拒絕新 CRL 撤銷的簽發 CA 簽的伺服器憑證 | 通過 | 同上的整合測試：先 Full 再 Resumed；換上 2 之後下一條是 `HandshakeKind::Full`；換上撤銷 broker 簽發 CA 的 3 之後連線以 "Revoked" 被拒（inner mTLS） |
| 3. 重開後沿用最後存下的根 CRL；寫入中斷不留壞檔 | 通過 | `revocation::tests::an_interrupted_write_leaves_the_last_saved_root_crl_which_a_restart_loads`（寫一半就失敗：舊檔完整、舊記號、沒有暫存檔，重啟載入舊的，下次更新補寫）。`a_failed_fetch_still_takes_over_a_newer_file_and_saves_what_is_held`（存檔失敗後即使取得失敗或 404 也補寫）。整合測試的最後一段：重新啟動載入 3，照樣拒絕被撤銷的 broker |
| 4. 接受過之後，刪檔或換成較低編號再重開，不會關掉撤銷檢查 | 通過 | `revocation::tests::once_a_root_crl_was_accepted_a_deleted_or_older_file_withholds_every_connection`、`a_power_cut_between_the_two_renames_of_a_root_change_leaves_a_marker_that_does_not_count`。`tests/root_crl_update.rs` 的 `deleting_or_rolling_back_the_root_crl_never_turns_revocation_checking_off`：刪檔與換舊檔都是 `Withheld`（編號 3），連線在 "root CRL" 這一步失敗；執行中的更新器立刻取得，拿到 4 後以完整交握連上。`tests/provision_install.rs`：刪檔後安裝較舊的，記號維持 3、agent 扣住、stderr 有說明 |
| 5. 心跳帶根 CRL 編號與是否過期；過期仍能連線 | 通過 | `tests/root_crl_update.rs` 的 `the_heartbeat_reports_the_root_crl_and_an_expired_one_still_connects`（2021-01-01 過期的 5 照樣連上，心跳 `expired: true`；換上 6 之後 `expired: false`）。`heartbeat` 的單元測試；protocol 的 `validation`、`compat`（舊接收端略過、選填欄位） |
| 6. 寫 broker.crl 與 `provision install` 取同一把目錄 flock、同樣只往前；與安裝同時執行不會倒退 | 通過 | `revocation::tests::the_files_are_read_and_written_only_under_the_lock_provision_install_takes`。`tests/provision_install.rs` 的 `the_agent_s_root_crl_update_and_an_install_never_roll_the_root_crl_back`：另一個持有者握著鎖，直到 agent 的更新（取得 3）與真正的 `provision install`（送 2）兩者都在 `/proc/locks` 等這把鎖；不論誰先，最後檔案與記號都是 3 |
| 7. 協定型別變更依 run-mutations 的方式檢查 | 通過 | 上方「結果」 |

延後的進階驗證（依 trustiot-v2 驗證分級）：在 lab 的閘道器上對真的 certs 服務取用（展示環境的 `/certs/root-crl` 還在 Cloudflare Access 後面，開放前要先做入口限流的 429 實測）、斷電注入的重複實測、aarch64 上執行測試。
