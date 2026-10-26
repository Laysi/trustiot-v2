# MQTT broker 伺服器憑證：叢集內簽發、只給 broker、到期前輪替（2026-10-24）

Laysi/trustiot-v2#535：broker 的伺服器憑證改由 chart 的 CronJob `<release>-mqtt-broker-certificate` 在叢集內產生金鑰、經 certs 服務的 broker 埠（8082）簽發，寫進 chart 宣告的空 Secret `<release>-mqtt-broker-tls`，效期過半時以新金鑰換發，broker 由 trust sidecar 送 SIGHUP 換上。做法、兩個決定（Opaque 的空 Secret 與 broker 在 Secret 填好之前的狀態；CronJob 自己的 ServiceAccount、第一次簽發也由 CronJob 做）、效期與換發時間、換發時閘道器會不會斷線、根儀式之前的狀態，都在 [deploy/chart/README.md](../../deploy/chart/README.md) 的「MQTT broker」「伺服器憑證」；簽發端點在 [services/certs/README.md](../../services/certs/README.md) 的「broker 伺服器憑證」；私鑰的保管在 [docs/architecture/secrets.md](../architecture/secrets.md) 的「broker 伺服器憑證私鑰」。

這份報告是四條驗收條件的證據：下列自動化測試在建置主機上以 `CI=1` 執行（容器測試不略過）。驗收條件 4 與驗收條件 2 在叢集裡的部分由 nightly 驗證，見最後的「nightly」。

## 環境

- **程式碼**：分支 `feat/535-broker-server-certificate`：功能的 commit 9dc4cae7、合併 main（0dc8fd96）的 b86e34aa、修正兩個測試的 b3233160、改寫私鑰 PEM 邊界寫法的 5cdbc138，以及依安全審查修正的 commit（這份報告與它在同一個 commit）。下面「指令」的完整一輪在前三個 commit 的樹上執行；之後兩個 commit 在各自的樹上重跑的檢查列在「推送前的 secret 掃描」與「安全審查的修正」。
- **建置主機**（經檢查佇列）：x86_64 Linux，`CI=1`；需要容器的測試以 podman 起拋棄式的 TLS Postgres（`crates/testkit`）與 Mosquitto（`docker.io/library/eclipse-mosquitto:2.1.2-alpine`，以 digest 釘選）。Rust 1.99.0（`rust-toolchain.toml`），bun 1.4.2、helm 4.3.0、kubeconform 0.8.0（`.prototools` 釘選的版本）。
- **工作站**：gitleaks（`.prototools` 釘選的版本）。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=1 cargo test --locked -p migrate -p certs -p contract -p repo-check --no-fail-fast
cargo run --locked -p openapi -- --check
cargo deny --locked check
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts
bun test deploy/chart
bun test deploy/flux
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/check-workflows.ts
bun scripts/ci/lint-workflows.ts
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

換發程式的其他規則（別人建立或弄壞的 Secret、寫入時被改過、certs 服務還不能簽：只有 chart 宣告、從沒填過（沒有資料，也沒有填入時寫的 annotation）的空 Secret 以 exit code 0 等待，該換發、已過期的憑證與填過之後被清空的 Secret 都讓執行失敗、回應不能用、輸出沒有私鑰）在 `tools/migrate/tests/mqtt_certificate.rs`；trust 容器在 Secret 填好後才放進閘道器 listener、只重啟 broker 一次、不對沒在聽 8883 的 broker 送 SIGHUP、Secret 不能用時拿掉 listener 並在修好後放回，在 `tools/migrate/tests/mqtt_trust.rs`；chart 的整份 Mosquitto 設定沒有伺服器憑證時照常服務 1883、不開 8883 在 `tools/migrate/tests/mqtt_broker.rs`。

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

最後一輪，2026-10-24，建置主機，上方「指令」依序執行，全部 exit 0：

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 沒有差異 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 沒有警告 |
| `CI=1 cargo test --locked -p migrate -p certs -p contract -p repo-check --no-fail-fast` | 44 個測試目標，317 通過，0 失敗，0 略過（容器測試以 `CI=1` 執行，不能略過） |
| `cargo run --locked -p openapi -- --check` | 與提交的 `api/openapi.json` 相同 |
| `cargo deny --locked check` | advisories、bans、licenses、sources 都 ok |
| `bun run --cwd scripts typecheck` | 沒有錯誤 |
| `bun test scripts` | 1697 通過，9 略過（需要 podman 端到端、arm64 執行檔或測試 pod 映像的測試，與這個變更無關），0 失敗 |
| `bun test deploy/chart` | 652 通過，0 失敗 |
| `bun test deploy/flux` | 77 通過，0 失敗 |
| `bun scripts/ci/check-secret-handling.ts` | 525 個檔案，沒有違規 |
| `bun scripts/ci/check-workflows.ts` | 4 個 workflow，沒有違規 |
| `bun scripts/ci/lint-workflows.ts` | 3 個 workflow，沒有錯誤 |

