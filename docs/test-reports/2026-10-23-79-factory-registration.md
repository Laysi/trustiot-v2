# certs 服務：出廠登錄 API（2026-10-23）

Laysi/trustiot-v2#79：產線工作站以工作站憑證簽請求，平台管理員登入，登錄閘道器的硬體 ID、TPM 金鑰與背書金鑰（EK），取得第一張閘道器憑證。每筆新登錄以平台角色 `certs_platform` 寫入，同一個交易發出全平台事件 `certs.gateway_registered`。規則與取捨在 [services/certs/README.md](../../services/certs/README.md) 的「出廠登錄」。這份報告是六條驗收條件的證據。

## 環境

- **程式碼**：分支 `feat/79-factory-registration`。
  - 最後的 commit 是 `f774a10b`，它合併了 main `1cd3da21`（含 Laysi/trustiot-v2#557 的 broker 伺服器憑證）。
  - 合併時，這個分支的 migration 從 `0003_gateway_registrations.sql` 改為 `0004_gateway_registrations.sql`，排在 #557 的 `0003_broker_certificates.sql` 之後。
  - 登錄簽第一張閘道器憑證改走 #557 的共用簽發路徑（`issue::sign_and_record_in`，見「結果」）。
- **本機檢查**：開發者工作站（x86_64 Linux），本機檢查佇列一次一個工作，每個工作限制記憶體，`CARGO_BUILD_JOBS=2`。
  - 需要容器的測試設了 `CI=1`，以拋棄式的 TLS Postgres 執行，不略過。
  - 工具版本是 `.prototools` 與 `rust-toolchain.toml` 釘選的：Rust 1.99.0、bun 1.4.2、helm 4.3.0、kubeconform 0.8.0、gitleaks 8.30.1。
- **本機檢查跑的樹**：合併的解決結果，與最後的 commit 只差 `0004_gateway_registrations.sql` 開頭的一段 SQL 註解。這段註解說明改號的理由，是 Codex 審查要求的，在檢查跑完之後才加上。註解不改變任何 SQL 敘述。
- **CI（與 main 合併前）**：PR 對合併前的 commit `15be7786` 的結果。最後的 commit 的 CI 記在 PR 上，不在這份報告。
- **日期**：2026-10-23。

## 指令

在 repo 根目錄，依序經本機檢查佇列執行：

```sh
cargo run -p openapi
CI=1 cargo test -p certs -p migrate -p contract
cargo clippy -p certs -p migrate -p contract --all-targets -- -D warnings
cargo test -p repo-check
cargo run -p openapi -- --check
bun install --cwd scripts --frozen-lockfile
bun test scripts
bun test deploy/chart
CI=1 cargo test -p identity --test event_log
CI=1 cargo test -p certs
```

不經佇列、直接執行：

```sh
cargo fmt --all --check
bun run --cwd frontend api:check
```

驗收證據的測試可以單獨重跑，`--nocapture` 印出每一步的證據行：

```sh
CI=1 cargo test -p certs --test registration -- --nocapture
```

## 結果

### 本機：全部通過

| 指令 | 結果 |
| --- | --- |
| `cargo run -p openapi` | 產生的 `api/openapi.json` 與合併的結果相同（#557 的 broker 端點在自己的埠上，不在規格裡）；`bun run --cwd frontend api:check` 通過 |
| `CI=1 cargo test -p certs -p migrate -p contract` | 通過，0 失敗、0 略過。各測試檔的數目見下面三張表 |
| `cargo clippy -p certs -p migrate -p contract --all-targets -- -D warnings` | 第一次因 `explicit_auto_deref` 失敗（`registrations.rs` 傳 `&mut **tx`），改成 `tx` 後通過 |
| `cargo test -p repo-check` | 17 + 9 通過 |
| `cargo run -p openapi -- --check` | `api/openapi.json is up to date` |
| `bun test scripts` | 1590 通過、9 略過、0 失敗 |
| `bun test deploy/chart` | 618 通過、0 失敗 |
| `CI=1 cargo test -p identity --test event_log` | 1 通過。步驟 5c：certs 的全平台事件只記進平台的事件紀錄 |
| `CI=1 cargo test -p certs`（clippy 修正之後） | 與上表 certs 的數目相同，全部通過 |
| `cargo fmt --all --check` | 通過 |

