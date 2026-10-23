# TrustIoT Helm chart

SaaS 與地端用同一份 chart（決策 1、5）：`values-saas.yaml` 與 `values-onprem.yaml` 描述兩種部署型態，客戶自有的 k8s 或 OpenShift 也用這份 chart。本文件列出每個 values 欄位，以及 chart 與服務之間的約定。

## 指令

helm、kubeconform 與 bun 的版本鎖在 repo 根目錄的 `.prototools`，先執行 `proto install`。以下指令都在 repo 根目錄執行。

```sh
# 全部檢查：兩份 values 的 helm lint 與 kubeconform、chart 規則、本文件的欄位清單
bun test deploy/chart

# 只跑名稱含指定字串的檢查
bun test deploy/chart -t "kubeconform"

# 個別執行（地端把 saas 換成 onprem）。一定要帶實例覆寫檔：只帶型態檔時，必填欄位（hosts.*、
# 叢集內 Postgres 與 ClickHouse 的 storage.size 等）沒有值，schema 驗證就失敗
helm lint deploy/chart --strict -f deploy/chart/values-saas.yaml -f deploy/chart/tests/values/saas-instance.yaml
helm template trustiot deploy/chart -n trustiot -f deploy/chart/values-saas.yaml -f deploy/chart/tests/values/saas-instance.yaml > saas.yaml
kubeconform -strict -summary -kubernetes-version 1.36.0 saas.yaml

# 安裝：叢集要先裝好平台元件（deploy/istio 與 deploy/cloudnative-pg 的 README），
# 第三個 -f 是這個安裝實例的覆寫檔（見下一節）
helm install trustiot deploy/chart -n trustiot --create-namespace -f deploy/chart/values-saas.yaml -f <覆寫檔>
```

`bun test` 以 Kubernetes 1.30.0（`Chart.yaml` 的 `kubeVersion` 下限）與 1.36.0（實驗機與 SaaS 的 k3s）各跑一次 kubeconform；第一次執行要下載 schema，需要網路。

proto 的 shim 偶爾會把自己的訊息寫進 stdout（例如另一個 proto 程序持有鎖時；在 agent 環境還會以 NDJSON 輸出），會混進 `helm template` 產生的清單。自動化流程（CI、nightly、腳本）請以 `proto bin helm --reporter text` 取得執行檔路徑後直接執行，`bun test deploy/chart` 就是這樣做。

## values 的三層

後面的層覆寫前面的層：

| 層 | 檔案 | 內容 |
| --- | --- | --- |
| 1 | `values.yaml` | 所有欄位的預設值 |
| 2 | `values-saas.yaml` 或 `values-onprem.yaml` | 部署型態，也就是 SaaS 與地端的差異 |
| 3 | 安裝實例的覆寫檔 | 這一個安裝的主機名稱、外部端點、Secret 名稱與叢集內資料庫的磁碟區大小（`postgres.bundled.storage.size`、`clickhouse.bundled.storage.size`）。SaaS 的放在 `deploy/flux/`（Laysi/trustiot-v2#70）；地端由客戶安裝時提供；`tests/values/` 有測試用的範例 |

`values.schema.json` 驗證合併後的結果：缺欄位、拼錯欄位、以 IP 當主機名稱、在 values 放明文密鑰、SaaS 的外部端點不是 https，都會讓 `helm lint`、`helm template` 與 `helm install` 直接失敗，不會裝出壞掉的版本。values 裡的字串、map key 與清單項目都不能含換行（CR、LF，以及 YAML 也當成換行的 NEL、LS、PS）：`templates/values-check.yaml` 在渲染時拒絕，略過 schema 驗證也一樣，因為印進 manifest 某一行的值不能開出新的一行或新的文件（docs/architecture/secrets.md，Laysi/trustiot-v2#398）。

## SaaS 與地端的差異

| 欄位 | `values-saas.yaml` | `values-onprem.yaml` | 依據 |
| --- | --- | --- | --- |
| `global.deploymentMode` | `saas` | `onprem` | 決策 1 |
| `ingress.exposure` | `tunnel`：入口 gateway 只有 ClusterIP、聽 80 的 HTTP，由主機上的 cloudflared 轉入，主機不開埠；NetworkPolicy 只放行 `ingress.tunnel.allowedSources`（本機）連 80 與 15021 | `direct`：入口 gateway 以 `ingress.direct.serviceType`（預設 LoadBalancer）在客戶網路開 443，以 `ingress.direct.tlsSecret` 終止外層 TLS | 決策 27 |
| 可信的來源 IP（由 `ingress.exposure` 推導，不能單獨設定） | `CF-Connecting-IP` | 入口 gateway 附加在 X-Forwarded-For 最右段的位址 | 決策 6、27 |
| `qdrant.mode` | `external`：Qdrant Cloud | `bundled`：叢集內自架 | 決策 19 |
| `objectStorage.*` | 託管的 S3 或 R2 | 客戶的 S3 相容儲存（細節未定） | 決策 20 |
| `smtp.*` | 我們的郵件服務 | 客戶的 SMTP | 決策 18 |
| `ai.*` | 雲端 API | 客戶的 OpenAI 相容端點，例如 vLLM、Ollama | 決策 15、16、26 |
| `observability.sentry.enabled` | `true`，送 sentry.io | `false`，客戶有自己的 Sentry 時可開 | 決策 24 |
| `observability.collector.forward.enabled` | `false`：日誌與 trace 只存在我們的 ClickHouse | `false`，客戶要求時在覆寫檔開啟，把遮蔽後的日誌（可加 trace）以 OTLP 轉送到客戶的 SIEM | 決策 22 |

兩種型態相同的部分：Postgres（含 Apache AGE，CloudNativePG 管理）與 ClickHouse 都在叢集內（`bundled`），規格也相同；MQTT broker 不直接對外，閘道器一律以 443 的 WebSocket 經入口 gateway 與 mqtt-bridge 連到 broker；cookie 驗證、例外路徑與標頭規則相同（見「入口 gateway」）；chart 產生的 Service 一律是 ClusterIP（測試強制），唯一對外的是 Istio 依 Gateway 產生的入口 Service。SaaS 在 Cloudflare 無法使用時，可改用 `ingress.exposure: direct` 直接對外，閘道器只改 DNS（docs/roadmap/risks.md）。

## 元件與開關

加入元件的 issue 要同時更新這張表。

| 元件 | 開關 | 2026-10-10 的狀態 | 後續 issue |
| --- | --- | --- | --- |
| Rust 服務（見下一節） | `services.<name>.enabled` | 通用樣板完成；映像還沒出，預設全關 | 各服務的 issue 出映像時改成開 |
| 入口 gateway（Istio ambient、Gateway API、ext_authz） | `ingress.*` | 完成：Gateway、路由、cookie 驗證、例外路徑、`/internal` 拒絕與 NetworkPolicy；服務的 pod 加入 ambient mesh。Istio 本身與 ext_authz provider 由 `deploy/istio` 安裝。服務的 cookie 路由隨各服務加入 `ingress.routes` | Laysi/trustiot-v2#62；真身分服務的端到端驗證在 Laysi/trustiot-v2#77 |
| 服務間授權（AuthorizationPolicy、NetworkPolicy） | `mesh.*` | 完成：每個服務自己的 ServiceAccount、依 `mesh.calls` 產生的 AuthorizationPolicy、namespace 預設拒絕的 NetworkPolicy；不部署 waypoint。服務之間的呼叫隨各服務加入 `mesh.calls`，兩個服務之間的呼叫也是代表使用者的呼叫（身分服務依同一份清單換 token，`TRUSTIOT_ON_BEHALF_CALLS`），矩陣見「呼叫矩陣」 | Laysi/trustiot-v2#63、Laysi/trustiot-v2#106 |
| Postgres（CloudNativePG 加 AGE） | `postgres.mode` | `bundled`：單一 instance 的 CloudNativePG Cluster、AGE extension 與 NetworkPolicy（見「叢集內的 Postgres」）；CloudNativePG operator 與 CRD 是叢集層級的平台元件，由 [deploy/cloudnative-pg](../cloudnative-pg/README.md) 安裝，不在 chart 裡。`external` 已接到平台設定。含 AGE 的映像在 [deploy/images/postgres](../images/postgres/README.md)，由 release.yml 發佈。每個服務的角色與 migration 由 migration Job 處理 | Laysi/trustiot-v2#134（備份與 PITR） |
| migration Job | `migrate.enabled` | 完成（預設開）：每個服務的 Postgres migration、ClickHouse DDL、服務角色與 collector 的 ClickHouse 帳號；每個服務的 pod 以 init container 等它完成才啟動（見「migration Job」） | Laysi/trustiot-v2#60；第一個 outbox 生產端在 Laysi/trustiot-v2#65，每租戶的 AGE 角色在 Laysi/trustiot-v2#176 |
| ClickHouse | `clickhouse.mode` | `bundled`：單節點 StatefulSet，bootstrap 管理帳號的密碼在叢集內產生（見「叢集內的 ClickHouse」）；`external` 已接到平台設定。schema、帳號與 row policy 由 migration Job 以 SQL 套用 | 管理帳號密碼的輪替見 docs/runbooks/secrets.md |
| MQTT broker（Mosquitto） | `mqttBroker.enabled` | 完成（預設關，只有 nightly 的 `tests/values/smoke.yaml` 開啟）：單一副本的 StatefulSet，閘道器埠 8883（TLS 1.3、只信任我們的 CA、CRL）只放行 mqtt-bridge，服務埠 1883 只放行有 broker 帳號的服務；dynamic security 預設拒絕，服務的密碼在叢集內產生；SQLite 持久化（見「MQTT broker」） | 伺服器憑證的簽發與輪替在 Laysi/trustiot-v2#535，閘道器帳號的登記與撤銷在 Laysi/trustiot-v2#82 |
| Qdrant | `qdrant.mode` | `external` 已接到平台設定；`bundled` 尚未加入 | Laysi/trustiot-v2#194 |
| 物件儲存、SMTP、AI 端點 | `objectStorage.*`、`smtp.*`、`ai.*` | 一律外部，設定已接到平台設定 | Laysi/trustiot-v2#133、Laysi/trustiot-v2#169、Laysi/trustiot-v2#139 |
| OpenTelemetry collector 與 OTLP 轉送 | `observability.collector.enabled` | 完成（預設開）：收服務的 OTLP、遮蔽後把日誌與 trace 寫進 ClickHouse 的 `trustiot_logs`，可轉送到 SIEM（見「OpenTelemetry collector」）。帳號由 migration Job 以 SQL 建立；指標先收下丟棄 | Laysi/trustiot-v2#183（指標存放與 Grafana）、Laysi/trustiot-v2#63（服務間授權要放行 collector） |
| Grafana | — | 尚未加入 | Laysi/trustiot-v2#183 |
| Sentry | `observability.sentry.*` | 開關、環境名稱與 DSN 已接到每個服務 | Laysi/trustiot-v2#184 |
| 內部身分（公鑰、簽章私鑰） | `internalIdentity.*` | 完成：金鑰對由身分服務 pod 的 init container 在叢集內產生，私鑰在只有身分服務掛載的 Secret，公鑰在每個服務都掛載的 ConfigMap，服務不重啟就換上新公鑰；金鑰超過 `maxKeyAgeSeconds`（預設 90 天）由 CronJob 自動輪替，人發起的輪替與撤銷只改一個數字；身分服務另外拿到需要 cookie 的路由（`TRUSTIOT_INGRESS_ROUTES`），決定 token 的受眾 | Laysi/trustiot-v2#61、Laysi/trustiot-v2#404、Laysi/trustiot-v2#429 |
| 閘道器憑證 CA（簽發 CA 私鑰、CSR、根與簽發 CA 憑證） | `certificateAuthority.*`（隨 `services.certs.enabled`） | 完成：簽發 CA 私鑰由 certs pod 的 init container 在叢集內產生，在只有 certs 掛載的 Secret；CSR 寫進 ConfigMap 給離線根 CA 簽發，簽出的憑證（公開）放在 values，服務不重啟就換上（見「閘道器憑證 CA」） | Laysi/trustiot-v2#78；broker 取用 CA 憑證與 CRL 見「MQTT broker」（Laysi/trustiot-v2#84），nightly 與展示環境啟用 certs 另開 issue |
| 密鑰 | 各個 `*Secret` 欄位、`services.<name>.credentials` | values 只存 Secret 名稱與 key；Sentry DSN 給每個服務，其他依賴的帳密只給列出它的服務；外部 ClickHouse 的 CA 掛進每個服務，Postgres 的 CA 只掛進在 `mesh.calls` 列出 `postgres` 的服務；chart 不渲染任何密鑰值（測試檢查），它宣告的 Secret 只有 migration Job 填入的空 Secret；叢集內 Postgres 的 Secret 由 CloudNativePG 產生，服務的自訂 `env` 讀不到；每個服務的資料庫密碼、telemetry-query 的 ClickHouse 服務帳號密碼與 collector 的 ClickHouse 密碼由 migration Job 產生，各放在自己的 Secret（`<release>-<服務>-password`，telemetry-query 的 ClickHouse 密碼在它的同一個 Secret；`<release>-otel-collector-password`），只給該服務或 collector，migration 帳號只給 migration Job | SaaS 的 Secret 以 SOPS 加密放在 `deploy/flux/`（Laysi/trustiot-v2#70）；地端由客戶建立。規則與清冊見 docs/architecture/secrets.md |

## 服務

`services` 的 key 與 docs/architecture/deployment.md 的服務表一致（測試比對），物件名稱是 `<release>-<key>`；release 名稱不含 `trustiot` 時是 `<release>-trustiot-<key>`；超過 63 個字元時縮短前段、保留 `-<key>`，不同服務不會撞名。每個服務產生一個 ServiceAccount、一個 ClusterIP Service 與一個 Deployment。

`web`、`identity`、`assets`、`certs`、`ingest`、`telemetry-query`、`gateway-ops`、`mqtt-bridge`、`assistant`、`ai-gateway`、`alerts`、`field-context`、`knowledge`

`web` 提供 SPA 的靜態檔（決策 2，Laysi/trustiot-v2#123）：開啟後 `hosts.app` 上 `/api`、`/auth` 以外的路徑都由它回應，不經 cookie 檢查，也拿不到 cookie 與身分標頭；不能有 cookie 路由（見「入口 gateway」的「SPA 的靜態檔」）。

`mqtt-bridge` 把閘道器的 WebSocket（`ingress.publicPaths.mqtt`）原封不動轉成 TCP 送到 broker 的閘道器埠（決策 6，Laysi/trustiot-v2#85，services/mqtt-bridge/README.md）。`mqttBroker.enabled` 時 chart 設定 `TRUSTIOT__BROKER__ADDRESS`（`<release>-mqtt-broker.<namespace>.svc:8883`），broker 沒開時不設，服務拒絕啟動；不論 broker 開不開，`services.mqtt-bridge.env` 與 `serviceDefaults.env` 都不能設定它（渲染失敗），轉接服務只連叢集內的 broker。記錄的來源 IP 依 `TRUSTIOT_CLIENT_IP_SOURCE`：目前只支援 `tunnel`（`cf-connecting-ip`），其他 `ingress.exposure`（`x-forwarded-for-rightmost`）在 Laysi/trustiot-v2#446 之前拒絕啟動。渲染不因此失敗，因為 e2e 與「每個服務都開」的渲染測試要在 onprem 形態開啟它。`mesh.calls` 不能把 `mqtt-bridge` 列為被呼叫端（渲染失敗）：只有入口 gateway 連得到它，否則呼叫端能自己設可信的來源 IP 標頭，也能經它連到 broker 的閘道器埠。

## values 欄位

「必填」指 schema 要求非空值。Secret 欄位一律是既有 Secret 的名稱與 key，名稱空白代表不設定。

### global、hosts、ingress

| 欄位 | 預設值 | 說明 |
| --- | --- | --- |
| `global.deploymentMode` | `""`，必填 | `saas` 或 `onprem`，由兩份 values 設定 |
| `global.imagePullSecrets` | `[]` | 拉映像用的既有 Secret 名稱（`kubernetes.io/dockerconfigjson`），例如 SaaS 的 GHCR 唯讀拉取 token |
| `hosts.app` | `""`，必填 | SPA 與 API 的主機名稱，同一網域（決策 2） |
| `hosts.gateway` | `""`，必填 | 閘道器連線的主機名稱：443 上包在 WebSocket 裡的 MQTT 與 HTTPS 憑證換發 |
| `hosts.ota` | `""`，必填 | 閘道器下載 OTA 的主機名稱 |
| `ingress.exposure` | `""`，必填 | `tunnel` 或 `direct`，見上一節；地端只能是 `direct` |
| `ingress.authzProvider` | `trustiot-identity` | istiod `meshConfig.extensionProviders` 裡呼叫身分服務的 ext_authz provider 名稱，由 `deploy/istio` 設定；chart 只以名稱引用 |
| `ingress.replicas` | `2` | 入口 gateway 的副本數，1–10；大於 1 時另有 `minAvailable: 1` 的 PodDisruptionBudget |
| `ingress.resources.requests.cpu` | `50m` | 入口 gateway（`istio-proxy` 容器）的資源需求 |
| `ingress.resources.requests.memory` | `64Mi` | 2026-10-10 實測閒置約 46–54 MiB |
| `ingress.resources.limits.cpu` | `"2"` | |
| `ingress.resources.limits.memory` | `512Mi` | 必填；1,000 條閘道器連線時兩個副本合計約多 94 MiB |
| `ingress.tunnel.allowedSources` | `[10.42.0.1/32]` | `tunnel` 時唯一能連入口 pod 的來源（IPv4 CIDR，至少 /8）：本機，而且只能連 listener 的 80 與 Istio 的狀態埠 15021。k3s 的主機以 cni0 連 pod，位址是節點 pod CIDR 的第一個位址（單節點預設安裝為 10.42.0.1）；其他 Kubernetes 改成 cloudflared 與 kubelet 的實際來源 |
| `ingress.tunnel.clusterIP` | `""` | `tunnel` 時入口 Service 固定的 ClusterIP（Laysi/trustiot-v2#478）；空字串由 Kubernetes 分配。cloudflared 的路由指向入口 Service 的 ClusterIP，所以 tunnel 指向入口的叢集都要設：Service 重建後拿回同一個位址。只能是一個 IPv4 位址（不能是 CIDR、前導零、`None` 或 IPv6），`direct` 時設了就拒絕。要放在叢集 Service 網段的靜態區段、避開已用的位址；選法、理由與改位址的步驟見「入口 gateway」的「固定的入口 ClusterIP」 |
| `ingress.direct.serviceType` | `LoadBalancer` | `direct` 時入口 Service 的類型：`LoadBalancer`（k3s 的 ServiceLB 或客戶的負載平衡器）、`NodePort` 或 `ClusterIP`（前面另有客戶的反向代理時） |
| `ingress.direct.tlsSecret` | `""` | `direct` 時必填；外層 TLS 憑證所在的既有 `kubernetes.io/tls` Secret，涵蓋三個主機名稱，用客戶的憑證或我們 CA 簽的 |
| `ingress.publicPaths.login` | `/auth/login` | 登入，`hosts.app` → `identity` |
| `ingress.publicPaths.healthCheck` | `/healthz` | 健康檢查（外部監控），`hosts.app` → `identity` |
| `ingress.publicPaths.certRenewal` | `/certs/renew` | 閘道器憑證換發（以 TPM 簽章驗證，決策 12），`hosts.gateway` → `certs` |
| `ingress.publicPaths.mqtt` | `/mqtt` | 閘道器的 MQTT WebSocket，`hosts.gateway` → `mqtt-bridge` |
| `ingress.routes` | `[]` | 需要 cookie 的路由，每項 `{host, path, match, service, port}`，見「入口 gateway」 |

三個主機名稱都用我們自己的網域（可搬移規則 1），schema 拒絕 IP 與大寫；搬遷時只改 DNS。

### 入口 gateway

決策 27 的入口：Istio 實作 Gateway API，每個請求都先以 AuthorizationPolicy 的 CUSTOM 動作把 cookie 交給身分服務（ext_authz），只有例外路徑免驗證；`services.web` 開啟時，`hosts.app` 上 SPA 的靜態檔也免驗證（見下方「SPA 的靜態檔」）。chart 產生下列物件，名稱前綴同服務（`<release>-ingress…`）：

| 物件 | 內容 |
| --- | --- |
| Gateway `<release>-ingress` | `gatewayClassName: istio`；`tunnel` 只有 80 的 HTTP listener，`direct` 只有 443 的 HTTPS listener（`tls.mode: Terminate`）；只接受同一個 namespace 的路由。Istio 依它建立 Deployment、Service 與 ServiceAccount `<release>-ingress-istio` |
| ConfigMap `<release>-ingress-options` | 經 `infrastructure.parametersRef` 交給 Istio：副本數、資源、Pod 的 `seccompProfile: RuntimeDefault`（符合 restricted）、Service 類型（`tunnel` 設了 `ingress.tunnel.clusterIP` 時再加上固定的 ClusterIP）與 PodDisruptionBudget |
| HTTPRoute `<release>-ingress-app`、`-gateway`、`-ota` | 每個主機一個，只有有規則時才產生。每條規則都把 `x-forwarded-proto` 設為 `https`（cloudflared 到入口是明文 HTTP，後端判斷 Secure cookie 與重導向網址要靠它）；每條規則都移除底線寫法的 `x_internal_identity`、`x-internal_identity`、`x_internal-identity`，例外路徑另外移除 `x-internal-identity`，SPA 的靜態檔再移除 `cookie` |
| AuthorizationPolicy `<release>-ingress-cookie-check` | CUSTOM，provider 為 `ingress.authzProvider`。第一條規則檢查例外路徑以外的所有路徑；每個例外路徑再一條規則，在它以外的主機照樣檢查，例外不會漏到別的主機；它自己的主機帶不帶埠號都算（`notHosts` 列 `<主機>` 與 `<主機>:*`）。沒有任何例外路徑時檢查所有請求。`services.web` 開啟時改成：第一條規則在 `hosts.app` 以外的主機檢查例外路徑以外的所有路徑，第二條在每個主機（包括 `hosts.app`）檢查 `/api`、`/auth`、它們底下的路徑與 `/internal*`（例外路徑除外），其餘同上；不在 `/api`、`/auth` 底下的例外路徑在 `hosts.app` 也不檢查，那裡由靜態檔回應 |
| AuthorizationPolicy `<release>-ingress-deny-internal` | DENY `/internal*`：服務之間的內部 API 不能從外面進來，即使有 `/` 這種全包的路由。CUSTOM 先執行，所以沒有 cookie 回 401、有 cookie 回 403。`//internal`、`/%2finternal` 這類寫法要靠 mesh 的路徑正規化先整理成 `/internal` 才比對得到，見下方「依賴 mesh 的路徑正規化」 |
| NetworkPolicy `<release>-ingress` | `tunnel`：只放行 `ingress.tunnel.allowedSources` 連 80（cloudflared）與 15021（kubelet 的探針），叢集內其他 pod 連不到入口；`direct`：只開 443 與 15021。兩種都不開 Envoy 的指標埠（15020、15090），見下方「入口 pod 的埠」 |

入口 pod 的埠（Laysi/trustiot-v2#287，決定於 2026-10-11）：NetworkPolicy 每條規則都帶 `ports`，只有兩個 TCP 埠，不在 values 另寫一份：

- listener 的埠：由 `templates/_ingress.tpl` 的 `trustiot.ingress.listenerPort` 決定（`tunnel` 80、`direct` 443），Gateway 的 listener 與 NetworkPolicy 都用它。Istio 產生的 Service 把 listener 的埠原樣轉到 pod 的同一個埠，Envoy 直接聽這個埠（Istio 在入口 pod 設 `net.ipv4.ip_unprivileged_port_start=0`），所以 NetworkPolicy 放行的就是這個數字。
- Istio 的狀態埠 15021（`trustiot.ingress.statusPort`）：Istio 的 kube-gateway 樣板固定讓 `istio-proxy` 的 startup 與 readiness 探針 `GET /healthz/ready` 打 15021，Service 也列為 `status-port`；這個數字由 Istio 決定，chart 無法從自己的設定推得。
- 節點來源：入口 pod 不在 ambient mesh 裡（Istio 給它 `istio.io/dataplane-mode: none`），kubelet 的探針不經 istio-cni 的 SNAT，來源是主機自己的位址（k3s 為 cni0），與 cloudflared 相同，所以 `tunnel` 只有一條規則：`allowedSources` 連 80 與 15021。服務用的節點來源 `169.254.7.127/32`（見「服務間授權」）不適用，NetworkPolicy 也不放行它；chart 測試檢查 Gateway 的 `infrastructure.labels` 不帶 `istio.io/dataplane-mode`，否則入口 pod 被放進 mesh 後探針會被擋。
- 同一個節點的主機不受這條規則限制：Kubernetes NetworkPolicy 一律放行來自 pod 所在節點的連線，k3s 的 kube-router 以「來源是本機位址」實作，cni0 的位址也算。所以單節點的 SaaS 主機上，cloudflared、kubelet 與主機上其他行程（包括其他 namespace 的 `hostNetwork` pod）都連得到入口 pod 的每個埠，Envoy 的指標埠 15020、15090 也一樣；2026-10-11 在實驗機實測（見上面的測試報告）。這條規則與它的埠限制管的是 `allowedSources` 裡不是 pod 所在節點的來源：多節點時的其他節點、改跑在其他節點的 cloudflared，或寫得比本機更大的網段。服務的節點來源 `169.254.7.127/32` 不是主機的位址，不適用這個例外：2026-10-10 的 PoC 實測預設拒絕時要明確放行它，探針才會通過（docs/feasibility/2026-10-10-edge-poc.md），所以「服務間授權」的探針埠限制在單節點上也有作用。
- 驗證：chart 測試對兩種型態比對每條規則的 `ports` 與 Gateway 的 listener 加 15021；nightly 與 PR 的 `smoke` job 在 k3d 上讀 Istio 實際產生的 Deployment 與 Service，確認探針與 Service 用的埠和 NetworkPolicy 放行的埠完全相同（`scripts/nightly/manifests.ts` 的 `gatewayPorts`）。Istio 升級改了狀態埠時，這一項會失敗。2026-10-11 在實驗機（Istio 1.30.5）的紀錄見 `docs/test-reports/2026-10-11-287-gateway-netpol-ports.md`。

固定的入口 ClusterIP（`ingress.tunnel.clusterIP`，Laysi/trustiot-v2#478，決定於 2026-10-21）：

