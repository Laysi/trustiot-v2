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

`values.schema.json` 驗證合併後的結果：缺欄位、拼錯欄位、以 IP 當主機名稱、在 values 放明文密鑰、SaaS 的外部端點不是 https，都會讓 `helm lint`、`helm template` 與 `helm install` 直接失敗，不會裝出壞掉的版本。

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
| 服務間授權（AuthorizationPolicy、NetworkPolicy） | `mesh.*` | 完成：每個服務自己的 ServiceAccount、依 `mesh.calls` 產生的 AuthorizationPolicy、namespace 預設拒絕的 NetworkPolicy；不部署 waypoint。服務之間的呼叫隨各服務加入 `mesh.calls` | Laysi/trustiot-v2#63；代表使用者呼叫的授權矩陣在 Laysi/trustiot-v2#106 |
| Postgres（CloudNativePG 加 AGE） | `postgres.mode` | `bundled`：單一 instance 的 CloudNativePG Cluster、AGE extension 與 NetworkPolicy（見「叢集內的 Postgres」）；CloudNativePG operator 與 CRD 是叢集層級的平台元件，由 [deploy/cloudnative-pg](../cloudnative-pg/README.md) 安裝，不在 chart 裡。`external` 已接到平台設定。含 AGE 的映像在 [deploy/images/postgres](../images/postgres/README.md)，由 release.yml 發佈。每個服務的角色與 migration 由 migration Job 處理 | Laysi/trustiot-v2#134（備份與 PITR） |
| migration Job | `migrate.enabled` | 完成（預設開）：每個服務的 Postgres migration、ClickHouse DDL、服務角色與 collector 的 ClickHouse 帳號；每個服務的 pod 以 init container 等它完成才啟動（見「migration Job」） | Laysi/trustiot-v2#60；第一個 outbox 生產端在 Laysi/trustiot-v2#65，每租戶的 AGE 角色在 Laysi/trustiot-v2#176 |
| ClickHouse | `clickhouse.mode` | `bundled`：單節點 StatefulSet（見「叢集內的 ClickHouse」）；`external` 已接到平台設定。schema、帳號與 row policy 由 migration Job 以 SQL 套用 | admin 密碼的 Secret 見「叢集內的 ClickHouse」與 docs/runbooks/secrets.md |
| MQTT broker（Mosquitto） | — | 尚未加入 | Laysi/trustiot-v2#84 |
| Qdrant | `qdrant.mode` | `external` 已接到平台設定；`bundled` 尚未加入 | Laysi/trustiot-v2#194 |
| 物件儲存、SMTP、AI 端點 | `objectStorage.*`、`smtp.*`、`ai.*` | 一律外部，設定已接到平台設定 | Laysi/trustiot-v2#133、Laysi/trustiot-v2#169、Laysi/trustiot-v2#139 |
| OpenTelemetry collector 與 OTLP 轉送 | `observability.collector.enabled` | 完成（預設開）：收服務的 OTLP、遮蔽後把日誌與 trace 寫進 ClickHouse 的 `trustiot_logs`，可轉送到 SIEM（見「OpenTelemetry collector」）。帳號由 migration Job 以 SQL 建立；指標先收下丟棄 | Laysi/trustiot-v2#183（指標存放與 Grafana）、Laysi/trustiot-v2#63（服務間授權要放行 collector） |
| Grafana | — | 尚未加入 | Laysi/trustiot-v2#183 |
| Sentry | `observability.sentry.*` | 開關、環境名稱與 DSN 已接到每個服務 | Laysi/trustiot-v2#184 |
| 內部身分（公鑰、簽章私鑰） | `internalIdentity.*` | 完成：公鑰以 JWK Set 放進平台設定 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`；簽章私鑰的 Secret 只掛進身分服務；身分服務另外拿到需要 cookie 的路由（`TRUSTIOT_INGRESS_ROUTES`），決定 token 的受眾 | Laysi/trustiot-v2#61；SaaS 的值放進 `deploy/flux/` 由 Laysi/trustiot-v2#70 做 |
| 密鑰 | 各個 `*Secret` 欄位、`services.<name>.credentials` | values 只存 Secret 名稱與 key；Sentry DSN 給每個服務，其他依賴的帳密只給列出它的服務；外部 ClickHouse 的 CA 掛進每個服務，Postgres 的 CA 只掛進在 `mesh.calls` 列出 `postgres` 的服務；chart 不渲染任何密鑰值（測試檢查），它宣告的 Secret 只有 migration Job 填入的空 Secret；叢集內 Postgres 的 Secret 由 CloudNativePG 產生，服務的自訂 `env` 讀不到；每個服務的資料庫密碼與 collector 的 ClickHouse 密碼由 migration Job 產生，各放在自己的 Secret（`<release>-<服務>-password`、`<release>-otel-collector-password`），只給該服務或 collector，migration 帳號只給 migration Job | SaaS 的 Secret 以 SOPS 加密放在 `deploy/flux/`（Laysi/trustiot-v2#70）；地端由客戶建立。規則與清冊見 docs/architecture/secrets.md |

## 服務

`services` 的 key 與 docs/architecture/deployment.md 的服務表一致（測試比對），物件名稱是 `<release>-<key>`；release 名稱不含 `trustiot` 時是 `<release>-trustiot-<key>`；超過 63 個字元時縮短前段、保留 `-<key>`，不同服務不會撞名。每個服務產生一個 ServiceAccount、一個 ClusterIP Service 與一個 Deployment。

`identity`、`assets`、`certs`、`ingest`、`telemetry-query`、`gateway-ops`、`mqtt-bridge`、`assistant`、`ai-gateway`、`alerts`、`field-context`、`knowledge`

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
| `ingress.direct.serviceType` | `LoadBalancer` | `direct` 時入口 Service 的類型：`LoadBalancer`（k3s 的 ServiceLB 或客戶的負載平衡器）、`NodePort` 或 `ClusterIP`（前面另有客戶的反向代理時） |
| `ingress.direct.tlsSecret` | `""` | `direct` 時必填；外層 TLS 憑證所在的既有 `kubernetes.io/tls` Secret，涵蓋三個主機名稱，用客戶的憑證或我們 CA 簽的 |
| `ingress.publicPaths.login` | `/auth/login` | 登入，`hosts.app` → `identity` |
| `ingress.publicPaths.healthCheck` | `/healthz` | 健康檢查（外部監控），`hosts.app` → `identity` |
| `ingress.publicPaths.certRenewal` | `/certs/renew` | 閘道器憑證換發（以 TPM 簽章驗證，決策 12），`hosts.gateway` → `certs` |
| `ingress.publicPaths.mqtt` | `/mqtt` | 閘道器的 MQTT WebSocket，`hosts.gateway` → `mqtt-bridge` |
| `ingress.routes` | `[]` | 需要 cookie 的路由，每項 `{host, path, match, service, port}`，見「入口 gateway」 |

三個主機名稱都用我們自己的網域（可搬移規則 1），schema 拒絕 IP 與大寫；搬遷時只改 DNS。

### 入口 gateway

決策 27 的入口：Istio 實作 Gateway API，每個請求都先以 AuthorizationPolicy 的 CUSTOM 動作把 cookie 交給身分服務（ext_authz），只有例外路徑免驗證。chart 產生下列物件，名稱前綴同服務（`<release>-ingress…`）：

| 物件 | 內容 |
| --- | --- |
| Gateway `<release>-ingress` | `gatewayClassName: istio`；`tunnel` 只有 80 的 HTTP listener，`direct` 只有 443 的 HTTPS listener（`tls.mode: Terminate`）；只接受同一個 namespace 的路由。Istio 依它建立 Deployment、Service 與 ServiceAccount `<release>-ingress-istio` |
| ConfigMap `<release>-ingress-options` | 經 `infrastructure.parametersRef` 交給 Istio：副本數、資源、Pod 的 `seccompProfile: RuntimeDefault`（符合 restricted）、Service 類型與 PodDisruptionBudget |
| HTTPRoute `<release>-ingress-app`、`-gateway`、`-ota` | 每個主機一個，只有有規則時才產生。每條規則都把 `x-forwarded-proto` 設為 `https`（cloudflared 到入口是明文 HTTP，後端判斷 Secure cookie 與重導向網址要靠它）；每條規則都移除底線寫法的 `x_internal_identity`、`x-internal_identity`、`x_internal-identity`，例外路徑另外移除 `x-internal-identity` |
| AuthorizationPolicy `<release>-ingress-cookie-check` | CUSTOM，provider 為 `ingress.authzProvider`。第一條規則檢查例外路徑以外的所有路徑；每個例外路徑再一條規則，在它以外的主機照樣檢查，例外不會漏到別的主機；它自己的主機帶不帶埠號都算（`notHosts` 列 `<主機>` 與 `<主機>:*`）。沒有任何例外路徑時檢查所有請求 |
| AuthorizationPolicy `<release>-ingress-deny-internal` | DENY `/internal*`：服務之間的內部 API 不能從外面進來，即使有 `/` 這種全包的路由。CUSTOM 先執行，所以沒有 cookie 回 401、有 cookie 回 403。`//internal`、`/%2finternal` 這類寫法要靠 mesh 的路徑正規化先整理成 `/internal` 才比對得到，見下方「依賴 mesh 的路徑正規化」 |
| NetworkPolicy `<release>-ingress` | `tunnel`：只放行 `ingress.tunnel.allowedSources` 連 80（cloudflared）與 15021（kubelet 的探針），叢集內其他 pod 連不到入口；`direct`：只開 443 與 15021。兩種都不開 Envoy 的指標埠（15020、15090），見下方「入口 pod 的埠」 |