這一輪之前，完整的測試抓到兩個這個分支自己的問題，已在同一個 PR 修正：`services/certs/tests/key_custody.rs` 掃描 `tools/migrate/src` 裡把私鑰變成位元組的程式碼，CronJob 寫 `tls.key` 的那一處需要審查後列入清單；`tools/migrate/tests/mqtt_broker.rs` 原本比對閘道器埠關閉時用戶端的錯誤文字，libmosquitto 依非阻塞 connect 失敗的時機回 `Connection refused` 或 `Bad file descriptor`，改成以 trust sidecar 的 `listens_on` 讀容器的 socket 表（1883 在聽、8883 沒有）；同一個測試在故意被拒的 TLS 握手之後檢查日誌沒有問題行，順序改成先檢查日誌再做被拒的連線。

證據檔是 b3233160 的程式碼在建置主機上重跑 `broker_certificate.rs` 的輸出；之後的 commit 沒有改這個測試走的流程（安全審查的修正讓換發程式多檢查 certs 回應的簽發 CA，真的 certs 服務的回應照樣通過），`broker_certificate.rs` 在下面兩輪的 `cargo test -p certs` 都通過。

### 推送前的 secret 掃描

在工作站以 `docs/architecture/secrets.md`「公開 repo 的資訊規則」的兩處掃描檢查。第一次掃描分支的 commit 範圍時，gitleaks 的 `private-key` 規則在功能的 commit（9dc4cae7）報了一筆：`tools/migrate/src/mqtt_certificate.rs` 把 CronJob 執行時產生的私鑰包成 PEM 的程式碼，BEGIN 與 END 兩行是字面字串，中間是組 base64 的迴圈，形狀像一把 PEM 私鑰；原始碼裡沒有金鑰。處理方式：

- 邊界改由常數 `PKCS8_KEY_LABEL` 組出，tree 不再符合這條規則，輸出的位元組不變（單元測試 `the_key_is_pkcs8_pem_and_matches_the_request` 仍以字面文字比對兩條邊界，並以 rcgen 解析出同一把金鑰）。
- CI 會掃 PR 的每個 commit，後面的 commit 清不掉前面 commit 的發現，所以 9dc4cae7 的那一筆依 `.gitleaksignore` 對其他只在分支上的 commit 的慣例，附理由列入。

改完之後在建置主機重跑，全部 exit 0：`cargo fmt --all --check` 沒有差異；`cargo clippy --locked --workspace --all-targets -- -D warnings` 沒有警告；`CI=1 cargo test --locked -p migrate -p repo-check --no-fail-fast` 26 個測試目標，181 通過，0 失敗，0 略過；`cargo test --locked -p certs --test key_custody` 3 通過。兩處掃描的結果在 PR 描述（掃描的對象包含這份報告所在的 commit）。

### 安全審查的修正

2026-10-24 對 5cdbc138 的獨立安全審查沒有 Critical 或 High；私鑰保管、RBAC 與兩種形態的網路政策的說法都成立。依驗證分級，Medium 與 Low 的修正不另加邊界測試組，只更新既有的測試：

