# certs 服務：MQTT broker 伺服器憑證的 profile 與簽發端點（2026-10-23）

Laysi/trustiot-v2#557：certs 服務以簽發 CA 為 MQTT broker 簽 TLS 伺服器憑證（profile `broker`，EKU 只有 serverAuth，名稱 `mqtt-broker.trustiot.internal` 加上 broker 的 Service 名稱），經只有 broker 連得到的埠 8082 簽發；簽發 CA 還沒有第一張 CRL 時不簽。名稱、信任錨、效期與端點在 [services/certs/README.md](../../services/certs/README.md) 的「broker 伺服器憑證」，mesh 的放行規則在 [deploy/chart/README.md](../../deploy/chart/README.md) 的「服務間授權（mesh）」與「伺服器憑證」。這份報告是四條驗收條件的證據，都由下列自動化測試在建置主機上以 `CI=1` 實際執行（容器測試不略過）。

## 環境

- **程式碼**：分支 `feat/557-broker-cert-endpoint`，建在 main `de994059` 之上（這次的變更是它之上的提交）。
- **建置主機**（經檢查佇列）：x86_64 Linux，`CI=1`；需要容器的測試以 rootless podman 起拋棄式的 TLS Postgres（`crates/testkit`、`crates/service-kit`）與 Mosquitto（`docker.io/library/eclipse-mosquitto:2.1.2-alpine`，以 digest 釘選）。Rust 1.99.0（`rust-toolchain.toml`），bun 1.4.2、helm 4.3.0、kubeconform 0.8.0（`.prototools` 釘選的版本）。建置主機在 2026-10-23 重新安裝過，這一輪是空的建置目錄從頭建出。
- **工作站**：`cargo fmt --all --check` 與 gitleaks 8.30.1。
- **時間**：完整檢查 2026-10-23 14:24 排入佇列，14:39 結束（Asia/Taipei）。

> **暫定的結果**：建置主機在這一輪結束後約 14:40 檔案系統轉為唯讀、隨後連不上（主機的虛擬磁碟損壞），這一輪執行時磁碟可能已經在出問題，所以下面的結果都先當作暫定，合併前要在正常的建置主機或 CI 再跑一次。另外，這一輪之後 chart 改過一處（Codex 審查指出：`<release>-certs-broker` 的 Service 埠原本跟著容器埠，改了容器埠時 broker 呼叫的 8082 就錯了；現在 Service 固定 8082，以埠名轉到容器埠，並新增 chart 測試 `the broker-certs Service stays on 8082 when the container port changes, and the policies follow the container port`）。這個修正只在工作站上以 `helm template` 渲染看過一次，**`bun test deploy/chart` 還沒有在修正之後跑過，一定要重跑**。

## 指令

在 repo 根目錄，一次佇列工作依序執行（輸出在 [checks.txt](data/2026-10-23-557-broker-cert-endpoint/checks.txt)，去掉了編譯進度的行）：

```sh
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --no-fail-fast
bun test deploy/chart
bun scripts/ci/check-secret-handling.ts
bun install --cwd scripts --frozen-lockfile
bun test scripts/ci/changes.test.ts
bun run --cwd scripts typecheck
```

驗收證據的測試可以單獨重跑：`cargo test -p certs --test broker -- --nocapture`（沒有 podman 或 docker 時容器測試略過，設 `CI` 時失敗），以及 `bun test deploy/chart -t "broker-certs"`。

## 結果

### 建置主機：全部通過（暫定，見「環境」）

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check`（工作站） | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test --locked --workspace --no-fail-fast` | 1245 通過、0 失敗、0 略過；包含 `repo-check`、`openapi`（`committed_spec_is_what_the_code_generates`，端點是內部的，不在 OpenAPI）與 migration 的測試 |
| `cargo test -p certs`（同一輪） | 單元測試 14、`broker` 5、`ceremony` 3、`crl` 2、`cross_tenant` 1、`issuance` 5、`key_custody` 3、`workstation` 4，全部通過 |
| `bun test deploy/chart` | 616 通過、0 失敗（Service 埠修正之前；修正與新測試之後還沒跑） |
| `bun scripts/ci/check-secret-handling.ts` | `508 file(s) checked, 6 container build file(s), no violations` |
| `bun test scripts/ci/changes.test.ts` | 282 通過、0 失敗 |
| `bun run --cwd scripts typecheck`（含 `deploy/chart/tests/`） | 通過 |

`gitleaks git --redact --log-opts="origin/main..HEAD"` 與匯出的整棵樹的 `gitleaks dir --redact` 都沒有發現。`Cargo.lock` 與第三方依賴沒有變動，所以沒有另跑 `cargo deny`。

### 驗收條件 1：伺服器憑證不能當閘道器的用戶端憑證