入口 pod 的埠（Laysi/trustiot-v2#287，決定於 2026-10-11）：NetworkPolicy 每條規則都帶 `ports`，只有兩個 TCP 埠，不在 values 另寫一份：

- listener 的埠：由 `templates/_ingress.tpl` 的 `trustiot.ingress.listenerPort` 決定（`tunnel` 80、`direct` 443），Gateway 的 listener 與 NetworkPolicy 都用它。Istio 產生的 Service 把 listener 的埠原樣轉到 pod 的同一個埠，Envoy 直接聽這個埠（Istio 在入口 pod 設 `net.ipv4.ip_unprivileged_port_start=0`），所以 NetworkPolicy 放行的就是這個數字。
- Istio 的狀態埠 15021（`trustiot.ingress.statusPort`）：Istio 的 kube-gateway 樣板固定讓 `istio-proxy` 的 startup 與 readiness 探針 `GET /healthz/ready` 打 15021，Service 也列為 `status-port`；這個數字由 Istio 決定，chart 無法從自己的設定推得。
- 節點來源：入口 pod 不在 ambient mesh 裡（Istio 給它 `istio.io/dataplane-mode: none`），kubelet 的探針不經 istio-cni 的 SNAT，來源是主機自己的位址（k3s 為 cni0），與 cloudflared 相同，所以 `tunnel` 只有一條規則：`allowedSources` 連 80 與 15021。服務用的節點來源 `169.254.7.127/32`（見「服務間授權」）不適用，NetworkPolicy 也不放行它；chart 測試檢查 Gateway 的 `infrastructure.labels` 不帶 `istio.io/dataplane-mode`，否則入口 pod 被放進 mesh 後探針會被擋。
- 同一個節點的主機不受這條規則限制：Kubernetes NetworkPolicy 一律放行來自 pod 所在節點的連線，k3s 的 kube-router 以「來源是本機位址」實作，cni0 的位址也算。所以單節點的 SaaS 主機上，cloudflared、kubelet 與主機上其他行程（包括其他 namespace 的 `hostNetwork` pod）都連得到入口 pod 的每個埠，Envoy 的指標埠 15020、15090 也一樣；2026-10-11 在實驗機實測（見上面的測試報告）。這條規則與它的埠限制管的是 `allowedSources` 裡不是 pod 所在節點的來源：多節點時的其他節點、改跑在其他節點的 cloudflared，或寫得比本機更大的網段。服務的節點來源 `169.254.7.127/32` 不是主機的位址，不適用這個例外：2026-10-10 的 PoC 實測預設拒絕時要明確放行它，探針才會通過（docs/feasibility/2026-10-10-edge-poc.md），所以「服務間授權」的探針埠限制在單節點上也有作用。
- 驗證：chart 測試對兩種型態比對每條規則的 `ports` 與 Gateway 的 listener 加 15021；nightly 與 PR 的 `smoke` job 在 k3d 上讀 Istio 實際產生的 Deployment 與 Service，確認探針與 Service 用的埠和 NetworkPolicy 放行的埠完全相同（`scripts/nightly/manifests.ts` 的 `gatewayPorts`）。Istio 升級改了狀態埠時，這一項會失敗。2026-10-11 在實驗機（Istio 1.30.5）的紀錄見 `docs/test-reports/2026-10-11-287-gateway-netpol-ports.md`。

例外路徑（`ingress.publicPaths`）的規則：

- 只有決策 27 列的四種，主機與後端固定在 chart，values 只能改路徑；schema 與樣板都只接受一個完整的小寫路徑，不能有萬用字元、`.`、`..`、`//`、結尾的 `/`、`%` 或 `;`，也不能在 `/internal` 底下。路由以 `Exact` 比對，Envoy 依 mesh 的路徑正規化（解碼 `%2F`、RFC 3986、合併斜線）整理後才比對，大小寫、編碼與 `..` 的變形都不是例外，照樣要 cookie。
- 後端服務關閉時，這個例外路徑既不路由也不免驗證，請求不會落到別的路由上。
- 例外路徑沒有 ext_authz 覆寫身分標頭，所以路由移除用戶端帶來的 `x-internal-identity`；需要 cookie 的路徑不能移除，因為路由的標頭規則在 ext_authz 之後執行，會把身分服務給的值一起移除。
- 例外只在它自己的主機成立，`Host` 帶不帶埠號都一樣（見下方「`Host` 帶埠號」）。

依賴 mesh 的路徑正規化（Laysi/trustiot-v2#255）：上面的路徑規則與 `/internal*` 的 DENY 都假設 istiod 設了 `meshConfig.pathNormalization.normalization: DECODE_AND_MERGE_SLASHES`（`deploy/istio/istiod-values.yaml`）。沒有它時，帶有效 cookie 的 `//internal/tenants` 或 `/%2finternal/tenants` 比對不到 DENY，會落到全包路由。chart 無法檢查叢集的 mesh 設定，所以每個叢集都要依 `deploy/istio/README.md` 安裝；`tests/e2e/ingress.e2e.ts` 的回歸案例在少了這個設定時會失敗。設定的影響（`%2F` 一律解碼、路徑參數不能夾帶 `/`）見該 README 的「路徑正規化」。

底線寫法的身分標頭（Laysi/trustiot-v2#255，決定於 2026-10-11）：Envoy 預設照轉名稱帶底線的標頭，`x_internal_identity` 對它是另一個標頭；但 CGI／WSGI 風格的伺服器把 `-` 與 `_` 視為相同（都變成 `HTTP_X_INTERNAL_IDENTITY`），偽造值可能被當成內部身分。每條路由（例外路徑與需要 cookie 的路徑）都移除 `x-internal-identity` 的三種底線寫法。不採用「拒絕所有帶底線的標頭」：Istio 沒有對應的設定，要用 EnvoyFilter（Istio 不保證跨版本相容，而且同樣是 mesh 層級），也可能擋掉閘道器或第三方客戶端合法的標頭；需要信任的標頭只有 `x-internal-identity` 一個（決策 27），移除它的變形就夠。服務仍然只讀 `x-internal-identity` 並驗簽（`crates/service-kit`）。

`Host` 帶埠號（Laysi/trustiot-v2#255，決定於 2026-10-11）：入口**接受** `Host` 帶埠號，例如閘道器送 `Host: gw.example.com:443`。Istio 的路由本來就把 `<主機>` 與 `<主機>:<任何埠>` 視為同一個主機，但例外的授權規則原本只列主機名稱，帶埠號時例外不成立，憑證換發與 MQTT 會回 401。現在例外規則同時列 `<主機>` 與 `<主機>:*`，與路由的比對方式一致：例外只在它自己的主機成立，`app` 主機帶埠號照樣要 cookie。閘道器韌體不必為此特別處理 `Host`；WebSocket 函式庫或客戶網路的代理放進埠號（預設的 443 或其他埠）都可以。Cloudflare Tunnel 轉發的 `Host` 沒有埠號，SaaS 原本就不受影響。這也是 Laysi/trustiot-v2#77（真身分服務與閘道器的端到端驗證）要沿用的約定。

需要 cookie 的路由（`ingress.routes`）：`host` 是 `app`、`gateway` 或 `ota`；`path` 是 `/` 或與例外路徑同樣格式的路徑，不能在 `/internal` 底下；`match` 是 `PathPrefix` 或 `Exact`；`service` 必須是已啟用的服務，`port` 是它的埠名（預設 `http`）。同一個主機不能有兩條相同路徑，也不能與例外路徑相同。例如資產服務的 API：

```yaml
ingress:
  routes:
    - {host: app, path: /api/assets, match: PathPrefix, service: assets}
```

與身分服務的約定（Laysi/trustiot-v2#61 實作，細節見 `services/identity/README.md`）：入口以 `<pathPrefix><原始路徑>` 呼叫身分服務，`pathPrefix` 是 `/internal/ext-authz`，只帶 `cookie`（另有 Host、方法、路徑與 Authorization）。

| 身分服務的回應 | 情況 | 入口 |
| --- | --- | --- |
| 200，剛好一個 `x-internal-identity` | session 有效，且原始請求的主機與路徑對得到 `ingress.routes` 的一條路由 | 以這個值覆寫用戶端帶來的同名標頭，轉給後端 |
| 401 `unauthenticated` | 沒有 session cookie、cookie 重複或格式不對、session 不存在、過期或已撤銷 | 原樣交給用戶端 |
| 403 `permission_denied` | session 有效，但身分服務的路由表對不到這個請求（正常情況下不會發生：入口沒有路由的請求不會呼叫身分服務） | 原樣交給用戶端 |
| 503 `unavailable` | 資料庫失敗，或 0.4 秒內查不完 | 把 5xx 當成錯誤，回自己的 503（`statusOnError`） |

身分服務無法連線、出錯或超過 0.5 秒時入口也回 503。session cookie 名稱是 `__Host-trustiot-session`。token 的受眾（`aud`）是請求要去的服務：chart 把 `ingress.routes` 換成實際主機名稱後以 `TRUSTIOT_INGRESS_ROUTES` 交給身分服務，身分服務以 Gateway API 的規則比對（主機相同、不看埠號；`Exact` 優先，其次最長、以路徑段為界的 `PathPrefix`）。比對不一致只會造成拒絕，不會放寬權限：token 只有它的受眾接受。入口的設定在 istiod 的 provider，見 `deploy/istio/README.md`。

