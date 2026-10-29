# agent：`trustiot-agent provision` 子命令（2026-10-29）

Laysi/trustiot-v2#613：閘道器端的出廠佈建命令，印出含 CSR 與 EK 的 JSON 報告（決策 12 的 (7)；參數、流程與結束碼見 [edge/agent/README.md](../../edge/agent/README.md) 的「出廠佈建」，報告欄位見 [crates/protocol/README.md](../../crates/protocol/README.md) 的「出廠佈建報告」）。這份報告是驗收條件的證據：全部由自動化測試以 `CI=1` 實際執行，TPM 是 `testkit::swtpm` 起的 swtpm（需要容器的測試不略過）；真的 SLB9670 沒有測（見「結論」）。

## 環境

- **程式碼**：分支 `feat/613-agent-provision`。功能 commit 先是 `9d97bb5e`（基於 main `24271deb`），之後 rebase 到 main `61e20b22`（Laysi/trustiot-v2#740 與 #732 的 gateway-ops、identity 等，沒有改到本變更的檔案，沒有衝突）成為 `5cd55d36`，內容相同。報告的 commit（`12b72040` 與之後的更新）只加文件與測試輸出。
- **機器**：建置主機（x86_64 Linux），經檢查佇列執行；swtpm 由 `testkit::swtpm` 在容器裡啟動，tpm2-tools 在同一個容器裡給出獨立的計算結果。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0；aarch64 建置用釘選的 zig 與 cargo-zigbuild。
- **設定**：`CI=1`。執行檔的測試只用不存在的 `/dev/tpmrm99999` 與非 TPM 的路徑，不會碰到主機的 TPM。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=1 cargo test --locked -p agent
cargo test --locked -p protocol
cargo test --locked -p repo-check
cargo run --locked -p openapi -- --check
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent -p protocol
cargo deny --locked check
bun install --cwd scripts --frozen-lockfile && bun test scripts
```

驗收證據可以單獨重跑：

```sh
CI=1 cargo test --locked -p agent --test provision --test binary --test tpm_provision --test tpm_endorsement
CI=1 cargo test --locked -p agent --lib -- provision:: resource_manager worker::tests
cargo test --locked -p protocol --lib provisioning
cargo test --locked -p agent --test dependencies
cargo test --locked -p protocol --test dependencies
```

## 結果

| 指令 | 哪個 commit | 結果 |
| --- | --- | --- |
| `cargo clippy --locked -p agent -p protocol --all-targets -- -D warnings`，`CI=1 cargo test --locked -p agent`，`cargo test --locked -p protocol` | `9d97bb5e` 的工作樹 | clippy 通過；agent 384 個測試通過（lib 230、`binary` 11、`provision` 3、`tpm_provision` 6、`tpm_endorsement` 5 等），protocol 136 個通過，0 失敗 |
| `cargo fmt --all --check`，`cargo clippy --locked --workspace --all-targets -- -D warnings`，`cargo test --locked -p repo-check`，`cargo run --locked -p openapi -- --check`，aarch64 的 `cargo zigbuild`（`-p agent -p protocol`） | `9d97bb5e` 的工作樹 | 全部通過：`api/openapi.json is up to date`；aarch64 release 建置完成（只有 zig 連結器既有的 `ignoring deprecated linker optimization setting '1'` 警告） |
| `cargo deny --locked check`，`bun test scripts` | 加上 sysfs 檢查之前的工作樹（`Cargo.toml` 與 `Cargo.lock` 與 `9d97bb5e` 相同） | `advisories ok, bans ok, licenses ok, sources ok`；1704 個通過、9 個略過、0 失敗 |
| 上面「驗收證據」的五個指令 | `9d97bb5e` | 全部通過，每個測試的名稱與結果在 [provision-tests.txt](data/2026-10-29-613-agent-provision/provision-tests.txt) |
| rebase 後的全部檢查（見下節） | `12b72040` | 全部通過 |

### rebase 後

rebase 到 main `61e20b22` 之後，在 `12b72040`（本報告的第一版，程式與 `5cd55d36` 相同）重跑 `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`CI=1 cargo test --locked -p agent -p protocol`、`cargo test --locked -p repo-check`、`cargo run --locked -p openapi -- --check`、`cargo deny --locked check` 與 `bun test scripts`，全部通過：agent 384 個、protocol 136 個、repo-check 28 個測試通過，0 失敗；`api/openapi.json is up to date`；`advisories ok, bans ok, licenses ok, sources ok`；scripts 1704 個通過、9 個略過、0 失敗（exit 0 at 15:22:11 (build host)）。

## 測試內容

- `tests/provision.rs`（swtpm，每次執行都是新的連線，與執行檔走同一段程式 `agent::provision::run`）：
  - 空的 handle：報告的 `key.state` 是 `present`、`created` true；CSR 以 rcgen 的 `CertificateSigningRequestParams::from_der`（與工作站相同的檢查）驗過簽章，CSR 的公鑰等於報告的公鑰，也等於 tpm2-tools 讀出的持久金鑰公鑰；subject 只有 `CN=<硬體 ID>`；報告的 EK 等於 `endorsement_key`（Laysi/trustiot-v2#612）重新計算的結果，也等於 `tpm2_createek -G ecc` 的 EK；結束後沒有暫時物件。在同一個連線與新的連線重跑、以及 `--inspect`，都是相同的公鑰、`created` false，持久 handle 沒有增加。
  - 空的 TPM 上 `--inspect`：`key.state` 是 `absent`，持久 handle 清單仍是空的，EK 仍與 tpm2-tools 相同。
  - EK 憑證屬於這把 EK：`ek_certificate` 是 `for_this_key`；屬於別的金鑰：結束碼 6，金鑰沒有被建立；handle 上有別的物件：結束碼 5；這些情況 stdout 都是空的。
  - 每個情境都搜尋報告、stderr 與日誌：PEM 只有 `PUBLIC KEY`、`CERTIFICATE REQUEST`、`CERTIFICATE`，PEM 以外不出現 private、secret、password。
