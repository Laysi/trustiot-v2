# MQTT broker 伺服器憑證：叢集內簽發、只給 broker、到期前輪替（2026-10-24）

Laysi/trustiot-v2#535：broker 的伺服器憑證改由 chart 的 CronJob `<release>-mqtt-broker-certificate` 在叢集內產生金鑰、經 certs 服務的 broker 埠（8082）簽發，寫進 chart 宣告的空 Secret `<release>-mqtt-broker-tls`，效期過半時以新金鑰換發，broker 由 trust sidecar 送 SIGHUP 換上。做法、兩個決定（Opaque 的空 Secret 與 broker 在 Secret 填好之前的狀態；CronJob 自己的 ServiceAccount、第一次簽發也由 CronJob 做）、效期與換發時間、換發時閘道器會不會斷線、根儀式之前的狀態，都在 [deploy/chart/README.md](../../deploy/chart/README.md) 的「MQTT broker」「伺服器憑證」；簽發端點在 [services/certs/README.md](../../services/certs/README.md) 的「broker 伺服器憑證」；私鑰的保管在 [docs/architecture/secrets.md](../architecture/secrets.md) 的「broker 伺服器憑證私鑰」。

這份報告是四條驗收條件的證據：下列自動化測試在建置主機上以 `CI=1` 執行（容器測試不略過）。驗收條件 4 與驗收條件 2 在叢集裡的部分由 nightly 驗證，見最後的「nightly（待補）」。

## 環境

- **程式碼**：分支 `feat/535-broker-server-certificate`。
- **建置主機**（經檢查佇列）：x86_64 Linux，`CI=1`；需要容器的測試以 rootless podman 起拋棄式的 TLS Postgres（`crates/testkit`）與 Mosquitto（`docker.io/library/eclipse-mosquitto:2.1.2-alpine`，以 digest 釘選）。Rust 1.99.0（`rust-toolchain.toml`），bun 1.4.2、helm 4.3.0、kubeconform 0.8.0（`.prototools` 釘選的版本）。
- **工作站**：gitleaks（`.prototools` 釘選的版本）。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p migrate -p certs -p contract --no-fail-fast
cargo deny --locked check
bun test deploy/chart
bun test deploy/flux
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/check-workflows.ts
```

驗收證據的測試可以單獨重跑：`cargo test -p certs --test broker_certificate -- --nocapture`（印出下面引用的證據行；沒有 podman 或 docker 時略過，設 `CI` 時失敗）、`cargo test -p migrate --test mqtt_certificate --test mqtt_trust --test mqtt_broker -- --nocapture`、`bun test deploy/chart -t "MQTT broker"`、`bun test scripts/nightly/broker-certificate.test.ts`。

## 結果

### 驗收條件 1 與 3：簽發、提供、驗證與一次輪替

`services/certs/tests/broker_certificate.rs`：chart 的 CronJob 執行的 `migrate mqtt-broker-certificate`（`converge_at`）對真的 certs 服務（`TRUSTIOT__BROKER__VALIDITY=1h`，服務接受的最短效期）與一個只有這個 Secret 的 Kubernetes API 替身；Mosquitto 以 chart 的閘道器 listener（`deploy/chart/files/mosquitto/gateway-listener.conf`：MQTT 5、TLS 1.3、`use_username_as_clientid`）提供 Secret 裡的 `tls.crt` 與 `tls.key`；用戶端是 rustls，信任錨只有根 CA（README 寫的信任錨），以閘道器憑證連線。輸出在 [broker-certificate.txt](data/2026-10-24-535-broker-server-certificate/broker-certificate.txt)（容器網路的位址改成 `<client>`）。

| 步驟 | 結果 |
| --- | --- |
| 第一次執行（Secret 是 chart 宣告的空 Secret） | `the Secret holds no certificate yet`：寫入一次，綁定 uid、代 `1`；憑證給 `mqtt-broker.trustiot.internal` 與 `trustiot-mqtt-broker.trustiot.svc`，notAfter − notBefore 是 3600 秒加往前的 300 秒，`tls.crt` 是憑證再接簽發 CA |
| 信任根、名稱 `mqtt-broker.trustiot.internal` | 驗過，序號與 Secret 裡的相同，CONNACK 成功（TLSv1.3、`TLS_CHACHA20_POLY1305_SHA256`） |
| 信任根、名稱 `wrong-name.trustiot.internal` | 用戶端拒絕：`InvalidCertificate(NotValidForNameContext { … })` |
| 信任別的 CA | 用戶端拒絕：`InvalidCertificate(UnknownIssuer)` |
| 效期過半前 60 秒（換發程式的時鐘） | 保留同一張憑證，沒有寫入 |
| 效期過半時 | `half of the certificate's validity has passed`：新序號、新金鑰，第二次寫入 |
| 送 SIGHUP 後的新連線 | 驗到新序號（第 1 次嘗試） |
| 重載前建立的連線 | 回 PINGRESP，沒有斷線 |
| 之後以實際時間執行 | 保留新憑證，沒有寫入 |
| broker 改提供別的 CA 為同一個名稱簽的憑證並重載 | 信任根的用戶端拒絕：`InvalidCertificate(UnknownIssuer)` |
| certs 服務與 Mosquitto 的日誌 | 沒有 `PRIVATE KEY`，也沒有兩把私鑰 PEM 的任何一行 |

