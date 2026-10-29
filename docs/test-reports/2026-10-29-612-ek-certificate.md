# ECC 背書金鑰（EK）與 EK 憑證比對（2026-10-29）

對應 issue：Laysi/trustiot-v2#612。範圍是 `agent::tpm::endorsement_key`：以 TCG 預設的 ECC P-256 範本算出 EK、讀出 NV `0x01C0000A` 的 EK 憑證並比對，以及新增的 TPM2_NV_ReadPublic 與 TPM2_NV_Read 編解碼。

## 環境

- 機器：建置主機（x86_64 Linux），經檢查佇列執行。容器測試以 podman 執行，並設了 `CI`，所以缺少容器執行環境時測試會失敗，不會略過。
- Rust 1.99.0（`.prototools` 與 `rust-toolchain.toml`）。
- 軟體 TPM：`crates/testkit/src/swtpm/Containerfile` 建出的映像（與 Laysi/trustiot-v2#611 相同，沒有變更），內含 swtpm、libtpms 與 tpm2-tools 5.7。每個測試各自啟動一顆全新的 TPM。
- 分支 `feat/612-ek-certificate`，基於 main `dcc603bf`。
- 沒有使用實體硬體與 lab 的閘道器。

## 指令

從 repository 根目錄執行：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=true cargo test --locked -p agent
cargo test --locked -p repo-check
cargo build --locked --target aarch64-unknown-linux-gnu -p agent

