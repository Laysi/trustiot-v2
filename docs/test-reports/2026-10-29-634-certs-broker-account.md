# chart：certs 以 service:certs 連上 broker，與服務取用 broker 帳號的共用 helper（2026-10-29）

Laysi/trustiot-v2#634：預設 values 的 `mesh.calls.certs` 加上 `mqtt-broker`；`templates/_mqtt-broker.tpl` 的 `trustiot.mqttBroker.serviceCredentials` 只把 Secret `<release>-mqtt-broker-accounts` 裡服務自己的 key 以目錄唯讀掛進它的 pod，並以服務的 `mqtt.*` 設定給 broker 位址與帳號（做法見 [deploy/chart/README.md](../../deploy/chart/README.md)「MQTT broker」的「服務的帳號」）。這份報告是 chart 測試部分的驗收證據；部署後的檢查是 PR 的 `smoke` nightly（見「結論」）。

## 環境

- **程式碼**：分支 `feat/634-certs-mqtt-account`，以 main `dcc603bf` 為基礎。
- **機器**：`bun test deploy/chart` 與 `bun run --cwd scripts typecheck` 經檢查佇列在建置主機（macOS，arm64）以 `CI=1` 執行；`bun test deploy/flux`、`bun test scripts/nightly`、`bun test scripts/ci/changes.test.ts`、`bun scripts/ci/check-secret-handling.ts` 與下方渲染結果在工作站（x86_64 Linux）執行。
- **工具**：`.prototools` 釘選的 Helm 4.3.0、Bun 1.4.2、kubeconform。
- 不需要叢集：chart 的規則以 `helm template` 的渲染結果檢查。

## 指令

在 repo 根目錄：

```sh
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test deploy/chart
bun test deploy/flux
bun test scripts/nightly
bun test scripts/ci/changes.test.ts
bun scripts/ci/check-secret-handling.ts
```

本 issue 的測試都在 `deploy/chart/tests/chart.test.ts` 的 `MQTT broker (decision 6, Laysi/trustiot-v2#84)` 底下，可以單獨重跑：`bun test deploy/chart -t "Laysi/trustiot-v2#634"`，以及 `bun test deploy/chart -t "a new configuration, account holder or password generation restarts the broker"`。

渲染結果（[rendered.json](data/2026-10-29-634-certs-broker-account/rendered.json)）來自兩份 values 各一次：

```sh
cd deploy/chart
helm template trustiot . --namespace trustiot --values values-saas.yaml --values tests/values/saas-instance.yaml \
  --set mqttBroker.enabled=true --set services.ingest.enabled=true --set services.gateway-ops.enabled=true \
  --set services.mqtt-bridge.enabled=true --set services.certs.enabled=true
# on-prem: values-onprem.yaml and tests/values/onprem-instance.yaml instead
```

從中取出 certs 的 `TRUSTIOT__MQTT__*`、`mqtt-broker-account` 的 volume 與掛載、有這個 Secret 的 volume 的 pod、其他帳號持有者的 `TRUSTIOT__MQTT__*`、broker 的 AuthorizationPolicy 與 NetworkPolicy，以及 init container 的 `TRUSTIOT__ACCOUNTS__SERVICES`。

## 結果

| 指令 | 結果 |
| --- | --- |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun test deploy/chart` | 687 pass、0 fail（8419 個 expect） |
| `bun test deploy/flux` | 78 pass、0 fail |
| `bun test scripts/nightly` | 381 pass、0 fail |
| `bun test scripts/ci/changes.test.ts` | 283 pass、0 fail |
| `bun scripts/ci/check-secret-handling.ts` | 537 個檔案，沒有違規 |

新增與修改的 chart 測試：

| 測試 | 檢查 |
| --- | --- |
| `saas`／`onprem: certs gets its broker account: its own key alone, mounted as a directory, ...` | certs 的 `TRUSTIOT__MQTT__HOST`、`PORT`（1883）、`USERNAME`（`service:certs`）、`PASSWORD_FILE`；volume 只選 `certs` 這個 key、0440、不是 optional；只有主容器以目錄唯讀掛載、沒有 `subPath`，init container 沒有；整份渲染裡只有 certs 的 pod 有這個 Secret 的 volume，沒有任何 env 以 `secretKeyRef` 讀它；ingest、gateway-ops、mqtt-bridge 沒有 broker 設定；broker 的 AuthorizationPolicy 1883 規則有 certs 的身分、8883 規則沒有；NetworkPolicy 放行 certs 的 pod（只到 15008）；只有 broker 的 Role 指到這個 Secret |
| `saas`／`onprem: without the broker, or without mqtt-broker in mesh.calls.certs, ...` | broker 關閉（只開 certs、或開所有服務）與 certs 沒有列 `mqtt-broker` 時，certs 沒有 broker 設定、沒有任何 pod 有這個 Secret 的 volume |
| `saas`／`onprem: custom env cannot set the broker settings of certs, ...` | broker 開或關，`services.certs.env` 與 `serviceDefaults.env` 寫 `TRUSTIOT__MQTT__HOST`、`PORT`、`USERNAME`、`PASSWORD_FILE`（不分大小寫）都渲染失敗；跳過 schema 時由模板的規則擋下並指出原因。`TRUSTIOT__DYNSEC__INTERVAL` 仍可設定 |
| `the helper hands every account holder its own key and nobody else's` | chart 的副本把 ingest、gateway-ops 也加進 `trustiot.mqttBroker.credentialServices`：三個 pod 各自只掛自己的 key、各自的 `service:<服務>`，沒有 `subPath`；ingest 不列 `mqtt-broker` 時它什麼都沒有；把 mqtt-bridge 或 assets 加進清單時渲染失敗 |
| `a new configuration, account holder or password generation restarts the broker, and nothing else`（修改） | 調大 `passwordGeneration` 或改記憶體上限時 broker 重啟，certs、ingest、gateway-ops、mqtt-bridge 的 pod template 不變；拿掉 certs 的 `mqtt-broker` 時 `checksum/mqtt-broker-accounts` 改變（加上時反之），certs 的 pod template 也改變；certs 的 pod annotation 只有 `checksum/platform-config` |
| `custom env of a service can read neither of the broker's Secrets`（修改） | 多檢查以 `secretKeyRef` 讀 `certs` 這個 key 也渲染失敗 |
| `rendered manifests carry no secret value, ...`（修改） | 名稱像密鑰的變數不能有字面值；唯一的例外是 `_FILE` 結尾、值是同一個容器以目錄掛的 Secret volume 裡的一個檔案路徑（`TRUSTIOT__MQTT__PASSWORD_FILE`） |