服務的 pod 帶 `istio.io/dataplane-mode: ambient` 加入 ambient mesh，入口到服務、服務之間由 ztunnel 加 mTLS；`podLabels` 不能改這個 label。服務之間的授權見下一節。

叢集上的 e2e 測試（cookie、例外路徑、標頭與其底線寫法、`Host` 帶埠號、28 種路徑繞過、`/internal` 的 11 種斜線寫法、身分服務停機）在 `tests/e2e/`，用法見該目錄的 README。

### 服務間授權（mesh）

決策 27 的服務間 mTLS 與授權：ztunnel 替每條連線加 mTLS，呼叫端的身分是它的 ServiceAccount（每個服務一個，`<release>-<服務>`）。「誰可以呼叫誰」只有一份清單，由兩層各自執行，任何一層單獨存在都擋得住清單以外的呼叫：

| 欄位 | 預設值 | 說明 |
| --- | --- | --- |
| `mesh.trustDomain` | `cluster.local` | mesh 的 SPIFFE trust domain（Istio 的 `meshConfig.trustDomain`）；呼叫端身分寫成 `<trustDomain>/ns/<namespace>/sa/<ServiceAccount>` |
| `mesh.calls` | `mesh.calls.ingest` 與 `mesh.calls.telemetry-query` 都是 `[clickhouse]`，`mesh.calls.identity` 是 `[postgres]` | 誰可以呼叫誰：`<呼叫端>: [<被呼叫端>, ...]`。呼叫端是 `services` 的 key；被呼叫端是 `services` 的 key、叢集內元件 `clickhouse`（`clickhouse.mode: bundled` 時）與 `otel-collector`（collector 開啟時），或 `postgres`（兩種 `postgres.mode` 都可以列，見下方）。遙測的呼叫由 chart 自動加上，不必列：每個啟用的服務 → `otel-collector`，`otel-collector` → `clickhouse`。不認得的名稱讓安裝失敗；服務未啟用或元件沒有部署時略過這條，所以可以先列還沒出映像的服務。預設值是決策 14 與 docs/architecture/deployment.md 寫明的 ClickHouse 用戶端：資料接收寫入、時序查詢以每租戶唯讀帳號讀取；以及使用 Postgres 的身分服務。各服務的 issue 加入它要呼叫的內部 API，用 Postgres 的服務加上 `postgres` |

```yaml
mesh:
  calls:
    assistant: [ai-gateway, telemetry-query]   # 例子：assistant 可以呼叫這兩個服務
```

除了 `mesh.calls`，入口 gateway（ServiceAccount `<release>-ingress-istio`）自動可以呼叫它路由到的服務（例外路徑與 `ingress.routes` 的後端），以及做 cookie 驗證（ext_authz）的身分服務。chart 產生下列物件：

| 物件 | 內容 |
| --- | --- |
| NetworkPolicy `<release>-default-deny` | 選取 namespace 內所有 pod、不放行任何連入：沒有其他 NetworkPolicy 放行的連線一律拒絕。服務全關時也會產生 |
| AuthorizationPolicy `<release>-<服務或元件>` | 每個啟用的服務與每個部署的叢集內元件（目前是 `bundled` 的 ClickHouse）一個，ALLOW，以 selector 選它的 pod，只允許呼叫端的 principals（ztunnel 在 L4 執行）。沒有任何呼叫端時是沒有規則的 ALLOW，拒絕所有連線 |
| NetworkPolicy `<release>-<服務或元件>` | 同上，每個一個：只從同一批呼叫端的 pod（與入口 gateway 的 pod）放行 TCP 15008，也就是 ztunnel 的 HBONE 埠，mesh 內的連線都從這裡進來，不直接連應用程式的埠；另對節點來源 `169.254.7.127/32`（kubelet 的健康檢查由 istio-cni SNAT 成這個位址）只放行該工作負載探針實際連的 TCP 埠，服務是健康埠（`healthPort`，預設 8081），見下方「節點來源」。只有 exec 探針時不產生這條規則；沒有呼叫端也沒有探針埠時 `ingress: []`，拒絕所有連入 |

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
- 不部署 waypoint：L4 的身分已足夠。服務之間需要 L7 規則（例如依路徑或方法）時才為該服務加 waypoint，並把它的 AuthorizationPolicy 改成以 `targetRefs` 指向 Service、在 ztunnel 層放行 waypoint 的 ServiceAccount；代表使用者呼叫的授權（內部身分 token）由服務自己驗證，不靠 waypoint（Laysi/trustiot-v2#106）。
- 叢集測試 `tests/e2e/mesh.e2e.ts` 以真實連線驗證清單內放行、清單外被拒，以及兩層各自生效，用法見該目錄的 README。
- 叢集內的 ClickHouse 走同一套規則：只有 `mesh.calls` 列出的 `ingest` 與 `telemetry-query`，以及 chart 自動加上的 OpenTelemetry collector（Laysi/trustiot-v2#73）與 migration Job（`migrate.enabled` 為 true 時，Laysi/trustiot-v2#60）連得到它，其他服務與 pod 都被拒，kubelet 的 `/ping` 探測經 `169.254.7.127/32` 照常通過，節點來源只開 HTTP 8123。`mesh.calls` 的呼叫端只能是服務，元件之間的呼叫由 `trustiot.mesh.plan` 自己加（`_mesh.tpl`），collector 與 migration Job 都是這樣；還沒有的 Grafana（Laysi/trustiot-v2#183）加入時也要這樣加進來。ClickHouse 的政策只以 chart 測試驗證，2026-10-10 的實驗機測試沒有部署它（見測試報告）。
- collector 也是 mesh 內的元件（`trustiot.mesh.components` 的 `otel-collector`，埠與探針定義在 `trustiot.otel-collector.pod`）：每個啟用的服務都自動是它的呼叫端（送 OTLP），其他 pod 連不到它，節點來源只開健康檢查的 13133。它連出到外部 ClickHouse 或 SIEM 不受 NetworkPolicy 限制（只管連入）；SIEM 在同一個 namespace 時，要另外以 NetworkPolicy 放行 collector 的 pod 連它。
- `postgres` 是「這個服務用 Postgres」的宣告，不經 mesh：Postgres 的每條連線本身就是 TLS 並以 verify-full 驗證伺服器，叢集內的 Postgres pod 也不加入 ambient mesh（見「叢集內的 Postgres」），所以它沒有 AuthorizationPolicy，也不走 HBONE。列出 `postgres` 的服務才會掛上 Postgres 的 CA 並拿到 `TRUSTIOT_POSTGRES_CA_FILE`（`external` 也一樣）；`bundled` 時 `<release>-postgres` 這個 NetworkPolicy 也只放行這些服務的 pod 連 5432。同一份清單決定兩件事，不會一邊有、一邊沒有。
- 預設拒絕套用到 namespace 內所有 pod，包括之後加入的叢集內元件（broker 等）：在 mesh 內的元件加進 `trustiot.mesh.components`，由同一份清單產生政策；不在 mesh 內的元件自帶 NetworkPolicy，只放行實際的用戶端（見「擴充 chart」），叢集內的 Postgres 就是這樣。chart 測試檢查每個 Deployment 與 StatefulSet 的 pod 都有自己的 NetworkPolicy 與 AuthorizationPolicy。

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
| `services.<name>.ports` | 整個取代埠清單；埠名與埠號不能重複，也不能用 `health` 或健康埠的埠號；探針只能指向清單內的埠名與 `health` |
| `services.<name>.healthPort` | 覆寫健康埠的埠號；服務的 `http.health_listen_addr` 要跟著改 |
| `services.<name>.env` | 接在 `serviceDefaults.env` 後面，名稱不能重複。`TRUSTIOT_` 後面接英數字、`SENTRY_`、`OTEL_` 開頭的名稱（不分大小寫）保留給 chart，避免覆寫平台設定與 chart 給的密鑰；`TRUSTIOT__`（兩個底線）開頭的名稱不在保留範圍。名稱像密鑰（含 SECRET、PASSW、PASSPHRASE、PWD、TOKEN、API_KEY、DSN、PRIVATE、CREDENTIAL，或以 `_KEY` 結尾，不分大小寫）的變數只能用 `valueFrom.secretKeyRef`，不能用字面值或 ConfigMap 等其他來源；每一項只能有 `value` 或一個 `valueFrom` 來源。任何變數都不能讀 values 指定的依賴帳密與 Sentry DSN（同一個 Secret 的同一個 key），依賴帳密要改列在 `credentials` |
| `services.<name>.credentials` | 只給這個服務的依賴帳密，可列 `qdrant-read`、`qdrant-write`、`object-storage`、`smtp`、`ai`，變數見「密鑰的環境變數」；不能放在 `serviceDefaults`，避免每個服務都拿到。`ai` 只能給 `ai-gateway`（決策 28）；`qdrant-read` 與 `qdrant-write` 只能給 `knowledge`，因為 Qdrant 查詢的租戶條件由知識庫服務加上（docs/architecture/rag.md） |
| `services.<name>.resources` | 逐欄覆寫資源設定 |
| `services.<name>.podSecurityContext` | 逐欄覆寫；限制同 `serviceDefaults` |
| `services.<name>.podSecurityContext.fsGroup` | 只有 `identity` 有預設值 `65532`：簽章私鑰檔的權限是 0440、屬於 root 與這個群組，服務以群組身分讀它，容器裡其他使用者讀不到（見「內部身分」）。OpenShift 設成 `null`，由 restricted-v2 SCC 從 namespace 的範圍指派；其他平台拿掉它，身分服務會讀不到私鑰而拒絕啟動 |
| `services.<name>.securityContext` | 逐欄覆寫；限制同 `serviceDefaults` |
| `services.<name>.probes` | 每個 probe 整個取代；liveness 與 readiness 不能是空的 |
| `services.<name>.preStopSleepSeconds` | 覆寫 preStop 等待秒數 |
| `services.<name>.drainSeconds` | 覆寫收尾秒數，例如持有長連線的服務 |
| `services.<name>.tmpSizeLimit` | 覆寫 `/tmp` 大小上限 |
| `services.<name>.automountServiceAccountToken` | 需要呼叫 Kubernetes API 的服務才設 `true` |
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