- `services/certs/tests/broker.rs` 的 `mosquitto_serves_the_broker_certificate_and_refuses_it_from_a_client`（容器，真的 certs 行程）：certs 經 broker 埠簽出伺服器憑證，Mosquitto 2.1.2 以它當 8883 的伺服器憑證，`cafile` 是服務發布的 `ca.pem`、`crlfile` 是 `crl.pem`、`require_certificate true`、`use_identity_as_username true`，與 chart 的 broker 相同。以這張伺服器憑證當用戶端憑證連線時被拒：用戶端看到 `ssl/tls alert unsupported certificate`，broker 記 `certificate verify failed`，沒有 CONNACK。同一個 broker 接受同一個簽發 CA 簽的閘道器憑證（對照，見條件 2）。
- 同一個檔案的 `the_ca_decides_the_broker_profile_and_tls_checks_its_name_and_purpose` 以 rustls（webpki）驗用戶端憑證：閘道器憑證通過，伺服器憑證被拒（`InvalidPurpose`）。反過來，閘道器憑證當伺服器憑證也被拒（`InvalidPurpose`）。閘道器 id 不能是 `mqtt-broker.trustiot.internal`（大小寫不同也不行，`ReservedGatewayId`）。
- profile 由 CA 決定：CSR 要求了別的 CN、DNS SAN（含萬用字元）與其他延伸欄位，簽出的憑證主體只有 `CN=mqtt-broker.trustiot.internal`，延伸欄位固定為 basicConstraints（critical，`cA=FALSE`）、keyUsage（critical，只有 digitalSignature）、extendedKeyUsage（只有 serverAuth）、subjectAltName（只有設定的兩個 DNS 名稱）、subject 與 authority key identifier；公鑰是 CSR 的，效期 30 天加往前的 5 分鐘。`a_broker_request_without_proof_of_possession_or_with_another_key_is_refused`：簽章不符的 CSR（`BadSignature`）、P-384 的 CSR 與不是 CSR 的資料都被拒。

### 驗收條件 2：用戶端以 README 的信任錨與名稱驗證；別的名稱或別的 CA 被拒

Mosquitto 測試的四次連線（輸出見 [mosquitto.txt](data/2026-10-23-557-broker-cert-endpoint/mosquitto.txt)；用戶端是 `mosquitto_pub`（OpenSSL），以 `--cafile` 指定信任錨，連線位址以 `--add-host` 對到 broker）：

| 用戶端的信任錨與名稱 | 結果 |
| --- | --- |
| 根 CA（README 的信任錨）、`mqtt-broker.trustiot.internal` | 接受：TLSv1.3，`received CONNACK (0)`，broker 以 CN `gw-557-accepted` 當使用者名稱 |
| 根 CA、別的名稱 | 拒絕：`certificate verify failed`，沒有 CONNACK |
| 別的 CA 的根、`mqtt-broker.trustiot.internal` | 拒絕：`certificate verify failed`，沒有 CONNACK |
| 伺服器憑證當用戶端憑證（條件 1） | 拒絕：`ssl/tls alert unsupported certificate` |

mosquitto.txt 取自這個測試第一次在建置主機上執行（同一天、變更 rebase 到目前的 main 之前）：連線結果與上表相同，那一次失敗只是因為測試對最後一列預期的錯誤字串寫錯（預期 OpenSSL 本機的用途錯誤，實際上用戶端收到的是 broker 送的 alert），修正預期的字串之後，上面的完整檢查裡這個測試通過。

rustls（webpki）在 `the_ca_decides_the_broker_profile_and_tls_checks_its_name_and_purpose`：以根為信任錨、broker 送的簽發 CA 為中繼時，兩個名稱都通過；`wrong-name.trustiot.internal`、`trustiot.internal`、`mqtt-broker.trustiot.internal.example`、CSR 裡偽造的名稱、`broker.example.invalid` 與 IP `192.0.2.1` 都以 `NotValidForName` 被拒。信任別的根時我們的憑證被拒，信任我們的根時別的 CA 為同一個名稱簽的憑證被拒（帶不帶我們的簽發 CA 都一樣），都是 `UnknownIssuer`；別的 CA 的憑證在它自己的根之下有效（對照）。

### 驗收條件 3：端點只接受 broker，拒絕其他服務；端點與日誌不含私鑰

