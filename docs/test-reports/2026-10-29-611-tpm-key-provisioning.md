# TPM 金鑰佈建：沿用、建立或拒絕（2026-10-29）

對應 issue：Laysi/trustiot-v2#611。範圍是 `agent::tpm::ensure_key`：沿用、建立或拒絕持久 handle 上的金鑰，以及搬到 `testkit::swtpm` 的軟體 TPM 測試環境。

## 環境

- 機器：建置主機（x86_64 Linux），經檢查佇列執行。容器測試以 podman 執行，並設了 `CI`，所以缺少容器執行環境時測試會失敗，不會略過。
- Rust 1.99.0（`.prototools` 與 `rust-toolchain.toml`）。
- 軟體 TPM：`crates/testkit/src/swtpm/Containerfile` 建出的映像。基底是 Debian trixie-slim（以 digest 釘選），內含 swtpm 0.7.1、libtpms 0.9.2 與 tpm2-tools 5.7。每個測試各自啟動一顆全新的 TPM。
- 分支 `feat/611-tpm-key-provisioning`，基於 main `5dad0429`。
- 沒有使用實體硬體與 lab 的閘道器。

## 指令

從 repository 根目錄執行：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=true cargo test --locked -p agent
CI=true cargo test --locked -p testkit
cargo test --locked -p repo-check
cargo build --locked --target aarch64-unknown-linux-gnu -p agent
bun scripts/ci/check-secret-handling.ts
bun test scripts/ci/changes.test.ts

