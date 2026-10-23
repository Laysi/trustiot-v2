# agent 執行期間的使用者委派（run 委派）

對應 issue：Laysi/trustiot-v2#551。

## 環境

- 建置主機（x86_64 Linux，Rust 1.99.0，見 `rust-toolchain.toml`），經共用的檢查佇列執行，`CI=1`（容器測試沒有 podman 時失敗而不是略過）；Postgres 由測試以 podman 起的暫時 TLS Postgres 提供。
- 分支 `feat/551-run-delegation`，基於 `cd2637ca`；測試時的工作樹是本報告所在 commit 的內容，只差 `services/identity/src/delegation/tests.rs` 裡一個型別別名（clippy `type_complexity` 的修正，見「結果」）。

## 指令

```sh
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p contract -p identity --lib
cargo test --locked -p service-kit --lib identity
cargo test --locked -p identity --test ext_authz
```

## 結果

輸出摘要在 [data/2026-10-23-551-run-delegation/check-output.txt](data/2026-10-23-551-run-delegation/check-output.txt)。

| 指令 | 結果 |
| --- | --- |
| `cargo test -p contract -p identity --lib` | contract 42 個、identity 95 個全部通過（含 `src/delegation/tests.rs` 的 run 委派測試） |
| `cargo test -p service-kit --lib identity` | 39 個通過（含 `RunDelegations` 用戶端的 3 個） |
| `cargo test -p identity --test ext_authz` | 14 個通過，含真的服務與 Postgres 的 `a_run_delegation_acts_for_the_user_until_the_session_ends` |
| `cargo clippy --workspace --all-targets` | 整個 workspace 編譯通過，只有一個錯誤：`src/delegation/tests.rs` 的 `type_complexity`，已改成型別別名；修正後還沒重跑 |

待測（建置主機的檔案系統在 2026-10-23 05:53 轉為唯讀，需要人離線修復，佇列停止）：修正後的 `cargo clippy --workspace --all-targets -- -D warnings`、`cargo clippy -p service-kit --no-default-features --all-targets -- -D warnings`、`cargo test --workspace`（含 repo-check、sql-check、openapi 檢查、`dependency_isolation`、doc test）與 `cargo deny --locked check`。本機只跑過 `cargo fmt --all --check`（通過）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 委派建立超過 60 秒後，仍能換到受眾正確的 60 秒 token，被呼叫的服務照常接受 | 通過：`a_delegation_outlives_the_request_token_and_exchanges_for_minute_tokens`（10 分鐘前建立，請求的 token 已過期）與 `ext_authz` 的 10 分鐘前簽的委派，certs 的驗證端接受 |
| session 撤銷或登出後、委派到期後、超過有效上限後，換 token 被拒 | 通過：`an_ended_session_stops_the_next_exchange`、`expired_or_over_long_delegations_are_refused`，`ext_authz` 撤銷 session 後 401 |
| 使用者在該租戶的角色被移除後，下一次換 token 被拒或帶新的角色 | 通過：`role_changes_show_in_the_next_token_and_a_removed_member_is_refused`，`ext_authz` 改角色後帶新角色、刪除成員資格後 403 |
| 換來的 token 不能再換下一跳；不在 `mesh.calls` 的受眾被拒；別的呼叫端拿到委派也換不到 token | 通過：`exchanged_tokens_go_no_further_and_a_delegation_is_no_token`、`only_listed_audiences_and_holders`、`another_caller_with_the_delegation_gets_no_token`、`the_proof_window_is_thirty_seconds_either_side`（固定時鐘：±30 秒接受、±31 秒拒絕；它取代的以真實時鐘檢查時間窗的寫法，在前一輪因測試與服務的時鐘差一秒而失敗） |
| 文件寫明委派的上限、撤銷行為與威脅模型 | 通過：services/identity/README.md「agent 執行期間的委派」，決策 27、secrets.md、chart、contract、service-kit README 同步更新 |