- **只有 broker 連得到**（`bun test deploy/chart`，兩種部署型態都測）：`the broker alone reaches the certs service's broker-certs port, which the certs Service leaves out`。broker 埠 8082 有自己的 Service `<release>-certs-broker`，certs 的 Service 不含這個埠；對 certs pod 的所有 AuthorizationPolicy 加總，能到 8082 的只有 mqtt-broker 的 principal；NetworkPolicy 只放行 broker 的 pod 連 HBONE 的 15008（ambient 下 ztunnel 經它轉送）；certs 的環境變數 `TRUSTIOT__BROKER__LISTEN_ADDR` 與 `TRUSTIOT__BROKER__SERVICE_NAME` 與埠一致。新增的 `the broker-certs Service stays on 8082 when the container port changes, and the policies follow the container port` 把容器埠改成 9443：Service 仍是 8082 並以埠名轉送，服務聽 9443，AuthorizationPolicy 比對 9443，certs 自己的 policy 仍只到 8080（還沒執行，見「環境」；工作站上 `helm template` 渲染的結果與它的斷言一致）。每個服務的 policy 測試確認其他服務只到 certs 的 API 埠（8080），沒有 broker 時 `certs-broker` 不放行任何人。渲染失敗的表格另有五種：服務取名 `certs-broker`、開啟 broker 卻沒有 `broker-certs` 埠、certs 只有 `broker-certs` 埠、入口路由指向 `broker-certs` 埠或身分服務的 token exchange 埠。叢集裡實際連線的端對端測試是待測，見「結論」。
- **端點本身**（`the_broker_port_issues_the_broker_certificate_and_nothing_else`，容器，真的 certs 行程）：broker 埠 POST CSR 回 201、`cache-control: no-store`，`certificate_chain` 是伺服器憑證接著簽發 CA 憑證，鏈到根；簽發紀錄多一列 `profile = broker`。沒有或錯的 `Content-Type`、PEM 回 415，PEM 當 pkcs10、不是 CSR、P-384 回 422，超過 4096 bytes 回 413，這些都沒有寫入簽發紀錄，回應不含 PEM。API 埠沒有這個路徑（404），broker 埠也只有它（`/`、`/healthz`、`/readyz`、`ca.pem`、`crl.pem` 與工作站 API 都 404，GET 這個路徑 405）。
- **私鑰**：broker 的私鑰在 broker 這一側產生，certs 只收 CSR。`tests/key_custody.rs` 的 `no_private_key_leaves_through_logs_endpoints_or_telemetry` 現在也經 broker 埠簽一張 broker 憑證，並以 GET 讀 broker 埠的 `/`、`/internal/certs/v1/keys` 與 `/internal/certs/v1/issuing-ca-keys.json`；服務的日誌、這些回應與遙測都不含簽發 CA 的私鑰。端點測試另外確認簽發的 INFO 日誌 `issued the MQTT broker's server certificate` 帶序號與金鑰 id，不含憑證或任何 PEM。`only_reviewed_code_turns_a_private_key_into_bytes` 照常通過（`src/broker.rs` 沒有私鑰序列化）。

### 驗收條件 4：沒有第一張 CRL 時拒絕簽發

`no_broker_certificate_before_the_issuing_ca_has_its_first_crl`（容器）：在測試行程裡以 `certs::broker::routes` 起端點，不跑 CRL 工作。簽發 CA 還沒有 CRL 時 POST 回 503，回應不含 PEM，簽發紀錄 0 列；寫入這個簽發 CA 的第一張 CRL 之後，同一個請求回 201，簽發紀錄 1 列。

### 文件

- `services/certs/README.md`：新增「broker 伺服器憑證」（名稱與信任錨、比較過的名稱做法、欄位表、效期 30 天與過半換發、簽發端點與回應表，以及「雲端服務以我們 CA 的用戶端憑證連 broker」不適用的說明：Laysi/trustiot-v2#84 以 dynamic security 帳密加 mesh mTLS）；「端點」「設定」「資料庫」「測試」「已知的限制」同步，已知的限制改為依 Laysi/trustiot-v2#556 的決定（根 CRL，Laysi/trustiot-v2#560）。
- `deploy/chart/README.md`：broker 埠的 Service 與 policy、呼叫矩陣的 `certs-broker` 列、伺服器憑證的端點與 SAN。
- `docs/architecture/secrets.md`：broker 伺服器憑證私鑰那一列寫明在 broker 的 pod 產生、certs 只收 CSR。
- `docs/roadmap/milestones.md`：broker 伺服器憑證的鏈加上 Laysi/trustiot-v2#557 與 Laysi/trustiot-v2#556，記下 Laysi/trustiot-v2#535 的拆分。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 伺服器憑證不能當閘道器的用戶端憑證（Mosquitto 與 rustls） | 通過 |
| 用戶端以 README 的信任錨與名稱驗證通過；別的名稱或別的 CA 被拒 | 通過（OpenSSL 與 rustls） |
| 端點只接受 broker、拒絕其他服務（`bun test deploy/chart`）；端點與日誌不含私鑰 | 通過（chart 的 policy 規則與渲染）；Service 埠修正之後的 `bun test deploy/chart` 要重跑；叢集裡由其他工作負載實際連 8082 被拒是待測，見下 |
| 沒有第一張 CRL 時拒絕簽發 | 通過 |

待測：在有 Istio ambient 的叢集裡，以 broker 以外的工作負載實際連 `<release>-certs-broker:8082` 被拒、broker 連得到。驗收條件要求的是 `bun test deploy/chart` 的證據，叢集實測屬於進階驗證，依組織的驗證分級延後；nightly 目前還沒有啟用 certs 服務（services/certs/README.md 的「映像」）。