# 只跑 TPM 相關的測試
cargo test --locked -p agent --lib tpm
CI=true cargo test --locked -p agent --test tpm_endorsement
```

## 結果

所有指令都通過。原始輸出見 [checks.txt](data/2026-10-29-612-ek-certificate/checks.txt)（完整批次）與 [tpm-tests.txt](data/2026-10-29-612-ek-certificate/tpm-tests.txt)（TPM 相關測試，列出每個測試名稱）。

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過，沒有警告 |
| `CI=true cargo test --locked -p agent` | 全部通過：lib 216、binary 7、broker 4、dependencies 1、desired_config 7、link 51、liveness 4、modbus 系列、polling_recovery 3、systemd_unit 6、tpm_endorsement 5、tpm_provision 6、tpm_swtpm 1、transport 19 |
| `cargo test --locked -p repo-check` | 28 通過 |
| `cargo build --locked --target aarch64-unknown-linux-gnu -p agent` | 通過 |
| `cargo test --locked -p agent --lib tpm` | 73 通過（marshal、provision、endorsement、worker 與 `TpmKey`） |
| `CI=true cargo test --locked -p agent --test tpm_endorsement` | 5 通過 |

`tests/tpm_endorsement.rs` 對 swtpm 容器跑以下五個測試，每個都用一顆新的 TPM。每個 `session` 相當於一個開啟 `/dev/tpmrm0` 的行程：session 結束時，它載入的暫時物件會被 flush。

| 測試 | 證明的事 |
| --- | --- |
| `the_ek_is_tpm2_tools_ecc_ek_and_the_same_on_every_run` | `endorsement_key` 算出的 SPKI 等於 `tpm2_createek -G ecc` 建立、`tpm2_readpublic -f der` 讀出的 SPKI。在同一條連線重跑與新連線上都得到相同的結果；送出的指令依序是 NV_ReadPublic（EK 範本）、NV_ReadPublic（EK nonce）、CreatePrimary、ReadPublic、FlushContext、NV_ReadPublic（EK 憑證），之後沒有暫時 handle，`snapshot()` 與 NV 公開資料與執行前相同 |
| `an_ek_certificate_is_returned_when_it_is_for_the_ek_and_refused_otherwise` | 以 `tpm2_nvdefine`／`tpm2_nvwrite` 在 `0x01C0000A` 寫入 rcgen 為這把 EK 產生的憑證（513 到 1023 bytes，需要兩次 NV_Read）：平台建立、TCG 屬性的 index 以 owner 授權（`0x40000001`）讀兩次並得到 `Matches`；只有 `authRead` 的 index 以 index 本身的授權讀兩次並得到 `Matches`；別的金鑰的憑證得到 `CertificateMismatch`；截斷的憑證得到 `CertificateInvalid`；定義了但沒寫入的 index 得到 `Absent` |
| `an_endorsement_authorization_is_refused_and_the_tpm_is_unchanged` | 以 `tpm2_changeauth -c e` 設定 endorsement 授權後，結果是 `EndorsementAuthorization`，回應碼 `0x9a2`（`TPM_RC_BAD_AUTH`），CreatePrimary 之後不再送任何指令；前後的 `snapshot()`（持久、暫時與 NV handle、每個物件的 readpublic、變動的屬性含字典攻擊計數）與 NV 公開資料完全相同 |
| `an_ek_template_or_nonce_in_nv_is_refused_before_anything_is_created` | NV 定義了 EK 範本（`0x01C0000C`）或 EK nonce（`0x01C0000B`）時，回 `CustomTemplate`，只送了 NV_ReadPublic（範本一次、nonce 兩次），沒有 CreatePrimary |
| `a_process_that_dies_while_the_ek_is_loaded_leaves_nothing` | 行程在 EK 載入後、ReadPublic 時死掉（之後的 FlushContext 也送不出去）：session 結束後沒有暫時 handle，TPM 狀態與之前相同；重跑得到 tpm2-tools 的 EK |

單元測試（`cargo test -p agent --lib tpm`）的內容：

- 以 swtpm 錄下的交換（`edge/agent/src/tpm/fixtures.rs`，錄製時 tpm2-tools 的 EK 與本實作的相同）逐 byte 比對 endorsement 階層的 CreatePrimary、ReadPublic、FlushContext、NV_ReadPublic 與兩次 NV_Read 的指令，並解析它們的回應：EK 的 SPKI 等於 tpm2-tools 的、名稱是公開區域的 SHA-256、ReadPublic 與 CreatePrimary 給的公開區域相同；NV_ReadPublic 的 index、屬性 `0x62072001` 與大小 798；兩段 NV_Read 接起來就是錄下的憑證；`TPM_RC_HANDLE`（index 不存在）與 `TPM_RC_BAD_AUTH`。
- `parse_ek_area` 拒絕範本前 54 bytes 任何一個 bit 的改變、截斷、多出的 byte 與 33 bytes 的座標；不是 EK 的 SRK 與閘道器金鑰也被拒絕。
- NV 與 EK 回應的每種截斷、延長、膨脹的內層長度與兩萬筆亂資料都被拒絕，而且不會 panic；NV_Read 回的資料長度與要求的不同時是錯誤。
- 憑證比對（rcgen 產生）：EK 的憑證相符；別的金鑰或同一把金鑰的壓縮點是 `CertificateMismatch`；憑證後多 1 或 16 bytes、截斷、空的、SPKI 本身與不定長度編碼是 `CertificateInvalid`；五千筆變造的憑證不會 panic，也不會在 SPKI 不同時相符。
- 以腳本化的假 TPM 測整個流程：沒有憑證時是 `Absent`、錄下的憑證分兩段讀出並相符（CreatePrimary 收到 `TPM_RC_RETRY` 時重送）、只有 `authRead` 時以 index 授權讀、4096 bytes 的 index 分八段讀出；EK 範本或 nonce 在送出 CreatePrimary 前被拒；NV_ReadPublic 描述的是別的 index 時是錯誤；屬性不同、名稱不是公開區域的 SHA-256、ReadPublic 不一致、點不在 P-256 上的 EK 都會被拒絕，而且一定送了 FlushContext；index 是計數器、沒有 ownerRead 也沒有 authRead、大小是 0、4097 或 65535 時是 `CertificateIndex`；沒寫入過是 `Absent`；NV_Read 回得比要求的短是錯誤。

### 安全審查後的修正

安全審查（head `2a02c602`）沒有 Critical 或 High，下列項目在之後的 commit 處理：

- **M1（文件）**：`EkCertificate::Matches` 只代表憑證的 SPKI 與 EK 的公鑰位元組相同。它不驗簽章與憑證鏈，也不證明 TPM 是真品：拿到 platform 授權的人可以改寫 `0x01C0000A`，假的 TPM 或匯流排上的中間人也能回報別台設備的 EK 和它真正的憑證。
  - 這個限制寫在 `Matches` 的 rustdoc、模組文件、`edge/agent/README.md`「背書金鑰」一節與決策 12 的 (6)，並連到 `services/certs/README.md`「規則與取捨」的「EK 只記錄、比對，不驗證」。
  - 新增單元測試 `a_self_signed_certificate_for_the_ek_key_still_matches_because_only_the_key_is_compared`：自簽、但公鑰與 EK 相同的憑證仍然得到 `Matches`。
- **L1**：
  - `marshal::nv_attribute` 新增 `NO_DA`（bit 25）。
  - index 沒有 ownerRead、只能以 authRead 讀時，必須帶 `noDA`，否則在送出任何授權之前就回 `CertificateIndex`；index 設了密碼時，空密碼的 NV_Read 才不會讓字典攻擊計數加一。
  - 回歸測試 `flow::an_auth_read_index_without_no_da_is_refused_before_any_read`：屬性為 `RECORDED_ATTRIBUTES & !OWNERREAD & !NO_DA` 時，結果是 `CertificateIndex`，送出的指令只到 NV_ReadPublic，沒有 NV_Read。
  - 字典攻擊計數在兩條讀取路徑上都不受影響：ownerRead 路徑用 owner 階層授權，階層授權不受字典攻擊保護；authRead 路徑只在有 `noDA` 時才送授權。swtpm 測試在兩條路徑讀取成功後都比對 `snapshot()`（含字典攻擊計數）。授權失敗時計數不變是依規範推論，沒有在 swtpm 上另外實測。
- **L2（文件）**：CreatePrimary 的回應格式錯誤或逾時時，不知道 handle，EK 不會被 flush，清理依賴核心的資源管理器。已寫進模組文件與 README；只接受 `/dev/tpmrm*` 的檢查是 Laysi/trustiot-v2#613 的前提條件。
- **I1**：尾端 bytes 的規則維持嚴格。Laysi/trustiot-v2#618 之後可考慮的放寬形式寫在 PR 內文：尾端全是 `0x00` 或 `0xFF`，或 TCG 的 NV 標頭 `10 01 00 <len16>`；兩種都只把 DER 本身往下游送。

修正後在建置主機上重跑以下檢查，原始輸出見 [checks-after-review.txt](data/2026-10-29-612-ek-certificate/checks-after-review.txt)：

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過，沒有警告 |
| `CI=true cargo test --locked -p agent` | 全部通過：lib 218（多了上面兩個新測試）、tpm_endorsement 5、tpm_provision 6、tpm_swtpm 1、dependencies 1，其餘測試檔與上表相同 |
| `cargo build --locked --target aarch64-unknown-linux-gnu -p agent` | 通過 |

`cargo test -p repo-check` 沿用上表的結果：這次修正只動 agent 的程式與文件。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 算出的 EK 公鑰與 tpm2-tools `tpm2_createek -G ecc` 的 ECC EK 相同，重跑與新連線相同 | 通過（`the_ek_is_tpm2_tools_ecc_ek_and_the_same_on_every_run`） |
| 以 `tpm2_nvdefine`／`tpm2_nvwrite` 在 `0x01C0000A` 寫入 rcgen 產生的 EK 憑證：相符通過、不符拒絕、沒有憑證標記為沒有 | 通過（`an_ek_certificate_is_returned_when_it_is_for_the_ek_and_refused_otherwise`，沒有憑證的情況另見 `the_ek_is_tpm2_tools_ecc_ek_and_the_same_on_every_run`） |
| 設了 endorsement 授權時以明確錯誤結束，TPM 狀態不變 | 通過（`an_endorsement_authorization_is_refused_and_the_tpm_is_unchanged`） |
| NV 回應解析器有錄製回應的單元測試；aarch64 建置通過；`dependencies.rs` 通過 | 通過 |
| `CI=true cargo test --locked -p agent` 與 `cargo clippy --workspace --all-targets -- -D warnings` 通過 | 通過 |

待測（依驗證分級延後為進階驗證，或屬於後續 issue 的範圍）：

- 在實體 TPM（SLB9670）上計算 EK 並讀出製造商的 EK 憑證（Laysi/trustiot-v2#618 的範圍）。其中要確認一件事：本實作要求 NV 裡剛好是一張 DER 憑證、後面不能有多餘的 bytes；若 SLB9670 的 EK 憑證 index 比憑證大、尾端有填充，會被拒絕為 `CertificateInvalid`，屆時再從上面 I1 列出的放寬形式中選擇。
- 只能經核心資源管理器（`/dev/tpmrm*`）存取的強制檢查屬於 Laysi/trustiot-v2#613。