決策 27 的內部身分 token：身分服務為每個通過 cookie 驗證的請求簽一個 60 秒的 token，每個服務以這裡的公鑰驗證。格式、驗證規則與輪替在 `crates/service-kit/README.md` 的「內部身分」與 `services/identity/README.md`。

| 欄位 | 預設值 | 說明 |
| --- | --- | --- |
| `internalIdentity.publicKeys` | `[]` | 身分服務的 Ed25519 公鑰，每把 `{kid, x}`：`kid` 是 1–64 個可見 ASCII 字元、不能重複，`x` 是 32 bytes 公鑰的 base64url（不補 `=`，43 個字元），最多 8 把。chart 組成 JWK Set 放進平台設定 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`；schema 只允許這兩個欄位，所以私鑰（`d`）放不進 values 與 ConfigMap。空的時候不設定，服務會拒絕啟動。輪替期間同時列新舊兩把 |
| `internalIdentity.signingKeySecret.name` | `""` | 放身分服務簽章私鑰的既有 Secret；身分服務啟用時必填（沒有它身分服務拒絕啟動）。私鑰是 PKCS#8 PEM（`openssl genpkey -algorithm ed25519`），以唯讀 volume 只掛進身分服務的 `/etc/trustiot/identity-signing-key/signing-key.pem`，檔案權限 0440（`defaultMode`），只有 root 與 pod 的 `fsGroup`（`services.identity.podSecurityContext.fsGroup`）可讀，路徑以 `TRUSTIOT_IDENTITY_SIGNING_KEY_FILE` 告訴它；任何服務的自訂 `env` 都不能讀這個 Secret 的這個 key。私鑰對應的公鑰必須在 `publicKeys` 裡，身分服務以它找出自己的 `kid`，找不到就拒絕啟動 |
| `internalIdentity.signingKeySecret.key` | `signing-key.pem` | |

身分服務只在啟動時讀私鑰，Kubernetes 不會因 Secret 改變而重啟 pod：換私鑰後要滾動重啟身分服務（`kubectl rollout restart`，或改 `services.identity.podAnnotations`），步驟見 `services/identity/README.md` 的「金鑰輪替」。

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
| `postgres.bundled.parameters.max_connections` | `100` | 10–1000；每個服務的連線池加總要低於它，並留給 migration Job 與 operator |
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
| `clickhouse.bundled.adminSecret.name` | `""`，`bundled` 時必填 | 放 bootstrap 管理帳號密碼的既有 Secret；服務的自訂 `env` 不能讀它 |
| `clickhouse.bundled.adminSecret.key` | `password` | |
| `clickhouse.bundled.adminSecretRevision` | `"1"` | ClickHouse 只在啟動時讀 admin 密碼。輪替密碼時先改 Secret，再改這個值，升級時 ClickHouse 會重啟並換上新密碼，migration Job 才不會拿新密碼去連還在用舊密碼的伺服器 |
| `clickhouse.bundled.storage.size` | `""`，`bundled` 時必填 | 資料磁碟區大小，每個部署依自己的資料量與保存期限估算，見「叢集內的 ClickHouse」 |
| `clickhouse.bundled.storage.storageClassName` | `""` | 空白時用叢集預設的 StorageClass |
| `clickhouse.bundled.resources` | requests `cpu: 250m`、`memory: 512Mi`；limits `cpu: "2"`、`memory: 2Gi` | ClickHouse 依記憶體上限的 90% 設定自己的上限與快取；schema 要求 `limits.memory` |
| `clickhouse.bundled.podSecurityContext` | `runAsNonRoot: true`、`runAsUser`／`runAsGroup`／`fsGroup: 101`、`seccompProfile: RuntimeDefault` | 映像沒有宣告 `USER`，所以固定 clickhouse 使用者的 UID 101；OpenShift 把三個 ID 設成 `null`，交給 SCC 指派。限制同 `serviceDefaults.podSecurityContext` |
| `clickhouse.bundled.nodeSelector` | `{}` | 映像是 multi-arch，不限架構 |
| `clickhouse.bundled.tolerations` | `[]` | |
| `clickhouse.external.url` | `""` | `external` 時必填；只接受 https，例如 `https://abc.clickhouse.cloud:8443` |
| `clickhouse.external.caSecret.name` | `""` | 服務只在私有 CA 簽發時需要；migration Job 開啟時必填：Job 只信任這個 CA、不信任公開根憑證，伺服器用公開 CA 時把那個根憑證放進來 |
| `clickhouse.external.caSecret.key` | `ca.crt` | |
| `clickhouse.external.migrationSecret.name` | `""`，`external` 且 Job 開啟時必填 | 有 access management 權限的帳號的帳密所在的既有 Secret（叢集內 ClickHouse 對應的是 `adminSecret`），只給 migration Job |
| `clickhouse.external.migrationSecret.usernameKey` | `username` | |
| `clickhouse.external.migrationSecret.passwordKey` | `password` | |
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
| `smtp.from` | `""` | 寄件者，例如 `TrustIoT <noreply@example.com>`；只能一行，避免注入郵件標頭 |
| `smtp.credentialsSecret.name` | `""` | relay 不需要驗證時留空 |
| `smtp.credentialsSecret.usernameKey` | `username` | |
| `smtp.credentialsSecret.passwordKey` | `password` | |
| `ai.chat.baseUrl` | `""` | OpenAI 相容的聊天端點，例如 `https://api.openai.com/v1`；SaaS 只接受 https |
| `ai.chat.apiKeySecret.name` | `""` | 只有 AI 閘道服務拿到金鑰（決策 28） |
| `ai.chat.apiKeySecret.key` | `api-key` | |
| `ai.embedding.baseUrl` | `""` | embedding 端點（決策 26） |
| `ai.embedding.apiKeySecret.name` | `""` | |
| `ai.embedding.apiKeySecret.key` | `api-key` | |
| `ai.rerank.baseUrl` | `""` | 重排用的聊天端點，搭配快速模型（決策 26） |
| `ai.rerank.apiKeySecret.name` | `""` | |
| `ai.rerank.apiKeySecret.key` | `api-key` | |
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

## migration Job

`migrate.enabled: true`（預設，SaaS 與地端相同）部署 `templates/migrate.yaml`，程式是 [tools/migrate](../../tools/migrate/README.md)（Laysi/trustiot-v2#60，決策 4、14、29，清單 M8）。每次執行都是冪等的：設定沒變時再跑一次，資料庫裡什麼都不變（測試比對整份 catalog）。

Job 做的事，依序：

1. 以 migration 角色（`bundled` 是 owner `trustiot_owner`，`external` 是 `postgres.external.migrationSecret`）用 verify-full 連上 Postgres，取得 session 層級的 advisory lock，整次執行都持有，ClickHouse 的部分也在鎖內；同時有另一次執行時就等它結束（`migrate.wait.lock`）。資料庫還沒起來時先等（`migrate.wait.databases`）。migration 角色必須擁有資料庫、有 CREATEROLE，不能是 superuser、不能有 BYPASSRLS，也不能擁有其他資料庫，不符就停止。
   接著在鎖內填密碼 Secret（「產生的密碼」）：缺的、不能用的、屬於較舊的 `migrate.passwordGeneration`、或在不是 Job 填過的 Secret 物件裡的 key 產生新密碼寫進去，之後的步驟把每個角色與帳號設成 Secret 裡的密碼。
