# certs 服務：出廠登錄的回應帶憑證鏈（2026-10-23）

Laysi/trustiot-v2#610：出廠登錄（`POST /api/certs/v1/gateway-registrations`）的回應 `GatewayRegistration` 新增 `certificate_chain`，內容是 leaf 憑證，再接簽出它的簽發 CA 憑證（PEM，不含根）。`certificate` 與 `root_certificate` 不變。行為與取捨寫在 [services/certs/README.md](../../services/certs/README.md) 的「出廠登錄」→「請求與回應」。這份報告是四條驗收條件的證據。

## 環境

- **程式碼**：分支 `feat/610-registration-certificate-chain`，從 main `87b51f33` 開出。
  - 之後 rebase 到 main `74216494`（含 Laysi/trustiot-v2#604），沒有衝突。
  - rebase 之後重跑的檢查記在 [after-rebase.txt](data/2026-10-23-610-registration-certificate-chain/after-rebase.txt)：fmt、OpenAPI 與前端用戶端的檢查、workspace clippy、`contract`、`repo-check` 與 `certs` 的測試，全部通過。
- **檢查**：建置主機（x86_64 Linux），經檢查佇列一次一個工作執行。佇列設了 `CI=1`，所以需要容器的測試一定會跑：它們以 podman 起一個拋棄式的 TLS Postgres，沒有容器時是失敗而不是略過。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的版本，Rust 1.99.0、bun 1.4.2。
- **nightly 部署（`smoke`）**：這個 PR 動到 `services/**` 與 `crates/contract/**`，所以 nightly 會在 k3d 部署一次。部署結果記在 PR 上，不在這份報告。
- **日期**：2026-10-23。

## 指令

在 repo 根目錄經檢查佇列執行：

```sh
cargo run --locked -p openapi            # 重新產生 api/openapi.json
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:generate      # 重新產生 frontend/src/api/schema.gen.ts
cargo fmt --all --check
cargo run --locked -p openapi -- --check
bun run --cwd frontend api:check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p contract
cargo test --locked -p repo-check
cargo clippy --locked -p certs --all-targets -- -D warnings
cargo test --locked -p certs
cargo test --locked -p certs --test registration -- --nocapture
```

## 結果

原始輸出：

- [first-run.txt](data/2026-10-23-610-registration-certificate-chain/first-run.txt)：第一輪。fmt、OpenAPI 與前端用戶端的檢查、整個 workspace 的 clippy、`contract` 與 `repo-check` 都通過，但 `registration.rs` 失敗，原因見下一段。
- [certs-tests.txt](data/2026-10-23-610-registration-certificate-chain/certs-tests.txt)：修正後，`cargo test -p certs` 全部通過（9 個測試檔），`registration.rs` 以 `--nocapture` 印出證據行。

第一輪失敗的原因是兩個測試共用一個資料庫。同一個測試 binary 的測試共用一個 Postgres（`testkit::product`），而且會同時跑。新加的輪替測試也會登錄閘道器，所以原本測試的計數（例如 `totals == (1, 1, 1)`）多算了輪替測試的列。修正方式是讓兩個測試各算各的：

- 原本的情境只用 `gw-*` 硬體 ID，計數與事件查詢只算 `gw-*`。
- 輪替測試改用 `rot-*` 硬體 ID。

兩個測試同時跑時都通過（certs-tests.txt）。

| 測試 | 驗證 |
| --- | --- |
| `registration_is_idempotent_and_needs_a_workstation_and_an_administrator` | 新登錄（201）與重送（200）都檢查 `certificate_chain`，檢查內容見下一段 |
| `the_chain_carries_the_issuing_ca_that_signed_the_certificate` | 以真的服務模擬一次輪替（輪替的步驟見 README 的「輪替」），步驟見下面的編號清單 |

兩個回應的 `certificate_chain` 都要通過這些檢查：

- 第一張就是 `certificate`。
- 第二張的公鑰驗得過第一張的簽章。
- 第二張的金鑰 id 等於 `ca_key_id`。
- 根憑證驗得過第二張的簽章。
- 對照組：根的公鑰驗不過 leaf。

輪替測試的步驟：

1. 第 1 代簽發 CA 有憑證，第 2 代的金鑰還在等儀式。這時登錄 `rot-0101`，由第 1 代簽，鏈裡是第 1 代。
2. 第 2 代的憑證加進清單，服務重新載入，等兩代都發布 CRL，這時第 2 代簽章。登錄 `rot-0102`，鏈裡是第 2 代。再重送 `rot-0101`：回 200，內容與第一次相同，鏈裡是第 1 代，也就是金鑰 id 等於它 `ca_key_id` 的那一把，而不是現在簽章的第 2 代。
3. 以 `ca_keys` 的 plan（`remove_before: 2`）移除第 1 代金鑰，並把它的憑證從清單拿掉，等服務載入「只有第 2 代」。重送 `rot-0101`：
   - 回 200，`certificate` 與第一次相同。
   - `certificate_chain` 只有 leaf，回應的其他欄位不變。
   - 記一行 WARN，欄位只有 `ca_key_id`，值是第 1 代的金鑰 id。
   - 這時重送 `rot-0102`，仍回原本的兩張憑證鏈。

證據行（certs-tests.txt）：

```text
201 certificate_chain: the certificate, then the issuing CA 4009fee9…; signatures verify up to the root
repeated: 200, the same record and certificate; totals (1, 1, 1)
two issuing CAs loaded: the new registration's chain carries generation 2 (55ab01bf…)
two issuing CAs loaded: the repeat's chain carries generation 1 (4c283b7e…), its ca_key_id
generation 1 removed: the repeat answers 200, chain = the certificate; {"fields":{"ca_key_id":"4c283b7e…"},"level":"WARN","message":"the issuing CA of a repeated gateway registration is no longer loaded, so its certificate chain is the gateway certificate alone",…}
test the_chain_carries_the_issuing_ca_that_signed_the_certificate ... ok
test registration_is_idempotent_and_needs_a_workstation_and_an_administrator ... ok
```

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 201 與 200 的回應都有 `certificate_chain`，第二張的公鑰驗得過 leaf，`root_certificate` 驗得過第二張 | 通過（`registration_is_idempotent_...`，以及輪替測試的每個回應） |
| 載入兩把簽發 CA 時，鏈裡的簽發 CA 是金鑰 id 等於 `ca_key_id` 的那一把 | 通過（輪替測試步驟 2：新登錄是第 2 代，重送的舊登錄是第 1 代） |
| 簽發 CA 不再載入時，重送仍回 200 與原本的 `certificate`，`certificate_chain` 只有 leaf | 通過（輪替測試步驟 3，WARN 只帶 `ca_key_id`） |
| `openapi -- --check`、`api:check`、`cargo test -p certs`、`cargo test -p contract`、workspace clippy 通過 | 通過（first-run.txt 與 certs-tests.txt） |

待測（Laysi/trustiot-v2#610 範圍以外，另開的 P2 驗證）：以真的 CA 儀式與輪替，端到端各確認一次。