- 為什麼：`tunnel` 時 cloudflared 的路由寫的是入口 Service 的 ClusterIP（主機解析不了叢集 DNS）。Service 被刪掉重建（chart 重裝、Gateway 重建）時 Kubernetes 會分配新的位址：路由失效，舊位址還可能被之後建立的別的 Service 拿到，gateway 主機名稱那兩條不經 Cloudflare Access 的路徑（`/mqtt`、`/certs/renew`）就會把那個 Service 公開到網際網路。設了這個值，ConfigMap 的 `service` overlay 在 `type: ClusterIP` 旁多一行 `clusterIP: <位址>`，Istio 產生的 Service 就固定用這個位址。ClusterIP 在 Service 建立後不能改，所以叢集第一次部署之前就要設好。
- 位址怎麼選：Kubernetes 把 Service 網段開頭的 `min(max(16, 網段大小 / 16), 256)` 個位址（不含網路位址本身）留作靜態區段，給指定位址的 Service 用；動態分配先用其餘的部分，用完才用靜態區段（[Kubernetes 文件](https://kubernetes.io/docs/concepts/services-networking/cluster-ip-allocation/)）。所以靜態區段裡的位址，實際上不會在入口 Service 重建的空檔被別的 Service 自動拿走；動態區段的位址則可能被任何先建立的 Service 拿走。k3s 預設的 `10.43.0.0/16` 靜態區段是 `10.43.0.1` 到 `10.43.1.0`，其中 `kubernetes` 用 `10.43.0.1`、`kube-dns` 用 `10.43.0.10`。選定前以 `kubectl get servicecidr kubernetes -o jsonpath='{.spec.cidrs}'` 確認網段，並確認 `kubectl get services -A -o wide` 沒有 Service 用這個位址；叢集提供 IPAddress API（`networking.k8s.io/v1`，Kubernetes 1.33 起 stable 且不能關閉，所以 1.36 一定有）時，再以 `kubectl get ipaddress <位址>` 確認回 `NotFound`：分配器為每個已分配的 ClusterIP 建一個以位址為名的 IPAddress 物件，`NotFound` 表示沒有 Service 占用這個位址（[Extend Service IP Ranges](https://kubernetes.io/docs/tasks/network/extend-service-ip-ranges/)、[IPAddress](https://kubernetes.io/docs/reference/kubernetes-api/networking/ip-address-v1/)；Kubernetes v1.36.5 的 `pkg/registry/core/service/ipallocator/ipallocator.go`）。chart 不知道叢集的網段，不檢查位址在不在靜態區段；位址不在網段內或已被占用時，Kubernetes 拒絕建立 Service，Gateway 就不會 `Programmed`。chart 只檢查格式（一個 IPv4 位址，schema 與 `templates/_ingress.tpl` 各檢查一次）與 `ingress.exposure` 是 `tunnel`。
- Istio 怎麼套用（讀 Istio 1.30.5 的原始碼，`pilot/pkg/config/kube/gatewaycommon/deploymentcontroller.go`）：deployment controller 把 `parametersRef` 指的 ConfigMap 的 `service` 以 strategic merge patch 合進它從 kube-gateway 樣板產生的 Service（`render`、`applyOverlay`），再以 server-side apply（force）套用（`apply`），所以 Service 第一次建立時就帶這個位址；nightly 的 smoke 部署實際確認（見下方「驗證」）。這一項接下來的內容都是依 Istio 1.30.5 與 Kubernetes 的原始碼推論，尚未實測（Laysi/trustiot-v2#479）：之後 overlay 的位址與現有 Service 不同時，API server 拒絕這次 apply（ClusterIP 不能改），istiod 只在日誌記錄 `error handling <namespace>/<gateway>`，重試到第 5 次後放棄，Gateway 的狀態不會出現對應的 condition，Service 維持原來的位址；同一次 reconcile 排在 Service 之後的 PodDisruptionBudget 也不會更新。overlay 與 Service 的位址不同的期間，Istio 之後對這個 Service 的任何變更（例如 listener 的埠）也都套用不上，因為每次 apply 都帶著那個被拒絕的位址。把值改回空字串不會改變既有 Service 的位址（Kubernetes 更新 Service 時保留已分配的 ClusterIP），只是之後重建的 Service 不再固定（依原始碼推論，尚未實測：Laysi/trustiot-v2#479）。
- 部署後核對：每次設定或改了這個值的部署之後，比對 `kubectl -n <namespace> get service <release>-ingress-istio -o jsonpath='{.spec.clusterIP}'` 與 values 的值。兩者不同時，依上一項的推論，Istio 已不再把 overlay 套用到這個 Service，而且沒有別的地方會報出來；照下一項「改位址」的步驟處理。展示叢集由 `scripts/demo/tunnel.ts` 自動比對 values、ConfigMap `<release>-ingress-options` 的 `service` overlay 與這個 Service 三者，並確認位址在叢集 Service 網段的靜態區段內（Laysi/trustiot-v2#483）。切換 tunnel 路由前的關卡是 `route-origin`：通過時才以 0 結束，並印出路由要用的 origin，路由只照它的輸出設定；`check --stage 2` 在探測之前跑同一個關卡；`status` 也印出比對結果，但只是報告。切換時執行在 Laysi/trustiot-v2#469，步驟見 `docs/runbooks/demo-environment.md` 的「路由」。
- 改位址：ClusterIP 不能改，要讓 Istio 重建 Service。順序要對：先部署新的值再刪 Service，先刪的話 Istio 會以舊的值重建。刪除 Service 會釋放舊位址，之後任何 Service 都可能拿到它，所以刪除之前，tunnel 指向入口的路由要先改成不到任何 Service 的回應，等新的 Service 拿到新位址之後才指向新位址；不能有路由指向已釋放的位址。
  1. 依上面的方法選新位址。
  2. 改 values 的 `ingress.tunnel.clusterIP` 並部署（GitOps 的叢集開 PR 合併）。這時 Service 仍是舊位址，入口照常運作；istiod 開始記錄 apply 失敗（依原始碼推論，尚未實測：Laysi/trustiot-v2#479）。
  3. 把 tunnel 指向入口的每一條路由改成不到任何 Service 的回應（展示環境是第 1 段的 `http_status:418`，見 `docs/runbooks/demo-environment.md` 的「路由」）。從這一步到第 5 步把路由改到新位址之間，入口斷線。
  4. 確認 ConfigMap `<release>-ingress-options` 的 `service` 已是新位址，然後刪掉 Istio 產生的 Service：`kubectl -n <namespace> delete service <release>-ingress-istio`。Istio 監看它產生的 Service，刪除後立刻依 Gateway 與新的 overlay 重建（依原始碼推論，尚未實測：Laysi/trustiot-v2#479）。
  5. 確認 `kubectl -n <namespace> get service <release>-ingress-istio -o jsonpath='{.spec.clusterIP}'` 是新位址、Gateway 為 `Programmed` 之後，才把路由改到新位址，再經 tunnel 實際連一次。展示叢集改用 `scripts/demo/tunnel.ts` 的 `route-origin`：它以 0 結束才切換，路由照它印出的 origin 設定，切換後讀回路由設定確認，再馬上執行 `check --stage 2`（`docs/runbooks/demo-environment.md` 的「路由」）。
- 驗證：chart 測試（有設時 overlay 帶這個位址；沒設時與加入這個值之前逐位元組相同；各種無效的值與 `direct` 時設了都被拒絕，有沒有 schema 驗證都一樣）；nightly 的 smoke 部署在 `tunnel` 型態設 `10.43.0.240`，先確認它在 k3d 叢集 Service 網段的靜態區段內、沒有被占用，部署後確認 Istio 產生的 Service 的 `spec.clusterIP` 就是它（`scripts/nightly/ingress-address.ts`）。2026-10-21 的紀錄見 `docs/test-reports/2026-10-21-478-ingress-fixed-cluster-ip.md`。

例外路徑（`ingress.publicPaths`）的規則：

- 只有決策 27 列的四種，主機與後端固定在 chart，values 只能改路徑；schema 與樣板都只接受一個完整的小寫路徑，不能有萬用字元、`.`、`..`、`//`、結尾的 `/`、`%` 或 `;`，也不能在 `/internal` 底下。路由以 `Exact` 比對，Envoy 依 mesh 的路徑正規化（解碼 `%2F`、RFC 3986、合併斜線）整理後才比對，大小寫、編碼與 `..` 的變形都不是例外，照樣要 cookie。
- 後端服務關閉時，這個例外路徑既不路由也不免驗證，請求不會落到別的路由上。
- 例外路徑沒有 ext_authz 覆寫身分標頭，所以路由移除用戶端帶來的 `x-internal-identity`；需要 cookie 的路徑不能移除，因為路由的標頭規則在 ext_authz 之後執行，會把身分服務給的值一起移除。
- 例外只在它自己的主機成立，`Host` 帶不帶埠號都一樣（見下方「`Host` 帶埠號」）。

SPA 的靜態檔（`services.web`，Laysi/trustiot-v2#123，決定於 2026-10-21）：SPA 與 API 同源（決策 2），開啟 `services.web` 時 `hosts.app` 多一條 `PathPrefix /` 的公開路由到 `web`，深連結與靜態檔都不需要 cookie。規則：

- 需要 cookie 的前綴固定為 `/api` 與 `/auth`（`templates/_ingress.tpl` 的 `trustiot.ingress.cookiePrefixes`，不開放 values 修改）。`hosts.app` 上前綴本身、它們底下的路徑與 `/internal*` 照樣經 cookie 檢查；其他路徑（包括 `/apix`、`/API/x` 這類大小寫或邊界的變形，Gateway API 與 Istio 都區分大小寫、以路徑段為界）由靜態檔回應。`/api`、`/auth` 底下沒有 cookie 路由的請求會落到靜態檔的路由，但經過 cookie 檢查：身分服務找不到對應的 cookie 路由，回 401 或 403，不會到 `web`。
- 靜態檔的路由移除 `cookie`、`x-internal-identity` 與它的底線寫法：`web` 拿不到 session cookie，也拿不到身分。
- `hosts.app` 的 cookie 路由必須是 `/api`、`/auth` 或在它們底下，否則渲染失敗（包括 `/` 這種全包路由）；`web` 不能有 cookie 路由；`hosts.gateway`、`hosts.ota` 與 `hosts.app` 同名時渲染失敗，否則 `hosts.app` 的豁免會蓋到它們的路由。
- `services.web` 關閉（預設）時，渲染結果與加入這個功能之前逐位元組相同。
- 跨站請求（CSRF）由身分服務在 cookie 檢查時擋下：入口把 `Origin` 與 `Sec-Fetch-Site` 交給身分服務（`deploy/istio/istiod-values.yaml` 的 `includeRequestHeadersInCheck`），規則見 `services/identity/README.md`。

依賴 mesh 的路徑正規化（Laysi/trustiot-v2#255）：上面的路徑規則與 `/internal*` 的 DENY 都假設 istiod 設了 `meshConfig.pathNormalization.normalization: DECODE_AND_MERGE_SLASHES`（`deploy/istio/istiod-values.yaml`）。沒有它時，帶有效 cookie 的 `//internal/tenants` 或 `/%2finternal/tenants` 比對不到 DENY，會落到全包路由。chart 無法檢查叢集的 mesh 設定，所以每個叢集都要依 `deploy/istio/README.md` 安裝；`tests/e2e/ingress.e2e.ts` 的回歸案例在少了這個設定時會失敗。設定的影響（`%2F` 一律解碼、路徑參數不能夾帶 `/`）見該 README 的「路徑正規化」。

底線寫法的身分標頭（Laysi/trustiot-v2#255，決定於 2026-10-11）：Envoy 預設照轉名稱帶底線的標頭，`x_internal_identity` 對它是另一個標頭；但 CGI／WSGI 風格的伺服器把 `-` 與 `_` 視為相同（都變成 `HTTP_X_INTERNAL_IDENTITY`），偽造值可能被當成內部身分。每條路由（例外路徑與需要 cookie 的路徑）都移除 `x-internal-identity` 的三種底線寫法。不採用「拒絕所有帶底線的標頭」：Istio 沒有對應的設定，要用 EnvoyFilter（Istio 不保證跨版本相容，而且同樣是 mesh 層級），也可能擋掉閘道器或第三方客戶端合法的標頭；需要信任的標頭只有 `x-internal-identity` 一個（決策 27），移除它的變形就夠。服務仍然只讀 `x-internal-identity` 並驗簽（`crates/service-kit`）。

`Host` 帶埠號（Laysi/trustiot-v2#255，決定於 2026-10-11）：入口**接受** `Host` 帶埠號，例如閘道器送 `Host: gw.example.com:443`。Istio 的路由本來就把 `<主機>` 與 `<主機>:<任何埠>` 視為同一個主機，但例外的授權規則原本只列主機名稱，帶埠號時例外不成立，憑證換發與 MQTT 會回 401。現在例外規則同時列 `<主機>` 與 `<主機>:*`，與路由的比對方式一致：例外只在它自己的主機成立，`app` 主機帶埠號照樣要 cookie。閘道器韌體不必為此特別處理 `Host`；WebSocket 函式庫或客戶網路的代理放進埠號（預設的 443 或其他埠）都可以。Cloudflare Tunnel 轉發的 `Host` 沒有埠號，SaaS 原本就不受影響。這也是 Laysi/trustiot-v2#77（真身分服務與閘道器的端到端驗證）要沿用的約定。2026-10-15 由 Laysi/trustiot-v2#77 確認：入口把客戶端的 `Host` 原樣（帶埠號也一樣）交給 cookie 檢查（`tests/e2e/ingress.e2e.ts` 以替身觀察），真的身分服務去掉埠號找路由、資產服務照常回應（nightly 的 API 檢查，`scripts/nightly/api.ts`）。

需要 cookie 的路由（`ingress.routes`）：`host` 是 `app`、`gateway` 或 `ota`；`path` 是 `/` 或與例外路徑同樣格式的路徑，不能在 `/internal` 底下；`match` 是 `PathPrefix` 或 `Exact`；`service` 必須是已啟用的服務，`port` 是它的埠名（預設 `http`）。同一個主機不能有兩條相同路徑，也不能與例外路徑相同。`services.web` 開啟時，`hosts.app` 的路由只能是 `/api`、`/auth` 或在它們底下。例如資產服務的 API，以及 certs 服務給平台管理員簽發與撤銷工作站憑證的 API（Laysi/trustiot-v2#534，見 `services/certs/README.md` 的「工作站憑證」）：

```yaml
ingress:
  routes:
    - {host: app, path: /api/assets, match: PathPrefix, service: assets}
    - {host: app, path: /api/certs, match: PathPrefix, service: certs}
```

與身分服務的約定（Laysi/trustiot-v2#61 實作，細節見 `services/identity/README.md`）：入口以 `<pathPrefix><原始路徑>` 呼叫身分服務，`pathPrefix` 是 `/internal/ext-authz`，只帶 `cookie`、`origin` 與 `sec-fetch-site`（另有 Host、方法、路徑與 Authorization）。

| 身分服務的回應 | 情況 | 入口 |
| --- | --- | --- |
| 200，剛好一個 `x-internal-identity` | session 有效，且原始請求的主機與路徑對得到 `ingress.routes` 的一條路由 | 以這個值覆寫用戶端帶來的同名標頭，轉給後端 |
| 401 `unauthenticated` | 沒有 session cookie、cookie 重複或格式不對、session 不存在、過期或已撤銷 | 原樣交給用戶端 |
| 403 `permission_denied`（`message_key` 為 `security.cross_site_request`） | 跨站請求：`Sec-Fetch-Site` 或 `Origin` 顯示請求不是來自同一個來源（Laysi/trustiot-v2#123）；在查 session 之前判斷 | 原樣交給用戶端 |
| 403 `permission_denied` | session 有效，但身分服務的路由表對不到這個請求（正常情況下不會發生：入口沒有路由的請求不會呼叫身分服務） | 原樣交給用戶端 |
| 503 `unavailable` | 資料庫失敗，或 0.4 秒內查不完 | 把 5xx 當成錯誤，回自己的 503（`statusOnError`） |

身分服務無法連線、出錯或超過 0.5 秒時入口也回 503。session cookie 名稱是 `__Host-trustiot-session`。token 的受眾（`aud`）是請求要去的服務：chart 把 `ingress.routes` 換成實際主機名稱後以 `TRUSTIOT_INGRESS_ROUTES` 交給身分服務，身分服務以 Gateway API 的規則比對（主機相同、不看埠號；`Exact` 優先，其次最長、以路徑段為界的 `PathPrefix`）。比對不一致只會造成拒絕，不會放寬權限：token 只有它的受眾接受。入口的設定在 istiod 的 provider，見 `deploy/istio/README.md`。

服務的 pod 帶 `istio.io/dataplane-mode: ambient` 加入 ambient mesh，入口到服務、服務之間由 ztunnel 加 mTLS；`podLabels` 不能改這個 label。服務之間的授權見下一節。

叢集上的 e2e 測試（cookie、例外路徑、標頭與其底線寫法、`Host` 帶埠號、cookie 檢查拿到的 `Host` 與正規化後的路徑、28 種路徑繞過、`/internal` 的 11 種斜線寫法、身分服務停機）在 `tests/e2e/`，用法見該目錄的 README。路由清單的證明（每條路由上每個會到後端的請求都經過 cookie 檢查，只有決策 27 的例外、只在它自己的主機不經過；身分服務停機時入口因此拒絕其餘一切）在 `tests/chart.test.ts` 的「route list」，以 `tests/ingress-authz.ts` 照 gateway 的語意評估渲染結果（Laysi/trustiot-v2#77，對照見 `docs/roadmap/ms0-acceptance.md`）。`services.web` 開啟的組合也在其中：SPA 的靜態檔是 `hosts.app` 上 cookie 前綴以外的例外，證明另外取樣前綴兩側的邊界（`/api`、`/api/`、`/apix`、`/API/x`），DENY 拒絕的 `/internal` 不算，經過檢查但沒有 cookie 路由的請求視為被身分服務拒絕（身分服務拿到的路由與渲染出的 cookie 路由一致，同一個測試檢查）（Laysi/trustiot-v2#123）。

### 服務間授權（mesh）

決策 27 的服務間 mTLS 與授權：ztunnel 替每條連線加 mTLS，呼叫端的身分是它的 ServiceAccount（每個服務一個，`<release>-<服務>`）。「誰可以呼叫誰」只有一份清單，由兩層各自執行，任何一層單獨存在都擋得住清單以外的呼叫：

| 欄位 | 預設值 | 說明 |
| --- | --- | --- |
| `mesh.trustDomain` | `cluster.local` | mesh 的 SPIFFE trust domain（Istio 的 `meshConfig.trustDomain`）；呼叫端身分寫成 `<trustDomain>/ns/<namespace>/sa/<ServiceAccount>` |
| `mesh.calls` | `mesh.calls.ingest` 是 `[clickhouse, mqtt-broker]`，`mesh.calls.telemetry-query` 是 `[clickhouse, postgres]`，`mesh.calls.gateway-ops` 與 `mesh.calls.mqtt-bridge` 是 `[mqtt-broker]`，`mesh.calls.identity`、`mesh.calls.assets`、`mesh.calls.certs` 與 `mesh.calls.ai-gateway` 是 `[postgres]` | 誰可以呼叫誰：`<呼叫端>: [<被呼叫端>, ...]`。呼叫端是 `services` 的 key；被呼叫端是 `services` 的 key、叢集內元件 `clickhouse`（`clickhouse.mode: bundled` 時）、`otel-collector`（collector 開啟時）與 `mqtt-broker`（`mqttBroker.enabled` 時，只接受固定的幾個呼叫端，見「MQTT broker」），或 `postgres`（兩種 `postgres.mode` 都可以列，見下方）。遙測的呼叫由 chart 自動加上，不必列：每個啟用的服務 → `otel-collector`，`otel-collector` → `clickhouse`。不認得的名稱讓安裝失敗；服務未啟用或元件沒有部署時略過這條，所以可以先列還沒出映像的服務。預設值是決策 14 與 docs/architecture/deployment.md 寫明的 ClickHouse 用戶端：資料接收寫入、時序查詢以每租戶唯讀帳號讀取；以及使用 Postgres 的身分服務、資產服務、時序查詢（資產服務與時序查詢也以平台角色讀身分服務的事件，Laysi/trustiot-v2#65、Laysi/trustiot-v2#119）、certs 服務（簽發紀錄與 CRL，Laysi/trustiot-v2#78）與 AI 閘道（模型角色的設定，Laysi/trustiot-v2#605）；以及 MQTT broker 的用戶端（決策 6）：mqtt-bridge 轉接閘道器的連線，資料接收與閘道器維運各以自己的 broker 帳號收送訊息（broker 沒有開啟時這三條略過）。各服務的 issue 加入它要呼叫的內部 API，用 Postgres 的服務加上 `postgres` |

```yaml
mesh:
  calls:
    assistant: [ai-gateway, telemetry-query]   # 例子：assistant 可以呼叫這兩個服務
```

`ai-gateway` 只能由 AI 助理（`assistant`）、知識庫（`knowledge`）與警報（`alerts`）呼叫（決策 28，Laysi/trustiot-v2#536）：它不驗證內部身分、花的是供應商的帳號，所以只有 mesh 擋得住其他服務。`mesh.calls` 裡任何其他服務把 `ai-gateway` 列為被呼叫端時渲染失敗，不論兩邊有沒有啟用、有沒有做 schema 驗證（`templates/_ai-gateway.tpl` 的 `trustiot.aiGateway.callers`，`_mesh.tpl` 的 `trustiot.mesh.plan` 執行）；它也不能有 cookie 路由（見「入口 gateway」）。這三個服務真的呼叫它時，由各自的 issue 加上 `mesh.calls`（例如 Laysi/trustiot-v2#145 的 `assistant: [ai-gateway]`）；預設 values 沒有，所以 AuthorizationPolicy 沒有規則、拒絕所有連線（下方「呼叫矩陣」）。

除了 `mesh.calls`，入口 gateway（ServiceAccount `<release>-ingress-istio`）自動可以呼叫它路由到的服務（例外路徑與 `ingress.routes` 的後端），以及做 cookie 驗證（ext_authz）的身分服務（只到它的 API 埠 `http`，見下一段）。

代表使用者呼叫（決策 27，Laysi/trustiot-v2#106）：ztunnel 不把呼叫端的身分交給應用程式，所以服務 A 處理使用者的請求、要以同一個使用者呼叫服務 B 時，先拿自己收到的內部身分 token（受眾是 A）向身分服務的 `POST /internal/token-exchange` 換一張受眾是 B 的 token，再帶著它呼叫 B（`crates/service-kit` 的 `OnBehalf`）。token exchange 在身分服務自己的埠（`token-exchange`，預設 8082），不在 API 埠，有自己的 Service `<release>-identity-exchange`（`http://<release>-identity-exchange:8082`，`templates/token-exchange.yaml`）；身分服務的 Service 不開這個埠。身分服務只沿著 `mesh.calls` 換：

- chart 從 `mesh.calls` 取出兩個啟用的服務之間的呼叫，以 JSON `{"<呼叫端>": ["<被呼叫端>", ...]}` 交給身分服務（`TRUSTIOT_ON_BEHALF_CALLS`，只有身分服務拿到），並讓清單上的每個呼叫端連得到身分服務的 `token-exchange` 埠；呼叫端是身分服務自己時也一樣（它本來就持有簽章金鑰，連得到自己不增加權限）。
- cookie 檢查（ext_authz）與 token exchange 各有自己的授權，兩邊不共用（Laysi/trustiot-v2#106 安全審查的 M2）：
  - 身分服務自己的 AuthorizationPolicy `<release>-identity` 只放行入口 gateway 與在 `mesh.calls` 把 `identity` 列為被呼叫端的服務，而且只到 API 埠（`to.operation.ports`，ztunnel 在 L4 執行；`services.identity.ports` 中 `token-exchange` 以外的埠，預設只有 `http`，`templates/_mesh.tpl` 的 `trustiot.mesh.identityPorts`）。
  - token exchange 的 AuthorizationPolicy `<release>-identity-exchange` 同樣選身分服務的 pod，只放行代表使用者呼叫的服務，而且只到 `token-exchange` 埠；沒有這種服務時沒有規則，什麼都不放行。ALLOW 政策是聯集，一條連線符合其中一條就通過。
  - 所以代表使用者呼叫的服務碰不到 API 埠上的 ext_authz 檢查：那個端點把任何有效的 session cookie 換成任何服務的 token，而入口 gateway 會把使用者的 cookie 原樣轉給後端。
  - NetworkPolicy 也分成兩個：`<release>-identity` 放行入口 gateway 與 `mesh.calls` 的呼叫端，`<release>-identity-exchange` 放行代表使用者呼叫的服務，都在 HBONE 的 15008。NetworkPolicy 看不到通道裡的應用程式埠，所以分埠只靠 AuthorizationPolicy。
  - `services.identity.ports` 覆寫時要保留 `token-exchange`：有代表使用者呼叫而沒有這個埠時渲染失敗；只剩 `token-exchange`、沒有 API 埠時也失敗。`services` 底下不能有叫 `identity-exchange` 的服務。
- 不在清單上的：`ai-gateway` 兩個方向都不在（決策 28：它不驗證內部身分，也不持有使用者的 token，呼叫它與它的呼叫只靠 mesh 授權）；叢集內元件與 `postgres` 不是服務的 API。
- 所以同一份 `mesh.calls` 同時決定連得到（AuthorizationPolicy 與 NetworkPolicy）與可以代表使用者。清單外的呼叫換不到 token（連不到 `token-exchange`，或身分服務回 403），直接把自己收到的 token 轉給別的服務則被對方以受眾不符拒絕（401）；換來的 token 不會比原本的晚到期，最多 60 秒。
- 只換一跳：身分服務以 token 的受眾認定呼叫端，所以清單套用在入口 gateway 簽給呼叫端的那張 token 上；換來的 token 帶 `act` claim（RFC 8693，記下替使用者呼叫的服務），身分服務不再換它（403）。否則清單會被串接：A 可以呼叫 B、B 可以呼叫 C 時，A 拿換來的 B token 就能再換出 C 的 token。A → B → C 這種多跳的鏈要先能驗證呼叫端的工作負載身分，記在 Laysi/trustiot-v2#509；在那之前 B 收到 A 換來的 token 時不能再代表同一個使用者往下呼叫。
- agent 執行期間的委派（Laysi/trustiot-v2#551）：一次 run 比 60 秒的 token 活得久的服務（只有 `assistant`），在請求的 token 還有效時在同一個 `token-exchange` 埠建立 run 委派（`POST /internal/run-delegations`），之後每次呼叫前換一張 60 秒的 token（`POST /internal/run-delegations/token`，`crates/service-kit` 的 `RunDelegations`）。它沿用同一份清單：換得到的受眾就是 `mesh.calls` 上 `assistant` 可以呼叫的服務，chart 不需要新的設定，`assistant` 有列任何服務時就連得到這個埠。委派最長 900 秒，每次換 token 身分服務都重查 session 與使用者在租戶的角色，登出或移出租戶後下一次呼叫就失敗；換出的 token 帶 `act`，不能再換。上限、撤銷與威脅模型在 [services/identity/README.md](../../services/identity/README.md) 的「agent 執行期間的委派」。
- 還沒處理、記在 Laysi/trustiot-v2#509 的：入口 gateway 把 session cookie 轉給身分服務以外的後端（應只轉給身分服務）。
- 背景工作與事件消費者沒有使用者的請求，不換 token，以服務自己的身分工作（自己的資料庫角色、事件；`crates/service-kit` 的 `ServiceIdentity`）。
- 預設 values 沒有服務之間的呼叫，所以清單是 `{}`；目前的矩陣見下方「呼叫矩陣」。

chart 產生下列物件：

| 物件 | 內容 |
| --- | --- |
| NetworkPolicy `<release>-default-deny` | 選取 namespace 內所有 pod、不放行任何連入：沒有其他 NetworkPolicy 放行的連線一律拒絕。服務全關時也會產生 |
| AuthorizationPolicy `<release>-<服務或元件>` | 每個啟用的服務與每個部署的叢集內元件（目前是 `bundled` 的 ClickHouse）一個，ALLOW，以 selector 選它的 pod，只允許呼叫端的 principals（ztunnel 在 L4 執行）。沒有任何呼叫端時是沒有規則的 ALLOW，拒絕所有連線。身分服務的政策只到 API 埠，token exchange 另有 `<release>-identity-exchange`（見上方「代表使用者呼叫」）；certs 服務的政策也只到 API 埠，簽 MQTT broker 伺服器憑證的埠另有 `<release>-certs-broker`（見下方 MQTT broker 一條） |
| NetworkPolicy `<release>-<服務或元件>` | 同上，每個一個（token exchange 另有 `<release>-identity-exchange`，broker 憑證的埠另有 `<release>-certs-broker`）：只從同一批呼叫端的 pod（與入口 gateway 的 pod）放行 TCP 15008，也就是 ztunnel 的 HBONE 埠，mesh 內的連線都從這裡進來，不直接連應用程式的埠；另對節點來源 `169.254.7.127/32`（kubelet 的健康檢查由 istio-cni SNAT 成這個位址）只放行該工作負載探針實際連的 TCP 埠，服務是健康埠（`healthPort`，預設 8081），見下方「節點來源」。只有 exec 探針時不產生這條規則；沒有呼叫端也沒有探針埠時 `ingress: []`，拒絕所有連入 |

- 兩層的分工：AuthorizationPolicy 驗證 mTLS 憑證裡的身分，ztunnel 對每條代理的連線檢查；NetworkPolicy 只看來源 pod 的 label 與 IP，擋的是新建立的 TCP 連線。拿掉 NetworkPolicy 時，清單外的呼叫仍被 ztunnel 重置；拿掉 AuthorizationPolicy 時，清單外的服務連不到對方的 15008，非 mesh 的 pod 連不到任何埠。被拒時呼叫端看到的是連線重置或拒絕，不是 HTTP 403（docs/feasibility/2026-10-10-edge-poc.md）。2026-10-10 在實驗機實測兩個方向（docs/test-reports/2026-10-10-63-mesh-authz.md）。
- NetworkPolicy 這一層的限制，主要防線是 AuthorizationPolicy：
  - mesh 內的連線都經 15008，所以 NetworkPolicy 只有在 15008 依呼叫端的 pod 限定時才是 mesh 內的第二層；只放行「任何來源的 15008」（2026-10-10 PoC 的寫法）對 mesh 內的呼叫端沒有作用，只擋非 mesh 的 pod。
  - NetworkPolicy 不切斷已建立的連線，而 ztunnel 會保留兩個 pod 之間的 HBONE 連線重複使用。之後才收緊或加上的 NetworkPolicy，對既有連線上的呼叫不起作用，要等連線關閉（實測恢復 NetworkPolicy 後 90 秒內仍持續通過）；AuthorizationPolicy 的變更則立即套用到新的呼叫。
  - NetworkPolicy 依 label 選來源，能在 namespace 裡建 pod 的人可以複製服務的 label 通過它；mTLS 身分來自 ServiceAccount，只有 AuthorizationPolicy 擋得住（實測的 imposter）。
- 節點來源 `169.254.7.127/32`：ambient 下 istio-cni 在節點上把「從節點的網路命名空間發出、連到 ambient pod」的連線一律 SNAT 成這個位址，pod 裡的 ztunnel 規則看到這個來源就直接交給應用程式的埠，不經 mTLS，也不套用 AuthorizationPolicy。kubelet 的健康檢查這樣進來，節點上其他行程也一樣，例如其他 namespace 的 `hostNetwork` pod 或節點上的系統服務。所以這條規則是兩層都管不到的入口，只能靠 NetworkPolicy 限縮：
  - chart 只對探針實際連的埠放行：startup、liveness、readiness 探針中 `httpGet`、`tcpSocket`、`grpc` 的埠，具名的埠換成 container port 的數字，exec 探針不需要網路規則（Laysi/trustiot-v2#265）。埠由 `templates/_mesh.tpl` 的 `trustiot.mesh.probePorts` 從 pod 用的同一份探針設定推得：服務來自 `serviceDefaults.probes` 與 `services.<name>.probes`，叢集內元件來自 `trustiot.<元件>.pod`（ClickHouse 是 `templates/_clickhouse.tpl` 的 `trustiot.clickhouse.pod`），不在 values 另寫一份，改探針時 NetworkPolicy 跟著變。chart 測試對兩份 values 的多種組合，逐一比對每個工作負載 pod spec 裡的探針埠與這條規則。
  - 服務的探針連獨立的健康埠（`serviceDefaults.healthPort`，預設 8081，埠名 `health`），service-kit 在這個埠只提供 `/healthz`、`/readyz`，其他路徑一律 404（`crates/service-kit` 的 `http.health_listen_addr`）。所以節點來源對服務只放行健康埠，碰不到 API 埠（`http`，8080），也就碰不到只靠 AuthorizationPolicy 限定呼叫端的 `/internal/` 路徑（Laysi/trustiot-v2#286）。健康埠只是 container port：Service 不開它，`ingress.routes` 也指不到它（`port: health` 會讓渲染失敗），只有 kubelet 會連。服務的 `ports` 不能用 `health` 這個名稱或健康埠的埠號。chart 測試對兩份 values 與各種疊加（全部服務、smoke 與 ingress e2e 的 overlay），逐一檢查每個服務的節點來源規則只有健康埠。
  - 覆寫 `services.<name>.probes` 讓探針連 `http` 會讓節點來源重新連得到那個服務的整個 API，所以服務不要這樣做。只有測試用的替身這樣設：`tests/values/smoke.yaml` 與 `tests/e2e/values-e2e.yaml` 的 traefik/whoami 只能聽一個埠，探針因此連 `http`；身分服務替身與 TrustIoT 服務一樣有健康埠。
  - ClickHouse 只開 HTTP 8123 給節點（`/ping` 以外的查詢都要帳密），原生協定 9000 從節點連不到。
- NetworkPolicy 只管連入（`Ingress`），不限制連出：外部依賴（Qdrant Cloud、S3、SMTP、AI 端點、外部資料庫）以主機名稱指定，NetworkPolicy 無法依主機名稱放行。
- 不部署 waypoint：L4 的身分已足夠。服務之間需要 L7 規則（例如依路徑或方法）時才為該服務加 waypoint，並把它的 AuthorizationPolicy 改成以 `targetRefs` 指向 Service、在 ztunnel 層放行 waypoint 的 ServiceAccount；代表使用者呼叫的授權（內部身分 token）由身分服務換 token 時依清單檢查、被呼叫端驗證 token 的受眾，不靠 waypoint（Laysi/trustiot-v2#106）。
- 叢集測試 `tests/e2e/mesh.e2e.ts` 以真實連線驗證清單內放行、清單外被拒，以及兩層各自生效；也驗證清單內的呼叫端連得到 `token-exchange`、連不到身分服務的 API 埠，清單外的服務帶著使用者的 token 仍被拒，用法見該目錄的 README。服務在這個測試裡跑身分服務替身，不驗證 token，所以它看到的拒絕都是 L4 的；token exchange 自己的檢查（清單、只換一跳）由 Rust 測試驗證。
- MQTT broker（`mqttBroker.enabled`）也在 `trustiot.mesh.components`，但它的 AuthorizationPolicy 依埠分開：`mqtt-bridge` 只到閘道器埠，有 broker 帳號的服務只到服務埠，`mesh.calls` 也只能列這幾個呼叫端（見「MQTT broker」）。它的探針是 exec，所以沒有節點來源的規則；它的 trust sidecar 連 certs 服務由 chart 自動加上。broker 取得自己的伺服器憑證也由 chart 自動放行（Laysi/trustiot-v2#557）：certs 服務在自己的埠 `broker-certs`（預設 8082，服務的 `broker.listen_addr`）簽這張憑證，那個埠有自己的 Service、AuthorizationPolicy 與 NetworkPolicy `<release>-certs-broker`，只放行 broker 的 ServiceAccount，而且只到這個埠；certs 自己的政策只放行它的呼叫端到 API 埠（`trustiot.mesh.certsPorts`），certs 的 Service 也不開這個埠。拿到 broker 名稱的憑證就能對每台閘道器冒充 broker，所以其他服務與入口 gateway 都連不到它；`ingress.routes` 指向 `broker-certs`（或身分服務的 `token-exchange`）時渲染失敗，`services.certs.ports` 覆寫時要保留 `broker-certs`（broker 開啟而沒有這個埠時渲染失敗），`services` 底下不能有叫 `certs-broker` 的服務。broker 沒有部署時這兩個政策沒有規則，什麼都不放行。
- 叢集內的 ClickHouse 走同一套規則：只有 `mesh.calls` 列出的 `ingest` 與 `telemetry-query`，以及 chart 自動加上的 OpenTelemetry collector（Laysi/trustiot-v2#73）與 migration Job（`migrate.enabled` 為 true 時，Laysi/trustiot-v2#60）連得到它，其他服務與 pod 都被拒，kubelet 的 `/ping` 探測經 `169.254.7.127/32` 照常通過，節點來源只開 HTTP 8123。`mesh.calls` 的呼叫端只能是服務，元件之間的呼叫由 `trustiot.mesh.plan` 自己加（`_mesh.tpl`），collector 與 migration Job 都是這樣；還沒有的 Grafana（Laysi/trustiot-v2#183）加入時也要這樣加進來。ClickHouse 的政策只以 chart 測試驗證，2026-10-10 的實驗機測試沒有部署它（見測試報告）。
- collector 也是 mesh 內的元件（`trustiot.mesh.components` 的 `otel-collector`，埠與探針定義在 `trustiot.otel-collector.pod`）：每個啟用的服務都自動是它的呼叫端（送 OTLP），其他 pod 連不到它，節點來源只開健康檢查的 13133。它連出到外部 ClickHouse 或 SIEM 不受 NetworkPolicy 限制（只管連入）；SIEM 在同一個 namespace 時，要另外以 NetworkPolicy 放行 collector 的 pod 連它。
- `postgres` 是「這個服務用 Postgres」的宣告，不經 mesh：Postgres 的每條連線本身就是 TLS 並以 verify-full 驗證伺服器，叢集內的 Postgres pod 也不加入 ambient mesh（見「叢集內的 Postgres」），所以它沒有 AuthorizationPolicy，也不走 HBONE。列出 `postgres` 的服務才會掛上 Postgres 的 CA 並拿到 `TRUSTIOT_POSTGRES_CA_FILE`（`ai-gateway` 是 `AI_GATEWAY_POSTGRES_CA_FILE`；`external` 也一樣）；`bundled` 時 `<release>-postgres` 這個 NetworkPolicy 也只放行這些服務的 pod 連 5432。同一份清單決定兩件事，不會一邊有、一邊沒有。
- 預設拒絕套用到 namespace 內所有 pod，包括之後加入的叢集內元件（broker 等）：在 mesh 內的元件加進 `trustiot.mesh.components`，由同一份清單產生政策；不在 mesh 內的元件自帶 NetworkPolicy，只放行實際的用戶端（見「擴充 chart」），叢集內的 Postgres 就是這樣。chart 測試檢查每個 Deployment 與 StatefulSet 的 pod 都有自己的 NetworkPolicy 與 AuthorizationPolicy。

#### 呼叫矩陣

預設 values、所有服務啟用時渲染出的授權，兩種部署形態相同。「允許的呼叫端」是被呼叫端的 AuthorizationPolicy 的 principals（省略 `<release>-`），NetworkPolicy 放行同一批 pod；身分服務的政策依埠分開：`identity` 的呼叫端（入口 gateway）只連得到 API 埠 `http`；`identity-exchange` 是身分服務的 token exchange（`token-exchange` 埠，選的是身分服務的 pod），呼叫端是 `TRUSTIOT_ON_BEHALF_CALLS` 的呼叫端，預設 values 沒有，它自己不是服務，所以「代表使用者呼叫」是 `—`；certs 服務同樣依埠分開：`certs` 的呼叫端只連得到 `http`，`certs-broker` 是簽 MQTT broker 伺服器憑證的 `broker-certs` 埠（選的是 certs 的 pod），只放行 MQTT broker，預設 values 不部署 broker，所以沒有呼叫端；「代表使用者呼叫」是身分服務願意為誰換出以它為受眾的 token（`TRUSTIOT_ON_BEHALF_CALLS`），`—` 表示被呼叫端不是服務、不收內部身分 token。`tests/chart.test.ts` 的「README.md's call matrix」比對這張表與渲染結果：改了預設的 `mesh.calls`、例外路徑或預設的 `ingress.routes`，要在同一個變更更新這張表。

| 被呼叫端 | 允許的呼叫端 | 代表使用者呼叫 |
| --- | --- | --- |
| `ai-gateway` | （無） | （無） |
| `alerts` | （無） | （無） |
| `assets` | （無） | （無） |
| `assistant` | （無） | （無） |
| `certs` | `ingress-istio` | （無） |
| `certs-broker` | （無） | — |
| `clickhouse` | `ingest`、`migrate`、`otel-collector`、`telemetry-query` | — |
| `field-context` | （無） | （無） |
| `gateway-ops` | （無） | （無） |
| `identity` | `ingress-istio` | （無） |
| `identity-exchange` | （無） | — |
| `identity-key-rotation` | （無） | — |
| `ingest` | （無） | （無） |
| `knowledge` | （無） | （無） |
| `migrate` | （無） | — |
| `mqtt-bridge` | `ingress-istio` | （無） |
| `otel-collector` | 每個啟用的服務 | — |
| `telemetry-query` | （無） | （無） |
| `web` | `ingress-istio` | （無） |

入口 gateway 到 `identity` 是 cookie 檢查與登入、到 `certs` 與 `mqtt-bridge` 是決策 27 的例外路徑，到 `web` 是 `hosts.app` 上 SPA 的靜態檔（見「SPA 的靜態檔」）；服務的 cookie 路由（`ingress.routes`）預設是空的，各服務的 issue 加上。`postgres` 不經 mesh，沒有 AuthorizationPolicy，連得到它的服務見上方 `postgres` 一條。預設 values 不部署 MQTT broker，所以表上沒有它；開啟後誰連得到它的哪個埠見「MQTT broker」的「誰連得到」。

### serviceDefaults 與 services.\<name\>

`serviceDefaults` 是每個服務的預設值，`services.<name>` 可以覆寫其中任何欄位。合併方式：map 逐個 key 遞迴合併，list 與單一值整個取代，`false`、`0` 與空字串也算覆寫；`probes` 底下的每個 probe 整個取代；`env` 接在 `serviceDefaults.env` 後面。

| 欄位 | 預設值 | 說明 |
| --- | --- | --- |
| `serviceDefaults.image.registry` | `ghcr.io/laysi/trustiot-v2` | 映像是 `<registry>/<服務名稱>` |
| `serviceDefaults.image.repository` | `""` | 完整的 repository，設定時取代 `<registry>/<服務名稱>` |
| `serviceDefaults.image.tag` | `""` | 空白時用 `Chart.yaml` 的 `appVersion`（服務一起發版，決策 4） |
| `serviceDefaults.image.digest` | `""` | `sha256:` 加 64 個十六進位字元；設定時以 digest 固定映像。repo 裡留空，發佈的 chart 由 release 在 `services.<name>.image.digest` 寫入各服務映像的 digest（見「映像的 digest」） |
| `serviceDefaults.image.pullPolicy` | `IfNotPresent` | `Always`、`IfNotPresent` 或 `Never` |
| `serviceDefaults.replicas` | `1` | 副本數 |
| `serviceDefaults.ports` | `http` 8080 | 容器埠清單，欄位見下表；Service 開同樣的埠號 |
| `serviceDefaults.healthPort` | `8081` | 健康埠：chart 另外加上名為 `health` 的 container port，服務在這裡只提供 `/healthz`、`/readyz`；探針連它，節點來源也只放行它（見「節點來源」）。Service 不開這個埠。1024–65535，不能與 `ports` 的埠名或埠號重複 |
| `serviceDefaults.env` | `[]` | 每個服務都加的環境變數（Kubernetes `EnvVar`）；每個服務都會拿到，所以不能讀 Secret（`valueFrom.secretKeyRef`）；名稱的限制同 `services.<name>.env` |
| `serviceDefaults.resources` | requests `cpu: 20m`、`memory: 32Mi`；limits `cpu: "1"`、`memory: 256Mi` | 資源需求與上限；schema 要求 `limits.memory` |
| `serviceDefaults.podSecurityContext` | `runAsNonRoot: true`、`seccompProfile: RuntimeDefault` | 不固定 `runAsUser`，讓 OpenShift 的 restricted-v2 SCC 指派 UID。只接受 restricted Pod Security Standard 允許的欄位與值：`runAsNonRoot` 只能是 `true`，seccomp 與 AppArmor 只能是 `RuntimeDefault` 或 `Localhost`，不能設 sysctls |
| `serviceDefaults.securityContext` | `allowPrivilegeEscalation: false`、`readOnlyRootFilesystem: true`、`capabilities.drop: [ALL]` | 同樣只接受 restricted 允許的設定：不能提權、不能 privileged、`capabilities.drop` 一定含 `ALL` 且不能加 capability、`procMount` 只能是 `Default` |
| `serviceDefaults.probes.startup` | 健康埠的 `GET /healthz`，每 2 秒，最多 30 次 | 設成 `{}` 停用 |
| `serviceDefaults.probes.liveness` | 健康埠的 `GET /healthz`，每 10 秒，逾時 2 秒，失敗 3 次 | 必填 |
| `serviceDefaults.probes.readiness` | 健康埠的 `GET /readyz`，每 5 秒，逾時 2 秒，失敗 2 次 | 必填 |
| `serviceDefaults.preStopSleepSeconds` | `5` | 送 SIGTERM 前先等幾秒，讓入口與 ztunnel 先移除這個 pod；`0` 停用，最多 60 |
| `serviceDefaults.drainSeconds` | `30` | SIGTERM 到 SIGKILL 之間給服務收尾的秒數。Kubernetes 把 preStop 的等待算在寬限期內，所以 pod 的 `terminationGracePeriodSeconds` 是 `preStopSleepSeconds + drainSeconds`（預設 35） |
| `serviceDefaults.tmpSizeLimit` | `64Mi` | 可寫入的 `/tmp`（emptyDir）大小上限；根目錄唯讀 |
| `serviceDefaults.automountServiceAccountToken` | `false` | 服務不呼叫 Kubernetes API |
| `serviceDefaults.nodeSelector` | `kubernetes.io/arch: arm64` | 映像只出 arm64（決策 5） |
| `serviceDefaults.tolerations` | `[]` | Kubernetes tolerations |
| `serviceDefaults.affinity` | `{}` | Kubernetes affinity |
| `serviceDefaults.podLabels` | `{}` | 加在 pod 上的 label；`app.kubernetes.io/` 與 `helm.sh/` 開頭的 key 保留給 chart，避免改到 selector |
| `serviceDefaults.podAnnotations` | `{}` | 加在 pod 上的 annotation；`checksum/` 開頭的 key 保留給 chart，也不接受舊式的 AppArmor 與 seccomp annotation |

每個服務的欄位：

| 欄位 | 說明 |
| --- | --- |
| `services.<name>.enabled` | 必填；服務的 issue 出映像後改成 `true` |
| `services.<name>.image` | 覆寫 `serviceDefaults.image` 的欄位 |
| `services.<name>.replicas` | 覆寫副本數 |
| `services.<name>.ports` | 整個取代埠清單；埠名與埠號不能重複，也不能用 `health` 或健康埠的埠號；探針只能指向清單內的埠名與 `health`。只有 `identity` 有預設值：`http` 8080 與 token exchange 的 `token-exchange` 8082（服務的 `exchange.listen_addr`），`token-exchange` 由自己的 Service `<release>-identity-exchange` 提供，mesh 依這兩個埠分開授權（見「服務間授權（mesh）」的代表使用者呼叫），覆寫時兩個都要保留 |
| `services.<name>.healthPort` | 覆寫健康埠的埠號；服務的 `http.health_listen_addr` 要跟著改 |
| `services.<name>.env` | 接在 `serviceDefaults.env` 後面，名稱不能重複。`TRUSTIOT_` 後面接英數字、`SENTRY_`、`OTEL_` 開頭的名稱（不分大小寫）保留給 chart，避免覆寫平台設定與 chart 給的密鑰；`TRUSTIOT__`（兩個底線）開頭的名稱不在保留範圍。名稱像密鑰（含 SECRET、PASSW、PASSPHRASE、PWD、TOKEN、API_KEY、DSN、PRIVATE、CREDENTIAL，或以 `_KEY` 結尾，不分大小寫）的變數只能用 `valueFrom.secretKeyRef`，不能用字面值或 ConfigMap 等其他來源；每一項只能有 `value` 或一個 `valueFrom` 來源。任何變數都不能讀 values 指定的依賴帳密與 Sentry DSN（同一個 Secret 的同一個 key），依賴帳密要改列在 `credentials`。`ai-gateway` 的設定前綴是 `AI_GATEWAY`（見「AI 閘道」），所以它另外保留 `AI_GATEWAY_` 後接英數字（平台設定與設定檔的名稱）、`AI_GATEWAY__PROVIDERS__`、`AI_GATEWAY__ROLES__` 開頭的名稱，以及它的 ConfigMap 裡的每個變數：供應商、金鑰變數、端點與模型只能來自 `ai`，container 的 `env` 會蓋過 ConfigMap |
| `services.<name>.credentials` | 只給這個服務的依賴帳密，可列 `qdrant-read`、`qdrant-write`、`object-storage`、`smtp`、`ai`，變數見「密鑰的環境變數」；不能放在 `serviceDefaults`，避免每個服務都拿到。`identity` 預設列 `smtp`：它用 `smtp` 的 relay 寄登入驗證碼（`services/identity/README.md`）。`ai` 只能給 `ai-gateway`（決策 28），`ai-gateway` 預設就列 `ai`，拿掉時開啟它會渲染失敗（閘道沒有金鑰不啟動）；`qdrant-read` 與 `qdrant-write` 只能給 `knowledge`，因為 Qdrant 查詢的租戶條件由知識庫服務加上（docs/architecture/rag.md） |
| `services.<name>.database` | 用 Postgres（`mesh.calls` 列出 `postgres`）的服務的連線設定；不能放在 `serviceDefaults` |
| `services.<name>.database.maxConnections` | 服務主要連線池的大小，chart 以 `TRUSTIOT__DATABASE__MAX_CONNECTIONS` 傳入（自訂 `env` 不能設定這個變數）。`identity` 預設 `10`（3–100，它常駐 3 條）、`assets` 與 `telemetry-query` 預設 `10`（2–100）、`certs` 預設 `5`（2–100），範圍外渲染失敗。用 Postgres 的服務一定要有；所有服務在整版滾動時的連線加總超過 `postgres.bundled.parameters.max_connections` 時渲染失敗，見「叢集內的 Postgres」的連線預算 |
| `services.<name>.resources` | 逐欄覆寫資源設定 |
| `services.<name>.podSecurityContext` | 逐欄覆寫；限制同 `serviceDefaults` |
| `services.<name>.podSecurityContext.fsGroup` | 只有 `identity`（`65532`）與 `certs`（`65533`）有預設值：簽章金鑰檔與簽發 CA 金鑰檔的權限是 0440、屬於 root 與這個群組，服務以群組身分讀它，容器裡其他使用者讀不到（見「內部身分」與「閘道器憑證 CA」）；兩者不同，其他 pod 都不在這兩個群組。OpenShift 設成 `null`，由 restricted-v2 SCC 從 namespace 的範圍指派；其他平台拿掉它，服務會讀不到金鑰而一直不就緒 |
| `services.<name>.securityContext` | 逐欄覆寫；限制同 `serviceDefaults` |
| `services.<name>.probes` | 每個 probe 整個取代；liveness 與 readiness 不能是空的 |
| `services.<name>.preStopSleepSeconds` | 覆寫 preStop 等待秒數 |
| `services.<name>.drainSeconds` | 覆寫收尾秒數，例如持有長連線的服務。`ai-gateway` 預設 `310`：模型的回答會串流好幾分鐘，又不能換到別的 pod 接著回，所以收尾時讓進行中的串流跑完；chart 把它的 `shutdown.drain_timeout` 設成 `drainSeconds` 減 10 秒，只接受 11 到 3605（見「AI 閘道」） |
| `services.<name>.tmpSizeLimit` | 覆寫 `/tmp` 大小上限 |
| `services.<name>.automountServiceAccountToken` | 需要呼叫 Kubernetes API 的服務才設 `true`；`identity` 不能設（渲染失敗，見「內部身分」） |
| `services.<name>.nodeSelector` | 逐個 key 合併 |
| `services.<name>.tolerations` | 整個取代 |
| `services.<name>.affinity` | 逐個 key 合併 |
| `services.<name>.podLabels` | 逐個 key 合併；保留的 key 同 `serviceDefaults.podLabels` |
| `services.<name>.podAnnotations` | 逐個 key 合併；限制同 `serviceDefaults.podAnnotations` |

`ports` 每一項的欄位：

| 欄位 | 說明 |
| --- | --- |
| `ports[].name` | 必填；最多 15 個字元的小寫名稱，probe 以名稱指定埠 |
| `ports[].containerPort` | 必填；1024–65535，服務不以 root 執行 |
| `ports[].servicePort` | Service 的埠號，預設同 `containerPort` |
| `ports[].protocol` | `TCP`（預設）、`UDP` 或 `SCTP` |
| `ports[].appProtocol` | 例如 `http`，供 Istio 判斷協定 |

### 映像的 digest

我們自己的映像（服務、`postgres.bundled.image` 與 `migrate.image`）的 tag 預設是 `Chart.appVersion`，所以 repo 裡的 chart 不寫 digest：寫死某一次發佈的 digest，之後的版本就會變成「新 tag＋舊 digest」。

發佈的 chart 以 digest 固定同一次發佈的映像（Laysi/trustiot-v2#57）。release 的 publish 推送映像之後，在這份 chart 的暫存複本寫入它們的 digest 再打包、簽章：服務寫在 `services.<name>.image.digest`（`values.yaml` 沒有這個欄位時加上），Postgres 寫在 `postgres.bundled.image.digest`，migration Job 寫在 `migrate.image.digest`。所以從 GHCR 拉下的 chart 會 render 出 `<repository>:<版本>@sha256:…`，verify 也會檢查（[CI 與發佈](../../docs/architecture/ci-release.md)「release.yml」）。

- `values-saas.yaml`、`values-onprem.yaml` 與 `deploy/flux` 的 values 不覆寫這些映像的 `registry`、`repository`、`tag`、`digest`（`bun test scripts` 檢查）。
- 安裝實例的覆寫檔也不要動它們。要從自己的 registry 鏡像時，只改 `serviceDefaults.image.registry`、`postgres.bundled.image.repository` 與 `migrate.image.repository`，鏡像要保留 digest（例如 `skopeo copy --preserve-digests`），digest 才對得上。
- 改用別的映像（例如臨時修補版）時，`tag` 與 `digest` 要一起改，或把 `digest` 設成 `""`。只改 `tag` 時，kubelet 仍然依 digest 拉原本的映像。
- `tests/values/smoke.yaml` 與 e2e 的 values 用本機建出的映像，套在 repo 裡（沒有 digest）的 chart 上，不受影響。

### 內部身分

決策 27 的內部身分 token：身分服務為每個通過 cookie 驗證的請求簽一個 60 秒的 token，每個服務以身分服務的公鑰驗證。金鑰對在叢集內產生，沒有人建立、看到、複製或輪替它（Laysi/trustiot-v2#404；設計與每個失敗情況在 [docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「內部身分簽章金鑰」，操作在 [docs/runbooks/secrets.md](../../docs/runbooks/secrets.md)）。token 的格式與驗證規則在 `crates/service-kit/README.md` 的「內部身分」。

- **私鑰**：chart 在身分服務啟用時宣告空的 Secret `<release>-identity-signing-keys`（`templates/passwords.yaml`，和 migration Job 的密碼 Secret 同樣沒有 `data`、`helm.sh/resource-policy: keep`、`trustiot.io/filled-by: migrate`）。身分服務 pod 的 init container `signing-keys`（migration 映像的 `migrate identity-signing-keys`，在 `wait-for-migrations` 之前）以身分服務 ServiceAccount 的 projected token 產生金鑰並寫入；Role `<release>-identity-signing-keys` 只有這個 Secret 與下面的 ConfigMap 的 `get`、`patch`。身分服務的主容器沒有 token（`services.identity.automountServiceAccountToken` 是 `true` 時渲染失敗），只以唯讀 volume 讀 `/etc/trustiot/identity-signing-keys/signing-keys.json`（0440，root 與 pod 的 `fsGroup`），路徑以 `TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE` 告訴它。其他 pod 都不掛它，任何服務的自訂 `env` 都不能讀它。
- **公鑰**：chart 在身分服務啟用時宣告空的 ConfigMap `<release>-internal-identity-keys`（同樣的標籤與 `keep`），init container 依 Secret 寫入 JWK Set `jwks.json`。每個服務掛整個 ConfigMap 在 `/etc/trustiot/internal-identity/`（`optional: true`，不用 `subPath`），平台設定 `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` 指向它；ConfigMap 改變後 kubelet 更新檔案，服務每 5 秒重讀，不必重啟。還沒有公鑰時服務照常啟動但不就緒。
- **全新安裝**：pod 建立時掛上的是 init container 寫入之前的空 Secret 與 ConfigMap，kubelet 同步 pod 時才換成新的內容。身分服務與每個服務都等 migration Job 完成才啟動，通常一啟動就讀到；沒有讀到的等 kubelet 下一次更新（正常不超過一個同步週期）才就緒，`helm install --wait` 的逾時要留這段時間。2026-10-15 lab 的 k3s 實測：金鑰公開後 25 到 31 秒身分服務與資產服務都就緒（[測試報告](../../docs/test-reports/2026-10-15-404-identity-keys-in-cluster.md)）。
- **依金鑰年齡自動輪替**（Laysi/trustiot-v2#429）：`maxKeyAgeSeconds` 不是 0 時，CronJob `<release>-identity-key-rotation` 依 `keyRotationSchedule`（預設每天 03:17 UTC）以身分服務的 ServiceAccount 執行同一個 `migrate identity-signing-keys`，多一個 `TRUSTIOT__KEYS__MAX_KEY_AGE_SECONDS`。最新一把金鑰公開超過 `maxKeyAgeSeconds` 時，它加一把同一代的新金鑰，照手動輪替的排程換掉舊的，不重啟任何 pod；舊金鑰在它的 token 都過期後的下一次執行移除。它不另外授權（沿用上面的 Role）、不掛 Secret，只掛與 init container 相同的 projected token；沒有人能連進它的 pod（沒有規則的 AuthorizationPolicy 與 `ingress: []` 的 NetworkPolicy）。`concurrencyPolicy: Forbid`、`startingDeadlineSeconds: 3600`，保留最近一個成功與三個失敗的 Job。`maxKeyAgeSeconds` 是 0、身分服務沒有啟用，或設了 `signingKeySecret`（搬移期間不換金鑰）時不渲染。CronJob 名稱最多 52 個字元（Job 控制器會加 11 個），release 名稱太長時截短前綴。設計、代的規則與同時執行在 [docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「依金鑰年齡自動輪替」。
- **身分服務沒有啟用**：沒有人產生金鑰，所以 chart 不宣告 Secret、ConfigMap 與 Role，服務不掛公鑰，也拿不到 `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`：驗證內部身分的服務拒絕啟動（與 Laysi/trustiot-v2#404 之前沒有 `publicKeys` 時相同），不會一直等不到金鑰。

| 欄位 | 預設值 | 說明 |
| --- | --- | --- |
| `internalIdentity.keyGeneration` | `"1"` | 要求的金鑰代，1 到 9 位數字。調大就輪替：身分服務滾動重啟（其他服務不重啟），第一個新 pod 的 init container 加一把新金鑰、先公開公鑰，`keyPropagationSeconds` 之後才用它簽章；舊金鑰在它簽的 token 都過期之後的下一次身分服務啟動（或輪替 CronJob 的下一次執行）時移除。只往上走：rollback 到較小的值不會再換一次。自動輪替沿用同一代，所以人要輪替或撤銷時，照這個欄位的值加一就好 |
| `internalIdentity.revokeBeforeGeneration` | `"0"` | 私鑰外洩時設成新的一代（`keyGeneration` 調到不小於它，否則渲染失敗）再升級：代比它小的金鑰立即從 Secret 與 ConfigMap 移除，新金鑰立即開始簽章；身分服務也從環境變數拿到這個值，不用被撤銷的金鑰簽章。之後留著不改。每個服務換上新公鑰之前仍接受外洩的金鑰，逐一確認的步驟在 runbook |
| `internalIdentity.keyPropagationSeconds` | `600` | 新公鑰公開之後多久才開始用新私鑰簽章，120 到 86400 秒。要比每個 pod 看到 ConfigMap 改變的時間長（kubelet 的同步週期加快取，預設約 1 分鐘）；調大了 kubelet `syncFrequency` 的叢集要跟著加大。身分服務的金鑰與公開的公鑰不一致超過它時記 ERROR |
| `internalIdentity.maxKeyAgeSeconds` | `7776000` | 依金鑰年齡自動輪替（Laysi/trustiot-v2#429）：最新一把金鑰公開超過這麼多秒時，輪替 CronJob 換掉它。0 關閉（不渲染 CronJob）；否則至少 `2 × keyPropagationSeconds + 305`（預設 1505，保證下一次輪替時前一把舊金鑰已經可以移除，否則渲染失敗），最多 31536000（365 天）。改它會讓身分服務滾動重啟一次（它以這個值檢查輪替是否逾期），不換金鑰 |
| `internalIdentity.keyRotationSchedule` | `"17 3 * * *"` | 輪替 CronJob 何時檢查金鑰年齡，時區 `Etc/UTC`（不能寫 `TZ=`）：`@daily`、`@midnight`、`@hourly`，或日、月、星期三欄都是 `*` 的五欄 cron，也就是每天至少一次。schema 拒絕更稀疏的排程：金鑰會比 `maxKeyAgeSeconds` 多用好幾週，身分服務也會在金鑰超過輪替時間 2 天時記 ERROR。改它不重啟任何 pod |
| `internalIdentity.publicKeys` | `[]` | 只給 Laysi/trustiot-v2#404 之前的安裝搬移用，和 `signingKeySecret` 一起設：人建立的私鑰的公鑰，每把 `{kid, x}`（`kid` 1–64 個可見 ASCII 字元、不能重複，`x` 是 32 bytes 公鑰的 base64url，最多 8 把；schema 只允許這兩個欄位，所以私鑰放不進 values）。init container 在這裡找出匯入的金鑰的 `kid`。只設其中一個時渲染失敗 |
| `internalIdentity.signingKeySecret.name` | `""` | 只給搬移用：人建立、放 PKCS#8 PEM 私鑰的既有 Secret。只以唯讀 volume 掛進身分服務的 init container，它在 Secret `<release>-identity-signing-keys` 還沒有金鑰時以原本的 `kid`、代 0 匯入一次；設定期間不產生新金鑰。新安裝留空 |
| `internalIdentity.signingKeySecret.key` | `signing-key.pem` | |

從人建立的私鑰搬過來（Laysi/trustiot-v2#404 之前的安裝），每一步都不中斷服務：

1. 升級到這個 chart，`signingKeySecret` 與 `publicKeys` 先不動：init container 匯入那把私鑰，新舊版的 pod 都用它；服務改從 ConfigMap 讀公鑰。`publicKeys` 裡其他的公鑰不帶過來，舊做法的輪替要在搬移之前做完。
2. 滾動完成後，從 values 移除 `signingKeySecret` 與 `publicKeys` 再升級：`keyGeneration`（至少 1）大於匯入的代 0，init container 產生新金鑰並照輪替換掉匯入的那一把。
3. ConfigMap 不再有匯入的 `kid` 之後，刪掉人建立的 Secret。

### 閘道器憑證 CA

決策 6、12 的自建 CA（Laysi/trustiot-v2#78）：離線的根 CA 簽出簽發 CA，certs 服務以簽發 CA 簽閘道器憑證、發 CRL。簽發 CA 的私鑰在叢集內產生，沒有人建立、看到或複製它；根 CA 的私鑰不進叢集。階層、儀式的每一步、輪替與外洩處理在 [services/certs/README.md](../../services/certs/README.md)，設計摘要在 [docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「閘道器憑證 CA」。以下物件只在 `services.certs.enabled` 時渲染；`certificateAuthority.*` 不論開關都會檢查。

- **私鑰**：空的 Secret `<release>-certs-ca-keys`（`templates/passwords.yaml`，與 migration Job 的密碼 Secret 相同的 metadata）。certs pod 的 init container `ca-keys`（migration 映像的 `migrate certs-ca-keys`，在 `wait-for-migrations` 之前）以 certs ServiceAccount 的 projected token 產生金鑰並寫入；Role `<release>-certs-ca-keys` 只有這個 Secret 與下面 CSR 的 ConfigMap 的 `get`、`patch`（`resourceNames`）。certs 的主容器沒有 token（`services.certs.automountServiceAccountToken` 是 `true` 時渲染失敗），只以唯讀 volume 讀 `/etc/trustiot/certs-ca-keys/issuing-ca-keys.json`（0440，root 與 pod 的 `fsGroup`）。其他 pod 都不掛它，任何服務的自訂 `env` 都不能讀它，chart 的其他 Role 不能寫 CSR 的 ConfigMap（chart 測試）。
- **CSR**：空的 ConfigMap `<release>-certs-ca-requests`（`templates/certs-ca.yaml`，`trustiot.io/filled-by: migrate`、`helm.sh/resource-policy: keep`），init container 寫入每把金鑰的 `issuing-ca-generation-<代>.csr.pem`。維運人員從這裡取出 CSR 帶到離線的根 CA。
- **憑證**：ConfigMap `<release>-certs-ca-certificates` 的 `root.pem` 與 `issuing.pem`，由下表的兩個欄位印成 PEM。certs 掛整個 ConfigMap 在 `/etc/trustiot/certs-ca/`，每 30 秒重讀，改憑證不重啟 pod。values 不接受換行，所以每張憑證寫成它的 DER 的一行 base64（`sed '/-----/d' cert.pem | tr -d '\n'`）；看起來不是憑證的值（PEM 文字、私鑰的 DER、長度不對）渲染失敗，私鑰因此放不進 values。

| 欄位 | 預設值 | 說明 |
| --- | --- | --- |
| `certificateAuthority.keyGeneration` | `"1"` | 要求的簽發 CA 金鑰代，1 到 9 位數字。調大就加一把：certs 滾動重啟（其他服務不重啟），第一個新 pod 的 init container 產生新金鑰與它的 CSR；舊的照常簽章，直到新金鑰的憑證放進 `issuingCertificates`。只往上走：rollback 到較小的值不會再產生 |
| `certificateAuthority.removeKeysBeforeGeneration` | `"0"` | 代比它小的金鑰在下一次 certs 啟動時移除：不再簽章，它的 CRL 不再發布，broker 因此拒絕它簽的每張憑證。輪替時等舊的一代簽的憑證都過期再設；外洩時與 `keyGeneration` 一起設成新的一代。不能大於 `keyGeneration`（渲染失敗）。0 不移除 |
| `certificateAuthority.rootCertificate` | `""` | 離線根 CA 的憑證，一行 base64 DER（公開）。還沒設時 certs 照常就緒但不能簽章（指標 `certs.ca.signing` 是 0） |
| `certificateAuthority.issuingCertificates` | `[]` | 根 CA 簽出的簽發 CA 憑證，每把金鑰一張，各是一行 base64 DER（公開），最多 4 張。對不上 Secret 裡任何金鑰的憑證被略過並記在日誌 |

### migration Job

做法與理由見後面的「migration Job」一節。

| 欄位 | 預設值 | 說明 |
| --- | --- | --- |
| `migrate.enabled` | `true` | 部署 migration Job，並在每個服務的 pod 加上等它完成的 init container。關掉時資料庫的 schema、服務角色與 collector 帳號都要另外建立；只有不接真正資料庫的叢集測試（`tests/e2e/values-*.yaml`）關掉它 |
| `migrate.image.repository` | `ghcr.io/laysi/trustiot-v2/migrate` | migration 映像（[deploy/images/migrate](../images/migrate)），由 release 與服務一起發佈；Job 與服務的 init container 用同一個映像 |
| `migrate.image.tag` | `""` | 空白時用 `Chart.appVersion`。必須等於每個在 `mesh.calls` 列出 `postgres` 的服務的 tag（chart 檢查，見「migration Job」的「版本一致」） |
| `migrate.image.digest` | `""` | sha256 digest；設定時以 digest 固定映像。repo 裡留空，發佈的 chart 由 release 寫入同一次發佈的 digest（見「映像的 digest」）。只以 digest 換掉某個服務的映像時（例如修補），migration 映像也要換成同一次發佈的 tag 與 digest |
| `migrate.image.pullPolicy` | `IfNotPresent` | |
| `migrate.revision` | `"1"` | 改這個值就讓 Job 重跑一次（Job 名稱跟著變），所有服務也隨之重啟。資料庫還原或改接之後、密碼 Secret 的資料遺失之後要改它：Job 補回角色、重新產生遺失的密碼（見「migration Job」的「產生的密碼」） |
| `migrate.passwordGeneration` | `"1"` | 數字（最多 9 位）。調大就輪替 Job 產生的每一個密碼（服務的登入角色、collector 的 ClickHouse 帳號）：下一次執行產生新密碼並設定，Job 名稱跟著變，服務與 collector 以新密碼重啟。只往上走：比 Secret 記的小（rollback）時不換。值本身不是密鑰 |
| `migrate.wait.databases` | `10m` | Job 等 Postgres 與 ClickHouse 開始接受連線的上限（全新安裝時資料庫與 Job 同時建立）；秒、分或 `1h`，最多 1 小時 |
| `migrate.wait.lock` | `10m` | 等另一次執行釋放 migration 鎖的上限 |
| `migrate.activeDeadlineSeconds` | `1800` | Kubernetes 在這麼多秒後停掉 Job；要大於上面兩個等待加上執行時間 |
| `migrate.backoffLimit` | `3` | 失敗後重試的次數；每次執行都是冪等的 |
| `migrate.resources` | requests `cpu: 20m`、`memory: 32Mi`；limits `cpu: "1"`、`memory: 128Mi` | Job 與每個服務的 init container 共用；schema 要求 `limits.memory` |
| `migrate.nodeSelector` | `kubernetes.io/arch: arm64` | 映像只有 arm64（決策 5） |
| `migrate.tolerations` | `[]` | |

### 外部依賴

所有外部依賴由 values 指定，託管或自架只差在 values（可搬移規則 2）。`bundled` 代表由這份 chart 部署在叢集內；`external` 代表用叢集外的服務。

| 欄位 | 預設值 | 說明 |
| --- | --- | --- |
| `postgres.mode` | `bundled` | `bundled`：chart 在 release 的 namespace 部署 CloudNativePG 的 Cluster，叢集要先裝好 operator（見「叢集內的 Postgres」）；`external`：自行維運、含 Apache AGE 的 Postgres。用 Postgres 的服務在 `mesh.calls` 列出 `postgres` |
| `postgres.bundled.image.repository` | `ghcr.io/laysi/trustiot-v2/postgres` | 我們的 CloudNativePG operand 映像：PostgreSQL 18 加 Apache AGE（deploy/images/postgres），由 release 與服務一起發佈 |
| `postgres.bundled.image.tag` | `""` | 空白時用 `Chart.appVersion`；映像的 tag 是 TrustIoT 的版本，PostgreSQL 的 major 由 chart 寫在 ImageCatalog |
| `postgres.bundled.image.digest` | `""` | sha256 digest；設定時以 digest 固定映像。repo 裡留空，發佈的 chart 由 release 寫入同一次發佈的 digest（見「映像的 digest」） |
| `postgres.bundled.image.pullPolicy` | `IfNotPresent` | |
| `postgres.bundled.storage.size` | `""`，`bundled` 時必填 | 資料與 WAL 的磁碟區大小，每個部署自己估算，見「叢集內的 Postgres」 |
| `postgres.bundled.storage.storageClassName` | `""` | 空白時用叢集預設的 StorageClass |
| `postgres.bundled.resources` | requests `cpu: 100m`、`memory: 256Mi`；limits `cpu: "2"`、`memory: 1Gi` | 兩種型態相同，配合 8 GB 單節點上的 ClickHouse 與服務；schema 要求 `limits.memory`。調高記憶體上限時一起調 `parameters` |
| `postgres.bundled.parameters.shared_buffers` | `128MB` | PostgreSQL 的記憶體設定，單位 `kB`、`MB` 或 `GB`。依記憶體上限估算：`shared_buffers` 約八分之一、`effective_cache_size` 約八分之三 |
| `postgres.bundled.parameters.effective_cache_size` | `384MB` | 查詢規劃用的快取估計，不實際配置記憶體 |
| `postgres.bundled.parameters.work_mem` | `4MB` | 每個排序或 hash 的上限，一個查詢可能用好幾份 |
| `postgres.bundled.parameters.maintenance_work_mem` | `64MB` | VACUUM、建索引用 |
| `postgres.bundled.parameters.max_connections` | `160` | 10–1000；`bundled` 時 chart 檢查：啟用的服務在整版滾動時所有連線池的加總，加上保留給 superuser、CloudNativePG、migration Job 與第一位平台管理員的 bootstrap 的 18 條，不能超過它，超過就渲染失敗。預設值放得下四個用 Postgres 的服務各一個副本（150 條），記憶體估算在 1Gi 以內；算法、每個服務的數字與估算見「叢集內的 Postgres」的連線預算 |
| `postgres.bundled.nodeSelector` | `kubernetes.io/arch: arm64` | 映像只有 arm64（決策 5） |
| `postgres.bundled.tolerations` | `[]` | |
| `postgres.bundled.operator.namespace` | `cnpg-system` | CloudNativePG operator 所在的 namespace；NetworkPolicy 只放行它的 pod 連 Postgres pod 的 8000（instance manager） |
| `postgres.bundled.operator.podLabels.app.kubernetes.io/name` | `cloudnative-pg` | operator pod 的 label，用來選來源；客戶已有自己的 operator（例如由 OperatorHub 安裝）時改成它的 namespace 與 label |
| `postgres.external.host` | `""` | `external` 時必填；主機名稱或 IPv4 |
| `postgres.external.port` | `5432` | |
| `postgres.external.database` | `trustiot` | 資料庫名稱 |
| `postgres.external.sslMode` | `verify-full` | 只接受 `verify-full`：一律驗證伺服器憑證（清單 M3） |
| `postgres.external.caSecret.name` | `""` | `external` 時必填；簽發伺服器憑證的 CA 所在的 Secret，以唯讀方式只掛進在 `mesh.calls` 列出 `postgres` 的服務與 migration Job |
| `postgres.external.caSecret.key` | `ca.crt` | |
| `postgres.external.migrationSecret.name` | `""`，`external` 且 Job 開啟時必填 | migration 角色的帳密所在的既有 Secret：資料庫的 owner，有 CREATEROLE，沒有 SUPERUSER 與 BYPASSRLS，而且不擁有其他資料庫（Job 檢查，不符就停止）。一個 Postgres 叢集只能放一個 TrustIoT 資料庫：角色是叢集層級的，Job 建立的角色名稱不帶實例名稱，兩個實例會共用同一組服務角色與密碼（見「migration Job」）。只給 migration Job |
| `postgres.external.migrationSecret.usernameKey` | `username` | |
| `postgres.external.migrationSecret.passwordKey` | `password` | |
| `clickhouse.mode` | `bundled` | `bundled`：叢集內自管（Laysi/trustiot-v2#59）；`external`：ClickHouse Cloud 等，只當備案（決策 14） |
| `clickhouse.bundled.image.repository` | `docker.io/clickhouse/clickhouse-server` | |
| `clickhouse.bundled.image.tag` | `26.8` | 26.8 LTS；實際版本由 digest 決定。Renovate 只提議 LTS 的 minor（`.3`、`.8`），換 minor 的 PR 會被 chart 測試擋下，要先重做決策 14 的檢查（[CI 與發佈](../../docs/architecture/ci-release.md)「Renovate」） |
| `clickhouse.bundled.image.digest` | 見 `values.yaml` | 必填（版本固定），multi-arch index 的 digest，含 arm64；tag 與 digest 必須與 `scripts/lib/stack.ts` 和 `services/ingest/tests/common/mod.rs` 相同（測試比對），Renovate 在同一個 PR 更新三處 |
| `clickhouse.bundled.image.pullPolicy` | `IfNotPresent` | |
| `clickhouse.bundled.adminUser` | `trustiot_admin` | bootstrap 管理帳號，有 access management 權限，只給 migration Job 使用；不能是 `default` |
| `clickhouse.bundled.adminPasswordGeneration` | `"1"` | bootstrap 管理帳號密碼的代（1 到 9 位數字）。密碼由 ClickHouse pod 的 init container 在叢集內產生（見「叢集內的 ClickHouse」）；調大這個值就是輪替：升級時 ClickHouse 重啟，init container 先產生新密碼。只往上走，rollback 渲染出較小的值時保留現有的密碼 |
| `clickhouse.bundled.adminSecret.name` | `""` | 只給 Laysi/trustiot-v2#403 之前的安裝過渡用：設定時 ClickHouse 改讀這個人建立的既有 Secret，沒有 init container，行為與之前相同。留空（預設）時密碼在叢集內產生；搬移步驟見「叢集內的 ClickHouse」。服務的自訂 `env` 不能讀它 |
| `clickhouse.bundled.adminSecret.key` | `password` | |
| `clickhouse.bundled.adminSecretRevision` | `"1"` | 只在 `adminSecret.name` 設定時有作用：ClickHouse 只在啟動時讀 admin 密碼，改了那個 Secret 之後也改這個值，升級時 ClickHouse 重啟並換上新密碼 |
| `clickhouse.bundled.storage.size` | `""`，`bundled` 時必填 | 資料磁碟區大小，每個部署依自己的資料量與保存期限估算，見「叢集內的 ClickHouse」 |
| `clickhouse.bundled.storage.storageClassName` | `""` | 空白時用叢集預設的 StorageClass |
| `clickhouse.bundled.resources` | requests `cpu: 250m`、`memory: 512Mi`；limits `cpu: "2"`、`memory: 2Gi` | ClickHouse 依記憶體上限的 90% 設定自己的上限與快取；schema 要求 `limits.memory` |
| `clickhouse.bundled.podSecurityContext` | `runAsNonRoot: true`、`runAsUser`／`runAsGroup`／`fsGroup: 101`、`seccompProfile: RuntimeDefault` | 映像沒有宣告 `USER`，所以固定 clickhouse 使用者的 UID 101；OpenShift 把三個 ID 設成 `null`，交給 SCC 指派。限制同 `serviceDefaults.podSecurityContext` |
| `clickhouse.bundled.nodeSelector` | `{}` | ClickHouse 映像是 multi-arch，但產生密碼的 init container 用 migration 映像（只有 arm64，決策 5），所以 `adminSecret.name` 留空時 chart 自動加上 `kubernetes.io/arch: arm64`；這裡設成其他架構時渲染失敗 |
| `clickhouse.bundled.tolerations` | `[]` | |
| `clickhouse.external.url` | `""` | `external` 時必填；只接受 https，例如 `https://abc.clickhouse.cloud:8443` |
| `clickhouse.external.caSecret.name` | `""` | 服務只在私有 CA 簽發時需要；migration Job 開啟時必填：Job 只信任這個 CA、不信任公開根憑證，伺服器用公開 CA 時把那個根憑證放進來 |
| `clickhouse.external.caSecret.key` | `ca.crt` | |
| `clickhouse.external.migrationSecret.name` | `""`，`external` 且 Job 開啟時必填 | 有 access management 權限的帳號的帳密所在的既有 Secret（叢集內 ClickHouse 的管理帳號在叢集內產生，見「叢集內的 ClickHouse」），只給 migration Job |
| `clickhouse.external.migrationSecret.usernameKey` | `username` | |
| `clickhouse.external.migrationSecret.passwordKey` | `password` | |
| `mqttBroker.enabled` | `false` | 部署叢集內的 MQTT broker（Mosquitto，決策 6），見「MQTT broker」。兩份 values 與 `deploy/flux` 都不開（flux 的測試檢查兩個叢集），目前只有 nightly 的 `tests/values/smoke.yaml` 開啟；開啟前要先有伺服器憑證的 Secret（Laysi/trustiot-v2#535） |
| `mqttBroker.image.repository` | `docker.io/library/eclipse-mosquitto` | |
| `mqttBroker.image.tag` | `2.1.2-alpine` | 實際版本由 digest 決定 |
| `mqttBroker.image.digest` | 見 `values.yaml` | 必填（版本固定），multi-arch index 的 digest，含 arm64；tag 與 digest 必須與 `crates/protocol/tests/broker.rs`、`tools/migrate/tests/mqtt_broker.rs` 和 `scripts/lib/stack.ts` 相同（chart 測試比對）。換版本前先讀新版的 SIGHUP、CRL 與 dynamic security 行為，「MQTT broker」一節的做法依賴 2.1.2 的原始碼 |
| `mqttBroker.image.pullPolicy` | `IfNotPresent` | |
| `mqttBroker.passwordGeneration` | `"1"` | 服務的 broker 密碼的代（1 到 9 位數字）。調大就是輪替：broker 重啟，init container 先產生新密碼（見「MQTT broker」的「服務的帳號」）。只往上走，rollback 渲染出較小的值時保留現有的密碼 |
| `mqttBroker.trustInterval` | `30s` | sidecar 向 certs 服務重取 CA 憑證與 CRL、檢查伺服器憑證 Secret 的間隔；秒、分或 `1h`，1 秒到 1 小時 |
| `mqttBroker.storage.size` | `1Gi` | 資料磁碟區：session、排隊與 retained 訊息（SQLite）、dynamic security 檔與信任檔 |
| `mqttBroker.storage.storageClassName` | `""` | 空白時用叢集預設的 StorageClass |
| `mqttBroker.resources` | requests `cpu: 50m`、`memory: 64Mi`；limits `cpu: "1"`、`memory: 256Mi` | broker 的 `memory_limit` 是 `limits.memory` 的四分之三，超過時拒收新訊息而不是被 kernel 終止；所以 `limits.memory` 必須是整數的 `Mi` 或 `Gi`，否則渲染失敗 |
| `mqttBroker.podSecurityContext` | `runAsNonRoot: true`、`runAsUser`／`runAsGroup`／`fsGroup: 1883`、`seccompProfile: RuntimeDefault` | 映像的 mosquitto 使用者。pod 的每個容器都以這個使用者執行：sidecar 只能對同一個使用者的行程送訊號。OpenShift 可以把三個 ID 設成 `null` 交給 SCC 指派（每個容器仍是同一個 UID；沒有 OpenShift 環境可測）。限制同 `serviceDefaults.podSecurityContext` |
| `mqttBroker.nodeSelector` | `{}` | Mosquitto 映像是 multi-arch，但 init container 與 sidecar 用 migration 映像（只有 arm64，決策 5），所以 chart 自動加上 `kubernetes.io/arch: arm64`；這裡設成其他架構時渲染失敗 |
| `mqttBroker.tolerations` | `[]` | |
| `qdrant.mode` | `""`，必填 | `external`（SaaS，Qdrant Cloud）或 `bundled`（地端自架，Laysi/trustiot-v2#194） |
| `qdrant.external.url` | `""` | `external` 時必填；只接受 https |
| `qdrant.external.credentialsSecret.name` | `""` | 放 API key 的 Secret |
| `qdrant.external.credentialsSecret.readApiKeyKey` | `read-api-key` | 檢索用的唯讀 key（docs/architecture/rag.md） |
| `qdrant.external.credentialsSecret.writeApiKeyKey` | `write-api-key` | 背景處理用的寫入 key |
| `objectStorage.endpoint` | `""` | S3 相容端點；空白時用 AWS S3。SaaS 只接受 https |
| `objectStorage.region` | `""` | 例如 R2 用 `auto` |
| `objectStorage.bucket` | `""` | bucket 名稱 |
| `objectStorage.forcePathStyle` | `false` | MinIO 等需要 path-style 時設 `true` |
| `objectStorage.credentialsSecret.name` | `""` | 放存取金鑰的 Secret |
| `objectStorage.credentialsSecret.accessKeyIdKey` | `access-key-id` | |
| `objectStorage.credentialsSecret.secretAccessKeyKey` | `secret-access-key` | |
| `smtp.host` | `""` | SMTP 主機名稱或 IPv4；地端是客戶的 SMTP |
| `smtp.port` | `587` | |
| `smtp.security` | `starttls` | `starttls` 或 `tls`（直接 TLS，通常是 465）；不支援明文 SMTP |
| `smtp.from` | `""` | 寄件者，例如 `TrustIoT <noreply@example.com>`；只能一行，避免注入郵件標頭。設了 `smtp.host` 就必填（schema 檢查） |
| `smtp.credentialsSecret.name` | `""` | relay 不需要驗證時留空；有值時平台設定多一個 `TRUSTIOT_SMTP_AUTH_REQUIRED`，引用是 optional（見「密鑰的環境變數」） |
| `smtp.credentialsSecret.usernameKey` | `username` | |
| `smtp.credentialsSecret.passwordKey` | `password` | |
| `ai.providers.anthropic.baseUrl` | `""` | Anthropic Messages 的端點；空字串是 Anthropic 自己的 API，要經 Cloudflare AI Gateway 時只改這裡（例如 `https://gateway.ai.cloudflare.com/v1/<account>/<gateway>/anthropic`）。只接受 https、有主機名稱、不含帳密、query 與 fragment（閘道把金鑰送到這裡），兩種部署形態都一樣。見「AI 閘道」 |
| `ai.providers.anthropic.apiKeySecret.name` | `""` | 放 Anthropic 金鑰的 Secret；有名稱這個供應商才算設定，只有 `ai-gateway` 拿到（決策 28） |
| `ai.providers.anthropic.apiKeySecret.key` | `api-key` | |
| `ai.providers.openai.baseUrl` | `""` | OpenAI Decisions 的端點；空字串是 OpenAI 自己的 API，規則同上 |
| `ai.providers.openai.apiKeySecret.name` | `""` | 放 OpenAI 金鑰的 Secret |
| `ai.providers.openai.apiKeySecret.key` | `api-key` | |
| `ai.roles` | `{}` | 模型角色：`<角色>: {models: [<供應商>/<模型 ID>, ...]}`，第一個是首選、後面依序備援（1 到 8 個），可另設上限與逾時；`ai-gateway` 開啟時至少要一個角色，每個用到的供應商都要有 `apiKeySecret.name`。欄位見「AI 閘道」 |
| `ai.requestTimeout` | `10m` | 閘道的 `http.request_timeout`，最多 `1h`；每個角色第一次嘗試的時間（`queueWait` 加上 `timeouts.response` 與 `timeouts.firstToken` 較長者）要放得下這個值減去十分之一（最多 2 秒），否則渲染失敗（閘道也不會啟動）；預設上限需要 300 秒 |
| `observability.sentry.enabled` | `false` | SaaS 開、地端預設關（決策 24）；開啟時每個服務都拿到 `SENTRY_DSN` 與 `SENTRY_ENVIRONMENT` |
| `observability.sentry.environment` | `""` | 開啟時必填，例如 `production`、`staging` |
| `observability.sentry.dsnSecret.name` | `""` | 開啟時必填；DSN 含金鑰，放在 Secret |
| `observability.sentry.dsnSecret.key` | `dsn` | |
| `observability.collector.enabled` | `true` | 部署 OpenTelemetry collector，並把 `TRUSTIOT__TELEMETRY__OTLP_ENDPOINT` 交給每個服務；見「OpenTelemetry collector」 |
| `observability.collector.image.repository` | `docker.io/otel/opentelemetry-collector-contrib` | contrib 版才有 ClickHouse exporter 與 redaction processor |
| `observability.collector.image.tag` | `0.161.0` | 實際版本由 digest 決定；換版本前先對照 exporter 的欄位與 `0006_logs.sql`（測試比對）：Renovate 換版本的 PR 會被 chart 測試的 `EXPORTER_VERSION` 擋下，直到有人對照過並改那個常數 |
| `observability.collector.image.digest` | 見 `values.yaml` | 必填（版本固定），multi-arch index 的 digest，含 arm64；同一版本的新 digest 由 Renovate 單獨更新 |
| `observability.collector.image.pullPolicy` | `IfNotPresent` | |
| `observability.collector.clickhouse.user` | `trustiot_collector` | collector 的 ClickHouse 帳號，由 migration Job 建立並授予 `trustiot_otel_collector` 角色，密碼由 Job 產生在 `<release>-otel-collector-password`（key 是帳號名稱）；不能是 `default` 或角色本身的名稱 |
| `observability.collector.forward.enabled` | `false` | 把遮蔽後的資料另外以 OTLP 送到客戶的 SIEM |
| `observability.collector.forward.endpoint` | `""`，開啟時必填 | `http` 時是 OTLP/HTTP 的 base URL（exporter 自己加 `/v1/logs`、`/v1/traces`），`grpc` 時是 `https://主機:埠`；不能帶帳密；SaaS 只接受 https。`http://` 只用在客戶內網可信的路段 |
| `observability.collector.forward.protocol` | `http` | `http`（OTLP/HTTP protobuf）或 `grpc` |
| `observability.collector.forward.signals` | `[logs]` | 要轉送的訊號：`logs`、`traces` |
| `observability.collector.forward.authHeader.name` | `""` | SIEM 驗證用的標頭名稱，例如 `Authorization`；空白代表不送 |
| `observability.collector.forward.authHeader.valueSecret.name` | `""`，設了標頭名稱時必填 | 放整個標頭值（例如 `Bearer <token>`）的既有 Secret；服務的自訂 `env` 不能讀它 |
| `observability.collector.forward.authHeader.valueSecret.key` | `value` | |
| `observability.collector.forward.caSecret.name` | `""` | SIEM 用私有 CA 時才設，endpoint 必須是 https |
| `observability.collector.forward.caSecret.key` | `ca.crt` | |
| `observability.collector.resources` | requests `cpu: 50m`、`memory: 128Mi`；limits `cpu: "1"`、`memory: 512Mi` | `memory_limiter` 以記憶體上限的 80% 為界，超過時先拒收（服務端會重試）；2026-10-11 本機實測閒置約 38 MiB |
| `observability.collector.podSecurityContext` | `runAsNonRoot: true`、`seccompProfile: RuntimeDefault` | 映像的 `USER` 是數字 UID 10001，不固定 UID，OpenShift 可以指派；限制同 `serviceDefaults.podSecurityContext` |
| `observability.collector.nodeSelector` | `{}` | 映像是 multi-arch，不限架構 |
| `observability.collector.tolerations` | `[]` | |

URL 欄位都不接受 `user:password@` 這種帶帳密的寫法。

## AI 閘道

`services.ai-gateway`（`services/ai-gateway`，決策 15、28，Laysi/trustiot-v2#536）保管 AI 供應商的金鑰，依模型角色以供應商的原生協定轉送請求。它與產品無關，設定前綴是 `AI_GATEWAY`，不讀平台設定（`TRUSTIOT_*`），所以 Postgres 的位置與帳號由 chart 以它的前綴另外給（下方「資料庫」）；chart 從 `ai` 產生它的設定，放在 ConfigMap `<release>-ai-gateway`，只以 `envFrom` 交給它的 container（`templates/ai-gateway.yaml`，規則在 `templates/_ai-gateway.tpl` 的 `trustiot.aiGateway.config`）。閘道只在啟動時讀設定，所以 pod 帶這份 ConfigMap 的 checksum，設定一變就滾動更新。

ConfigMap 只有名稱與上限，沒有金鑰（chart 測試檢查）：

| 變數 | 來源 | 何時有 |
| --- | --- | --- |
| `AI_GATEWAY__PROVIDERS__<供應商>__PROTOCOL` | `anthropic` 是 `anthropic-messages`，`openai` 是 `openai-decisions` | 該供應商的 `apiKeySecret.name` 有值時 |
| `AI_GATEWAY__PROVIDERS__<供應商>__KEY_ENV` | `TRUSTIOT_AI_ANTHROPIC_API_KEY`、`TRUSTIOT_AI_OPENAI_API_KEY`：chart 以 `secretKeyRef` 從 `apiKeySecret` 放進這兩個變數（「密鑰的環境變數」） | 同上 |
| `AI_GATEWAY__PROVIDERS__<供應商>__BASE_URL` | `ai.providers.<供應商>.baseUrl` | 同上，而且有值時；沒有時閘道用供應商自己的 API |
| `AI_GATEWAY__ROLES__<角色>__MODELS` 與角色的上限 | `ai.roles.<角色>`，見下表 | 每個角色 |
| `AI_GATEWAY__HTTP__REQUEST_TIMEOUT` | `ai.requestTimeout` | 一律 |
| `AI_GATEWAY__SHUTDOWN__DRAIN_TIMEOUT` | `services.ai-gateway.drainSeconds` 減 10 秒（預設 300 秒）：service-kit 的 5 秒 drain delay 加上取消剩餘工作與送出遙測的 3 秒，要在 SIGTERM 後的 `drainSeconds` 內做完，再留 2 秒 | 一律 |

collector 開啟時另有 `AI_GATEWAY__TELEMETRY__OTLP_ENDPOINT`（與其他服務的 `TRUSTIOT__TELEMETRY__OTLP_ENDPOINT` 相同，只是前綴不同）。`ai.roles.<角色>` 的欄位（閘道的鍵與預設見 `services/ai-gateway/README.md`「設定」）：

| 欄位 | 變數（`<P>` 是 `AI_GATEWAY__ROLES__<角色>__`） | 規則 |
| --- | --- | --- |
| `models` | `<P>MODELS`，以逗號串接 | 必填，1 到 8 個、不重複，`<供應商>/<模型 ID>`，第一個是首選；模型 ID 是 1 到 256 個可列印 ASCII，不含空白與逗號；供應商要有 `apiKeySecret.name` |
| `maxConcurrent` | `<P>MAX_CONCURRENT` | 1 到 10000（閘道預設 32） |
| `queueWait` | `<P>QUEUE_WAIT` | 最多 `1h`（預設 0） |
| `timeouts.connect`、`firstToken`、`streamIdle`、`streamTotal`、`response` | `<P>TIMEOUTS__CONNECT` 等 | 大於 0、最多 `1h`，`streamTotal` 最多 `24h`（預設 10 秒、60 秒、60 秒、60 分、300 秒） |
| `maxResponseBytes` | `<P>MAX_RESPONSE_BYTES` | 1024 到 268435456（預設 32 MiB） |
| `maxEventBytes` | `<P>MAX_EVENT_BYTES` | 1024 到 `maxResponseBytes`（預設 4 MiB；`maxResponseBytes` 沒設時以 32 MiB 比較） |

角色名稱是 1 到 32 個小寫英文字母、數字或 `_`，字母開頭。時間寫成一個數字加一個單位（`500ms`、`30s`、`10m`、`1h`）。

**資料庫**（Laysi/trustiot-v2#605）：閘道把管理員更換的模型清單、每個請求確認的設定世代與變更紀錄存在 Postgres 的 schema `ai_gateway`（`services/ai-gateway/README.md`「資料庫」），所以預設的 `mesh.calls.ai-gateway` 是 `[postgres]`。chart 照其他用 Postgres 的服務處理它：掛 Postgres 的 CA、`bundled` 時 `<release>-postgres` 的 NetworkPolicy 放行它，migration Job 為 `ai_gateway_app` 產生密碼到 Secret `<release>-ai-gateway-password`；只是變數用閘道的前綴（`templates/_service.tpl`）：

| 變數 | 來源 |
| --- | --- |
| `AI_GATEWAY_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE` | 與平台 ConfigMap 的 `TRUSTIOT_POSTGRES_*` 相同的值（`templates/_postgres.tpl` 的 `trustiot.postgres.connection`），`bundled` 與 `external` 都一樣 |
| `AI_GATEWAY_POSTGRES_CA_FILE` | `/etc/trustiot/postgres-ca/ca.crt` |
| `AI_GATEWAY__DATABASE__USERNAME`、`AI_GATEWAY__DATABASE__PASSWORD` | `ai_gateway_app` 與 Secret `<release>-ai-gateway-password` 的同名 key |

這些名稱保留給 chart，自訂 `env` 不能設定。閘道讀不到 Postgres 時以 503 拒絕 AI 呼叫，readiness 也包含 migration 與 Postgres 的檢查；`mesh.calls.ai-gateway` 沒有 `postgres` 時閘道缺少資料庫設定、不會啟動，所以渲染失敗（見下一段）。

開啟 `ai-gateway` 時，閘道會拒絕啟動的設定都讓渲染失敗，不論有沒有做 schema 驗證，所以 values 的錯誤不會變成一直重啟的 pod：`credentials` 沒有 `ai`、`mesh.calls.ai-gateway` 沒有 `postgres`、沒有角色、角色或模型不合規則、用到沒有 `apiKeySecret.name` 的供應商、上限超出範圍、某個角色的 `queueWait` 加上 `timeouts.response` 與 `timeouts.firstToken` 較長者放不進 `ai.requestTimeout` 減去十分之一（最多 2 秒；chart 以毫秒計算並無條件進位，所以不會比閘道寬鬆）、`drainSeconds` 不在 11 到 3605 之間。chart 只認得 `anthropic` 與 `openai` 兩個供應商。

閘道在收到請求之前不連供應商，readiness 也不依賴供應商連得到：沒有金鑰的 CI 與 nightly 以假的 Secret 值就能讓它就緒（`scripts/nightly/api.ts` 的 `AI_GATEWAY_SMOKE`），但這不證明金鑰能用。確認一把金鑰能用要送一次真的請求（`services/ai-gateway/README.md` 的「範例：一次最小的呼叫」）。內部 API 只開給 mesh 授權的三個呼叫端，所以從叢集外送時以 `kubectl port-forward` 連進閘道的 pod：port-forward 進入 pod 自己的網路命名空間，不經 ztunnel 與 NetworkPolicy，需要能對那個 namespace 的 pod 做 `pods/portforward` 的權限，與 `kubectl exec` 同級，只給維運的管理者：

```sh
kubectl -n <namespace> port-forward deploy/<release>-ai-gateway 18080:8080
# 另一個終端機；main 換成用那個供應商的角色
curl -sS -D - -X POST "http://127.0.0.1:18080/internal/roles/main/v1/messages" \
  -H 'content-type: application/json' -H 'ai-gateway-tenant: key-check' \
  -d '{"max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

## migration Job

`migrate.enabled: true`（預設，SaaS 與地端相同）部署 `templates/migrate.yaml`，程式是 [tools/migrate](../../tools/migrate/README.md)（Laysi/trustiot-v2#60，決策 4、14、29，清單 M8）。每次執行都是冪等的：設定沒變時再跑一次，資料庫裡什麼都不變（測試比對整份 catalog）。

Job 做的事，依序：

1. 以 migration 角色（`bundled` 是 owner `trustiot_owner`，`external` 是 `postgres.external.migrationSecret`）用 verify-full 連上 Postgres，取得 session 層級的 advisory lock，整次執行都持有，ClickHouse 的部分也在鎖內；同時有另一次執行時就等它結束（`migrate.wait.lock`）。資料庫還沒起來時先等（`migrate.wait.databases`）。migration 角色必須擁有資料庫、有 CREATEROLE，不能是 superuser、不能有 BYPASSRLS，也不能擁有其他資料庫，不符就停止。
   接著在鎖內填密碼 Secret（「產生的密碼」）：缺的、不能用的、屬於較舊的 `migrate.passwordGeneration`、或在不是 Job 填過的 Secret 物件裡的 key 產生新密碼寫進去，之後的步驟把每個角色與帳號設成 Secret 裡的密碼。
2. 套用 `crates/service-kit/sql/database.sql`（收回 PUBLIC 的 TEMPORARY 等）。
3. 為每個有 Postgres migration 的服務建立角色與 schema：`<服務>_owner`（不能登入，擁有 schema 與每張表，migration 以它執行）、`<服務>_app`（服務登入用，不擁有任何物件，沒有 BYPASSRLS，`search_path` 是自己的 schema）、需要時還有 `<服務>_platform`（決策 29 的平台角色）。登入角色的密碼來自該服務的密碼 Secret（見下方「產生的密碼」）：先以這個密碼實際登入，登不進去才設定（只送 SCRAM-SHA-256 的 verifier，明文不進 SQL），所以密碼沒變時什麼都不改。已存在的角色若有 SUPERUSER、BYPASSRLS、CREATEROLE、多了成員關係、在叢集的其他資料庫擁有物件或權限、有 `search_path` 以外的角色設定或這個資料庫專屬的設定，或服務角色能在自己的 schema 建立物件、能建立暫存表，Job 停止並寫出原因，不自動修改。所有服務的角色都建好之後，Job 檢查服務的登入角色能不能連到叢集的其他資料庫（`datallowconn` 的資料庫，例如 `postgres`、`template1`）：能連時每個資料庫記一行警告（日誌的 WARN），不停止（見下方「叢集的其他資料庫」）。
4. 以 `<服務>_owner` 角色依序套用該服務 `migrations/postgres/` 的每個檔案，一個檔案一個交易，記在 `trustiot_migrate.applied`；已套用的檔案內容若改了，Job 停止（migration 只能新增檔案）；檔案結束時若已離開原本的交易或角色（`END`、`COMMIT AND CHAIN`、`SET LOCAL ROLE` 等），Job 停止且不記錄這個檔案。outbox 的表與權限（`crates/outbox`、`crates/service-kit/sql/outbox-event.sql` 與 `outbox-aggregate.sql`）依 tools/migrate 的服務清單接上；每租戶角色的 hook 在這一步依租戶表建立或補回角色（目前沒有服務註冊，AGE 角色在 Laysi/trustiot-v2#176）。
5. ClickHouse：以有 access management 的帳號（`bundled` 是 `clickhouse.bundled.adminUser`，密碼經 Kubernetes API 從 ClickHouse 的 init container 填的 Secret 讀取，見「叢集內的 ClickHouse」；`external` 是 `clickhouse.external.migrationSecret`）套用 `services/ingest/migrations/clickhouse/`，每個檔案記在 ClickHouse 自己的 `trustiot_migrate.applied`；collector 開啟時建立 `observability.collector.clickhouse.user`，只授予 `trustiot_otel_collector` 角色（設為預設角色），密碼來自 collector 的密碼 Secret。帳號已存在但多了其他角色、權限或設定時，Job 停止，由管理者決定是否刪除；`trustiot_otel_collector` 角色多了 `0006_logs.sql` 以外的權限或角色時也停止。

**順序**：每個服務與 collector 的 pod 有一個 init container `wait-for-migrations`（同一個映像的 `migrate wait-for-job`），每 2 秒向 Kubernetes API 讀一次這個 Job，Job 完成才讓服務的容器啟動；Job 失敗時 init container 也失敗，pod 依 kubelet 的退避重試，修好之後自動繼續。所以 `helm install --wait` 與 `helm upgrade --wait` 都等到 Job 完成、服務就緒才結束，服務不會在比自己舊的 schema 上啟動。Job 不是 Helm hook：叢集內的 Postgres 與 ClickHouse 由同一次安裝建立，`pre-install` hook 會在資料庫存在之前執行；`post-install` hook 又要等 `--wait` 等的服務就緒，而服務在等 Job，兩者互等。

**Job 何時再跑**：Job 的名稱是 `<release>-migrate-<8 位 hash>`，hash 來自 Job 的整份 spec（映像、設定、Secret 的名稱、`migrate.revision`、`migrate.passwordGeneration`、每個在 `mesh.calls` 列出 `postgres` 的服務的映像參照）與平台設定的 checksum。升級時這些有任何改變，就產生新名稱的 Job（Helm 刪掉舊的），服務的 init container 也改等新的 Job，所以服務跟著滾動更新；都沒變時沿用已完成的 Job，服務不重啟。換新版本（映像的 tag 隨 `Chart.appVersion`）一定會跑。Secret 的內容改了（例如 migration 帳號的密碼）chart 看不到，要同時改 `migrate.revision`；Job 產生的密碼則以 `migrate.passwordGeneration` 輪替。完成的 Job 保留到下一次換名為止（沒有 `ttlSecondsAfterFinished`），之後才啟動的 pod（重啟、擴充）仍然找得到它。升級進行中，舊版的 pod 若被重建，它的 init container 會等已被刪除的舊 Job，直到滾動更新把它換掉；這時它的日誌寫出等的是哪個 Job、可能的原因與處理方式。資料庫從備份還原、或改接另一個資料庫之後，也要改 `migrate.revision`：角色與密碼是叢集層級的，不在資料庫備份裡，Job 再跑一次才會補回。

**版本一致**：Job 的映像帶著每個服務的 migration，而 Job 只在自己的 spec 改變時再跑。用 Postgres 的服務換了映像（只換 digest 也算）時，Job 的 spec 跟著變，所以 Job 再跑一次、服務等它跑完才啟動；但 Job 套用的是它自己映像裡的 migration。所以在 `mesh.calls` 列出 `postgres` 的服務，映像的 tag（`services.<name>.image.tag`、`serviceDefaults.image.tag`，空白時 `Chart.appVersion`）必須等於 `migrate.image.tag`，不同時 chart 渲染失敗；否則服務會在另一個版本的 schema 上啟動，沒有 Job 替它套用 migration。只以 digest 換掉服務映像（修補）時，`migrate.image.tag` 與 `migrate.image.digest` 也要換成同一次發佈的。

**一個 Postgres 叢集一個 TrustIoT 資料庫**：Postgres 的角色是叢集層級的，Job 建立的角色名稱（`identity_app` 等）不帶實例名稱，所以同一個 Postgres 叢集上的兩個 TrustIoT 資料庫會共用同一組服務角色、密碼與權限，一個實例的帳號就能讀另一個實例的資料。Job 拒絕這種部署：migration 角色擁有第二個資料庫時停止；另一個 migration 角色遇到已在其他資料庫擁有物件或權限、或不是它建立的角色時也停止，不修改它。`bundled` 每個 release 有自己的 Cluster；`external` 的每個實例要用自己的 Postgres 叢集，也不要以 `ADMIN OPTION` 把一個實例的角色分給另一個（[docs/runbooks/migrations.md](../../docs/runbooks/migrations.md)）。

**叢集的其他資料庫**（Laysi/trustiot-v2#400）：Postgres 在 initdb 之後讓 `PUBLIC` 能連 `postgres` 與 `template1`，並能在 `postgres` 建立暫存表與暫存函式；服務角色連得上 `template1` 時，它開著的 session 也會讓 `CREATE DATABASE` 失敗。服務角色不需要這兩個資料庫，所以要收回 `PUBLIC` 在這兩個資料庫的 `CONNECT` 與 `TEMPORARY`（PostgreSQL 15 起 `PUBLIC` 本來就不能在 `public` schema 建立物件）。migration 角色不擁有這兩個資料庫，收不回來：

- `bundled`：Cluster 在 bootstrap 時以 superuser 執行 `files/postgres/system-databases.sql`（見「叢集內的 Postgres」）。
- `external`：由資料庫管理者以 superuser 執行同樣的收回，步驟在 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md) 的「`postgres` 與 `template1`：收回 PUBLIC 的 CONNECT 與 TEMPORARY」。沒有收回時，Job 每次執行都對每個還讓服務角色連得上的資料庫記一行警告，例如 `database postgres: service roles assets_app, … may connect to it and create temporary tables there; …`，但照常完成：客戶自己的應用也許需要 `PUBLIC` 連得上某個資料庫，這是管理者的決定。

**權限**：Job 的 pod 與 ServiceAccount 都是 `automountServiceAccountToken: false`；有密碼 Secret 要填時，只有 Job 的容器掛一個 1 小時效期的 projected token 與叢集的 CA，Role `<release>-migrate` 只讓它 `get` 與 `patch` 這些 Secret（以 `resourceNames` 列名），叢集內 ClickHouse 的管理帳號 Secret 則只能 `get`：沒有 `create`（無法以名稱限制，而且能替 namespace 裡任何 ServiceAccount 建 token Secret）、`list`、`watch`，也碰不到別的 Secret。另外兩個碰 Secret 的 Role 是 `<release>-clickhouse`：只讓 ClickHouse 的 ServiceAccount `get` 與 `patch` 它的管理帳號 Secret，以及 `<release>-identity-signing-keys`：只讓身分服務的 ServiceAccount `get` 與 `patch` 它的簽章金鑰 Secret 與公鑰 ConfigMap（身分服務 pod 的 init container 與輪替 CronJob 用它，見「內部身分」）；chart 的其他 Role 都不碰 Secret，也不能寫 ConfigMap（測試檢查）。服務與 collector 的 pod 仍然 `automountServiceAccountToken: false`；只有 init container 掛同樣的 projected token 與 CA（`kube-root-ca.crt`），Role `<release>-migrate-status` 只讓服務與 collector 的 ServiceAccount `get` 這一個 Job，沒有任何新的 Secret 權限。chart 的 NetworkPolicy 都只限制連入，init container 可以連到 API server。Job 的 pod 在 ambient mesh 內，叢集內的 ClickHouse 以它的 ServiceAccount 放行它；`<release>-postgres` 的 NetworkPolicy 也放行它；沒有任何東西能連進 Job 的 pod。

**服務的資料庫帳號**：在 `mesh.calls` 列出 `postgres` 的服務，chart 給它 `TRUSTIOT__DATABASE__USERNAME=<服務>_app` 與從自己的 Secret `<release>-<服務>-password` 同名 key 讀的 `TRUSTIOT__DATABASE__PASSWORD`。outbox 的消費端還要以平台角色讀 producer 的事件（決策 29，`crates/outbox`），寫全平台事件的產生端也要以平台角色寫，列在 `templates/_migrate.tpl` 的 `trustiot.migrate.platformLogins`（目前是 `assets`，Laysi/trustiot-v2#65，保存事件紀錄、以平台角色讀事件的 `identity`，Laysi/trustiot-v2#130，`telemetry-query`，Laysi/trustiot-v2#119，以及寫出廠登錄事件的 `certs`，Laysi/trustiot-v2#79）：同一個 Secret 多一個 `<服務>_platform` key，服務多拿到 `TRUSTIOT__DATABASE__PLATFORM_USERNAME=<服務>_platform` 與 `TRUSTIOT__DATABASE__PLATFORM_PASSWORD`。不在清單裡的服務，平台角色沒有綁密碼，Job 讓它維持 `NOLOGIN`。這四個名稱保留給 chart，自訂 `env` 不能設定。設定前綴不是 `TRUSTIOT` 的服務（`ai-gateway`）以自己的前綴拿到同樣的變數，另加 `<前綴>_POSTGRES_*`（見「AI 閘道」）。Job 開始前先確認這些角色都在 tools/migrate 的服務清單裡，否則什麼都不改就停止；清單與 `PRODUCT` 的一致由 `tools/migrate/tests/registry.rs` 檢查。

**產生的密碼**（docs/architecture/secrets.md）：服務登入角色與 collector 的 ClickHouse 帳號是 Job 自己建立的內部帳號，密碼也由它產生，沒有人建立、複製或輪替它們。

- chart 為每個用 Postgres 的服務與 collector 各宣告一個 Secret（`templates/passwords.yaml`）：沒有 `data`、`stringData`，帶 `helm.sh/resource-policy: keep` 與標籤 `trustiot.io/filled-by: migrate`。每個消費者一個 Secret，所以節點的 kubelet 只讀得到排在它上面的 pod 的密碼；以平台角色登入的服務，兩個密碼在它自己的同一個 Secret（`<服務>_app` 與 `<服務>_platform` 兩個 key），Job 的 Role 對每個 Secret 只列一次。`migrate.enabled` 關掉時也宣告，由取代 Job 的人填入（只有叢集測試這樣做）。
- Job 在 migration 鎖內讀每個 Secret；缺的 key、不能用的值、`migrate.passwordGeneration` 大於 annotation `trustiot.io/password-generation` 記的代、或 annotation `trustiot.io/password-secret-uid` 不等於 Secret 的 `metadata.uid`（不是 Job 填過的那個物件）時，以作業系統的 CSPRNG 產生 256 位元的新密碼（64 個十六進位字元），用一個帶 `resourceVersion` 的 JSON merge patch（field manager `trustiot-migrate`）寫回，同一個 patch 記下代與 uid（chart 不渲染這兩個 annotation）；Secret 在讀與寫之間被別人改過時 API 拒絕這個 patch，Job 重讀再決定，不會覆蓋別人的寫入。之後把每個角色與帳號設成 Secret 裡的值。日誌只寫角色、帳號與 Secret 的名稱，不寫密碼、密碼的 hash 或 Secret 的資料。
- Helm 不擁有這些 Secret 的資料：升級與 rollback 渲染出的 Secret 都沒有 `data`，所以 Job 寫的值保留下來（`tests/e2e/postgres.e2e.ts` 在叢集上以釘選的 helm 檢查升級與 rollback 後資料不變）。Flux 的 HelmRelease 沒有開 drift detection，升級失敗時的 rollback 也一樣保留；將來開 drift detection 時見 [deploy/flux/README.md](../flux/README.md)。
- 輪替：調大 `migrate.passwordGeneration`。Job 名稱跟著變，新的 Job 產生並設定新密碼，服務與 collector 的 pod 以新的 Job 名稱滾動更新、讀到新密碼。從 Job 設定新密碼到舊 pod 被換掉之間，舊 pod 已經開著的 Postgres 連線不受影響（Postgres 只在登入時驗密碼），但舊 pod 開新連線會被拒；舊的 collector 寫 ClickHouse 時也會被拒（新連線要驗密碼，外部 ClickHouse 走 HTTPS 時每個請求都驗），在這段時間重試（最多 5 分鐘），新的 collector 就緒後恢復。Postgres 一個角色只有一個密碼，要完全避免這段時間得輪流使用兩個角色，目前不做；需要零中斷時在低流量時段輪替。代只往上走：`helm rollback`（或 Flux 升級失敗後的 rollback）渲染出較小的值時 Job 保留現有的密碼，記的代不變，之後再升級回來也不換。
- 只信任 Job 自己填過的 Secret 物件：Job 寫入時記下 Secret 的 uid，之後讀到的 uid 不同（有人刪掉 Secret 再建一個同名的、帶 Helm 的 ownership metadata 讓 `helm install` 接手，或從備份還原）就把每個 key 當成不能用，產生新密碼。只能建立 Secret 的人事先不知道 uid，所以無法讓 Job 採用他選的密碼；能 patch 這些 Secret 的主體（即使沒有 `get`）能寫入自選的密碼或加上 replicator 的 annotation，所以 patch 權限等同能取得密碼，只能給受信任的主體；chart 只給 Job。
- 資料遺失（有人刪了 key 或整個 Secret、或以 `helm upgrade --force-replace` 換掉了 Secret；這個 chart 不要用 `--force-replace`，原因見 runbook）：改 `migrate.revision` 再升級。Helm 升級時重建被刪掉的 Secret，Job 重跑時為缺的 key 產生新密碼並設定，服務重啟後以新密碼登入。Job 不能建立 Secret，所以 Secret 不存在時它停止並寫出這個處理方式。
- 停用某個服務時，下一次 Job 讓它的登入角色 `NOLOGIN`（角色、它擁有的物件與權限都留著，重新啟用時 Job 先設密碼再恢復 LOGIN）；關掉 collector 或改它的帳號名稱時，舊的 ClickHouse 帳號改成 `HOST NONE`。Secret 留著（`keep`），Role 不再列它；裡面的密碼已經登不進去，清掉的步驟見 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md) 的「停用服務之後留下的 Secret」。

**失敗時**：Job 在 `migrate.backoffLimit` 次重試後失敗，服務停在 `Init`，舊版的 pod 繼續服務。看 `kubectl -n <namespace> logs job/<release>-migrate-<hash>`（JSON 行，只寫 stdout）；每一項變更與停止的原因都寫在裡面，不含密碼。處理方式見 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md)。

### 第一位平台管理員

`migrate.enabled` 與身分服務都啟用時，chart 另外渲染 CronJob `<release>-platform-admin-bootstrap`（`templates/platform-admin-bootstrap.yaml`，Laysi/trustiot-v2#582）。它不會自己執行（`suspend: true`，排程是不存在的 2 月 31 日），只是操作者建立第一位平台管理員時 `kubectl create job --from=cronjob/...` 的範本；步驟、結束碼與復原在 [docs/runbooks/platform-admin.md](../../docs/runbooks/platform-admin.md)。

- 執行同一個 migrate 映像的 `migrate bootstrap-platform-admin`（[tools/migrate](../../tools/migrate/README.md)），以 migration 角色連 Postgres：與 Job 相同的 CA、`TRUSTIOT__POSTGRES__*` 與 `migrate.wait.databases`，沒有 `TRUSTIOT__PASSWORDS__*`、ClickHouse 設定與 projected token（它不碰 Kubernetes API）。
- 電子郵件地址只從 stdin 來：容器 `stdin: true`、`stdinOnce: true`，操作者以 `kubectl attach -i` 寫入一行。chart、values、Job 的參數與環境變數裡都沒有地址，程式也不把它寫進日誌。
- pod 用 migration Job 的 ServiceAccount（不掛 token）與 selector 標籤（`app.kubernetes.io/component: migrate`，另加 `trustiot.io/task: platform-admin-bootstrap`），在 ambient mesh 內，所以 `<release>-postgres` 的 NetworkPolicy 放行它，migration Job 的 AuthorizationPolicy 與 NetworkPolicy 讓任何東西都連不進它的 pod（上方呼叫矩陣的 `migrate` 一條；測試比對每個 policy 的 selector 對兩者的結果相同）。
- 一次嘗試（`backoffLimit: 0`）：第二個 pod 會等一個沒人再寫的地址。`activeDeadlineSeconds: 900` 結束沒人 attach 的 pod；Job 與 pod（日誌有使用者 ID）在結束一天後刪除（`ttlSecondsAfterFinished: 86400`），runbook 要求操作者看完日誌就刪。`kubectl create job --from=cronjob/...` 讓 CronJob 成為 Job 的 controller，CronJob controller 即使在暫停中也會刪掉超過保留數的已結束 Job，所以 `successfulJobsHistoryLimit` 與 `failedJobsHistoryLimit` 都是 1：設 0 時成功的 Job 結束後幾秒就被刪掉，操作者讀不到結束碼。CronJob 名稱最多 52 個字元，release 名稱太長時截短前綴。`services.platform-admin-bootstrap` 保留給它。

## 叢集內的 Postgres

`postgres.mode: bundled`（預設，SaaS 與地端相同）部署 `templates/postgres.yaml`：一個由 CloudNativePG 管理的 Postgres，映像是我們的 PostgreSQL 18 加 Apache AGE（[deploy/images/postgres](../images/postgres/README.md)，決策 21、清單 M8）。

叢集要先裝好 CloudNativePG operator 與它的 CRD（[deploy/cloudnative-pg](../cloudnative-pg/README.md)），chart 不安裝它們：CRD、webhook 與 ClusterRole 是整個叢集共用的，一個叢集只能有一份，和 Istio 一樣是平台元件（決策 5 的 2026-10-12 補充）。沒有 operator 時 `helm install` 因為不認得 `postgresql.cnpg.io/v1` 而失敗，不會裝出半套。

chart 產生的物件（`<cluster>` 是 `<release>-postgres`；CloudNativePG 限制 Cluster 名稱 50 個字元，超過時保留 release 名稱的前段，在 `-postgres` 前加上完整名稱 SHA-256 的前 8 個字元，同一個 namespace 裡開頭相同的 release 不會撞名）：

| 物件 | 內容 |
| --- | --- |
| ImageCatalog `<cluster>` | PostgreSQL major 18 對應的映像（`postgres.bundled.image`）。映像的 tag 是 TrustIoT 的版本，CloudNativePG 從這裡得知 major |
| Cluster `<cluster>` | 一個 instance、不建 PodDisruptionBudget（單一 instance 的 PDB 會擋住節點 drain）、不開 superuser 存取；bootstrap 建立資料庫 `trustiot` 與它的 owner `trustiot_owner`，locale 用 PostgreSQL 內建的 `C.UTF-8`（排序與字元分類不隨作業系統的 C library 改變，換映像不會讓索引失效）；bootstrap 時以 `postInitSQL` 收回 `PUBLIC` 在 `postgres` 與 `template1` 的 `CONNECT`、`TEMPORARY`（見下方「其他資料庫」）；`shared_preload_libraries` 含 `age`；pg_hba 加上 `hostnossl all all all reject`；pod 帶 `istio.io/dataplane-mode: none` |
| Database `<cluster>-trustiot` | 由 operator 以 superuser 在 `trustiot` 建立 AGE extension，版本變更時執行 `ALTER EXTENSION age UPDATE`；`databaseReclaimPolicy: retain`，刪掉這個物件不會刪資料庫 |
| NetworkPolicy `<release>-postgres` | 選 Postgres pod（`cnpg.io/cluster`、`cnpg.io/podRole: instance`），只放行在 `mesh.calls` 列出 `postgres` 的服務與 migration Job 連 5432，以及 operator（`postgres.bundled.operator`）連 instance manager 的 8000 |

operator 依 Cluster 再產生：pod 與 PVC `<cluster>-1`；Service `<cluster>-rw`（永遠指向 primary，服務連這個）、`<cluster>-ro`、`<cluster>-r`；Secret `<cluster>-ca`、`<cluster>-server`、`<cluster>-replication`、`<cluster>-app`。

- **TLS（清單 M3）**：operator 產生自己的 CA（`<cluster>-ca`）與伺服器憑證（`<cluster>-server`，主機名稱含 `<cluster>-rw.<namespace>.svc`），效期 90 天、到期前 7 天自動換發，不停機；CA 換發時沿用同一把私鑰（CloudNativePG 1.30.1 的 `internal/controller/cluster_pki.go`）。服務的平台設定是 `TRUSTIOT_POSTGRES_HOST=<cluster>-rw.<namespace>.svc`、`TRUSTIOT_POSTGRES_SSLMODE=verify-full`；用 Postgres 的服務掛上 `<cluster>-ca` 裡的 `ca.crt`，volume 的 `items` 只選這個 key，`ca.key` 不會進任何服務的容器。kubelet 會更新掛載的檔案，service-kit 每條新連線都重讀 CA 檔，所以換發不必重啟服務。不走 TLS 的 TCP 連線，不論用戶端的 `sslmode`，都被伺服器的 pg_hba 拒絕。
- **帳號（決策 4、29）**：沒有 superuser 密碼，也沒有 `<cluster>-superuser` Secret，`postgres` 角色只能在 pod 內經 local socket 登入，operator 就是這樣管理資料庫。owner `trustiot_owner` 是登入角色，沒有 SUPERUSER、BYPASSRLS；bootstrap 時以 `postInitApplicationSQL` 給它 CREATEROLE（Laysi/trustiot-v2#60），因為它也是 migration 角色，要建立每個服務的角色。PostgreSQL 16 起 CREATEROLE 不能建立 superuser、BYPASSRLS 或 REPLICATION 角色，也不能改它沒有 ADMIN 的角色。密碼由 operator 產生在 `<cluster>-app`，只給 migration Job 用。2026-10-13 之前 bootstrap 的 Cluster 沒有 CREATEROLE，要照 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md) 補一次。operator 產生的 Secret（`<cluster>-ca`、`-server`、`-app`、`-replication`、`-superuser`）任何服務的自訂 `env` 都讀不到，寫了就渲染失敗；服務只經 chart 拿到 CA 憑證。每個服務自己的角色、密碼與 `crates/service-kit/sql/database.sql` 的權限設定由 migration Job 處理（見「migration Job」）。
- **其他資料庫（Laysi/trustiot-v2#400）**：bootstrap 時 operator 以 superuser 在 `postgres` 依序執行 `files/postgres/system-databases.sql` 的每一句（`bootstrap.initdb.postInitSQL`，在建立 `trustiot` 之前）：收回 `PUBLIC` 在 `postgres` 與 `template1` 的 `CONNECT` 與 `TEMPORARY`，只把 `postgres` 的 `CONNECT` 依名稱給 CloudNativePG 自己需要它的兩個角色 `streaming_replica`（`pg_rewind`、replica 檢查 primary、replica 的 replication slot）與 `cnpg_metrics_exporter`（監控查詢）。operator 在 instance 第一次啟動時才建立這兩個角色，比 `postInitSQL` 晚，所以檔案先以 operator 建立它們的方式建好（operator 接受已存在的角色）。superuser 不受資料庫權限限制，operator 的其他連線（instance manager、Database 與角色的調和、備份）都以它經 pod 內的 local socket 進行；每個連線查到的來源見 [deploy/cloudnative-pg](../cloudnative-pg/README.md) 的「其他資料庫的權限」。權限記在 catalog 裡，replica 與從備份還原的 Cluster 都帶著它；`trustiot` 在之後才從 `template1` 建立，而新資料庫不複製 `template1` 的權限，所以服務照常連得上。語句只有一套：檔案是來源（空行分隔語句、`--` 開頭的行是註解，每句以 `;` 結束），`templates/postgres.yaml` 把每一句寫成 `postInitSQL` 的一項，因為 template 不讀 chart 的檔案（`scripts/ci/check-secret-handling.ts`）；chart 測試在兩者不同時失敗，`tools/migrate/tests/system_databases.rs` 以同樣的切法在 PostgreSQL 18 上逐句執行檔案，runbook 也執行同一個檔案。這之前 bootstrap 的 Cluster 照 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md) 補一次。
- **網路**：Postgres pod 不加入 ambient mesh：每條連線本身已經是 verify-full 的 TLS，而在 mesh 外的 operator 要連每個 instance 的 8000，放進 mesh 會被 AuthorizationPolicy 擋下。namespace 的預設拒絕照樣套用到它，`<release>-postgres` 只放行上表的來源；kubelet 的探針從 pod 所在的節點來，NetworkPolicy 不擋節點本身的連線。namespace 內沒有列 `postgres` 的服務與其他 pod 都連不到。
- **規格**：資源與參數見「外部依賴」的 `postgres.bundled.*`，兩種型態相同。
- **連線預算**：見下方「連線預算」，`max_connections` 依它設定。
- **刪除**：Cluster 與 ImageCatalog 帶 `helm.sh/resource-policy: keep`，`helm uninstall` 或 Flux 移除 release 時保留資料庫（CloudNativePG 刪 Cluster 時會一起刪掉 PVC，資料就沒了）。確定要刪資料時，明確刪除 Cluster：`kubectl -n <namespace> delete cluster <cluster>`，再刪 ImageCatalog。PVC 不會在 `kubectl delete` 回來時就消失：Kubernetes 在 Cluster 刪除後於背景回收它，等 Postgres 的 pod 停止才刪得掉；要確認時以 `kubectl -n <namespace> get pvc -l cnpg.io/cluster=<cluster>` 查到沒有為止。
- **升級**：映像隨 chart 版本更新，單一 instance 會短暫中斷。AGE 升級時，`deploy/images/postgres/Containerfile` 的 `AGE_VERSION` 與 `templates/_postgres.tpl` 的 `trustiot.postgres.ageVersion` 一起改（chart 測試比對兩者），operator 執行 `ALTER EXTENSION`。PostgreSQL major 升級要一起改映像的基底與 `trustiot.postgres.majorVersion`（測試同樣比對），步驟要另外驗證，還沒做過。
- **備份與 PITR** 尚未設定，之後以同一個 operator 加入（Laysi/trustiot-v2#134）。

磁碟區大小沒有預設值，每個部署在安裝實例的覆寫檔設定 `postgres.bundled.storage.size`（資料與 WAL 共用一個磁碟區）。業務資料與知識圖的大小要等服務上線後才量得到，測試範例先用 20Gi；用量告警見清單 I2。

在叢集上檢查：`kubectl -n <namespace> get cluster <cluster>` 的 STATUS 是 `Cluster in healthy state`，`kubectl -n <namespace> get database <cluster>-trustiot` 的 APPLIED 是 `true`。Helm 4 的 `helm install --wait` 依 Cluster 的 `Ready` 條件等到它健康才結束（2026-10-12 實驗機兩種型態都是這樣，約 20 到 26 秒）；Database 的 AGE 不在等待之列，再晚約 20 秒才套用，所以用到 AGE 的 migration 要等 `age` extension 出現再開始（owner 不是 superuser，不能自己建立它；目前沒有服務的 migration 用到 AGE，第一個用到的 issue 要在 migration Job 加上這個等待，Laysi/trustiot-v2#176）。叢集測試 `tests/e2e/postgres.e2e.ts` 以真實連線驗證 verify-full、錯誤 CA、錯誤主機名稱與沒列 `postgres` 的 pod 被拒、明文連線被拒、沒有 superuser 帳號、uninstall 保留資料，用法見該目錄的 README，實驗機的結果見 [測試報告](../../docs/test-reports/2026-10-12-58-cnpg-postgres.md)。

### 連線預算

服務的角色沒有個別的連線數上限，所以所有服務、所有副本的連線池加總要放得進 Postgres 的 `max_connections`；超過時新連線收到 SQLSTATE 53300，新副本開不了常駐的連線（身分服務常駐 3 條）、無法就緒，滾動更新就卡住。換版時新舊副本同時存在（服務的 Deployment `maxSurge: 1`、`maxUnavailable: 0`），決策 4 又讓所有服務一起發版，所以要以「全部服務同時滾動」計算（Laysi/trustiot-v2#575，2026-10-23）。

**chart 的檢查**（`templates/_postgres.tpl` 的 `trustiot.postgres.checkConnections`，由 `templates/values-check.yaml` 執行）：對每個啟用、而且在 `mesh.calls` 列出 `postgres` 的服務，

```
需要的連線 = Σ（database.maxConnections + 其他連線池）×（replicas + maxSurge） + 保留的連線
```

`bundled` 時超過 `postgres.bundled.parameters.max_connections` 就渲染失敗，錯誤訊息列出每個服務的「每副本 × pod 數」與保留的條數。`maxSurge` 與 Deployment 讀同一個 helper（`templates/_service.tpl` 的 `trustiot.service.maxSurge`）。不論哪一種模式，列出 `postgres` 而 chart 沒有它的預算、或沒有 `database.maxConnections`、或數字超出服務接受的範圍，也渲染失敗。

**每個服務的數字**（`trustiot.postgres.connectionBudgets`）。服務的主要連線池由 chart 以 `services.<name>.database.maxConnections` 決定，經 `TRUSTIOT__DATABASE__MAX_CONNECTIONS` 傳入；其他連線池的大小寫死在服務的程式裡，chart 的預算記它們的最大值：

| 服務 | `database.maxConnections` 預設 | 其他連線池 | 每副本 | 整版滾動（一個副本加一個新的） |
| --- | --- | --- | --- | --- |
| `assets` | `10` | 4 | 14 | 28 |
| `certs` | `5` | 4 | 9 | 18 |
| `identity` | `10` | 21 | 31 | 62 |
| `telemetry-query` | `10` | 2 | 12 | 24 |
| 合計 | | | 66 | 132 |

- `assets`：平台角色的連線池（`PLATFORM_CONNECTIONS`）：讀身分服務與 certs 事件的兩個消費端各一條、就緒檢查一條、平台管理員讀設備池一條。`telemetry-query`：平台角色讀身分服務事件的連線池（`EVENT_READER_CONNECTIONS`）。`certs`：平台角色的出廠登錄連線池（`REGISTRATION_CONNECTIONS`）。
- `identity`：以 `identity_app` 連線的租戶、登入、寄信、run 委派與事件紀錄連線池共 16 條，以 `identity_platform` 連線的事件紀錄連線池 5 條（消費的產生端數加 2；certs 在 2026-10-23 加入後是 3 個產生端），見 [services/identity/README.md](../../services/identity/README.md) 的「連線池」。
- assistant 的程式已經會用 Postgres（Laysi/trustiot-v2#588），但 chart 還沒讓它連（`mesh.calls` 沒有列）。讓它連的 issue 要在預算加上它的連線池、在 values 給它 `database.maxConnections`，檢查自動算進去；`max_connections` 不夠時，依下方的記憶體估算調整。

**保留的連線**（`trustiot.postgres.reservedConnections`，共 18 條，依 CloudNativePG 1.30.1 的原始碼與 PostgreSQL 18 的預設值）：

| 用途 | 條數 | 來源 |
| --- | --- | --- |
| `superuser_reserved_connections` | 3 | PostgreSQL 的預設值，CloudNativePG 不改（`pkg/postgres/configuration.go`），values 也不能設定：最後 3 條只給 superuser |
| CloudNativePG 的 instance manager（`postgres`，pod 內的 local socket） | 6 | 每個資料庫一個連線池，最多 3 條、不留閒置連線（`pkg/management/postgres/pool/pool.go` 的 `SetMaxOpenConns(3)`、`SetMaxIdleConns(0)`）；`postgres` 與 `trustiot`（AGE extension 的調和），`template1` 只在 bootstrap，服務還沒啟動 |
| `cnpg_metrics_exporter` | 6 | 同樣的連線池，`postgres` 與 `trustiot` |
| migration Job | 2 | 它以 owner 開的一條 session，與檢查角色密碼時的一次登入（`tools/migrate/src/postgres.rs`） |
| 第一位平台管理員的 bootstrap | 1 | 操作者從 CronJob `<release>-platform-admin-bootstrap` 建立的 Job（`templates/platform-admin-bootstrap.yaml`、[docs/runbooks/platform-admin.md](../../docs/runbooks/platform-admin.md)），以 owner 開一條 session（`tools/migrate/src/platform_admin.rs`）；操作者可能在換版時執行它 |

所以預設值、`values-saas.yaml`、`values-onprem.yaml` 與展示環境（`deploy/flux/clusters/demo/trustiot/values-demo.yaml`）在四個服務全開時需要 132 + 18 = 150 條；`max_connections` 預設 160，還有 10 條餘裕。只換一個服務、或只開部分服務時需要的更少。2026-10-23 之前預設是 100，四個服務全開時整版滾動會超過（Laysi/trustiot-v2#497 的安全審查 F3）。

**程式與預算的綁定**：預算是 JSON，Helm 以 `fromJson` 讀，用 Postgres 的服務各有一個 Rust 測試（`services/<服務>/tests/chart_connections.rs`）以 `crates/testkit` 的 `chart::postgres_connection_budget` 讀同一段文字，比對程式的常數（其他連線池、`database.maxConnections` 接受的範圍），並以 `chart::pool_size_calls` 列出程式裡每個 `.max_connections(...)`：連線池變大、或多了一個連線池而預算沒跟著改，測試就失敗。身分服務的其他連線池目前由測試把各連線池的常數加總；Laysi/trustiot-v2#497 會改用身分服務自己的 `connections_per_replica()`。主要連線池的預設值由 chart 傳入，程式裡的預設值不影響部署。

**記憶體估算**（`resources.limits.memory` 1Gi；PostgreSQL 的參數：`shared_buffers` 128MB、`work_mem` 4MB、`maintenance_work_mem` 64MB、`autovacuum_max_workers` 3、`autovacuum_work_mem` -1 即沿用 `maintenance_work_mem`、`hash_mem_multiplier` 2.0；CloudNativePG 的預設 `max_worker_processes` 32、`max_replication_slots` 32、`shared_memory_type` mmap、不設 `huge_pages`，節點沒有預留 huge pages 時就是不用）。在 lab 實測延後到 Laysi/trustiot-v2#596。

```
記憶體 ≈ 共享記憶體 + (max_connections + 背景行程) × 每條連線的固定開銷
        + (autovacuum_max_workers + 1) × maintenance_work_mem
        + 同時以滿額執行的排序數 × work_mem（hash 是 work_mem × hash_mem_multiplier）
```

| 項目 | 參數與出處 | `max_connections` 160 | 原本的 100 |
| --- | --- | --- | --- |
| 共享記憶體（含 `shared_buffers`、每個連線槽的 PGPROC 與 lock table） | PostgreSQL 18.6 的 `postgres -C shared_memory_size`，以上面的參數算（2026-10-23，在 `postgres:18.6-alpine` 的 initdb 資料目錄上執行；AGE 的共享記憶體不在內，很小）。每多一個連線槽約 70 KiB | 157 MB | 152 MB |
| 每條連線的固定開銷 | 2 MiB：Andres Freund 的量測（[Measuring the Memory Overhead of a Postgres Connection](https://blog.anarazel.de/2020/10/07/measuring-the-memory-overhead-of-a-postgres-connection/)，2020-10-07）是一條跑過簡單 OLTP 查詢的連線約 1.3 MiB（私有記憶體加 page table，huge pages 開啟）；不用 huge pages 時每個行程再多一份 page table，對應 157 MB 共享記憶體、4 KiB 的頁約 0.3 MiB；加上 TLS 與我們的 catalog，取整到 2 MiB。所有連線槽都開滿，加上約 10 個背景行程（checkpointer、background writer、WAL writer、archiver、autovacuum launcher、logical replication launcher、PostgreSQL 18 的 I/O worker 3 個） | (160 + 10) × 2 = 340 MB | (100 + 10) × 2 = 220 MB |
| autovacuum 與 migration | 3 個 autovacuum worker 加 migration Job 建索引，各用滿 `maintenance_work_mem` | 4 × 64 = 256 MB | 256 MB |
| 小計 | | 753 MB | 628 MB |
| 剩給 `work_mem` | 1024 MB 減小計 | 271 MB：同時 67 個滿額排序，或 33 個滿額 hash | 396 MB |

結論：調到 160 後，固定的部分比原本多 125 MB，仍有 271 MB 給查詢的排序與 hash。服務的查詢是以索引查少量資料的 OLTP，排序只配置實際需要的大小，很少用到 `work_mem` 的上限；Postgres 的 CPU 上限又是 2 core。若要求「每條連線同時跑一個滿額排序」也放得下，160 需要 753 + 160 × 4 = 1393 MB，超過 1Gi；原本的 100 也要 1028 MB，同樣超過，所以這個上限從來不是這樣估的。cgroup 也計入 Postgres 讀檔的 page cache，但那可以回收，不會讓 Postgres 被 OOM kill。

**`external` 模式**：chart 不檢查加總，`max_connections` 由客戶設定；它至少要是上面算出的「需要的連線」，扣掉 CloudNativePG 的 12 條、加上客戶自己的監控與管理連線。chart 仍檢查每個服務的 `database.maxConnections` 在它接受的範圍內。

## 叢集內的 ClickHouse

`clickhouse.mode: bundled`（預設，SaaS 與地端相同）部署 `templates/clickhouse.yaml`：

- 一個副本的 StatefulSet `<release>-clickhouse`，資料在 PVC `data-<release>-clickhouse-0`（`/var/lib/clickhouse`）；StatefulSet 刪除或縮容時保留 PVC。ClusterIP Service 只開 HTTP 8123 與 native 9000。
- 伺服器設定只有一個 ConfigMap（`<release>-clickhouse-config`，掛成 `config.d`）：監聽位址、關掉 MySQL／PostgreSQL 相容埠與 interserver 埠、日誌只寫 stdout。設定變更時 pod 自動重啟。
- 帳號、角色、row policy、settings profile 與 quota 一律不寫在設定檔（決策 14；ClickHouse Cloud 不能改設定檔）：由 migration Job（Laysi/trustiot-v2#60）以 bootstrap 管理帳號套用 `services/ingest/migrations/clickhouse/` 的 SQL，存在 ClickHouse 自己的 access storage（PVC 上的 `access/`）。租戶帳號只能讀的 view `trustiot.tenant_telemetry` 與它的 DEFINER 帳號（不能登入）也由這些 SQL 建立，chart 不必為它們設定任何值（Laysi/trustiot-v2#260）。唯一由映像產生的帳號是 bootstrap 管理帳號：映像的 entrypoint 依 `CLICKHOUSE_USER`、`CLICKHOUSE_PASSWORD_FILE`（下方「管理帳號的密碼」）與 `CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1` 寫進 `users.d`（記憶體 emptyDir），並移除沒有密碼的 `default` 帳號。
- 根目錄唯讀、以 UID 101 非 root 執行、不能提權、drop 所有 capability；探針是 `GET /ping`。pod 帶 `istio.io/dataplane-mode: ambient` 加入 ambient mesh，服務連 ClickHouse 由 ztunnel 加 mTLS。

磁碟區大小沒有預設值，每個部署在安裝實例的覆寫檔設定 `clickhouse.bundled.storage.size`：每日筆數 × 保存天數 × 約 3.1 bytes（2026-10-09 實測，30 天 52 億筆佔 15.2 GiB）× 1.5（merge 時新舊 part 並存與系統表的餘裕）。例如每天 1 億筆、保存 400 天約 190 GB（遙測表的保存期限暫定 400 天，Laysi/trustiot-v2#92 定案）。磁碟用量的告警與擴充見清單 I2（硬碟水位告警）。

### 管理帳號的密碼

bootstrap 管理帳號只給 migration Job 用，密碼在叢集內產生，沒有人建立、看到或複製它（Laysi/trustiot-v2#403，docs/architecture/secrets.md「叢集內 ClickHouse 管理帳號的密碼」）。安裝前不必建立任何 Secret。

- chart 宣告一個空的 Secret `<release>-clickhouse-admin-password`（`templates/passwords.yaml`，沒有 `data`，帶 `helm.sh/resource-policy: keep` 與 `trustiot.io/filled-by: migrate`），key 是 `clickhouse.bundled.adminUser`。
- ClickHouse 的 pod 先跑 init container `admin-password`（migration 映像的 `migrate clickhouse-admin-password`）：以 1 小時效期的 projected token 讀這個 Secret；key 缺了、值不能用、`clickhouse.bundled.adminPasswordGeneration` 大於 Secret 記的代、或 Secret 不是它填過的物件（uid 不同）時，以作業系統的 CSPRNG 產生 256 位元的新密碼，用帶 `resourceVersion` 的 merge patch 寫回（與 migration Job 填服務密碼的規則相同，見「migration Job」的「產生的密碼」）。接著把 Secret 裡的密碼寫進記憶體 emptyDir 的 `/etc/trustiot/clickhouse-admin/password`（mode 0400，先寫暫存檔再改名），ClickHouse 的容器唯讀掛這個 emptyDir，以 `CLICKHOUSE_PASSWORD_FILE` 讀它。日誌只寫 Secret 與帳號的名稱。
- 權限：ClickHouse 的 pod 與 ServiceAccount 都是 `automountServiceAccountToken: false`，只有 init container 掛 projected token 與 `kube-root-ca.crt`；Role `<release>-clickhouse` 只讓 ClickHouse 的 ServiceAccount 對這一個 Secret `get` 與 `patch`。migration Job 的 Role 對它只有 `get`：Job 經 Kubernetes API 讀密碼（`TRUSTIOT__CLICKHOUSE__PASSWORD_SECRET`），不以 `secretKeyRef` 讀，所以 Secret 還沒填好時 Job 的 pod 照樣啟動、等它填入，而不是停在 `CreateContainerConfigError`；ClickHouse 還在用舊密碼（拒絕登入）時 Job 重讀 Secret 再試，直到 `migrate.wait.databases` 用完。服務與 collector 的 pod 讀不到它：它在 `trustiot.dependencySecretNames`，自訂 `env` 寫了就渲染失敗，Secret 也不掛進它們的 pod（chart 測試）。
- init container 用 migration 映像，只有 arm64，所以 chart 在 ClickHouse 的 `nodeSelector` 自動加上 `kubernetes.io/arch: arm64`。它的映像隨 `migrate.image` 換，所以每次換 TrustIoT 版本 ClickHouse 也重啟一次（單一副本，短暫中斷；collector 重試）。
- 輪替：調大 `clickhouse.bundled.adminPasswordGeneration`，見 [docs/runbooks/secrets.md](../../docs/runbooks/secrets.md)。值進到 ClickHouse pod 的 annotation `checksum/admin-secret`，所以升級時 ClickHouse 重啟，init container 先產生並寫入新密碼，伺服器以新密碼啟動；migration Job 不重跑、服務不重啟（它們不用這個帳號）。代只往上走：rollback 渲染出較小的值時保留現有的密碼。
- Secret 遺失（有人刪了 key 或整個 Secret）：已經在跑的 ClickHouse 不受影響，但下一次 Job 讀不到密碼，等到 `migrate.wait.databases` 用完就失敗。調大 `clickhouse.bundled.adminPasswordGeneration` 再升級：Helm 重建被刪掉的 Secret，ClickHouse 重啟，init container 產生新密碼（runbook）。init container 不能建立 Secret，所以 Secret 不存在時它失敗並寫出這個處理方式，ClickHouse 停在 `Init`。
- 從人建立的 Secret 搬過來（Laysi/trustiot-v2#403 之前的安裝）：在同一次升級把 `clickhouse.bundled.adminSecret.name` 改回空白。ClickHouse 重啟，init container 產生新密碼，伺服器改用它（bootstrap 帳號由 entrypoint 依每次啟動的密碼寫成，舊密碼之後不能登入）；Job 跟著換名重跑，以新密碼連線，服務與 collector 隨新的 Job 名稱滾動更新一次。確認 ClickHouse 與 Job 都正常後，刪掉舊的 Secret（SaaS 刪 `deploy/flux/` 的 SOPS 檔與 kustomization 的引用）。過渡期間 `adminSecret.name` 仍可設定，行為與之前完全相同，升級不會因此重啟 ClickHouse。

## MQTT broker

`mqttBroker.enabled: true` 部署 `templates/mqtt-broker.yaml`（決策 6，Laysi/trustiot-v2#84），雲端唯一的 MQTT broker：閘道器經入口 gateway 與 mqtt-bridge 連到它，雲端服務在叢集內連它。預設關：兩份 values 與 `deploy/flux` 都不開（`deploy/flux` 的測試檢查 SaaS 與展示叢集都沒有設定 `mqttBroker`），目前只有 nightly 的 `tests/values/smoke.yaml` 開啟。

- 一個副本的 StatefulSet `<release>-mqtt-broker`，Mosquitto 2.1（`mqttBroker.image`，以 digest 固定）。資料在 PVC `data-<release>-mqtt-broker-0`（`/mosquitto/data`）：SQLite 的 session、排隊與 retained 訊息、dynamic security 檔與信任檔；StatefulSet 刪除或縮容時保留 PVC。
- ClusterIP Service `<release>-mqtt-broker` 只開 `mqtt-gateways`（8883）與 `mqtt-services`（1883），兩種部署形態相同，沒有 nodePort，任何入口路由都不能指向它（`ingress.routes` 寫了就渲染失敗）。chart 測試檢查兩份 values 的渲染結果；nightly 在 k3d 上檢查部署後的每個 Service 都是沒有 node port、外部 IP 與 load balancer 位址的 ClusterIP，也沒有任何 Route 指向 broker 的 Service。
- 設定只有一個 ConfigMap（`<release>-mqtt-broker-config`），以 subPath 掛成一般檔案 `/mosquitto/config/mosquitto.conf`。內容是 `files/mosquitto/mosquitto.conf` 加上依記憶體上限算出的 `memory_limit`（chart 測試比對兩者；`tools/migrate/tests/mqtt_broker.rs` 在固定的映像裡跑同一個檔）。subPath 的檔案不會隨 ConfigMap 更新，所以設定變更時 pod 依 checksum 重啟。
- 主容器直接執行 `/usr/sbin/mosquitto -c /mosquitto/config/mosquitto.conf`，不經映像的 entrypoint（它只在以 root 執行時改檔案擁有者）。每個容器以 UID／GID 1883 非 root 執行、根目錄唯讀、不能提權、drop 所有 capability；pod 與 ServiceAccount 都不自動掛 token。pod 帶 `istio.io/dataplane-mode: ambient` 加入 ambient mesh。
- 探針：startup、liveness、readiness 都是 exec，在容器裡以 `mosquitto_pub` 經 Unix socket `/run/mosquitto/probe.sock` 建立一條 MQTT 連線，broker 接受了連線才算成功，不只是 TCP 埠開著。不經 TLS，所以健康檢查不留下 TLS 錯誤日誌（`tools/migrate/tests/mqtt_broker.rs` 檢查跑探針之後的日誌沒有錯誤、警告或 OpenSSL 訊息）；也不經網路，所以 NetworkPolicy 沒有節點來源的規則，kubelet 碰不到 broker 的任何埠。這個 socket 接受匿名連線（最多 8 條），匿名用戶沒有角色，發布與訂閱都被 dynamic security 拒絕。

### 誰連得到

| 埠 | 呼叫端（mesh 放行） | broker 的驗證 |
| --- | --- | --- |
| `mqtt-gateways` 8883 | `mqtt-bridge`，轉送閘道器的連線 | TLS 1.3，套件依 broker 的順序選、ChaCha20-Poly1305 排第一（`ciphers_tls1.3`，決策 6：閘道器的 CPU 沒有 AES 指令；不設時 Mosquitto 選 AES-256-GCM，Laysi/trustiot-v2#564），閘道器的用戶端憑證要由 certs 服務的 CA 簽發、在效期內、EKU 有 clientAuth、不在 CRL 上（沒有憑證、別的 CA、過期、沒有 clientAuth、已撤銷都在 TLS 握手被拒，Rust 測試逐一驗證）。憑證的 CN 就是 username（`use_identity_as_username`）也是 client id（`use_username_as_clientid`），所以閘道器不能冒用別的閘道器，也接不走它的 session |
| `mqtt-services` 1883 | 有 broker 帳號的服務：`ingest`、`gateway-ops`、`certs` | mesh 的 mTLS（ztunnel），再以 dynamic security 的帳號 `service:<服務>` 與密碼登入（見下方「服務的帳號」）。閘道器 ID 不能有 `:`，所以沒有閘道器憑證能帶服務的名稱 |

- `mesh.calls` 只能把 `mqtt-broker` 列在這四個服務底下，列在其他服務底下渲染失敗（清單在 `templates/_mqtt-broker.tpl` 的 `trustiot.mqtt-broker.callers`）。預設 values 列了 `ingest`、`gateway-ops` 與 `mqtt-bridge`；`certs` 沒有列，負責閘道器帳號登記與撤銷的 Laysi/trustiot-v2#82 加上。
- AuthorizationPolicy `<release>-mqtt-broker` 每個埠一條規則（`to.operation.ports`，ztunnel 在 L4 執行）：`mqtt-bridge` 只到 8883，其他三個只到 1883；沒有呼叫端時沒有規則，拒絕所有連線。NetworkPolicy 看不到 HBONE 通道裡的應用程式埠，只放行同一批呼叫端的 pod 連 15008，分埠只靠 AuthorizationPolicy（與身分服務的 token exchange 相同）。
- broker 這一側也有兩條呼叫：`services.certs.enabled` 時 chart 自動讓 broker 的 pod 連 certs 服務的 `http` 埠，取 CA 憑證與 CRL（`TRUSTIOT__TRUST__CERTS_URL`，見下方「信任檔與重新載入」），以及 certs 服務的 `broker-certs` 埠（`http://<release>-certs-broker.<namespace>.svc:8082`），取自己的伺服器憑證（見下方「伺服器憑證」）。後者只有 broker 連得到（AuthorizationPolicy 與 NetworkPolicy `<release>-certs-broker`，見「服務間授權（mesh）」）。

### 角色（dynamic security）

broker 的權限全部在 dynamic security，預設拒絕：`defaultACLAccess` 的發布、接收、訂閱、取消訂閱都是拒絕（包括 Mosquitto 預設允許的接收），匿名群組沒有角色。dynamic security 檔在 PVC 上，broker 每次啟動前由 init container `accounts` 改寫（`tools/migrate/src/mqtt_accounts.rs`），所以 Mosquitto 從不自己產生它的預設檔（那個檔有 admin 帳號與可以存取 `#` 的 client）。

| 角色 | 給誰 | 規則 |
| --- | --- | --- |
| `trustiot-gateway` | 每個閘道器 client（登記在 Laysi/trustiot-v2#82） | `protocol::gateway_role_acls()`（`crates/protocol` 的 `acl` 模組）：只能發布到自己 `trustiot/v1/gw/<CN>/up/` 底下的協定頻道、只能訂閱與接收自己的 `down/`。dynamic security 不能依 retain 旗標授權，所以閘道器可以在自己的 `up` 頻道發 retained，每個頻道最多一則（Laysi/trustiot-v2#268） |
| `trustiot-service-ingest` | `service:ingest` | 接收所有閘道器的 `up/telemetry`、`up/log`，發布 `down/telemetry-ack` |
| `trustiot-service-gateway-ops` | `service:gateway-ops` | 接收 `up/status`、`up/heartbeat`、`up/config-report`、`up/live`、`up/log-dump`，發布 `down/config`、`down/live-control`、`down/log-request` |
| `trustiot-service-certs` | `service:certs` | 沒有頻道 |
| `trustiot-dynsec-admin` | `service:certs` | 只有 `$CONTROL/dynamic-security/#`：送 dynamic security 的指令、讀回應。持有者可以改每個 client 與角色，等同掌控 broker，所以只給 certs |

- 管理角色只給 certs（2026-10-23 決定，docs/architecture/secrets.md 的「MQTT dynsec 管理帳密」）：閘道器的登記跟著 certs 簽發憑證，撤銷靠 certs 發的 CRL（以憑證登入的 client 停用不了，見下方「信任檔與重新載入」）。gateway-ops 不持有它：持有者等同掌控 broker，而 gateway-ops 另外處理即時控制與閘道器的資料，給它會擴大它被入侵時的影響。Laysi/trustiot-v2#82 的流程需要 gateway-ops 也能管理時，把 `tools/migrate/src/mqtt_accounts.rs` 的 `SERVICES` 裡 gateway-ops 的 `dynsec_admin` 改成 `true`，並更新這段與 secrets.md 那一列。
- 服務接收的頻道可以直接訂閱，也可以用任何群組的 shared subscription（`$share/<群組>/trustiot/v1/gw/+/up/<頻道>`）讓多個副本分攤；topic 表以 `crates/protocol/README.md` 為準，服務的 issue 改 `SERVICES` 裡自己的那一列。
- init container 每次都把 `trustiot-gateway`、`trustiot-dynsec-admin` 與 `trustiot-service-*` 換成這個版本的定義，移除不再存在的 `trustiot-service-*` 角色、離開清單的服務帳號與群組裡的服務帳號成員（服務帳號只有 chart 給的角色）；broker 存下的閘道器 client、群組與其他角色保留。

### 服務的帳號

密碼在叢集內產生，沒有人建立、看到或複製它（docs/architecture/secrets.md「MQTT broker 的服務帳號密碼」）。安裝前不必建立任何 Secret。

- chart 宣告一個空的 Secret `<release>-mqtt-broker-accounts`（`templates/passwords.yaml`，沒有 `data`，帶 `helm.sh/resource-policy: keep` 與 `trustiot.io/filled-by: migrate`）。
- broker 的 pod 先跑 init container `accounts`（migration 映像的 `migrate mqtt-broker-accounts`）：以 broker ServiceAccount 1 小時效期的 projected token 讀這個 Secret，為每個帳號持有者（在 `mesh.calls` 列出 `mqtt-broker` 的 `ingest`、`gateway-ops`、`certs`，`TRUSTIOT__ACCOUNTS__SERVICES`）保留一個以服務名稱為 key 的密碼；key 缺了、值不能用、`mqttBroker.passwordGeneration` 大於 Secret 記的代、或 Secret 不是它填過的物件（uid 不同）時，以作業系統的 CSPRNG 產生 256 位元的新密碼，用帶 `resourceVersion` 的 merge patch 寫回（規則與 migration Job 填服務密碼相同，見「migration Job」的「產生的密碼」）。dynamic security 檔裡只有每個密碼的 PBKDF2-SHA512 雜湊（Mosquitto 的 `$7$` 格式）。日誌只寫 Secret 與帳號的名稱。
- 權限：Role `<release>-mqtt-broker` 只讓 broker 的 ServiceAccount 對這一個 Secret `get` 與 `patch`；只有 `accounts` 這個 init container 掛 token，主容器與 sidecar 都沒有。服務的自訂 `env` 讀不到這個 Secret 與下方的伺服器憑證 Secret（`trustiot.dependencySecretNames`，chart 測試）。
- 給服務的介面：Secret 的 key 是服務名稱，使用者名稱是 `service:<服務>`，位址是 `<release>-mqtt-broker.<namespace>.svc:1883`。chart 還沒把密碼交給任何服務：第一個接上 broker 的服務在它的 issue 把自己那個 key 只交給自己的 pod，並處理輪替後的重啟。
- 輪替：調大 `mqttBroker.passwordGeneration`。值與帳號持有者的清單進到 broker pod 的 annotation `checksum/mqtt-broker-accounts`，所以升級時 broker 重啟，init container 先產生新密碼並寫入雜湊。代只往上走：rollback 渲染出較小的值時保留現有的密碼。
- 帳號持有者改變（`mesh.calls` 加上或拿掉 `mqtt-broker`）也讓 broker 重啟：新的服務拿到帳號，離開的服務的帳號被移除。
- dynamic security 檔不能用時，init container 停止並指出是哪一筆、哪個欄位，不改那個檔，broker 停在 `Init`。不能用是指：不是 JSON、`roles`/`clients`/`groups` 不是陣列（這種檔 Mosquitto 自己也不會啟動）、同一個物件裡重複的 key（Mosquitto 讀第一個，init container 用的 JSON 程式庫讀最後一個），或保留下來的角色、client、群組有 Mosquitto 會默默略過或讀錯的內容：缺名稱或名稱重複、欄位型別不對（例如 `disabled` 或 `allow` 寫成字串）、不認得的 ACL 類型、重複的 ACL，或欄位名稱只差大小寫（Mosquitto 查欄位不分大小寫，手改的項目可能因此蓋過 chart 的帳號或預設權限）。broker 自己存的檔一定符合（`tools/migrate/tests/mqtt_broker.rs` 驗過）；chart 管理的角色與服務帳號不檢查，因為每次都整個換掉。處理：修正那一筆、從備份還原，或刪掉檔案，broker 以沒有閘道器 client 的新檔啟動，閘道器要重新登記（Laysi/trustiot-v2#82）。init container 不替它猜，也不丟掉看不懂的項目。

### 信任檔與重新載入

閘道器憑證的 CA 與 CRL 由 certs 服務提供（[services/certs/README.md](../../services/certs/README.md) 的「介面」）：`/internal/certs/v1/ca.pem` 是根 CA 與每個可用的簽發 CA，`/internal/certs/v1/crl.pem` 是每個簽發 CA 的 CRL。broker 的 pod 以 migration 映像的 `migrate mqtt-broker-trust`（`tools/migrate/src/mqtt_trust.rs`）取用：

- init container `trust-files`（`--once`）在 broker 啟動前裝好信任檔，並確認伺服器憑證與私鑰是一對；sidecar `trust` 之後每 `mqttBroker.trustInterval`（預設 30 秒）重取一次。
- 檢查：每張 CA 憑證都要是 CA、至少一張自簽的根 CA、每個簽發 CA 都有 CRL、每個 CRL 都由檔案裡同名的 CA 簽章（以它的公鑰驗過）並有 nextUpdate（沒有 nextUpdate 的 CRL 永遠不會過期，certs 停止更新後撤銷資料仍會被一直信任），從 certs 取得的 CRL 還要在效期內（thisUpdate 不在未來、nextUpdate 還沒到；sidecar 與 broker 在同一個節點，看的是同一個時鐘）。不符就不裝，留著原本的檔並記警告：broker 對簽發者找得到、卻驗不過或不在效期內的 CRL，會拒絕那個簽發 CA 的每一個閘道器。已經裝好的檔不再以時鐘檢查，CRL 之後過期也不會被換成佔位 CA。
- 檔案寫進 PVC 上的新目錄，再以一次 rename 換掉 symlink `trust/current`，broker 讀到的 CA 與 CRL 一定是同一次取得的。
- certs 連不到或回 503（某個簽發 CA 還沒有 CRL）時保留手上的檔。全新的磁碟區又拿不到檔、沒有部署 certs 服務、或 certs 沒有可用的簽發 CA（`crl.pem` 是空檔，簽發 CA 外洩而移除金鑰之後就是這樣）時，改裝一組佔位的 CA（在這裡產生，私鑰立刻丟掉）與它的 CRL：broker 照常啟動，但拒絕每一張閘道器憑證。不把空的 `crl.pem` 交給 Mosquitto：2.1.2 讀到空的 CRL 檔會直接停止。
- CRL 剩不到 1 天到期時 sidecar 每小時記一次錯誤：CRL 過期後，那個簽發 CA 的閘道器全部被拒絕（certs 服務也有自己的告警指標）。

信任檔或伺服器憑證有變化時，sidecar 對 broker 送 SIGHUP。Mosquitto 2.1.2 收到 SIGHUP 重新讀設定的憑證、CA、CRL 與 plugin 設定，不中斷既有的連線（原始碼的 `src/signals.c`；Rust 測試驗證新的 CRL 與新的伺服器憑證在 SIGHUP 之後生效，已連線的 session 不斷）。為了送訊號：

- pod 共用 process namespace（`shareProcessNamespace: true`），每個容器都是同一個 UID：sidecar 只能對同一個使用者的行程送訊號。它以 `/proc/<pid>/comm` 找 `mosquitto`，並等 `/proc/<pid>/status` 的 `SigCgt` 顯示 broker 已經處理 SIGHUP 才送（在那之前 SIGHUP 會讓它結束）。
- 代價：共用 process namespace 時同一個 pod 的容器看得到彼此的行程與 `/proc/<pid>/root`。這個 pod 的三個容器本來就掛同一個資料磁碟區與伺服器憑證，init container 結束後 pod 裡沒有 token，所以沒有多給誰權限。

重新載入不中斷連線，也表示 CRL 更新之後，已經連線的被撤銷閘道器不會斷線，直到它重連；撤銷時主動踢掉它由 Laysi/trustiot-v2#82 處理。另外，以憑證登入的 client 不經 dynamic security 的帳密檢查（Mosquitto 2.1.2 的 `src/handle_connect.c`），所以停用 client（`disableClient`）擋不住持有效憑證的閘道器重連：撤銷要靠 CRL，再刪除 client 或拿掉它的角色（Laysi/trustiot-v2#82）。

### 伺服器憑證（給 Laysi/trustiot-v2#535）

chart 不渲染、不建立伺服器憑證或私鑰。certs 服務簽它（Laysi/trustiot-v2#557：profile、名稱、效期與換發時機在 [services/certs/README.md](../../services/certs/README.md) 的「broker 伺服器憑證」），broker 這一側產生私鑰、向 certs 取得憑證並輪替是 Laysi/trustiot-v2#535。簽發的端點：

- `POST http://<release>-certs-broker.<namespace>.svc:8082/internal/certs/v1/broker-certificate`，本文是 DER 的 PKCS #10 CSR（`Content-Type: application/pkcs10`），201 回 JSON，`certificate_chain` 是伺服器憑證接著簽發 CA 憑證（PEM），可以直接當 `tls.crt`。certs 只取 CSR 的公鑰，名稱與用途由它決定；私鑰不經過 certs。
- Service `<release>-certs-broker` 的埠固定是 8082，以埠名轉到 certs 容器的 `broker-certs` 埠：改 `services.certs.ports` 裡 `broker-certs` 的 `containerPort` 時，broker 呼叫的位址不變，AuthorizationPolicy 跟著比對新的容器埠（ztunnel 看到的是容器埠）。
- 只有 broker 的 pod 連得到（見上方「誰連得到」）；certs 開啟 broker 時以 `TRUSTIOT__BROKER__SERVICE_NAME` 把下表的 Service 名稱加進憑證。

broker 讀這個 Secret：

| 項目 | 約定 |
| --- | --- |
| 名稱 | `<release>-mqtt-broker-tls`（與其他物件相同的命名規則，`templates/_mqtt-broker.tpl` 的 `trustiot.mqttBroker.tlsSecret`），在 release 的 namespace |
| 型別 | `kubernetes.io/tls` |
| `tls.crt` | 伺服器憑證，後面接著中繼 CA（如果有），PEM |
| `tls.key` | 伺服器憑證的私鑰，PEM |
| 憑證內容 | EKU 只有 serverAuth；SAN 是閘道器驗證的固定名稱 `mqtt-broker.trustiot.internal`（SaaS 與地端相同），以及 Service 的 `<release>-mqtt-broker.<namespace>.svc`（certs 簽的那張都有，Laysi/trustiot-v2#557）。nightly 以自己的 CA 簽的那張另外有 `<release>-mqtt-broker`、`<release>-mqtt-broker.<namespace>` 與 `<release>-mqtt-broker.<namespace>.svc.cluster.local`。閘道器只信任根 CA、以根 CRL 檢查簽發 CA（[services/certs/README.md](../../services/certs/README.md) 的「閘道器對 broker 伺服器憑證的信任」，Laysi/trustiot-v2#556） |

- 只有 broker 的 pod 掛它：主容器、init container `trust-files` 與 sidecar，唯讀，整個 Secret 掛在 `/etc/trustiot/mqtt-broker-tls/`（0440，root 與 pod 的 `fsGroup`），不用 subPath，kubelet 才會在 Secret 改變時更新檔案。Mosquitto 經 kubelet 的 `..data` symlink 讀檔（Rust 測試以同樣的目錄結構驗證）。chart 測試檢查沒有別的 pod、Role 或自訂 `env` 指到它。
- Secret 還不存在時 pod 停在 `ContainerCreating`，等它建立後才啟動；`helm install --wait` 因此等不到 broker 就緒。nightly 在安裝 chart 之前，以這次執行自己產生的 CA 簽一張放進去（`scripts/nightly/smoke.ts`）。
- 換憑證不必重啟：改 Secret 的內容，kubelet 更新掛載的檔案，sidecar 在下一次檢查確認新的憑證與私鑰是一對，再送 SIGHUP。不是一對時不送，broker 繼續用舊的，sidecar 記錯誤。

### 持久化與限制

- SQLite 持久化（`mosquitto_persist_sqlite`）存 session、排隊與 retained 訊息。`plugin_opt_flush_period 0`：每一輪事件迴圈的變更在下一輪開始、broker 等待新流量之前寫入，SIGKILL 最多失去它打斷的那一輪，而不是預設的 5 秒。Rust 測試以 SIGKILL 停掉 broker 再啟動，retained 訊息、session 與排隊的訊息都還在。SIGTERM 時 Mosquitto 寫入最後的變更再結束。

| 設定 | 值 | 說明 |
| --- | --- | --- |
| `max_packet_size` | `1056768` | `crates/protocol` 的 `MIN_MAX_PACKET_BYTES`：最大的 payload（`MAX_LARGE_BYTES`，1 MiB）加上 topic 與 properties 的 8 KiB（Rust 測試比對）。送更大封包的用戶端被斷線；每個頻道自己較小的上限由收端檢查。不設 `message_size_limit`：Mosquitto 2.1 建議改用 `max_packet_size` |
| `max_queued_messages`、`max_queued_bytes` | `1000`、16 MiB | 每個用戶端：離線或太慢時，在途之外排隊的 QoS 1 訊息 |
| `max_inflight_messages` | `20` | 每個用戶端 |
| `max_keepalive` | `120` | 閘道器用 60 秒；不能不設 keepalive |
| `persistent_client_expiration` | `30d` | 不再回來的用戶端的 session 30 天後移除 |
| `memory_limit` | `mqttBroker.resources.limits.memory` 的四分之三 | 超過時 broker 拒收新訊息，而不是被 kernel 終止 |

Mosquitto 沒有的限制，不另外發明：

- retained 訊息的數量與總大小沒有上限設定（`retain_available` 只能整個開或關）。retained 的量由角色決定：閘道器只能發布到自己的協定頻道，每個頻道最多一則 retained，每則不超過 `max_packet_size`；服務只能發布到 `down` 頻道；總量再由 `memory_limit` 兜底。
- `check_retain_source` 用預設的 `true`：retained 訊息送給新訂閱者之前，broker 檢查發布它的 client 現在還有沒有權限。
- 沒有每個用戶端的發布速率限制；1883 上的服務是我們自己的程式。

還沒處理的風險：

- 1883 上的服務帳號可以用任何 client id，包括閘道器的 ID，因此一個被入侵的服務可以接走某個閘道器的 session。dynamic security 只能把一個 client 綁在一個 client id，有多個副本的服務不能這樣綁；目前靠 mesh 只放行三個服務來限縮。
- 叢集內的實測只在 nightly 的 k3d：nightly 不讀日誌，所以健康檢查不留 TLS 錯誤日誌以探針的設計（不經 TLS、不經網路）與 Rust 測試的日誌檢查為證；sidecar 對 broker 送 SIGHUP、伺服器憑證在叢集內輪替，還沒在叢集上觀察過。

## OpenTelemetry collector

`observability.collector.enabled: true`（預設，SaaS 與地端相同）部署 `templates/otel-collector.yaml`，實作決策 22 的日誌管線：

- 一個副本的 Deployment `<release>-otel-collector`，ClusterIP Service 只開 OTLP/HTTP 4318；每個服務拿到 `TRUSTIOT__TELEMETRY__OTLP_ENDPOINT=http://<release>-otel-collector.<namespace>.svc:4318`，service-kit 因此開啟日誌、trace 與指標的匯出（叢集內明文 HTTP，pod 之間由 ztunnel 加 mTLS）。collector 開啟時服務的自訂 `env` 不能改這個變數，每個服務的資料都經過遮蔽。
- 設定只有一個 ConfigMap（`<release>-otel-collector-config`），變更時 pod 自動重啟。密碼與 SIEM 的標頭不在設定檔裡：設定以 `${env:...}` 引用環境變數，Deployment 以 `secretKeyRef` 取得（ClickHouse 密碼從 migration Job 產生的 `<release>-otel-collector-password`，SIEM 標頭從 values 指定的 Secret）。
- migration Job 開啟時，pod 先以 init container `wait-for-migrations` 等 Job 完成（和服務一樣），所以第一次安裝時不會在密碼產生之前啟動、也不會因為讀不到 key 而報錯；這個 init container 跑的是只有 arm64 的 migration 映像，所以 chart 在 `nodeSelector` 加上 `kubernetes.io/arch: arm64`。
- 管線：`memory_limiter` → `redaction`（trace 再加 `transform/redact`）→ exporter。日誌與 trace 寫進 ClickHouse 的 `trustiot_logs.otel_logs`、`trustiot_logs.otel_traces`（`services/ingest/migrations/clickhouse/0006_logs.sql`，保存 30 天，以寫入時間計算）；轉送開啟時同一份遮蔽後的資料也送到 SIEM。指標先收下丟棄（`nop`），存放位置由 Laysi/trustiot-v2#183 決定，服務的匯出不會因此報錯。
- ClickHouse exporter 關掉 `create_schema`，只送 `INSERT`；表由 migration 建立。叢集內 ClickHouse 走 native 協定（9000），`external` 走 `clickhouse.external.url` 的 HTTPS，並掛 `clickhouse.external.caSecret` 的 CA。
- 根目錄唯讀、非 root、不能提權、drop 所有 capability、沒有 ServiceAccount token；探針是 health_check 擴充的 `GET /`（13133，不在 Service 上）。pod 帶 `istio.io/dataplane-mode: ambient` 加入 ambient mesh。

ClickHouse 帳號（決策 14：一律以 SQL 建立，不寫伺服器設定檔）：`0006_logs.sql` 建角色 `trustiot_otel_collector`，只有兩張日誌表的 `INSERT` 與 `SHOW COLUMNS`（exporter 啟動時以 `DESC TABLE` 讀欄位），沒有 `SELECT`，也碰不到其他資料庫。帳號由 migration Job（Laysi/trustiot-v2#60）以它產生在 `<release>-otel-collector-password` 的密碼建立（以 SHA-256 hash 送出，明文不進 SQL），效果等於：

```sql
CREATE USER trustiot_collector IDENTIFIED WITH sha256_password BY '<password>';
GRANT trustiot_otel_collector TO trustiot_collector;
```

`migrate.enabled: false` 時要以 bootstrap 管理帳號手動執行這兩句，並把密碼寫進 `<release>-otel-collector-password`（`tests/e2e/otel.e2e.ts` 就是這樣做）。帳號還不存在時 collector 照常啟動並回報就緒（exporter 記一行 `schema detection failed`），寫入失敗的資料重試 5 分鐘後丟棄；帳號建好後不用重啟就開始寫入（2026-10-11 本機實測）。

遮蔽規則（決策 22、[Laysi/trustiot#110](https://github.com/Laysi/trustiot/issues/110)）在 `templates/_otel-collector.tpl` 的 `trustiot.collector.redactionRules`，對每個來源都一樣，不只 service-kit 的服務：

- 欄位名稱：名稱像憑證、個資、request body 或聊天內容的屬性（resource、scope、日誌、span、span event、指標資料點）與 map 形式的日誌 body，值換成 `****`。名單與 service-kit 相同（`crates/service-kit/src/telemetry/redact.rs`，chart 測試逐一比對它的測試名單），例如 `password`、`authorization`、`cookie`、`token`、`email`、`phone`、`prompt`、`http.request.body`、`message.content`；`http.request.body.size`、`content_type` 這類大小與型別保留。
- 值的內容：每個屬性值、字串形式的日誌 body、span 名稱與 status message 裡符合下列格式的部分換成 `****`：私鑰、URL 裡的帳密、`Bearer`／`Basic` 標頭值、JWT、常見的 API key 格式、`password=…` 這類鍵值、email、台灣手機與國際電話號碼、身分證字號。
- 遮蔽發生在所有 exporter 之前，ClickHouse 與 SIEM 拿到的是同一份遮蔽後的資料；不加稽核屬性（`summary: silent`），不透露哪些欄位被遮蔽。
- 這仍是最後防線：寫在訊息裡、格式不在名單內的內容（例如一段聊天文字）攔不到，程式本身不能記錄這些值（docs/architecture/observability.md）。

轉送到 SIEM（地端，`observability.collector.forward`）：例如客戶的 SIEM 收 OTLP/HTTP、以 Bearer token 驗證、用自己的 CA：

```yaml
observability:
  collector:
    forward:
      enabled: true
      endpoint: https://siem.factory.test:4318
      protocol: http
      signals: [logs]
      authHeader:
        name: Authorization
        secret: {name: trustiot-siem-auth, key: value}   # 值是 "Bearer <token>"
      caSecret:
        name: trustiot-siem-ca
```

SIEM 連不上時 exporter 在記憶體佇列重試 5 分鐘，不影響寫進 ClickHouse；collector 重啟時佇列裡的資料會遺失。

服務間授權（見「服務間授權（mesh）」）：collector 是 mesh 內的元件，chart 自動讓每個啟用的服務連它、讓它連叢集內的 ClickHouse，kubelet 的健康檢查照常通過；其他連線一律被拒。

## chart 與服務的約定

服務照下列約定實作，就能直接用通用樣板部署（docs/architecture/deployment.md 的服務設計規則）：

- **設定**：ConfigMap `<release>-platform` 以 `envFrom` 交給每個服務，另外每個服務有 `OTEL_SERVICE_NAME=<服務名稱>`，collector 開啟時還有 `TRUSTIOT__TELEMETRY__OTLP_ENDPOINT`（service-kit 的 OTLP 匯出位址，見「OpenTelemetry collector」；`ai-gateway` 的設定前綴不同，是 `AI_GATEWAY__TELEMETRY__OTLP_ENDPOINT`，它也不讀平台設定，見「AI 閘道」）。`TRUSTIOT_` 後接英數字、`SENTRY_` 與 `OTEL_` 開頭的變數只由 chart 設定，`env` 不能覆寫，服務讀到的平台設定一定是 values 決定的值。沒有設定的項目不會出現，服務可以分辨「沒設定」與「空字串」。平台設定變更時 pod 會自動滾動更新。
- **密鑰**：不放進 ConfigMap，以 `valueFrom.secretKeyRef` 從 values 指定的 Secret 取用。Sentry 開啟時每個服務有 `SENTRY_DSN`；其他依賴的帳密只給在 `services.<name>.credentials` 列出它的服務（見後面「密鑰的環境變數」），Secret 名稱空白的項目不注入。資料庫的 CA 憑證以唯讀方式掛在 `/etc/trustiot/<postgres|clickhouse>-ca/ca.crt`：外部 ClickHouse 的掛進每個服務；Postgres 的（`external` 是 `postgres.external.caSecret`，`bundled` 是 CloudNativePG 產生的 CA 裡的 `ca.crt`）只掛進在 `mesh.calls` 列出 `postgres` 的服務。內部身分的簽章私鑰只掛進身分服務，公鑰的 ConfigMap 掛進每個服務（見「內部身分」）；簽發 CA 的私鑰只掛進 certs（見「閘道器憑證 CA」）。服務自己的資料庫帳號（每個服務一個角色，決策 4）由 migration 的 issue 加入（Laysi/trustiot-v2#60）。
- **埠與探針**：API 監聽 8080（埠名 `http`）；健康埠 8081（埠名 `health`）只提供 `GET /healthz`（startup 與 liveness）與 `GET /readyz`（readiness），其他路徑回 404。探針連健康埠，節點上的行程因此碰不到 API（「節點來源」）。service-kit 預設就是這樣（`http.listen_addr`、`http.health_listen_addr`），`crates/service-kit/tests/chart_contract.rs` 對照 `values.yaml` 檢查。
- **SIGTERM**：pod 刪除時先等 `preStopSleepSeconds`（預設 5 秒），再送 SIGTERM；服務收到後 readiness 立即回失敗、處理完進行中的請求、主動關閉長連線，在 `drainSeconds`（預設 30 秒）內結束，之後才會被 SIGKILL。服務是容器內的 PID 1，要自己處理 SIGTERM。
- **執行環境**：根目錄唯讀，只有 `/tmp` 可寫；以非 root 執行，映像的 `USER` 要是數字 UID，並能以 OpenShift 指派的任意 UID 執行；沒有 ServiceAccount token；只排到 arm64 節點。

平台設定的環境變數：

| 變數 | 來源 | 何時出現 |
| --- | --- | --- |
| `TRUSTIOT_DEPLOYMENT_MODE` | `global.deploymentMode` | 一律 |
| `TRUSTIOT_APP_HOST`、`TRUSTIOT_GATEWAY_HOST`、`TRUSTIOT_OTA_HOST` | `hosts.*` | 一律 |
| `TRUSTIOT_CLIENT_IP_SOURCE` | 由 `ingress.exposure` 推導：`cf-connecting-ip` 或 `x-forwarded-for-rightmost` | 一律 |
| `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` | 掛載的公鑰 ConfigMap 裡的 JWK Set `/etc/trustiot/internal-identity/jwks.json` | 身分服務啟用時 |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE` | `external`：`postgres.external.*`；`bundled`：`<release>-postgres-rw.<namespace>.svc`、`5432`、`trustiot`、`verify-full` | 一律 |
| `TRUSTIOT_CLICKHOUSE_URL` | `external`：`clickhouse.external.url`；`bundled`：`http://<release>-clickhouse.<namespace>.svc:8123`（叢集內明文 HTTP，pod 之間由 Istio ambient 的 ztunnel 加上 mTLS） | 一律 |
| `TRUSTIOT_QDRANT_URL` | `qdrant.external.url` | `qdrant.mode` 為 `external` |
| `TRUSTIOT_S3_ENDPOINT`、`_REGION`、`_BUCKET`、`_FORCE_PATH_STYLE` | `objectStorage.*` | 各自有值時；`_FORCE_PATH_STYLE` 隨 bucket |
| `TRUSTIOT_SMTP_HOST`、`_PORT`、`_SECURITY`、`_FROM` | `smtp.*` | 有 host 時（`_FROM` 有值時） |
| `TRUSTIOT_SMTP_AUTH_REQUIRED` | `"true"`：relay 需要帳號 | 有 host 且 `smtp.credentialsSecret.name` 有值時；身分服務據此把缺少的帳密當成不能用的寄信設定（見「密鑰的環境變數」） |
| `TRUSTIOT_CLICKHOUSE_CA_FILE` | 掛載的 CA 憑證路徑 `/etc/trustiot/clickhouse-ca/ca.crt` | `clickhouse.mode` 為 `external` 且設定了 `caSecret.name` |
| `SENTRY_ENVIRONMENT` | `observability.sentry.environment` | Sentry 開啟時（`SENTRY_DSN` 來自 Secret，見上） |

`TRUSTIOT_POSTGRES_CA_FILE`（`/etc/trustiot/postgres-ca/ca.crt`）不在 ConfigMap：chart 只設給在 `mesh.calls` 列出 `postgres` 的服務，和 CA 的掛載一起出現，`bundled` 與 `external` 都一樣（`ai-gateway` 的名稱是 `AI_GATEWAY_POSTGRES_CA_FILE`，Postgres 的位置也以 `AI_GATEWAY_POSTGRES_*` 給它，見「AI 閘道」）。`bundled` 模式的叢集內端點，由部署該元件的 issue 加進這張表（Postgres 與 ClickHouse 見上面）。服務連 ClickHouse 的帳號與密碼還沒有：ingest 的寫入帳號在 ingest 服務加入時由 migration Job 建立（Laysi/trustiot-v2#91），每租戶唯讀帳號由 Laysi/trustiot-v2#119 加入。

密鑰的環境變數（以 `valueFrom.secretKeyRef` 注入，不在 ConfigMap）：

| 變數 | 來源 | 哪些服務拿到 |
| --- | --- | --- |
| `SENTRY_DSN` | `observability.sentry.dsnSecret` | Sentry 開啟時每個服務 |
| `TRUSTIOT_QDRANT_READ_API_KEY` | `qdrant.external.credentialsSecret` 的 `readApiKeyKey` | `knowledge` 的 `credentials` 含 `qdrant-read`，且 `qdrant.mode` 為 `external` |
| `TRUSTIOT_QDRANT_WRITE_API_KEY` | `qdrant.external.credentialsSecret` 的 `writeApiKeyKey` | `knowledge` 的 `credentials` 含 `qdrant-write`，且 `qdrant.mode` 為 `external` |
| `TRUSTIOT_S3_ACCESS_KEY_ID`、`TRUSTIOT_S3_SECRET_ACCESS_KEY` | `objectStorage.credentialsSecret` | `credentials` 含 `object-storage` |
| `TRUSTIOT_SMTP_USERNAME`、`TRUSTIOT_SMTP_PASSWORD` | `smtp.credentialsSecret`，引用是 `optional: true` | `credentials` 含 `smtp` |
| `TRUSTIOT_AI_ANTHROPIC_API_KEY`、`TRUSTIOT_AI_OPENAI_API_KEY` | `ai.providers.<供應商>.apiKeySecret` | `ai-gateway` 的 `credentials` 含 `ai`（決策 28；預設就含），各自的 Secret 有名稱時。閘道的設定以 `KEY_ENV` 指向這兩個名稱（見「AI 閘道」） |

引用的 Secret 不存在時，pod 停在 `CreateContainerConfigError`，只有 SMTP 帳密例外：它的引用是 `optional: true`。身分服務以它寄登入驗證碼，又負責每個帶 cookie 的請求都要經過的 ext_authz 檢查，寄信的帳密缺了不該讓整個平台的請求都失敗。Secret 或其中的 key 不存在時 pod 照常啟動、沒有這兩個變數；平台設定 `TRUSTIOT_SMTP_AUTH_REQUIRED` 告訴服務 relay 需要帳號，所以它啟動時記一行 ERROR、`send_code` 回 503，而不是不帶帳號寄信（`services/identity/README.md`「寄信（SMTP）」）。這也表示 Secret 漏建時 `helm install --wait` 與 Flux 不會失敗：要看身分服務的 ERROR。叢集測試（`tests/e2e/`）的 namespace 沒有實例 overlay 指定的依賴 Secret，chart 測試確認每個叢集測試的安裝都不需要它們（「e2e overlays」）。

只給身分服務的變數（chart 設定，`TRUSTIOT_` 開頭，自訂 `env` 不能覆寫）：

| 變數 | 來源 | 何時出現 |
| --- | --- | --- |
| `TRUSTIOT_INGRESS_ROUTES` | `ingress.routes` 換成實際主機名稱的 JSON 陣列 `[{host, path, match, service}]`，決定 token 的受眾 | 一律（沒有路由時是 `[]`） |
| `TRUSTIOT_ON_BEHALF_CALLS` | `mesh.calls` 中兩個啟用的服務之間、不含 `ai-gateway` 的呼叫，JSON `{"<呼叫端>": ["<被呼叫端>", ...]}`：token 交換只替這些呼叫換 token（見「服務間授權（mesh）」） | 一律（沒有這種呼叫時是 `{}`） |
| `TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE` | 掛載的簽章金鑰檔 `/etc/trustiot/identity-signing-keys/signing-keys.json` | 一律 |
| `TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION` | `internalIdentity.revokeBeforeGeneration`：不用代比它小的金鑰簽章 | 一律 |
| `TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS` | `internalIdentity.keyPropagationSeconds`：金鑰與公開的公鑰不一致多久之後記 ERROR | 一律 |
| `TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS` | 輪替 CronJob 渲染時是 `internalIdentity.maxKeyAgeSeconds`，否則 `0`：正在簽章的金鑰的 `published_at` 超過它加 `keyPropagationSeconds` 再加 2 天時記一次 ERROR（輪替沒在跑或一直失敗） | 一律 |

只給 certs 服務的變數（chart 設定，自訂 `env` 不能用同樣的名稱）：

| 變數 | 來源 | 何時出現 |
| --- | --- | --- |
| `TRUSTIOT__CA__KEYS_FILE` | 掛載的簽發 CA 金鑰檔 `/etc/trustiot/certs-ca-keys/issuing-ca-keys.json` | 一律 |
| `TRUSTIOT__CA__ROOT_CERTIFICATE_FILE` | 掛載的根憑證 `/etc/trustiot/certs-ca/root.pem`（`certificateAuthority.rootCertificate`） | 一律 |
| `TRUSTIOT__CA__CERTIFICATES_FILE` | 掛載的簽發 CA 憑證 `/etc/trustiot/certs-ca/issuing.pem`（`certificateAuthority.issuingCertificates`） | 一律 |

## 擴充 chart

- **新增服務**：在 `values.yaml` 的 `services` 加一個 key，並同步 docs/architecture/deployment.md 的服務表（測試比對兩邊）。服務特有的埠、資源、探針與 Secret 寫在該 key 底下。
- **服務之間的呼叫**：服務要呼叫另一個服務的內部 API 時，在服務的 issue 把這條呼叫加進 `values.yaml` 的 `mesh.calls`；沒列的呼叫會被重置。兩個服務之間的呼叫同時也是代表使用者的呼叫（身分服務替它換 token），並更新「呼叫矩陣」那張表。服務間授權的規則集中在 `templates/_mesh.tpl` 的 `trustiot.mesh.plan` 與 `trustiot.mesh.onBehalfCalls`，AuthorizationPolicy、NetworkPolicy 與 `TRUSTIOT_ON_BEHALF_CALLS` 都由它們產生，不要為單一服務另外寫政策。
- **對外的路由**：服務要經入口對外時，在服務的 issue 把路由加進 `values.yaml` 的 `ingress.routes`（預設需要 cookie）。不要新增例外路徑：決策 27 只允許登入、閘道器憑證換發、MQTT 轉接與健康檢查四種，加上 `services.web` 開啟時 `hosts.app` 上 SPA 的靜態檔，要再加必須先改決策。`hosts.app` 的 API 路由放在 `/api` 底下：`services.web` 開啟時其他路徑都是 SPA 的。身分服務以同一份路由決定 token 的受眾（`TRUSTIOT_INGRESS_ROUTES`），所以路由的後端就是收 token 的服務；`ai-gateway` 不驗證內部身分（決策 28），不能有 cookie 路由。入口的規則集中在 `templates/_ingress.tpl` 的 `trustiot.ingress.plan`；可信的來源 IP 沿用 `trustiot.clientIpSource`，不要另外加欄位。
- **新增叢集內元件**（broker、Postgres、ClickHouse、Qdrant、collector、Grafana）：放在自己的 `templates/<元件>.yaml`，以 `*.mode` 或開關控制；Service 只能是 ClusterIP；namespace 預設拒絕所有連入：元件在 mesh 內時（pod 帶 `istio.io/dataplane-mode: ambient`）把它加進 `templates/_mesh.tpl` 的 `trustiot.mesh.components`、用戶端列進 `mesh.calls`，並定義 `trustiot.<元件>.pod`（container ports 與探針，元件的模板也從它渲染，NetworkPolicy 依它只對節點來源開探針埠），AuthorizationPolicy 與 NetworkPolicy 由 chart 產生；不在 mesh 內時（例如叢集內的 Postgres）自帶 NetworkPolicy，只對實際的用戶端開它的埠，並讓 kubelet 的健康檢查到得了；用戶端以 `mesh.calls` 宣告時，把名稱加進 `templates/_mesh.tpl` 的 `trustiot.mesh.directCallees`；在 `values.schema.json` 加欄位定義、在本文件加欄位說明（測試檢查 `values.yaml` 每個欄位都在本文件），並更新「元件與開關」與平台設定的表。
- **Secret**：values 只能出現 `{name, key}` 形式的引用（測試掃描兩份 values、`values.yaml` 與 `tests/values/` 的每個欄位：名稱像密鑰的欄位只能是 `*Secret` 底下的 `name`、`key` 或 `*Key`，任何值都不能像私鑰、token 或帶帳密的 URL；並確認渲染結果沒有 Secret、只以名稱與 key 讀 Secret）；schema 的 `additionalProperties: false` 會擋下放明文的新欄位。服務要拿新的依賴帳密時，在 `_helpers.tpl` 的 `trustiot.credentialCatalog` 加一列（變數名稱、Secret、可以拿的服務）、在 schema 的 `credentials` 列舉加上名稱，並更新「密鑰的環境變數」表；自訂 `env` 自動不能讀這個帳密。叢集內元件自己產生的 Secret（例如 CloudNativePG 的 CA、伺服器私鑰與資料庫帳號）整個加進 `trustiot.dependencySecretNames`，自訂 `env` 就讀不到它的任何 key。新的平台變數用 `TRUSTIOT_` 開頭，自動受保留規則保護。
- **測試**：規則寫進 `tests/chart.test.ts`。`tests/values/` 的兩個安裝實例範例與 `smoke.yaml`（以 traefik/whoami 當替身服務）也供 nightly（Laysi/trustiot-v2#69）在叢集上安裝使用；叢集測試是 `tests/e2e/ingress.e2e.ts`（入口）、`tests/e2e/mesh.e2e.ts`（服務間授權）、`tests/e2e/otel.e2e.ts`（日誌管線）、`tests/e2e/postgres.e2e.ts`（叢集內的 Postgres）、`tests/e2e/postgres-replication.e2e.ts`（收回 `postgres` 與 `template1` 的權限之後，叢集內 Postgres 的複寫、switchover、經 Barman Cloud plugin 的備份與還原）與 `tests/e2e/clickhouse-admin.e2e.ts`（ClickHouse 管理帳號密碼的產生、輪替、rollback 與從人建立的 Secret 搬移）；MQTT broker 沒有自己的叢集測試，由 nightly 以 `smoke.yaml` 部署並檢查（見「MQTT broker」），前兩個共用 `tests/e2e/cluster.ts`，nightly 可直接沿用。`bun test` 的 kubeconform 已帶 CRD schema 目錄，Gateway API、Istio 與 CloudNativePG 的資源可以直接驗證。兩個實例範例會部署叢集內的 Postgres 與 ClickHouse（`bundled`）與 collector，在叢集上安裝前要先裝好 CloudNativePG operator（見「叢集內的 Postgres」），ClickHouse 的管理帳號密碼在叢集內產生，不必事先建立 Secret；`smoke.yaml` 改用 nightly 自己建置的 Postgres 映像。入口與日誌管線的叢集測試用 `tests/e2e/values-e2e.yaml`、`values-otel-e2e.yaml` 把 Postgres 改成 `external`（沒有服務列出 `postgres`，所以不需要 CA），不部署 Postgres；入口測試的 ClickHouse 也改成 `external`，並關掉 collector。`tests/` 不會打包進 chart（`.helmignore`）。

## OpenShift 與其他 Kubernetes

- Pod 符合 restricted Pod Security Standard，且不固定 UID，可以在 OpenShift 的 restricted-v2 SCC 下執行。例外是固定的群組 ID：身分服務與 certs 服務的 `fsGroup`（`services.identity.podSecurityContext.fsGroup`、`services.certs.podSecurityContext.fsGroup`）與叢集內 ClickHouse 的 UID／GID，在 OpenShift 都設成 `null` 交給 SCC 指派。MQTT broker 的 UID／GID（`mqttBroker.podSecurityContext`）也一樣，但還沒在 OpenShift 試過。
- 預設只排到 arm64 節點；客戶用 x86 主機時，要先加出 amd64 映像（決策 5）再調整 `serviceDefaults.nodeSelector`（叢集內 Postgres 是 `postgres.bundled.nodeSelector`）。
- 叢集內的 Postgres 需要 CloudNativePG operator。OpenShift 通常由 OperatorHub 安裝（operator 在 `openshift-operators`），這時把 `postgres.bundled.operator.namespace` 與 `podLabels` 改成它實際的 namespace 與 pod label，NetworkPolicy 才放行它；Postgres pod 的安全設定由 operator 決定。目前沒有 OpenShift 環境可測。
- chart 需要叢集已裝 Istio ambient 與 Gateway API CRD，並設好 ext_authz provider。k3s 用 `global.platform=k3s`，OpenShift 用 `global.platform=openshift` 或 OpenShift Service Mesh 3，步驟見 `deploy/istio/README.md`。
- 入口 gateway 的 pod 由 Istio 產生，以 UID 1337 執行，符合 restricted Pod Security Standard（2026-10-10 在 restricted namespace 實測）；OpenShift 的 restricted-v2 SCC 會指派其他 UID，目前沒有 OpenShift 環境可測。
- `ingress.tunnel.allowedSources` 的預設值是 k3s 單節點的本機位址；其他 CNI 要改成 cloudflared 與 kubelet 實際的來源位址。