2. 套用 `crates/service-kit/sql/database.sql`（收回 PUBLIC 的 TEMPORARY 等）。
3. 為每個有 Postgres migration 的服務建立角色與 schema：`<服務>_owner`（不能登入，擁有 schema 與每張表，migration 以它執行）、`<服務>_app`（服務登入用，不擁有任何物件，沒有 BYPASSRLS，`search_path` 是自己的 schema）、需要時還有 `<服務>_platform`（決策 29 的平台角色）。登入角色的密碼來自該服務的密碼 Secret（見下方「產生的密碼」）：先以這個密碼實際登入，登不進去才設定（只送 SCRAM-SHA-256 的 verifier，明文不進 SQL），所以密碼沒變時什麼都不改。已存在的角色若有 SUPERUSER、BYPASSRLS、CREATEROLE、多了成員關係、在叢集的其他資料庫擁有物件或權限、有 `search_path` 以外的角色設定或這個資料庫專屬的設定，或服務角色能在自己的 schema 建立物件、能建立暫存表，Job 停止並寫出原因，不自動修改。
4. 以 `<服務>_owner` 角色依序套用該服務 `migrations/postgres/` 的每個檔案，一個檔案一個交易，記在 `trustiot_migrate.applied`；已套用的檔案內容若改了，Job 停止（migration 只能新增檔案）；檔案結束時若已離開原本的交易或角色（`END`、`COMMIT AND CHAIN`、`SET LOCAL ROLE` 等），Job 停止且不記錄這個檔案。outbox 的表與權限（`crates/outbox`、`crates/service-kit/sql/outbox-event.sql`）依 tools/migrate 的服務清單接上；每租戶角色的 hook 在這一步依租戶表建立或補回角色（目前沒有服務註冊，AGE 角色在 Laysi/trustiot-v2#176）。
5. ClickHouse：以有 access management 的帳號（`bundled` 是 `clickhouse.bundled.adminUser`，`external` 是 `clickhouse.external.migrationSecret`）套用 `services/ingest/migrations/clickhouse/`，每個檔案記在 ClickHouse 自己的 `trustiot_migrate.applied`；collector 開啟時建立 `observability.collector.clickhouse.user`，只授予 `trustiot_otel_collector` 角色（設為預設角色），密碼來自 collector 的密碼 Secret。帳號已存在但多了其他角色、權限或設定時，Job 停止，由管理者決定是否刪除；`trustiot_otel_collector` 角色多了 `0006_logs.sql` 以外的權限或角色時也停止。

**順序**：每個服務與 collector 的 pod 有一個 init container `wait-for-migrations`（同一個映像的 `migrate wait-for-job`），每 2 秒向 Kubernetes API 讀一次這個 Job，Job 完成才讓服務的容器啟動；Job 失敗時 init container 也失敗，pod 依 kubelet 的退避重試，修好之後自動繼續。所以 `helm install --wait` 與 `helm upgrade --wait` 都等到 Job 完成、服務就緒才結束，服務不會在比自己舊的 schema 上啟動。Job 不是 Helm hook：叢集內的 Postgres 與 ClickHouse 由同一次安裝建立，`pre-install` hook 會在資料庫存在之前執行；`post-install` hook 又要等 `--wait` 等的服務就緒，而服務在等 Job，兩者互等。

**Job 何時再跑**：Job 的名稱是 `<release>-migrate-<8 位 hash>`，hash 來自 Job 的整份 spec（映像、設定、Secret 的名稱、`migrate.revision`、`migrate.passwordGeneration`、每個在 `mesh.calls` 列出 `postgres` 的服務的映像參照）與平台設定的 checksum。升級時這些有任何改變，就產生新名稱的 Job（Helm 刪掉舊的），服務的 init container 也改等新的 Job，所以服務跟著滾動更新；都沒變時沿用已完成的 Job，服務不重啟。換新版本（映像的 tag 隨 `Chart.appVersion`）一定會跑。Secret 的內容改了（例如 migration 帳號的密碼）chart 看不到，要同時改 `migrate.revision`；Job 產生的密碼則以 `migrate.passwordGeneration` 輪替。完成的 Job 保留到下一次換名為止（沒有 `ttlSecondsAfterFinished`），之後才啟動的 pod（重啟、擴充）仍然找得到它。升級進行中，舊版的 pod 若被重建，它的 init container 會等已被刪除的舊 Job，直到滾動更新把它換掉；這時它的日誌寫出等的是哪個 Job、可能的原因與處理方式。資料庫從備份還原、或改接另一個資料庫之後，也要改 `migrate.revision`：角色與密碼是叢集層級的，不在資料庫備份裡，Job 再跑一次才會補回。

**版本一致**：Job 的映像帶著每個服務的 migration，而 Job 只在自己的 spec 改變時再跑。用 Postgres 的服務換了映像（只換 digest 也算）時，Job 的 spec 跟著變，所以 Job 再跑一次、服務等它跑完才啟動；但 Job 套用的是它自己映像裡的 migration。所以在 `mesh.calls` 列出 `postgres` 的服務，映像的 tag（`services.<name>.image.tag`、`serviceDefaults.image.tag`，空白時 `Chart.appVersion`）必須等於 `migrate.image.tag`，不同時 chart 渲染失敗；否則服務會在另一個版本的 schema 上啟動，沒有 Job 替它套用 migration。只以 digest 換掉服務映像（修補）時，`migrate.image.tag` 與 `migrate.image.digest` 也要換成同一次發佈的。

**一個 Postgres 叢集一個 TrustIoT 資料庫**：Postgres 的角色是叢集層級的，Job 建立的角色名稱（`identity_app` 等）不帶實例名稱，所以同一個 Postgres 叢集上的兩個 TrustIoT 資料庫會共用同一組服務角色、密碼與權限，一個實例的帳號就能讀另一個實例的資料。Job 拒絕這種部署：migration 角色擁有第二個資料庫時停止；另一個 migration 角色遇到已在其他資料庫擁有物件或權限、或不是它建立的角色時也停止，不修改它。`bundled` 每個 release 有自己的 Cluster；`external` 的每個實例要用自己的 Postgres 叢集，也不要以 `ADMIN OPTION` 把一個實例的角色分給另一個（[docs/runbooks/migrations.md](../../docs/runbooks/migrations.md)）。

**權限**：Job 的 pod 與 ServiceAccount 都是 `automountServiceAccountToken: false`；有密碼 Secret 要填時，只有 Job 的容器掛一個 1 小時效期的 projected token 與叢集的 CA，Role `<release>-migrate` 只讓它 `get` 與 `patch` 這些 Secret（以 `resourceNames` 列名）：沒有 `create`（無法以名稱限制，而且能替 namespace 裡任何 ServiceAccount 建 token Secret）、`list`、`watch`，也碰不到別的 Secret；chart 的其他 Role 都不碰 Secret（測試檢查）。服務與 collector 的 pod 仍然 `automountServiceAccountToken: false`；只有 init container 掛同樣的 projected token 與 CA（`kube-root-ca.crt`），Role `<release>-migrate-status` 只讓服務與 collector 的 ServiceAccount `get` 這一個 Job，沒有任何新的 Secret 權限。chart 的 NetworkPolicy 都只限制連入，init container 可以連到 API server。Job 的 pod 在 ambient mesh 內，叢集內的 ClickHouse 以它的 ServiceAccount 放行它；`<release>-postgres` 的 NetworkPolicy 也放行它；沒有任何東西能連進 Job 的 pod。

**服務的資料庫帳號**：在 `mesh.calls` 列出 `postgres` 的服務，chart 給它 `TRUSTIOT__DATABASE__USERNAME=<服務>_app` 與從自己的 Secret `<release>-<服務>-password` 同名 key 讀的 `TRUSTIOT__DATABASE__PASSWORD`；這兩個名稱保留給 chart，自訂 `env` 不能設定。Job 開始前先確認這些角色都在 tools/migrate 的服務清單裡，否則什麼都不改就停止。

**產生的密碼**（docs/architecture/secrets.md）：服務登入角色與 collector 的 ClickHouse 帳號是 Job 自己建立的內部帳號，密碼也由它產生，沒有人建立、複製或輪替它們。

- chart 為每個用 Postgres 的服務與 collector 各宣告一個 Secret（`templates/passwords.yaml`）：沒有 `data`、`stringData`，帶 `helm.sh/resource-policy: keep` 與標籤 `trustiot.io/filled-by: migrate`。每個消費者一個 Secret，所以節點的 kubelet 只讀得到排在它上面的 pod 的密碼。`migrate.enabled` 關掉時也宣告，由取代 Job 的人填入（只有叢集測試這樣做）。
- Job 在 migration 鎖內讀每個 Secret；缺的 key、不能用的值、`migrate.passwordGeneration` 大於 annotation `trustiot.io/password-generation` 記的代、或 annotation `trustiot.io/password-secret-uid` 不等於 Secret 的 `metadata.uid`（不是 Job 填過的那個物件）時，以作業系統的 CSPRNG 產生 256 位元的新密碼（64 個十六進位字元），用一個帶 `resourceVersion` 的 JSON merge patch（field manager `trustiot-migrate`）寫回，同一個 patch 記下代與 uid（chart 不渲染這兩個 annotation）；Secret 在讀與寫之間被別人改過時 API 拒絕這個 patch，Job 重讀再決定，不會覆蓋別人的寫入。之後把每個角色與帳號設成 Secret 裡的值。日誌只寫角色、帳號與 Secret 的名稱，不寫密碼、密碼的 hash 或 Secret 的資料。
- Helm 不擁有這些 Secret 的資料：升級與 rollback 渲染出的 Secret 都沒有 `data`，所以 Job 寫的值保留下來（`tests/e2e/postgres.e2e.ts` 在叢集上以釘選的 helm 檢查升級與 rollback 後資料不變）。Flux 的 HelmRelease 沒有開 drift detection，升級失敗時的 rollback 也一樣保留；將來開 drift detection 時見 [deploy/flux/README.md](../flux/README.md)。
- 輪替：調大 `migrate.passwordGeneration`。Job 名稱跟著變，新的 Job 產生並設定新密碼，服務與 collector 的 pod 以新的 Job 名稱滾動更新、讀到新密碼。從 Job 設定新密碼到舊 pod 被換掉之間，舊 pod 已經開著的 Postgres 連線不受影響（Postgres 只在登入時驗密碼），但舊 pod 開新連線會被拒；舊的 collector 寫 ClickHouse 時也會被拒（新連線要驗密碼，外部 ClickHouse 走 HTTPS 時每個請求都驗），在這段時間重試（最多 5 分鐘），新的 collector 就緒後恢復。Postgres 一個角色只有一個密碼，要完全避免這段時間得輪流使用兩個角色，目前不做；需要零中斷時在低流量時段輪替。代只往上走：`helm rollback`（或 Flux 升級失敗後的 rollback）渲染出較小的值時 Job 保留現有的密碼，記的代不變，之後再升級回來也不換。
- 只信任 Job 自己填過的 Secret 物件：Job 寫入時記下 Secret 的 uid，之後讀到的 uid 不同（有人刪掉 Secret 再建一個同名的、帶 Helm 的 ownership metadata 讓 `helm install` 接手，或從備份還原）就把每個 key 當成不能用，產生新密碼。只能建立 Secret 的人事先不知道 uid，所以無法讓 Job 採用他選的密碼；能 patch 這些 Secret 的人本來就讀得到它們。
- 資料遺失（有人刪了 key 或整個 Secret、或以 `helm upgrade --force-replace` 換掉了 Secret）：改 `migrate.revision` 再升級。Helm 升級時重建被刪掉的 Secret，Job 重跑時為缺的 key 產生新密碼並設定，服務重啟後以新密碼登入。Job 不能建立 Secret，所以 Secret 不存在時它停止並寫出這個處理方式。
- 停用某個服務時它的 Secret 留著（`keep`），Role 不再列它，Job 不再管它；要清掉時手動刪除。