`cargo test -p certs`，每個測試檔：

| 測試檔 | 通過 |
| --- | --- |
| 單元測試（`src/lib.rs`） | 18 |
| `tests/broker.rs` | 5 |
| `tests/ceremony.rs` | 3 |
| `tests/crl.rs` | 2 |
| `tests/cross_tenant.rs` | 1 |
| `tests/issuance.rs` | 5 |
| `tests/key_custody.rs` | 3 |
| `tests/registration.rs` | 4 |
| `tests/workstation.rs` | 4 |

`cargo test -p contract`：

| 測試檔 | 通過 |
| --- | --- |
| 單元測試 | 43 |
| `tests/client.rs` | 14 |
| `tests/client_logging.rs` | 1 |
| `tests/error_format.rs` | 7 |
| `tests/events.rs` | 13 |

`cargo test -p migrate`：

| 測試檔 | 通過 |
| --- | --- |
| 單元測試 | 55 |
| `tests/registry.rs` | 6 |
| `tests/fresh_database.rs` | 1 |
| `tests/file_escapes.rs` | 1 |
| `tests/outbox_and_tenant_roles.rs` | 1 |
| 其他 14 個測試檔（ClickHouse、MQTT 帳號與信任、密碼、簽章金鑰、系統資料庫等） | 全部通過 |

`tests/registry.rs` 比對 `services/certs/src/schema.rs` 的清單與 `migrations/postgres/` 目錄。`fresh_database.rs` 在空的資料庫依序套用 0001 到 0004。

合併前另做的一項突變測試，目前的樹沒有再跑：第三輪修正加上的檢查（工作站憑證要有 keyUsage digitalSignature、不能是 CA）暫時從服務拿掉時，新的測試 `a_workstation_certificate_needs_digital_signature_and_no_ca` 失敗；還原後通過。

### CI（與 main 合併前）

PR 對合併前的 commit `15be7786` 的結果：

