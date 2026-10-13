# contract 與 protocol 型別變更使依賴它的 crate 編譯失敗（2026-10-13）

對應 issue：Laysi/trustiot-v2#8 Cargo workspace 與共用 crate，第二條驗收條件：「任一服務改動 contract 或 protocol 的型別時，依賴它的 crate 編譯失敗（以 CI 的 cargo check --workspace 證明）」。原始資料在 [data/2026-10-13-8-contract-type-changes/](data/2026-10-13-8-contract-type-changes/)。

先前的 [2026-10-10-49-contract-crate.md](2026-10-10-49-contract-crate.md) 只證明 contract crate 自己的 compile_fail 範例；本報告對整個 workspace 改型別，記錄哪些 crate 因此編譯失敗。

## 環境

- 機器：建置主機（`<build host>`，20 核，有 Docker），經由 check queue（`rcheck.sh`）執行。主機上的目錄是工作目錄的複本：一個全新的 git repository，只有一個快照 commit，所以 `git apply` 與還原都在複本上做，不碰工作目錄。
- 受測版本：分支 `test/8-contract-type-changes`，基底 `origin/main` 的 `f2ff4a8`；工作目錄除了本報告與 data 目錄之外沒有變更。`run-mutations.out` 開頭的 `base commit` 是建置主機上那份複本的快照 commit，不在本 repo 的歷史裡，內容就是這個基底加上本報告與 data 目錄。變更以 patch 檔套用，每個 patch 在乾淨的樹上單獨套用，跑完就還原，不進 commit。
- 工具：rustc 1.99.0（b940084d7 2026-09-28）、cargo 1.99.0，由 `.prototools` 與 `rust-toolchain.toml` 鎖定；Bun 1.4.2 只用來跑 `scripts/ci/changes.ts` 的判斷。
- CI 的 `rust` job（`.github/workflows/ci.yml`）沒有獨立的 `cargo check` 步驟：`cargo clippy --workspace --all-targets --locked -- -D warnings` 先編譯整個 workspace，型別錯誤在那一步就是編譯錯誤；`cargo test --workspace --locked` 也會編譯同樣的目標。驗收條件寫的 `cargo check --workspace` 另外跑，指令與 CI 的 clippy 步驟相同，只差 `check` 與 `clippy`。

## 指令

從 repository 根目錄、在乾淨的樹上執行（我是用 `rcheck.sh 8 'bash docs/test-reports/data/2026-10-13-8-contract-type-changes/run-mutations.sh'` 在建置主機執行）：

```sh
bash docs/test-reports/data/2026-10-13-8-contract-type-changes/run-mutations.sh

# CI 是否會因為這些路徑跑 rust job
bun docs/test-reports/data/2026-10-13-8-contract-type-changes/changes-decision.ts
bun test scripts/ci/changes.test.ts

# 依賴關係（不編譯）
cargo tree -i contract --workspace --locked -e normal,dev,build --depth 2
cargo tree -i protocol --workspace --locked -e normal,dev,build
```

`run-mutations.sh` 先在沒有變更的樹上跑基準，之後對 data 目錄裡每個 `*.patch`：`git apply`，依序執行下列四個指令，再 `git reset --hard` 還原並確認樹是乾淨的。

1. `cargo check --package <被改的 crate> --all-targets --locked`：確認被改的 crate 自己編譯過（contract），或看它自己的錯誤在哪（protocol）。
2. `cargo clippy --workspace --all-targets --locked -- -D warnings`：CI `rust` job 的 clippy 步驟，原樣。
3. `cargo check --workspace --all-targets --locked`：驗收條件的指令。
4. `cargo check --workspace --all-targets --locked --keep-going`：多一個 `--keep-going`。cargo 預設在第一個 crate 失敗後不再排新的工作，前兩個指令列出的失敗 crate 因此不一定完整；這個指令把互不依賴的 crate 都編譯到底，列出全部。

## 結果

### 依賴 contract 與 protocol 的 crate

資料：[dependents.txt](data/2026-10-13-8-contract-type-changes/dependents.txt)。

| 被依賴的 crate | 依賴它的 workspace crate（宣告的種類） |
| --- | --- |
| `contract` | `identity`（normal）、`openapi`（normal）、`outbox`（normal）、`service-kit`（normal）、`testkit`（normal） |
| `protocol` | 沒有。`edge/` 還沒有成員（MS1 才加），其他 `Cargo.toml` 都沒有提到 protocol |