**失敗時**：Job 在 `migrate.backoffLimit` 次重試後失敗，服務停在 `Init`，舊版的 pod 繼續服務。看 `kubectl -n <namespace> logs job/<release>-migrate-<hash>`（JSON 行，只寫 stdout）；每一項變更與停止的原因都寫在裡面，不含密碼。處理方式見 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md)。

## 叢集內的 Postgres

`postgres.mode: bundled`（預設，SaaS 與地端相同）部署 `templates/postgres.yaml`：一個由 CloudNativePG 管理的 Postgres，映像是我們的 PostgreSQL 18 加 Apache AGE（[deploy/images/postgres](../images/postgres/README.md)，決策 21、清單 M8）。

叢集要先裝好 CloudNativePG operator 與它的 CRD（[deploy/cloudnative-pg](../cloudnative-pg/README.md)），chart 不安裝它們：CRD、webhook 與 ClusterRole 是整個叢集共用的，一個叢集只能有一份，和 Istio 一樣是平台元件（決策 5 的 2026-10-12 補充）。沒有 operator 時 `helm install` 因為不認得 `postgresql.cnpg.io/v1` 而失敗，不會裝出半套。

chart 產生的物件（`<cluster>` 是 `<release>-postgres`；CloudNativePG 限制 Cluster 名稱 50 個字元，超過時保留 release 名稱的前段，在 `-postgres` 前加上完整名稱 SHA-256 的前 8 個字元，同一個 namespace 裡開頭相同的 release 不會撞名）：

| 物件 | 內容 |
| --- | --- |
| ImageCatalog `<cluster>` | PostgreSQL major 18 對應的映像（`postgres.bundled.image`）。映像的 tag 是 TrustIoT 的版本，CloudNativePG 從這裡得知 major |
| Cluster `<cluster>` | 一個 instance、不建 PodDisruptionBudget（單一 instance 的 PDB 會擋住節點 drain）、不開 superuser 存取；bootstrap 建立資料庫 `trustiot` 與它的 owner `trustiot_owner`，locale 用 PostgreSQL 內建的 `C.UTF-8`（排序與字元分類不隨作業系統的 C library 改變，換映像不會讓索引失效）；`shared_preload_libraries` 含 `age`；pg_hba 加上 `hostnossl all all all reject`；pod 帶 `istio.io/dataplane-mode: none` |
| Database `<cluster>-trustiot` | 由 operator 以 superuser 在 `trustiot` 建立 AGE extension，版本變更時執行 `ALTER EXTENSION age UPDATE`；`databaseReclaimPolicy: retain`，刪掉這個物件不會刪資料庫 |
| NetworkPolicy `<release>-postgres` | 選 Postgres pod（`cnpg.io/cluster`、`cnpg.io/podRole: instance`），只放行在 `mesh.calls` 列出 `postgres` 的服務與 migration Job 連 5432，以及 operator（`postgres.bundled.operator`）連 instance manager 的 8000 |

operator 依 Cluster 再產生：pod 與 PVC `<cluster>-1`；Service `<cluster>-rw`（永遠指向 primary，服務連這個）、`<cluster>-ro`、`<cluster>-r`；Secret `<cluster>-ca`、`<cluster>-server`、`<cluster>-replication`、`<cluster>-app`。

- **TLS（清單 M3）**：operator 產生自己的 CA（`<cluster>-ca`）與伺服器憑證（`<cluster>-server`，主機名稱含 `<cluster>-rw.<namespace>.svc`），效期 90 天、到期前 7 天自動換發，不停機；CA 換發時沿用同一把私鑰（CloudNativePG 1.30.1 的 `internal/controller/cluster_pki.go`）。服務的平台設定是 `TRUSTIOT_POSTGRES_HOST=<cluster>-rw.<namespace>.svc`、`TRUSTIOT_POSTGRES_SSLMODE=verify-full`；用 Postgres 的服務掛上 `<cluster>-ca` 裡的 `ca.crt`，volume 的 `items` 只選這個 key，`ca.key` 不會進任何服務的容器。kubelet 會更新掛載的檔案，service-kit 每條新連線都重讀 CA 檔，所以換發不必重啟服務。不走 TLS 的 TCP 連線，不論用戶端的 `sslmode`，都被伺服器的 pg_hba 拒絕。
- **帳號（決策 4、29）**：沒有 superuser 密碼，也沒有 `<cluster>-superuser` Secret，`postgres` 角色只能在 pod 內經 local socket 登入，operator 就是這樣管理資料庫。owner `trustiot_owner` 是登入角色，沒有 SUPERUSER、BYPASSRLS；bootstrap 時以 `postInitApplicationSQL` 給它 CREATEROLE（Laysi/trustiot-v2#60），因為它也是 migration 角色，要建立每個服務的角色。PostgreSQL 16 起 CREATEROLE 不能建立 superuser、BYPASSRLS 或 REPLICATION 角色，也不能改它沒有 ADMIN 的角色。密碼由 operator 產生在 `<cluster>-app`，只給 migration Job 用。2026-10-13 之前 bootstrap 的 Cluster 沒有 CREATEROLE，要照 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md) 補一次。operator 產生的 Secret（`<cluster>-ca`、`-server`、`-app`、`-replication`、`-superuser`）任何服務的自訂 `env` 都讀不到，寫了就渲染失敗；服務只經 chart 拿到 CA 憑證。每個服務自己的角色、密碼與 `crates/service-kit/sql/database.sql` 的權限設定由 migration Job 處理（見「migration Job」）。
- **網路**：Postgres pod 不加入 ambient mesh：每條連線本身已經是 verify-full 的 TLS，而在 mesh 外的 operator 要連每個 instance 的 8000，放進 mesh 會被 AuthorizationPolicy 擋下。namespace 的預設拒絕照樣套用到它，`<release>-postgres` 只放行上表的來源；kubelet 的探針從 pod 所在的節點來，NetworkPolicy 不擋節點本身的連線。namespace 內沒有列 `postgres` 的服務與其他 pod 都連不到。
- **規格**：資源與參數見「外部依賴」的 `postgres.bundled.*`，兩種型態相同。
- **刪除**：Cluster 與 ImageCatalog 帶 `helm.sh/resource-policy: keep`，`helm uninstall` 或 Flux 移除 release 時保留資料庫（CloudNativePG 刪 Cluster 時會一起刪掉 PVC，資料就沒了）。確定要刪資料時，明確刪除 Cluster：`kubectl -n <namespace> delete cluster <cluster>`，再刪 ImageCatalog。PVC 不會在 `kubectl delete` 回來時就消失：Kubernetes 在 Cluster 刪除後於背景回收它，等 Postgres 的 pod 停止才刪得掉；要確認時以 `kubectl -n <namespace> get pvc -l cnpg.io/cluster=<cluster>` 查到沒有為止。
- **升級**：映像隨 chart 版本更新，單一 instance 會短暫中斷。AGE 升級時，`deploy/images/postgres/Containerfile` 的 `AGE_VERSION` 與 `templates/_postgres.tpl` 的 `trustiot.postgres.ageVersion` 一起改（chart 測試比對兩者），operator 執行 `ALTER EXTENSION`。PostgreSQL major 升級要一起改映像的基底與 `trustiot.postgres.majorVersion`（測試同樣比對），步驟要另外驗證，還沒做過。
- **備份與 PITR** 尚未設定，之後以同一個 operator 加入（Laysi/trustiot-v2#134）。

磁碟區大小沒有預設值，每個部署在安裝實例的覆寫檔設定 `postgres.bundled.storage.size`（資料與 WAL 共用一個磁碟區）。業務資料與知識圖的大小要等服務上線後才量得到，測試範例先用 20Gi；用量告警見清單 I2。