- [ci run 39918488556](https://github.com/Laysi/trustiot-v2/actions/runs/39918488556)：每個 job 都通過。
  - `cargo test --workspace --locked` 1247 通過、0 失敗。
  - 其中 `services/certs/tests/registration.rs` 3 通過。第 4 個測試（keyUsage 與 CA 的檢查）在之後的 commit `0f629e96` 才加入，只在本機跑過。
- [nightly run 39918488576](https://github.com/Laysi/trustiot-v2/actions/runs/39918488576)：smoke 通過。

### 合併的解決結果

- **migration 的順序**：`0004_gateway_registrations.sql` 只新增表、trigger 與授權，不碰 #557 在 0003 放寬的 `issued_certificates` profile CHECK（`gateway`、`workstation`、`broker`），所以疊在 0003 的結果上。
  - 舊名 `0003_gateway_registrations.sql` 從未進入 main 或任何 release，只有拋棄式的測試資料庫套用過，所以改號不影響任何已部署的資料庫。
  - main 已有的 `0003_broker_certificates.sql` 保留原號。
- **簽發路徑**：#557 的 `issue::sign_and_record` 改為在 `sign_and_record_in` 外開交易、提交；`sign_and_record_in` 在呼叫者的連線上讀 CRL、選簽發 CA、簽章並寫進簽發紀錄。
  - 登錄在自己的平台角色交易裡呼叫 `sign_and_record_in`，所以憑證、登錄與事件仍一起提交，或一起不提交。
  - 工作站憑證與 broker 憑證照舊呼叫 `sign_and_record`。
- **撤銷**：工作站憑證的撤銷仍在自己的交易裡先取 advisory lock 的排他鎖（`issue::LOCK_WORKSTATION`），與登錄的共享鎖互斥。
- **跨租戶測試**：certs 從這個分支起是事件的產生者，outbox 的兩張表以租戶表探測，所以不用 #557 的 `without_tenant_data`。這個宣告表示沒有租戶表，會與 outbox 的兩張表矛盾。

## 結論

逐條對照 Laysi/trustiot-v2#79 的驗收條件，對應的測試都在 `services/certs/tests/registration.rs`。

| 驗收條件 | 測試 | 結果 |
| --- | --- | --- |
| 同一台設備重複登錄回傳同一筆紀錄 | 容器情境 `registration_is_idempotent_and_needs_a_workstation_and_an_administrator` 的 Acceptance 1。重送（新的 nonce）回 200、同一筆紀錄與同一張憑證，不簽新憑證、不寫事件；原請求原封不動重送以 `replayed` 拒絕；兩個相同的新登錄同時到達，只簽一張、一筆事件 | 通過 |
| 試圖修改已登錄的公鑰被拒絕 | 同一情境的 Acceptance 2。同一個硬體 ID 換 TPM 公鑰或換 EK 回 409；新的硬體 ID 帶已登錄的公鑰或使用中的 EK 也回 409 | 通過 |
| 已除役的硬體 ID 不能再用原紀錄登錄 | 同一情境的 Acceptance 3。superuser 設定除役後，原金鑰與新金鑰的登錄都回 409 `gateway_registration.decommissioned`；superuser 也不能刪除、清空、改寫或撤回除役（42501）；`certs_app` 讀不到登錄表 | 通過 |
| 缺少工作站身分或平台管理員身分的請求被拒絕 | 同一情境的 Acceptance 4，以及單元測試 `only_a_workstation_certificate_signs_for_a_workstation` 與 `a_workstation_certificate_needs_digital_signature_and_no_ca`（細節見下） | 通過 |
| 每筆登錄都產生一筆事件 | 同一情境的第一次登錄：一筆全平台事件，actor 是平台管理員，內容欄位正確；重送與被拒的請求不增加事件。身分服務的 `tests/event_log.rs` 步驟 5c 確認事件進平台的事件紀錄 | 通過 |
| 以閘道器憑證冒充工作站的請求被拒 | 同一情境的 Acceptance 6：已登錄的閘道器以自己的憑證與 TPM 私鑰正確簽請求，回 403 `not_workstation`；單元測試對同一個 CA 簽的閘道器憑證也拒絕 | 通過 |

驗收條件 4 的拒絕情況：

- 容器情境（Acceptance 4）：
  - 有平台管理員但沒有工作站標頭：回 403 `missing`。
  - 沒有 token，或 token 是別的簽發者簽的：回 401。
  - 租戶管理員，或交換來的管理員 token：回 403。
  - 別的私鑰簽的請求，或簽的是別的本文：回 403 `signature_invalid`。
  - 以上情況都不登錄任何東西。
  - 撤銷工作站憑證後，它的請求回 403 `revoked`。撤銷會等待已經讀到該憑證有效的登錄結束。
- 單元測試：
  - 時間差超過 300 秒、過期的憑證、別的 CA 簽的憑證，以及缺少、重複或格式錯的標頭，都被拒絕。
  - keyUsage 沒有 digitalSignature、沒有 keyUsage，或 CA:TRUE 的工作站憑證，以 `not_workstation` 拒絕。

使用者在 2026-10-23 對四個產品判斷的決定：

- 接受：工作站被拒時回 403（不回 401，免得前端當成 session 過期）。
- 接受：不驗證 EK 憑證鏈，只檢查 EK 的演算法與長度。
- 接受：重送時回傳原本的憑證。
- 暫定，移到 Laysi/trustiot-v2#115：除役的硬體 ID 目前不能再登錄，程式維持這個從嚴的行為。除役的硬體 ID 能不能再登錄（例如維修後當作一筆新的登錄），在 #115 與資產服務一起決定；要放寬需要新的 migration，因為登錄表的主鍵與事件的 aggregate id 都是硬體 ID。除役本身不可逆。

待測：沒有在叢集裡經真的入口、身分服務與產線工作站工具呼叫登錄 API。工作站端工具是 Laysi/trustiot-v2#80，nightly 與展示環境也還沒有啟用 certs 服務。根 CRL 不在這個 API 的範圍，由 Laysi/trustiot-v2#560 之後在同一個回應加入。