`ingest` 與其他工具 crate 不依賴 contract。沒有 crate 以 dev 或 build 依賴指向 contract 或 protocol。

### 基準（沒有變更）

資料：[run-mutations.out](data/2026-10-13-8-contract-type-changes/run-mutations.out)。

| 指令 | 結束碼 |
| --- | --- |
| `cargo check --workspace --all-targets --locked` | 0 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 0 |

### 變更與結果

每個變更是一個 patch 檔，都放在 data 目錄。「自己」是被改的 crate 單獨編譯的結束碼。其餘三欄的結束碼全是 101（編譯失敗），所以只列失敗的 crate 與編譯目標。

| 變更（patch） | 改了什麼 | 自己 | clippy 步驟失敗的 crate | `cargo check --workspace` 失敗的 crate | 加 `--keep-going` 失敗的 crate |
| --- | --- | --- | --- | --- | --- |
| [m1](data/2026-10-13-8-contract-type-changes/m1-contract-claims-exp-type.patch) contract 身分 token 的 claims | `InternalClaims.exp` 由 `i64` 改成 `u64`（服務簽發、驗證 token 的欄位） | 0 | `service-kit`（lib、lib test） | `service-kit`（lib、lib test） | `service-kit`（lib、lib test） |
| [m2](data/2026-10-13-8-contract-type-changes/m2-contract-envelope-tenant-rename.patch) contract 事件信封 | `EventEnvelope.tenant_id` 改名為 `tenant`，以 `serde(rename)` 保持傳輸格式 | 0 | `outbox`（lib） | `outbox`（lib） | `outbox`（lib、lib test） |
| [m3](data/2026-10-13-8-contract-type-changes/m3-contract-error-code-timeout-removed.patch) contract 錯誤代碼 | 刪除 `ErrorCode::Timeout`，contract 內部與它的測試一併改成 `Unavailable` | 0 | `service-kit`（lib、lib test） | `service-kit`（lib、lib test） | `service-kit`（lib、lib test） |
| [m4](data/2026-10-13-8-contract-type-changes/m4-contract-tenant-role-admin-renamed.patch) contract 租戶角色 | `TenantRole::Admin` 改名為 `Owner`，以 `serde(rename)` 保持傳輸格式 | 0 | `identity`（lib）、`testkit`（lib、lib test） | `identity`（lib）、`testkit`（lib、lib test） | `identity`（lib）、`testkit`（lib、lib test） |
| [p1](data/2026-10-13-8-contract-type-changes/p1-protocol-ack-server-time-rename.patch) protocol 訊息 | `TelemetryAck.server_time` 改名為 `sent_at`，以 `serde(rename)` 保持傳輸格式 | 101（8 個整合測試目標） | protocol 自己的整合測試目標（6 個，cargo 提早停止） | protocol 自己的整合測試目標（6 個，cargo 提早停止） | protocol 自己的 9 個整合測試目標 |

錯誤位置與錯誤碼（完整內容在 [run-mutations.out](data/2026-10-13-8-contract-type-changes/run-mutations.out)，取 `cargo check --workspace` 那一段）：

| 變更 | 錯誤碼 | 位置 |
| --- | --- | --- |
| m1 | E0308（型別不符）×4 | `crates/service-kit/src/identity/token.rs:115`、`:116`、`:120`（兩處） |
| m2 | E0609（沒有該欄位）×3 | `crates/outbox/src/consumer.rs:234`、`:1112`；`crates/outbox/src/producer.rs:91` |
| m3 | E0599（沒有該 variant）×1 | `crates/service-kit/src/http.rs:127` |
| m4 | E0599 | `services/identity/src/session.rs:566`；`crates/testkit/src/identity.rs:154`；`crates/testkit/src/isolation.rs:265` |
| p1 | E0560（沒有該欄位）×2 | `crates/protocol/tests/common/mod.rs:115`；`crates/protocol/tests/limits.rs:66`（其餘目標經由 `tests/common` 失敗） |

說明：