| 項目 | 修正 | 測試 |
| --- | --- | --- |
| M1（Medium）：Secret 在能用之後變得不能用時，閘道器 listener 留在 emptyDir（容器重啟時還在，init container 也不再跑），broker 容器一重啟就讀不到憑證而反覆停止，1883 跟著中斷 | init container 與 sidecar 在 Secret 不能用時拿掉 `listeners/gateway.conf`，執行中的 broker 不受影響。修好後放回：broker 還在聽 8883 就送 SIGHUP，重啟過、沒有 8883 的再送一次 SIGTERM | `tests/mqtt_trust.rs`：init container 拿掉之前留下的 listener；sidecar 在私鑰不是一對時拿掉、修好後放回並只送 SIGHUP；Secret 被清空時拿掉、不送訊號，broker 重啟後 Secret 再填好時送一次 SIGTERM |
| M2（Medium）：填過的 Secret 被清空或重建成空的，certs 又不能簽時 Job 以 0 結束 | 只有沒有資料、也沒有填入時寫的 uid 或代 annotation 的 Secret 能等待，其他空的 Secret 讓 Job 失敗。Helm 重新宣告的 Secret 沒有 annotation，CronJob 分不出來，README 寫明這時看 sidecar 的錯誤日誌，並新增「監看」段落 | `src/mqtt_certificate.rs` 的單元測試：清空的、只有其中一個 annotation 的、uid 不同的空 Secret 都是 unusable；`tests/mqtt_certificate.rs`：清空之後，以及從清空的複本重建之後，certs 回 503 時執行失敗、Secret 不變 |
| L3（Low）：Secret 不能用的期間，CRL 的變更也不重新載入 | 每一輪擋下信任檔的變更時記錯誤；README 的失敗情況寫明撤銷的閘道器憑證在這段期間仍被接受 | 只有日誌，沒有新測試 |
| L4（Low）：`check_chain` 只檢查鏈本身是否一致 | 也檢查葉憑證不是 CA、notBefore 最多比現在晚 300 秒（certs 往前的量）；certs 的回應還要求鏈的第二張是回應的 `ca_key_id` 指的簽發 CA。`ca_key_id` 依 `crates/ca-keys` 的定義是 SubjectPublicKeyInfo 的 SHA-256，不是憑證的 subject key identifier 擴充欄位，所以比對簽發 CA 的 SPKI 雜湊 | 單元測試加上 CA 葉憑證、尚未生效、別的簽發 CA 的 `ca_key_id` 三種情況；真的 certs 服務的回應照樣通過（`services/certs/tests/broker_certificate.rs`） |
| Info 7：升級說明 | README 寫明刪掉舊 Secret 到 CronJob 填入之間閘道器連不上，以及 M1 的行為 | 文件 |

只記錄、不修改：L5（`include_dir` 讓 trust sidecar 能放進 Mosquitto 的設定；與既有的共用 process namespace、同一個 uid 是同一個信任範圍，接受現狀）；L6（main 上的 `internalIdentity.keyRotationSchedule` 接受 `60 * * * *` 這類排程，延到 Laysi/trustiot-v2#660）。

修正之後在建置主機重跑（2026-10-24，`CI=1`）。除了第一次的 fmt，全部 exit 0：

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 第一次：新增的測試行有排版差異（exit 1）；照 rustfmt 的輸出改好後沒有差異 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 沒有警告 |
| `CI=1 cargo test --locked -p migrate -p certs -p repo-check --no-fail-fast` | 37 個測試目標，226 通過，0 失敗，0 略過 |
| `bun test deploy/chart` | 652 通過，0 失敗 |
| `bun test scripts` | 1697 通過，9 略過（與上面相同的那 9 個），0 失敗 |

排版改好後在最後的樹上重跑：`cargo fmt --all --check` 沒有差異；`cargo clippy --locked -p migrate --all-targets -- -D warnings` 沒有警告；`CI=1 cargo test --locked -p migrate --no-fail-fast` 22 個測試目標，152 通過，0 失敗，0 略過。排版只動測試的換行，其他檢查沿用上表。

## nightly

工作站與建置主機不跑 Kubernetes，所以以下由 PR 帶 `smoke` label 的 nightly smoke（k3d、Istio ambient）驗證。每份 values 在 certs 的 CA 儀式之後：

1. 從渲染結果確認設計：一個 CronJob、在 ambient mesh、自己的 ServiceAccount（不是 broker 的）、只掛投射的 token、一個容器、不掛不讀任何 Secret；它寫的 Secret 就是 broker 以 `server-tls` 掛的那一個，其他 workload 都不掛不讀。
2. 以 `kubectl create job --from=cronjob/trustiot-mqtt-broker-certificate` 執行一次：Job 成功，代表 CronJob 的 pod 在 mesh 裡連到 `trustiot-certs-broker:8082` 並拿到憑證（驗收條件 4 的正面）。
3. Secret：Opaque、綁定自己、代與 CronJob 相同、只有 `tls.crt` 與 `tls.key`；`tls.crt` 是給兩個名稱、只有 serverAuth、由這次執行的簽發 CA 簽的憑證，再接簽發 CA（只讀 `tls.crt`，`tls.key` 不離開叢集）。
4. broker 的 pod 在 300 秒內開始聽 8883，`mosquitto` 容器最多重啟一次。
5. 從 broker 自己的 pod（mesh 裡的另一個 workload，API 埠的 `ca.pem` 前後都是 200）POST 到 8082 拿不到 HTTP 回應（驗收條件 4 的負面）。
6. namespace 的每個 ServiceAccount 以 `kubectl auth can-i --as` 問：只有 CronJob 的能 `get` 與 `patch` 這個 Secret（驗收條件 2 的叢集部分）。