在叢集上檢查：`kubectl -n <namespace> get cluster <cluster>` 的 STATUS 是 `Cluster in healthy state`，`kubectl -n <namespace> get database <cluster>-trustiot` 的 APPLIED 是 `true`。Helm 4 的 `helm install --wait` 依 Cluster 的 `Ready` 條件等到它健康才結束（2026-10-12 實驗機兩種型態都是這樣，約 20 到 26 秒）；Database 的 AGE 不在等待之列，再晚約 20 秒才套用，所以用到 AGE 的 migration 要等 `age` extension 出現再開始（owner 不是 superuser，不能自己建立它；目前沒有服務的 migration 用到 AGE，第一個用到的 issue 要在 migration Job 加上這個等待，Laysi/trustiot-v2#176）。叢集測試 `tests/e2e/postgres.e2e.ts` 以真實連線驗證 verify-full、錯誤 CA、錯誤主機名稱與沒列 `postgres` 的 pod 被拒、明文連線被拒、沒有 superuser 帳號、uninstall 保留資料，用法見該目錄的 README，實驗機的結果見 [測試報告](../../docs/test-reports/2026-10-12-58-cnpg-postgres.md)。

## 叢集內的 ClickHouse

`clickhouse.mode: bundled`（預設，SaaS 與地端相同）部署 `templates/clickhouse.yaml`：

- 一個副本的 StatefulSet `<release>-clickhouse`，資料在 PVC `data-<release>-clickhouse-0`（`/var/lib/clickhouse`）；StatefulSet 刪除或縮容時保留 PVC。ClusterIP Service 只開 HTTP 8123 與 native 9000。
- 伺服器設定只有一個 ConfigMap（`<release>-clickhouse-config`，掛成 `config.d`）：監聽位址、關掉 MySQL／PostgreSQL 相容埠與 interserver 埠、日誌只寫 stdout。設定變更時 pod 自動重啟。
- 帳號、角色、row policy、settings profile 與 quota 一律不寫在設定檔（決策 14；ClickHouse Cloud 不能改設定檔）：由 migration Job（Laysi/trustiot-v2#60）以 bootstrap 管理帳號套用 `services/ingest/migrations/clickhouse/` 的 SQL，存在 ClickHouse 自己的 access storage（PVC 上的 `access/`）。租戶帳號只能讀的 view `trustiot.tenant_telemetry` 與它的 DEFINER 帳號（不能登入）也由這些 SQL 建立，chart 不必為它們設定任何值（Laysi/trustiot-v2#260）。唯一由映像產生的帳號是 bootstrap 管理帳號：映像的 entrypoint 依 `CLICKHOUSE_USER`、`CLICKHOUSE_PASSWORD_FILE`（掛載的 Secret）與 `CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1` 寫進 `users.d`（記憶體 emptyDir），並移除沒有密碼的 `default` 帳號。
- 根目錄唯讀、以 UID 101 非 root 執行、不能提權、drop 所有 capability；探針是 `GET /ping`。pod 帶 `istio.io/dataplane-mode: ambient` 加入 ambient mesh，服務連 ClickHouse 由 ztunnel 加 mTLS。

磁碟區大小沒有預設值，每個部署在安裝實例的覆寫檔設定 `clickhouse.bundled.storage.size`：每日筆數 × 保存天數 × 約 3.1 bytes（2026-10-09 實測，30 天 52 億筆佔 15.2 GiB）× 1.5（merge 時新舊 part 並存與系統表的餘裕）。例如每天 1 億筆、保存 400 天約 190 GB（遙測表的保存期限暫定 400 天，Laysi/trustiot-v2#92 定案）。磁碟用量的告警與擴充見清單 I2（硬碟水位告警）。

安裝前要先有 `clickhouse.bundled.adminSecret` 指定的 Secret：SaaS 以 SOPS 加密放在 `deploy/flux/`，由 Flux 解密（docs/architecture/ci-release.md「SaaS 的祕密怎麼進叢集」）；地端由客戶在安裝前建立，例如：

```sh
kubectl -n trustiot create secret generic trustiot-clickhouse-admin --from-literal=password="$(openssl rand -hex 32)"
```

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

- **設定**：ConfigMap `<release>-platform` 以 `envFrom` 交給每個服務，另外每個服務有 `OTEL_SERVICE_NAME=<服務名稱>`，collector 開啟時還有 `TRUSTIOT__TELEMETRY__OTLP_ENDPOINT`（service-kit 的 OTLP 匯出位址，見「OpenTelemetry collector」）。`TRUSTIOT_` 後接英數字、`SENTRY_` 與 `OTEL_` 開頭的變數只由 chart 設定，`env` 不能覆寫，服務讀到的平台設定一定是 values 決定的值。沒有設定的項目不會出現，服務可以分辨「沒設定」與「空字串」。平台設定變更時 pod 會自動滾動更新。
- **密鑰**：不放進 ConfigMap，以 `valueFrom.secretKeyRef` 從 values 指定的 Secret 取用。Sentry 開啟時每個服務有 `SENTRY_DSN`；其他依賴的帳密只給在 `services.<name>.credentials` 列出它的服務（見後面「密鑰的環境變數」），Secret 名稱空白的項目不注入。資料庫的 CA 憑證以唯讀方式掛在 `/etc/trustiot/<postgres|clickhouse>-ca/ca.crt`：外部 ClickHouse 的掛進每個服務；Postgres 的（`external` 是 `postgres.external.caSecret`，`bundled` 是 CloudNativePG 產生的 CA 裡的 `ca.crt`）只掛進在 `mesh.calls` 列出 `postgres` 的服務。內部身分的簽章私鑰只掛進身分服務（見「內部身分」）。服務自己的資料庫帳號（每個服務一個角色，決策 4）由 migration 的 issue 加入（Laysi/trustiot-v2#60）。
- **埠與探針**：API 監聽 8080（埠名 `http`）；健康埠 8081（埠名 `health`）只提供 `GET /healthz`（startup 與 liveness）與 `GET /readyz`（readiness），其他路徑回 404。探針連健康埠，節點上的行程因此碰不到 API（「節點來源」）。service-kit 預設就是這樣（`http.listen_addr`、`http.health_listen_addr`），`crates/service-kit/tests/chart_contract.rs` 對照 `values.yaml` 檢查。
- **SIGTERM**：pod 刪除時先等 `preStopSleepSeconds`（預設 5 秒），再送 SIGTERM；服務收到後 readiness 立即回失敗、處理完進行中的請求、主動關閉長連線，在 `drainSeconds`（預設 30 秒）內結束，之後才會被 SIGKILL。服務是容器內的 PID 1，要自己處理 SIGTERM。
- **執行環境**：根目錄唯讀，只有 `/tmp` 可寫；以非 root 執行，映像的 `USER` 要是數字 UID，並能以 OpenShift 指派的任意 UID 執行；沒有 ServiceAccount token；只排到 arm64 節點。

平台設定的環境變數：

| 變數 | 來源 | 何時出現 |
| --- | --- | --- |
| `TRUSTIOT_DEPLOYMENT_MODE` | `global.deploymentMode` | 一律 |
| `TRUSTIOT_APP_HOST`、`TRUSTIOT_GATEWAY_HOST`、`TRUSTIOT_OTA_HOST` | `hosts.*` | 一律 |
| `TRUSTIOT_CLIENT_IP_SOURCE` | 由 `ingress.exposure` 推導：`cf-connecting-ip` 或 `x-forwarded-for-rightmost` | 一律 |
| `TRUSTIOT_INTERNAL_IDENTITY_KEYS` | `internalIdentity.publicKeys` 組成的 JWK Set（JSON） | 有公鑰時 |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE` | `external`：`postgres.external.*`；`bundled`：`<release>-postgres-rw.<namespace>.svc`、`5432`、`trustiot`、`verify-full` | 一律 |
| `TRUSTIOT_CLICKHOUSE_URL` | `external`：`clickhouse.external.url`；`bundled`：`http://<release>-clickhouse.<namespace>.svc:8123`（叢集內明文 HTTP，pod 之間由 Istio ambient 的 ztunnel 加上 mTLS） | 一律 |
| `TRUSTIOT_QDRANT_URL` | `qdrant.external.url` | `qdrant.mode` 為 `external` |
| `TRUSTIOT_S3_ENDPOINT`、`_REGION`、`_BUCKET`、`_FORCE_PATH_STYLE` | `objectStorage.*` | 各自有值時；`_FORCE_PATH_STYLE` 隨 bucket |
| `TRUSTIOT_SMTP_HOST`、`_PORT`、`_SECURITY`、`_FROM` | `smtp.*` | 有 host 時（`_FROM` 有值時） |
| `TRUSTIOT_AI_CHAT_BASE_URL`、`TRUSTIOT_AI_EMBEDDING_BASE_URL`、`TRUSTIOT_AI_RERANK_BASE_URL` | `ai.*.baseUrl` | 各自有值時 |
| `TRUSTIOT_CLICKHOUSE_CA_FILE` | 掛載的 CA 憑證路徑 `/etc/trustiot/clickhouse-ca/ca.crt` | `clickhouse.mode` 為 `external` 且設定了 `caSecret.name` |
| `SENTRY_ENVIRONMENT` | `observability.sentry.environment` | Sentry 開啟時（`SENTRY_DSN` 來自 Secret，見上） |

`TRUSTIOT_POSTGRES_CA_FILE`（`/etc/trustiot/postgres-ca/ca.crt`）不在 ConfigMap：chart 只設給在 `mesh.calls` 列出 `postgres` 的服務，和 CA 的掛載一起出現，`bundled` 與 `external` 都一樣。`bundled` 模式的叢集內端點，由部署該元件的 issue 加進這張表（Postgres 與 ClickHouse 見上面）。服務連 ClickHouse 的帳號與密碼還沒有：ingest 的寫入帳號在 ingest 服務加入時由 migration Job 建立（Laysi/trustiot-v2#91），每租戶唯讀帳號由 Laysi/trustiot-v2#119 加入。