換發程式的其他規則（別人建立或弄壞的 Secret、寫入時被改過、certs 服務還不能簽：只有 chart 宣告、從沒填過的空 Secret 以 exit code 0 等待，該換發、已過期或被清空的憑證都讓執行失敗、回應不能用、輸出沒有私鑰）在 `tools/migrate/tests/mqtt_certificate.rs`；trust 容器在 Secret 填好後才放進閘道器 listener、只重啟 broker 一次、不對沒在聽 8883 的 broker 送 SIGHUP 在 `tools/migrate/tests/mqtt_trust.rs`；chart 的整份 Mosquitto 設定沒有伺服器憑證時照常服務 1883、不開 8883 在 `tools/migrate/tests/mqtt_broker.rs`。

### 驗收條件 2：私鑰只在 broker 掛載的 Secret

`bun test deploy/chart` 的 “MQTT broker”：

- `the server certificate: a Secret the chart declares empty, mounted whole and read-only by the broker pod alone, written by the certificate CronJob alone (Laysi/trustiot-v2#535)`：Secret 是空的 Opaque、`helm.sh/resource-policy: keep`；只有 broker 的 pod 掛載（`mosquitto`、`trust-files`、`trust`，唯讀，沒有 subPath）；其他 pod 都不提到它，CronJob 只在設定裡寫它的名稱、不掛任何 Secret、沒有 `secretKeyRef`；只有 CronJob 的 Role 提到它。
- `custom env of a service can read neither of the broker's Secrets`：服務的自訂 `env` 以 `secretKeyRef` 讀 `tls.key` 時 chart 拒絕渲染。
- `the certificate CronJob: its own ServiceAccount and a Role on the server certificate Secret alone, a projected token, no Secret mounted, nobody connects to it`：Role 只有這個 Secret 的 `get` 與 `patch`。
- `%s: the broker's certificate CronJob alone reaches the certs service's broker-certs port, which the certs Service leaves out`：兩種形態的 AuthorizationPolicy 與 NetworkPolicy 只放行 CronJob 的 ServiceAccount。
- `the certificate CronJob renders only with the broker and the certs service; its name, values and schedule are checked`。

日誌與端點：換發程式只記序號、簽發 CA 的 key id、名稱與效期（`tools/migrate/tests/mqtt_certificate.rs` 的 `the_command_prints_neither_the_key_nor_the_certificate`）；certs 服務的端點只回公開的憑證鏈，日誌與端點不含私鑰由上表最後一列與 `services/certs/tests/key_custody.rs` 涵蓋。叢集裡的 `kubectl auth can-i` 檢查在 nightly（下方）。

### 驗收條件 4：8082 的安全關卡（Istio ambient）

chart 層的放行規則由上面的 chart 測試證明；在 ambient mesh 裡實際呼叫的檢查寫在 `scripts/nightly/broker-certificate.ts`（`checkBrokerCertificate`，nightly 在 certs 的 CA 儀式之後執行），它的解析、判斷與對渲染結果的設計檢查由 `scripts/nightly/broker-certificate.test.ts` 在沒有叢集的情況下測過。叢集裡的結果見下一節。

### 其他檢查

（填入最後一輪的結果）

## nightly（待補）

工作站與建置主機不跑 Kubernetes，所以以下在 PR 帶 `smoke` label 的 nightly smoke（k3d、Istio ambient）第一次執行後補上結果與 run 連結。每份 values 在 certs 的 CA 儀式之後：

1. 從渲染結果確認設計：一個 CronJob、在 ambient mesh、自己的 ServiceAccount（不是 broker 的）、只掛投射的 token、一個容器、不掛不讀任何 Secret；它寫的 Secret 就是 broker 以 `server-tls` 掛的那一個，其他 workload 都不掛不讀。
2. 以 `kubectl create job --from=cronjob/trustiot-mqtt-broker-certificate` 執行一次：Job 成功，代表 CronJob 的 pod 在 mesh 裡連到 `trustiot-certs-broker:8082` 並拿到憑證（驗收條件 4 的正面）。
3. Secret：Opaque、綁定自己、代與 CronJob 相同、只有 `tls.crt` 與 `tls.key`；`tls.crt` 是給兩個名稱、只有 serverAuth、由這次執行的簽發 CA 簽的憑證，再接簽發 CA（只讀 `tls.crt`，`tls.key` 不離開叢集）。
4. broker 的 pod 在 300 秒內開始聽 8883，`mosquitto` 容器最多重啟一次。
5. 從 broker 自己的 pod（mesh 裡的另一個 workload，API 埠的 `ca.pem` 前後都是 200）POST 到 8082 拿不到 HTTP 回應（驗收條件 4 的負面）。
6. namespace 的每個 ServiceAccount 以 `kubectl auth can-i --as` 問：只有 CronJob 的能 `get` 與 `patch` 這個 Secret（驗收條件 2 的叢集部分）。

結果：待補（PR 的 nightly smoke run 連結與兩種形態的報告行）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 1. broker 以簽出的憑證提供 inner TLS；用戶端以 README 的信任錨與名稱驗過；錯的名稱或不是我們 CA 簽的憑證被拒（測試） | 通過（`broker_certificate.rs`） |
| 2. 私鑰只在 broker 掛載的 Secret：其他 pod 不掛、自訂 `env` 讀不到、簽發 workload 的 Role 只碰這個 Secret（`bun test deploy/chart`）；日誌與端點不含私鑰 | 通過（chart 測試、`mqtt_certificate.rs`、`broker_certificate.rs`）；叢集裡的 `can-i` 檢查待 nightly |
| 3. 到期前輪替，量測一次：短效期、broker 換上新憑證、新連線驗到它（測試） | 通過（`broker_certificate.rs`，效期 1 小時、過半時換發） |
| 4. Istio ambient 的安全關卡前提：簽發 workload 連得到 `<release>-certs-broker:8082` 並拿到憑證，其他 workload 被拒 | 待補（nightly，見上一節）；chart 層的規則與 nightly 檢查的邏輯已測 |