結果（2026-10-26，PR Laysi/trustiot-v2#663 的 head `f2c55af0`，nightly run [40352104018](https://github.com/Laysi/trustiot-v2/actions/runs/40352104018) 的 `smoke` job，k3d、Istio ambient）：兩種形態都通過。報告行如下（序號省略）：

- onprem（namespace `nightly-onprem`）：`trustiot-mqtt-broker-certificate ran once as trustiot-mqtt-broker-certificate (Job trustiot-mqtt-broker-certificate-smoke) and succeeded within 4 s; Secret trustiot-mqtt-broker-tls (empty before the run): Opaque, bound to itself, generation 1, only tls.crt,tls.key; tls.crt: serial … for mqtt-broker.trustiot.internal, trustiot-mqtt-broker.nightly-onprem.svc, serverAuth only, signed by the issuing CA, then the issuing CA, until Oct 28 15:54:09 2026 GMT; the broker listened on 8883 after 57 s more (mosquitto restarts 0 -> 1); from the broker's pod ca.pem on the API port answered 200 before and after, /internal/certs/v1/broker-certificate on the broker port got no answer: command terminated with exit code 1; of 15 ServiceAccounts only trustiot-mqtt-broker-certificate may get and patch trustiot-mqtt-broker-tls (63 can-i answers)`
- saas（namespace `nightly-saas`）：同上，名稱是 `trustiot-mqtt-broker.nightly-saas.svc`，效期到 Oct 28 15:59:08 2026 GMT，broker 在 51 秒後開始聽 8883，其餘相同。

對應上面的六項：

1. 渲染結果與設計相符（檢查在 `checkBrokerCertificate` 執行 Job 之前做，不符時整個 smoke 失敗）。
2. Job 成功，代表 CronJob 的 pod 在 mesh 裡連到 `trustiot-certs-broker:8082` 並拿到憑證（驗收條件 4 的正面）。
3. Secret 在執行前是空的，執行後是 Opaque、綁定自己、generation 1、只有 `tls.crt` 與 `tls.key`；`tls.crt` 給兩個名稱、只有 serverAuth、由簽發 CA 簽，再接簽發 CA。
4. broker 在 57 秒與 51 秒後開始聽 8883，`mosquitto` 重啟一次。
5. 從 broker 自己的 pod：API 埠的 `ca.pem` 前後都是 200，broker 埠的 `/internal/certs/v1/broker-certificate` 拿不到回應（驗收條件 4 的負面）。
6. 15 個 ServiceAccount 共 63 個 `can-i` 回答，只有 CronJob 的能 `get` 與 `patch` 這個 Secret（驗收條件 2 的叢集部分）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 1. broker 以簽出的憑證提供 inner TLS；用戶端以 README 的信任錨與名稱驗過；錯的名稱或不是我們 CA 簽的憑證被拒（測試） | 通過（`broker_certificate.rs`） |
| 2. 私鑰只在 broker 掛載的 Secret：其他 pod 不掛、自訂 `env` 讀不到、簽發 workload 的 Role 只碰這個 Secret（`bun test deploy/chart`）；日誌與端點不含私鑰 | 通過（chart 測試、`mqtt_certificate.rs`、`broker_certificate.rs`）；叢集裡的 `can-i` 檢查也通過（nightly run 40352104018） |
| 3. 到期前輪替，量測一次：短效期、broker 換上新憑證、新連線驗到它（測試） | 通過（`broker_certificate.rs`，效期 1 小時、過半時換發） |
| 4. Istio ambient 的安全關卡前提：簽發 workload 連得到 `<release>-certs-broker:8082` 並拿到憑證，其他 workload 被拒 | 通過（nightly run 40352104018，兩種形態，見「nightly」）；chart 層的規則與 nightly 檢查的邏輯另有測試 |