密鑰的環境變數（以 `valueFrom.secretKeyRef` 注入，不在 ConfigMap）：

| 變數 | 來源 | 哪些服務拿到 |
| --- | --- | --- |
| `SENTRY_DSN` | `observability.sentry.dsnSecret` | Sentry 開啟時每個服務 |
| `TRUSTIOT_QDRANT_READ_API_KEY` | `qdrant.external.credentialsSecret` 的 `readApiKeyKey` | `knowledge` 的 `credentials` 含 `qdrant-read`，且 `qdrant.mode` 為 `external` |
| `TRUSTIOT_QDRANT_WRITE_API_KEY` | `qdrant.external.credentialsSecret` 的 `writeApiKeyKey` | `knowledge` 的 `credentials` 含 `qdrant-write`，且 `qdrant.mode` 為 `external` |
| `TRUSTIOT_S3_ACCESS_KEY_ID`、`TRUSTIOT_S3_SECRET_ACCESS_KEY` | `objectStorage.credentialsSecret` | `credentials` 含 `object-storage` |
| `TRUSTIOT_SMTP_USERNAME`、`TRUSTIOT_SMTP_PASSWORD` | `smtp.credentialsSecret` | `credentials` 含 `smtp` |
| `TRUSTIOT_AI_CHAT_API_KEY`、`TRUSTIOT_AI_EMBEDDING_API_KEY`、`TRUSTIOT_AI_RERANK_API_KEY` | `ai.*.apiKeySecret` | `ai-gateway` 的 `credentials` 含 `ai`（決策 28） |

只給身分服務的變數（chart 設定，`TRUSTIOT_` 開頭，自訂 `env` 不能覆寫）：

| 變數 | 來源 | 何時出現 |
| --- | --- | --- |
| `TRUSTIOT_INGRESS_ROUTES` | `ingress.routes` 換成實際主機名稱的 JSON 陣列 `[{host, path, match, service}]`，決定 token 的受眾 | 一律（沒有路由時是 `[]`） |
| `TRUSTIOT_IDENTITY_SIGNING_KEY_FILE` | 掛載的簽章私鑰路徑 `/etc/trustiot/identity-signing-key/signing-key.pem` | `internalIdentity.signingKeySecret.name` 有值時 |

## 擴充 chart

- **新增服務**：在 `values.yaml` 的 `services` 加一個 key，並同步 docs/architecture/deployment.md 的服務表（測試比對兩邊）。服務特有的埠、資源、探針與 Secret 寫在該 key 底下。
- **服務之間的呼叫**：服務要呼叫另一個服務的內部 API 時，在服務的 issue 把這條呼叫加進 `values.yaml` 的 `mesh.calls`；沒列的呼叫會被重置。服務間授權的規則集中在 `templates/_mesh.tpl` 的 `trustiot.mesh.plan`，AuthorizationPolicy 與 NetworkPolicy 都由它產生，不要為單一服務另外寫政策。
- **對外的路由**：服務要經入口對外時，在服務的 issue 把路由加進 `values.yaml` 的 `ingress.routes`（預設需要 cookie）。不要新增例外路徑：決策 27 只允許登入、閘道器憑證換發、MQTT 轉接與健康檢查四種，要加第五種必須先改決策。身分服務以同一份路由決定 token 的受眾（`TRUSTIOT_INGRESS_ROUTES`），所以路由的後端就是收 token 的服務；`ai-gateway` 不驗證內部身分（決策 28），不能有 cookie 路由。入口的規則集中在 `templates/_ingress.tpl` 的 `trustiot.ingress.plan`；可信的來源 IP 沿用 `trustiot.clientIpSource`，不要另外加欄位。
- **新增叢集內元件**（broker、Postgres、ClickHouse、Qdrant、collector、Grafana）：放在自己的 `templates/<元件>.yaml`，以 `*.mode` 或開關控制；Service 只能是 ClusterIP；namespace 預設拒絕所有連入：元件在 mesh 內時（pod 帶 `istio.io/dataplane-mode: ambient`）把它加進 `templates/_mesh.tpl` 的 `trustiot.mesh.components`、用戶端列進 `mesh.calls`，並定義 `trustiot.<元件>.pod`（container ports 與探針，元件的模板也從它渲染，NetworkPolicy 依它只對節點來源開探針埠），AuthorizationPolicy 與 NetworkPolicy 由 chart 產生；不在 mesh 內時（例如叢集內的 Postgres）自帶 NetworkPolicy，只對實際的用戶端開它的埠，並讓 kubelet 的健康檢查到得了；用戶端以 `mesh.calls` 宣告時，把名稱加進 `templates/_mesh.tpl` 的 `trustiot.mesh.directCallees`；在 `values.schema.json` 加欄位定義、在本文件加欄位說明（測試檢查 `values.yaml` 每個欄位都在本文件），並更新「元件與開關」與平台設定的表。
- **Secret**：values 只能出現 `{name, key}` 形式的引用（測試掃描兩份 values、`values.yaml` 與 `tests/values/` 的每個欄位：名稱像密鑰的欄位只能是 `*Secret` 底下的 `name`、`key` 或 `*Key`，任何值都不能像私鑰、token 或帶帳密的 URL；並確認渲染結果沒有 Secret、只以名稱與 key 讀 Secret）；schema 的 `additionalProperties: false` 會擋下放明文的新欄位。服務要拿新的依賴帳密時，在 `_helpers.tpl` 的 `trustiot.credentialCatalog` 加一列（變數名稱、Secret、可以拿的服務）、在 schema 的 `credentials` 列舉加上名稱，並更新「密鑰的環境變數」表；自訂 `env` 自動不能讀這個帳密。叢集內元件自己產生的 Secret（例如 CloudNativePG 的 CA、伺服器私鑰與資料庫帳號）整個加進 `trustiot.dependencySecretNames`，自訂 `env` 就讀不到它的任何 key。新的平台變數用 `TRUSTIOT_` 開頭，自動受保留規則保護。
- **測試**：規則寫進 `tests/chart.test.ts`。`tests/values/` 的兩個安裝實例範例與 `smoke.yaml`（以 traefik/whoami 當替身服務）也供 nightly（Laysi/trustiot-v2#69）在叢集上安裝使用；叢集測試是 `tests/e2e/ingress.e2e.ts`（入口）、`tests/e2e/mesh.e2e.ts`（服務間授權）、`tests/e2e/otel.e2e.ts`（日誌管線）與 `tests/e2e/postgres.e2e.ts`（叢集內的 Postgres），前兩個共用 `tests/e2e/cluster.ts`，nightly 可直接沿用。`bun test` 的 kubeconform 已帶 CRD schema 目錄，Gateway API、Istio 與 CloudNativePG 的資源可以直接驗證。兩個實例範例會部署叢集內的 Postgres 與 ClickHouse（`bundled`）與 collector，在叢集上安裝前要先裝好 CloudNativePG operator（見「叢集內的 Postgres」），並建立 `trustiot-clickhouse-admin` Secret（見「叢集內的 ClickHouse」）與 collector 的 `trustiot-otel-collector-clickhouse` Secret；`smoke.yaml` 改用 nightly 自己建置的 Postgres 映像。入口與日誌管線的叢集測試用 `tests/e2e/values-e2e.yaml`、`values-otel-e2e.yaml` 把 Postgres 改成 `external`（沒有服務列出 `postgres`，所以不需要 CA），不部署 Postgres；入口測試的 ClickHouse 也改成 `external`，並關掉 collector。`tests/` 不會打包進 chart（`.helmignore`）。

## OpenShift 與其他 Kubernetes

- Pod 符合 restricted Pod Security Standard，且不固定 UID，可以在 OpenShift 的 restricted-v2 SCC 下執行。例外是固定的群組 ID：身分服務的 `fsGroup`（`services.identity.podSecurityContext.fsGroup`）與叢集內 ClickHouse 的 UID／GID，在 OpenShift 都設成 `null` 交給 SCC 指派。
- 預設只排到 arm64 節點；客戶用 x86 主機時，要先加出 amd64 映像（決策 5）再調整 `serviceDefaults.nodeSelector`（叢集內 Postgres 是 `postgres.bundled.nodeSelector`）。
- 叢集內的 Postgres 需要 CloudNativePG operator。OpenShift 通常由 OperatorHub 安裝（operator 在 `openshift-operators`），這時把 `postgres.bundled.operator.namespace` 與 `podLabels` 改成它實際的 namespace 與 pod label，NetworkPolicy 才放行它；Postgres pod 的安全設定由 operator 決定。目前沒有 OpenShift 環境可測。
- chart 需要叢集已裝 Istio ambient 與 Gateway API CRD，並設好 ext_authz provider。k3s 用 `global.platform=k3s`，OpenShift 用 `global.platform=openshift` 或 OpenShift Service Mesh 3，步驟見 `deploy/istio/README.md`。
- 入口 gateway 的 pod 由 Istio 產生，以 UID 1337 執行，符合 restricted Pod Security Standard（2026-10-10 在 restricted namespace 實測）；OpenShift 的 restricted-v2 SCC 會指派其他 UID，目前沒有 OpenShift 環境可測。
- `ingress.tunnel.allowedSources` 的預設值是 k3s 單節點的本機位址；其他 CNI 要改成 cloudflared 與 kubelet 實際的來源位址。