- m1 與 m3 只列出 `service-kit`，是因為 `identity`、`outbox`、`testkit` 都依賴 service-kit 的 lib，service-kit 編譯失敗時 cargo 不會去編譯它們，`--keep-going` 也一樣。它們不是沒有受影響：`services/identity/src/issuer.rs:132`、`crates/testkit/src/identity.rs:74` 同樣使用 `exp`，只是要等 service-kit 修好才會輪到。m4 的 `identity` 與 `testkit` 不經過 service-kit，所以兩個都直接列出。
- `openapi` 依賴 contract，但沒有用到這些被改的型別，所以四個 contract 變更都沒有讓它失敗。型別變更只會讓用到那個型別的 crate 失敗；沒有用到的 crate 照常編譯，這是預期的行為。
- m3 第一次嘗試漏改了 contract 自己的一個單元測試（`crates/contract/src/client.rs`），所以 contract 的 lib test 先失敗，其他 crate 根本沒輪到。這個失敗本身也是編譯期保證，但不是我要證明的情境，所以補上那處、重新產生 patch 再跑；最後一次執行才是本報告的資料。
- p1 的 `cargo check --package protocol --all-targets` 失敗，是因為 protocol 的整合測試目標（`allocation`、`limits`、`golden`、`compat`、`robustness`、`split`、`adversarial`、`validation`、`fuzz`）直接建立 `TelemetryAck`；protocol 的 lib 本身編譯過，輸出裡沒有 lib 的錯誤。`fuzz_split`、`broker`、`dependencies`、`readme` 沒用到該欄位，所以沒有失敗。目前沒有任何 workspace crate 依賴 protocol，所以這項只證明 protocol 自己的測試與測試向量會擋住型別變更，不能證明「依賴它的 crate」編譯失敗。
- 每次變更後 `git status --porcelain` 都是空的（腳本最後印出 `all mutations reverted; tree clean`）。

### CI 會因為這些路徑執行 rust job

資料：[changes-decision.out](data/2026-10-13-8-contract-type-changes/changes-decision.out)，由 [changes-decision.ts](data/2026-10-13-8-contract-type-changes/changes-decision.ts) 對 `scripts/ci/changes.ts` 匯出的 `decide()` 一次傳一個變更路徑得到。

| 變更的路徑 | `rust` job |
| --- | --- |
| `crates/contract/src/lib.rs`、`identity.rs`、`event.rs`、`error.rs` | runs |
| `crates/contract/Cargo.toml` | runs（`cargo-deny` 也 runs） |
| `crates/protocol/src/lib.rs`、`telemetry.rs` | runs |
| `crates/protocol/tests/golden/current/telemetry-ack-committed.json` | runs |
| `docs/README.md`（對照） | skipped |

任何 `.rs` 檔都會執行 `rust` job（`isReadAnywhere`：`tools/sql-check` 會掃描所有 `.rs`），crate 目錄底下沒有列在忽略清單的路徑也會。`bun test scripts/ci/changes.test.ts`：170 pass、0 fail。

## 結論

- **contract：通過。** 四個不同種類的型別變更（欄位型別、欄位改名、刪除錯誤代碼 variant、列舉 variant 改名），每一個都在 contract 自己編譯過的情況下，讓依賴它且用到該型別的 crate（`service-kit`、`outbox`、`identity`、`testkit`）在 CI 的 clippy 步驟與 `cargo check --workspace --all-targets --locked` 都以結束碼 101 失敗，並給出檔案、行號與錯誤碼；沒有變更的基準兩個指令都是 0。變更 contract 的 PR 會讓 `rust` job 執行。這個證明限於編譯期：用到被改型別的程式碼一定會失敗，沒用到的 crate（例如 `openapi`）不受影響；經由 service-kit 間接依賴的 crate，要等 service-kit 修好才會輪到。
- **protocol：待測。** 目前沒有任何 workspace crate 依賴 protocol（`edge/` 的成員從 MS1 才加入），「依賴它的 crate 編譯失敗」沒有對象可以測。已證明的是：改 protocol 的訊息型別，protocol 自己的 9 個整合測試目標編譯失敗（p1），CI 的 `rust` job 因為 `crates/protocol/` 的變更而執行。第一個依賴 protocol 的 `edge/` crate 併入後，要對它重跑這份報告的指令（在 `run-mutations.sh` 加一個變更，確認該 crate 出現在失敗清單裡），才能勾這一條的 protocol 部分。
- 第二條驗收條件整體：contract 部分有證據，protocol 部分要等第一個依賴它的 crate 才能完整證明。