# 只跑 TPM 相關的測試
cargo clippy --locked -p agent -p testkit --all-targets -- -D warnings
cargo test --locked -p agent --lib tpm
CI=true cargo test --locked -p agent --test tpm_provision --test tpm_swtpm
```

## 結果

所有指令都通過。原始輸出見 [checks.txt](data/2026-10-29-611-tpm-key-provisioning/checks.txt)（完整批次）與 [tpm-tests.txt](data/2026-10-29-611-tpm-key-provisioning/tpm-tests.txt)（TPM 相關測試的第一次執行）。

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過，沒有警告 |
| `CI=true cargo test --locked -p agent` | 全部通過：lib 179、binary 7、broker 4、dependencies 1、link 51、liveness 4、modbus 系列、systemd_unit 6、tpm_provision 6、tpm_swtpm 1、transport 19 |
| `CI=true cargo test --locked -p testkit` | 全部通過：lib 42（含 `swtpm::tests::handle_listings_are_parsed`）、container_volumes 2、modbus 21、postgres 8 |
| `cargo test --locked -p repo-check` | 28 通過 |
| `cargo build --locked --target aarch64-unknown-linux-gnu -p agent` | 通過 |
| `cargo test --locked -p agent --lib tpm` | 43 通過（marshal、provision、worker 與 `TpmKey`） |
| `bun scripts/ci/check-secret-handling.ts` | 536 個檔案、7 個容器建置檔，沒有違規 |
| `bun test scripts/ci/changes.test.ts` | 283 通過 |

`tests/tpm_provision.rs` 對 swtpm 容器跑以下六個測試，每個都用一顆新的 TPM。每個 `session` 相當於一個開啟 `/dev/tpmrm0` 的行程：session 結束時，它載入的暫時物件會被 flush。

| 測試 | 證明的事 |
| --- | --- |
| `an_empty_handle_gets_the_decision_12_key_and_reruns_reuse_it` | 標準 SRK 下的空 handle 得到金鑰（`created = true`），送出的指令依序是 ReadPublic、ReadPublic、Create、Load、EvictControl、FlushContext、ReadPublic、Sign。`tpm2_readpublic` 顯示屬性 `0x40072`（fixedtpm\|fixedparent\|sensitivedataorigin\|userwithauth\|sign）、ECC NIST P-256、ECDSA-SHA256、sym null。SPKI 與 tpm2-tools 算出的相同，簽章能以公鑰驗證。在同一條連線與新連線上重跑都得到 `created = false`，只送 ReadPublic 與 Sign，SPKI 相同，持久 handle 仍是 `[0x81000001, 0x81000100]`，沒有暫時 handle。`TpmKey::open` 讀到同一把金鑰 |
| `a_non_standard_srk_is_left_alone_and_a_transient_primary_is_used` | `0x81000001` 上不是標準 SRK 時，改用暫時的 primary。primary 用完就 flush，`0x81000001` 不變 |
| `objects_that_do_not_match_are_refused_and_the_tpm_is_unchanged` | 拒絕 RSA 2048、restricted、P-384（ECDSA-SHA384）、有密碼且 noDA、只能解密、儲存金鑰這六種物件。每次只送 ReadPublic，有密碼的那個多送一次 Sign。前後的 `snapshot()` 完全相同，涵蓋持久、暫時與 NV handle、每個物件的 readpublic，以及變動的屬性（含字典攻擊計數）。有密碼但沒有 noDA 的金鑰也被拒絕（`TPM_RC_AUTH_FAIL`），唯一的變化是 `TPM2_PT_LOCKOUT_COUNTER` 從 0x0 變成 0x1 |
| `two_processes_creating_at_once_end_with_one_key` | 兩個 session 同時建立，會合點讓兩邊都停在 EvictControl 前再同時放行。結果一個 `created = true`，另一個收到 `TPM_RC_NV_DEFINED` 後讀回並沿用。兩者 SPKI 相同，與 tpm2-tools 讀到的一致，持久 handle 只多一個，沒有暫時 handle |
| `a_process_that_dies_before_the_key_is_persisted_leaves_nothing_and_a_rerun_creates_it` | 行程分別死在 Load 與 EvictControl：之後沒有暫時 handle，持久 handle 只有 SRK。重跑時建立金鑰 |
| `after_tpm2_clear_a_rerun_creates_a_different_key` | 執行 `tpm2_clear` 後，SRK 與金鑰都不見了。重跑時以暫時 primary 建立新金鑰，SPKI 與 clear 前不同、與 tpm2-tools 讀到的一致，持久 handle 只剩新金鑰 |

單元測試（`cargo test -p agent --lib tpm`）的內容：

- 以 swtpm 錄下的整段交換（`edge/agent/src/tpm/fixtures.rs`）逐 byte 比對 CreatePrimary、Create、Load、EvictControl、FlushContext、ReadPublic 與 Sign 的指令，並解析它們的回應，包括 `TPM_RC_RETRY`、`TPM_RC_NV_DEFINED`、`TPM_RC_HANDLE`，以及 flush 不存在的 handle。
- 驗證暫時 primary 的名稱等於 tpm2-tools 建立的 SRK 名稱，以及建立時的公開區域等於持久化後讀回的公開區域。
- 以截斷、延長、膨脹的內層長度與亂資料測試解析，確認都被拒絕而且不會 panic。
- 以腳本化的假 TPM 測 `ensure_key` 的流程：重送 RETRY，最多 9 次就放棄；暫時 primary 被 flush；NV_DEFINED 走沿用；不支援的 handle 在送出任何指令前就被拒絕；授權失敗被歸類為拒絕。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| swtpm 上空的 0x81000100 得到決策 12 屬性的金鑰（tpm2_readpublic 確認）；重跑與新連線得到同一個公鑰，持久 handle 清單不變 | 通過（`an_empty_handle_gets_the_decision_12_key_and_reruns_reuse_it`） |
| 不符的物件（RSA、restricted、P-384、有密碼）被拒絕，TPM 狀態與之前完全相同 | 通過（`objects_that_do_not_match_are_refused_and_the_tpm_is_unchanged`）。有一個例外寫在決策 12 的理由欄：有密碼而沒有 noDA 的金鑰，自測簽章會使字典攻擊計數加一，與 agent 開啟它時相同 |
| 兩個行程同時執行，最後只有一把金鑰、同一個公鑰 | 通過（`two_processes_creating_at_once_end_with_one_key`；兩個行程以同一顆 swtpm 上的兩個 session 模擬） |
| tpm2_clear 後重跑得到不同的新金鑰 | 通過（`after_tpm2_clear_a_rerun_creates_a_different_key`） |
| 解析器有錄製回應的單元測試；aarch64 建置通過；`dependencies.rs` 通過 | 通過 |
| `CI=true cargo test --locked -p agent`、`cargo test -p testkit`、`cargo clippy --workspace --all-targets -- -D warnings` 通過 | 通過 |

待測（依驗證分級延後為進階驗證）：

- 在實體 TPM（SLB9670）上重跑佈建（Laysi/trustiot-v2#618 的範圍）。
- 在核心資源管理器（`/dev/tpmrm0`）下，讓真正的兩個行程同時執行。
- 在 Create 到 EvictControl 之間的其他位置做故障注入，例如斷電。