渲染結果摘要（[rendered.json](data/2026-10-29-634-certs-broker-account/rendered.json)，兩份 values 相同）：

- certs：`TRUSTIOT__MQTT__HOST=trustiot-mqtt-broker.trustiot.svc`、`TRUSTIOT__MQTT__PORT=1883`、`TRUSTIOT__MQTT__USERNAME=service:certs`、`TRUSTIOT__MQTT__PASSWORD_FILE=/etc/trustiot/mqtt-broker-account/certs`；volume `mqtt-broker-account` 是 `trustiot-mqtt-broker-accounts` 的 `items: [{key: certs, path: certs}]`、`defaultMode: 288`（0440）；掛在 `/etc/trustiot/mqtt-broker-account`、`readOnly: true`。
- 有這個 Secret 的 volume 的 pod：只有 `Deployment/trustiot-certs`。
- broker 的 AuthorizationPolicy：8883 只有 `trustiot-mqtt-bridge`，1883 是 `trustiot-certs`、`trustiot-gateway-ops`、`trustiot-ingest`；init container 的 `TRUSTIOT__ACCOUNTS__SERVICES` 是 `certs,gateway-ops,ingest`。

另外：`bun test deploy/flux` 與 `bun test scripts/nightly` 在 macOS 的建置主機上各有與本變更無關的失敗（README 的 shell 指令與 LibreSSL 的 `openssl` 行為），在 Linux（CI 的平台）上全部通過；本變更沒有動 `deploy/flux` 與 `scripts/`。

## 安全審查後的修正

2026-10-29 的安全審查（以 `d86fdfe9` 為準）沒有 Critical 或 High，有下面四項，都在下一個 commit 修正：

- **M1**：values 裡指向 Secret 的欄位沒有比對受保護的 Secret，例如 `ai.providers.anthropic.apiKeySecret.name` 設成 `<release>-mqtt-broker-accounts`、key 設成 `certs`，就會把 dynsec 管理員的密碼交給 ai-gateway，當成 API key 送給 AI 供應商。修正：`templates/_helpers.tpl` 新增 `trustiot.values.checkSecretRefs`，由 `templates/values-check.yaml` 呼叫。下列欄位有名稱、而且名稱在 `trustiot.dependencySecretNames` 裡時渲染失敗，訊息寫出欄位名稱：`ingress.direct.tlsSecret`、`internalIdentity.signingKeySecret.name`、`postgres.external.caSecret.name`、`clickhouse.bundled.adminSecret.name`、`clickhouse.external.caSecret.name`、`qdrant.external.credentialsSecret.name`、`objectStorage.credentialsSecret.name`、`smtp.credentialsSecret.name`、`ai.providers.<供應商>.apiKeySecret.name`、`observability.sentry.dsnSecret.name`、`observability.collector.forward.authHeader.valueSecret.name`、`observability.collector.forward.caSecret.name`。外部 Postgres 與 ClickHouse 的 `migrationSecret.name` 本身受保護，所以只比對 chart 自己的 Secret（`trustiot.dependencySecretNames` 拆成 `trustiot.chartSecretNames` 與 `trustiot.migrationSecretNames`，合起來與原本相同）。`global.imagePullSecrets` 不在其中：只有 kubelet 讀它，容器看不到。
- **L2**：`trustiot.dependencySecretNames` 的註解還寫 broker 的 Secret 只有 broker 的 pod 使用，已改為 broker 的 pod 與每個帳號持有者的 pod（只掛自己的 key，經 `trustiot.mqttBroker.serviceCredentials`）。
- **L1**（只改文件）：broker 開啟後，certs 第一次拿到帳號或 accounts Secret 重建之後，certs 的新 pod 在 broker 的 init container `accounts` 寫入 key 之前停在 `FailedMount`。`docs/runbooks/secrets.md`「MQTT broker 的服務帳號密碼」新增排查步驟（`FailedMount` 事件、broker 的 `accounts` 日誌、調大 `passwordGeneration`），`deploy/chart/README.md`「服務的帳號」也寫明這個依賴。改成 `optional`、缺檔時 certs 照常啟動的做法與叢集上的實測另開 Laysi/trustiot-v2#736。
- **I1**：`deploy/chart/README.md`「服務的帳號」補一句：覆寫 `services.certs.podSecurityContext` 拿掉 `fsGroup` 時，certs 讀不到密碼檔，啟動時就結束而 CrashLoop。