- `tests/binary.rs`（真的執行檔）：沒有 TPM 裝置結束碼 3、stdout 空；不合法的硬體 ID、handle（不是十六進位、超出持久範圍、SRK 的 handle）與重複或未知的參數結束碼 2；`/dev/tpm0`、`/dev/null`、`/dev/tpmrm0/../tpm0` 與暫存檔結束碼 4，暫存檔仍是空的（沒有寫入任何命令）。
- 單元測試：`TpmTransport::Device("/dev/tpm0")` 等路徑在開啟前就回 `NotResourceManager`，`run` 在呼叫連線之前就結束（連線函式會 panic）；名稱相符的路徑再以 sysfs 檢查：`/sys/dev/char/<major>:<minor>` 的連結只有 `tpmrm/tpmrm<N>` 算資源管理器（`tpm/tpm0`、`mem/null` 等不算），`/dev/null` 被拒為 `NotResourceManagerDevice`；裝置號碼以 glibc 的 `major`、`minor` 規則拆開（含超過 12 與 20 位元的號碼）；CSR 被竄改、別的金鑰簽的、別的名稱與多餘位元組都會被本地檢查拒絕；報告型別不含 verified、trusted 這類字眼，不認得的欄位忽略、其他版本拒絕。

## 安全審查

2026-10-29 對 `6af60e97` 的安全審查沒有 Critical、High、Medium。各項的處理如下，修正在 `b6f7eafe`：

| 項目 | 處理 |
| --- | --- |
| L1：報告契約沒寫讀取端要做什麼（登錄 API 不比對 CSR 主體，CN 取自請求 body 的 `hardware_id`） | `protocol::provisioning` 的模組文件與 [crates/protocol/README.md](../../crates/protocol/README.md)「讀取端的責任」寫明：硬體 ID 用工作站自己的，報告的 `hardware_id` 必須相同；讀取端自己解析 CSR，確認主體只有等於該 ID 的 CN、SubjectPublicKeyInfo 與 `public_key` 逐 byte 相同、簽章驗得過；stdout 最多讀 `MAX_REPORT_BYTES`（16 KiB，與登錄 API 的 body 上限相同）。agent 也不印超過這個大小的報告（結束碼 1）；單元測試確認 EK 憑證最大（4096 bytes）、硬體 ID 最長（64 字元）的報告不超過 9 KiB，更大的報告不印 |
| L2：`provision --help` 以 0 結束但不是 JSON | 保留結束碼 0，在 USAGE、agent README 與讀取端的責任寫明：成功是結束碼 0 而且 stdout 解析得成 `ProvisioningReport`。`--help` 改為只能單獨使用，與其他參數一起給是結束碼 2（單元測試與 `tests/binary.rs` 的結束碼 2 測試） |
| I1：寫到一半失敗會留下部分 JSON | 讀取端的責任第 1 條與 agent README：先看結束碼，不是 0 就丟掉 stdout |
| I2：開啟裝置沒有 `O_NOCTTY` | 開啟時加 `custom_flags(libc::O_NOCTTY)`；`libc` 原本就是 agent 的依賴，沒有新增依賴 |
| I3–I6 | 依審查結論不改。I6（沒有 EK 憑證仍會建立金鑰，工作站要定政策）與讀取端的責任由協調者帶到 Laysi/trustiot-v2#616 |

修正後在建置主機（x86_64 Linux）重跑 `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`CI=1 cargo test --locked -p agent -p protocol` 與 `cargo test --locked -p repo-check`，全部通過：agent 385 個（lib 231，多了報告大小的測試）、protocol 136 個、repo-check 28 個，0 失敗（exit 0 at 15:37:30 (build host)）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| swtpm：CSR 簽章可驗、CSR 公鑰等於報告的金鑰公鑰、報告的 EK 等於 #612 的計算 | 通過（`the_report_has_a_verifying_csr_for_the_key_and_the_tpms_ek_and_reruns_reuse_the_key`） |
| 報告與 stderr 沒有私密資料 | 通過（`tests/provision.rs` 的三個測試都檢查） |
| 空 TPM `--inspect` 報告金鑰不存在、持久 handle 清單不變 | 通過（`inspect_on_an_empty_tpm_reports_the_key_absent_and_changes_nothing`） |
| 重跑（含新連線）得到相同公鑰、`created` false | 通過（第一個測試的重跑段落） |
| 沒有 TPM 裝置有固定結束碼、stdout 空；不合法的硬體 ID 與 handle 被拒 | 通過（`tests/binary.rs` 的結束碼 3 與 2 兩個測試） |
| 不是 `/dev/tpmrm*` 的裝置有固定結束碼、不開啟 TPM | 通過（結束碼 4 的執行檔測試與單元測試；名稱相符但核心不認得的裝置也在開啟前拒絕） |
| aarch64 建置；`edge/agent/tests/dependencies.rs` 與 protocol 的依賴檢查 | 通過 |
| `CI=true cargo test --locked -p agent`、`cargo test -p protocol`、`cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| 在真的 SLB9670 與閘道器的 `/dev/tpmrm0` 上執行、確認 sysfs 連結的形狀 | 待測：不在本 issue 的範圍，目前不動硬體；列在 Laysi/trustiot-v2#618 |
