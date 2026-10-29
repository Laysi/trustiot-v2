# certs：閘道器憑證撤銷三步驟（2026-10-30）

Laysi/trustiot-v2#636：平台管理員撤銷一台閘道器每一張還有效的憑證，服務在回應之前為它們的簽發 CA 重發 CRL，接著由 broker 同步刪除它的 dynsec client、斷開它的連線；以 keyCompromise 撤銷過的閘道器不能再換發（規則、N 與 M 見 [services/certs/README.md](../../services/certs/README.md) 的「閘道器憑證的撤銷」）。這份報告是驗收條件的證據：全部由下列自動化測試以 `CI=1` 實際執行（需要容器的測試不略過）。

## 環境

- **程式碼**：分支 `issue-636-gateway-revocation`，基於 main `e4994a25`。功能 commit `79121dde`，安全審查後的修正 `fc288711`（換發拒絕 keyCompromise 撤銷過的閘道器、撤銷與換發的簽發以同一個鎖排序），`22e4653b` 只改一行證據輸出（CRL 以 CA 的金鑰 ID 稱呼）；本報告的 commit 只加文件與測試輸出。
- **機器**：建置主機（arm64 Linux VM），經檢查佇列執行。容器測試由 podman 起 `testkit::product` 的拋棄式 TLS Postgres（以 migration Job 的程式套用整個產品的 schema），以 `certs_app` 與 `certs_platform` 啟動真的 `certs` 執行檔；`tests/dynsec.rs` 另外起 chart 釘住的 Mosquitto `eclipse-mosquitto:2.1.2-alpine`，用 chart 的 `mosquitto.conf`、閘道器 listener 與 init container 寫的 dynsec 檔。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 與 Bun。
- **設定**：`CI=1`。`tests/gateway_revocation.rs` 的服務 CRL 工作每小時一輪（CRL 只會因撤銷而變）。`tests/dynsec.rs` 的撤銷測試：先以一個服務（完整比對 2 秒一次）登錄並綁定兩台閘道器，停掉它，再起兩個副本（完整比對 1 小時一次、日誌 `debug`），所以連線時的那一次之後，broker 只會因請求自己的同步而改變；撤銷送到第二個副本。broker 的 sidecar 以測試代替：從服務取 `crl.pem`、經 `migrate::mqtt_trust::validate` 檢查、寫進 broker 的信任目錄、送 SIGHUP。同一個檔案的兩個測試共用 Postgres，以鎖一次跑一個。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -p openapi -- --check
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
CI=1 cargo test --locked --workspace --exclude agent --no-fail-fast
CI=1 cargo test --locked -p certs --test gateway_revocation --test dynsec -- --nocapture --test-threads 1
```

## 結果

| 指令 | 在哪裡、哪個 commit | 結果 |
| --- | --- | --- |
| 「指令」一節全部 | 建置主機，`fc288711` | 全部通過：fmt、clippy、`openapi --check`、`api:check`、typecheck、lint；前端 218 個測試通過；workspace 248 個測試執行檔、1960 個測試、0 失敗（[checks.txt](data/2026-10-30-636-gateway-revocation/checks.txt)：輸出只留摘要，其中幾行 `... ok` 是名稱含 `error` 或 `panic` 的測試被篩選帶出）；`gateway_revocation` 1 個、`dynsec` 2 個通過 |
| `cargo clippy --locked -p certs --all-targets -- -D warnings` 與「指令」一節最後一行 | 建置主機，`22e4653b` | 通過；`gateway_revocation` 1 個、`dynsec` 2 個通過（[certs-gateway-revocation-dynsec.txt](data/2026-10-30-636-gateway-revocation/certs-gateway-revocation-dynsec.txt)） |

證據行（`--nocapture`，`22e4653b`）：

- **哪些 dynsec 指令會斷線**（Mosquitto 2.1.2，已連線、有角色的閘道器，下指令後發布兩次）：`removeClientRole`、`addClientRole`、`disableClient`、`deleteClient` 都斷線，`DISCONNECT reason 0x98`；`enableClient` 不斷線。停用之後閘道器以憑證重連，發布照樣得到 0x10：停用擋不住以憑證登入的閘道器。
- **N**：`gw-636-revoked revoked through the second replica: 200 after 7.218026ms; its open connection ended 9.402158ms after the request (DISCONNECT reason 0x98)`。測試要求小於 README 的 N（30 秒）。
- **順序與副本**：第二個副本的日誌 `revoked a gateway's certificates`（第 84 行）、`issued a new CRL` cause=revocation（第 93 行）、`changed a gateway's broker permissions` action=removed（第 103 行）；第一個副本沒有任何變更，兩個副本各只有連線時的一次完整比對；回應時 `crl.pem` 已列出它。
- **M 的兩段**：broker 載入新 CRL 之前 `CONNACK 0x00, PUBACK 0x87`；載入之後 `CONNACK: received fatal alert: CertificateRevoked`，另一台照常發布（0x10）。
- **換發**：不寫原因的撤銷之後換發成功，`action=added`，新憑證發布 0x10。再以 `key_compromise` 撤銷：斷線（0x98），換發 `403 {"code":"permission_denied","message_key":"certificate_renewal.key_compromised"}`，簽發紀錄 2 張前後不變，broker 沒有 client。
- **競態**：測試拿著這台閘道器的鎖，換發通過讀取後等鎖，期間提交 keyCompromise 撤銷，換發得到同樣的 403，簽發紀錄 1 張前後不變。
- **API**（`tests/gateway_revocation.rs`）：14 個被拒絕的請求（401 兩種、403 三種、400 四種、422 兩種、404 兩種、409 一種）前後，簽發紀錄 6 列與兩個 CA 的 CRL 編號都不變；成功時兩個 CA 各一張還有效的憑證以 `key_compromise` 撤銷，CRL 編號各從 1 到 2 並列出它們，先前的撤銷與過期的憑證不變；稽核行帶 `requested_by`、`user_id`、硬體 ID、序號；再撤銷一次 409。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 1. 已連線的閘道器在撤銷後 N 秒內被斷開，N 寫在 README（`CI=1 cargo test -p certs`） | 通過。README 的 N 是正常時 30 秒以內；兩個副本、請求送到不是做同步的那一個時，量到 9.4 毫秒（`fc288711` 那一輪 8.7 毫秒），`DISCONNECT 0x98` |
| 2. broker 載入新 CRL 之前重連 TLS 通過、發布 0x87；載入之後 TLS 交握得到 `CertificateRevoked` | 通過 |
| 3. 簽出新憑證（模擬換發）後同步補回角色，閘道器以新憑證連上並能發布 | 通過（不寫原因的撤銷之後）。以 `key_compromise` 撤銷的閘道器不能換發，這是安全審查之後的決定，見 README「撤銷與換發」 |
| 4. 沒有 token 401；租戶管理員、`act` token 403；沒登錄的硬體 ID 404；全部已撤銷 409；錯的 Content-Type 400；都不改紀錄 | 通過 |
| 5. `cargo run -p openapi -- --check`、`bun run --cwd frontend api:check`、`bun run --cwd frontend test`、`cargo test -p certs --test cross_tenant`、`cargo clippy --workspace --all-targets -- -D warnings` | 通過（`cross_tenant` 在 workspace 測試裡） |

待測（延後的進階驗證，不在這個 issue）：叢集上經 mqtt-bridge 量 N 與 M、lab 的閘道器實際撤銷、重複的故障注入。