新增的 chart 測試（`deploy/chart/tests/chart.test.ts`，MQTT broker 底下）：

| 測試 | 檢查 |
| --- | --- |
| `every Secret field of the values is in the list the chart checks against its protected Secrets` | `values.yaml` 裡每個以 `Secret` 結尾的欄位都在測試的清單 `PROTECTED_SECRET_FIELDS` 裡，清單也沒有多的；非 AI、非 migration 的欄位在 `_helpers.tpl` 的檢查裡逐一列名。之後新增指向 Secret 的欄位而沒有加進檢查時，這個測試失敗 |
| `<欄位> cannot name a protected Secret, so no pod but certs gets the broker account's key`（每個欄位一個，共 15 個） | 兩份 values、broker 與其帳號持有者都開、加上讓這個欄位生效的設定（例如 ai-gateway 開啟並有角色、collector 轉送開啟、外部資料庫）：欄位指向自己的 Secret 時渲染成功、渲染結果用到它，而且整份渲染只有 certs 的 pod 有 accounts Secret 的 volume；欄位指向 `<release>-mqtt-broker-accounts` 或 `<release>-certs-ca-keys` 時渲染失敗，訊息寫出欄位名稱與 Secret 名稱 |
| `saas`／`onprem: an external database's CA cannot share the migration account's Secret` | 外部 Postgres 與 ClickHouse 的 CA 指向各自的 migration Secret 時渲染失敗 |

修正後的結果（經檢查佇列在建置主機（macOS）以 `CI=1` 執行，`check-secret-handling` 另在工作站（x86_64 Linux）跑過一次，結果相同）：

| 指令 | 結果 |
| --- | --- |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun test deploy/chart` | 705 pass、0 fail（8619 個 expect；原本 687 個加上新增的 18 個） |
| `cargo test --locked -p repo-check` | 28 pass、0 fail |
| `bun scripts/ci/check-secret-handling.ts` | 538 個檔案，沒有違規 |

相容性：外部 Postgres 或 ClickHouse 的 CA 原本可以和 migration 帳號放在同一個 Secret（CA 以 `items` 只掛 `ca.crt`），現在渲染失敗，要把 CA 放進另一個 Secret。兩份 values 與測試用的 values 都沒有這樣設定。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 兩份 values 的 certs pod 只掛 `certs` 這個 key、設了 broker 位址與帳號；其他 pod 沒有掛這個 Secret；自訂 `env` 覆寫時渲染失敗；broker 關閉時 certs 沒有這些設定 | 通過（上表前三個測試，兩份 values 各一次） |
| 另一個服務也列 `mqtt-broker` 時，helper 只給它自己的 key | 通過，但做法與 issue 的「以測試用的 values」不同：服務要在程式讀得懂 broker 設定時才加進 `trustiot.mqttBroker.credentialServices`（否則 `deny_unknown_fields` 讓它啟動失敗），values 改不了這份清單，所以測試在 chart 的副本裡把 ingest 與 gateway-ops 加進清單再渲染，就是 Laysi/trustiot-v2#642、Laysi/trustiot-v2#93 之後要做的改動 |
| 這個 Secret 的掛載都不用 `subPath` | 通過（每個掛載都檢查 `subPath`） |
| AuthorizationPolicy 的 1883 規則有 certs、8883 沒有；NetworkPolicy 放行 certs 連 15008 | 通過 |
| 加上 certs 後 `checksum/mqtt-broker-accounts` 改變；調大 `passwordGeneration` 時 certs 的 pod template 不變 | 通過 |
| 帶 `smoke` label 的 nightly 部署（兩份 values）全部 workload 就緒、沒有 CrashLoopBackOff | 待測：要在 PR 加上 `smoke` label 後由 CI 執行，結果記在 issue |
| README 與 secrets.md 已更新；`bun test deploy/flux` 通過 | 通過（另更新 docs/runbooks/secrets.md 的輪替步驟與 services/certs/README.md） |
