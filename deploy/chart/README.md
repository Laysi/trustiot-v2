# TrustIoT Helm chart

SaaS 與地端用同一份 chart（決策 1、5）：`values-saas.yaml` 與 `values-onprem.yaml` 描述兩種部署型態，客戶自有的 k8s 或 OpenShift 也用這份 chart。本文件列出每個 values 欄位，以及 chart 與服務之間的約定。

## 指令

helm、kubeconform 與 bun 的版本鎖在 repo 根目錄的 `.prototools`，先執行 `proto install`。以下指令都在 repo 根目錄執行。

```sh
# 全部檢查：兩份 values 的 helm lint 與 kubeconform、chart 規則、本文件的欄位清單
bun test deploy/chart

# 只跑名稱含指定字串的檢查
bun test deploy/chart -t "kubeconform"

# 個別執行（地端把 saas 換成 onprem）
helm lint deploy/chart --strict -f deploy/chart/values-saas.yaml -f deploy/chart/tests/values/saas-instance.yaml
helm template trustiot deploy/chart -n trustiot -f deploy/chart/values-saas.yaml -f deploy/chart/tests/values/saas-instance.yaml > saas.yaml
kubeconform -strict -summary -kubernetes-version 1.36.0 saas.yaml

# 安裝：第三個 -f 是這個安裝實例的覆寫檔（見下一節）
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
| 3 | 安裝實例的覆寫檔 | 這一個安裝的主機名稱、外部端點與 Secret 名稱。SaaS 的放在 `deploy/flux/`（Laysi/trustiot-v2#70）；地端由客戶安裝時提供；`tests/values/` 有測試用的範例 |

`values.schema.json` 驗證合併後的結果：缺欄位、拼錯欄位、以 IP 當主機名稱、在 values 放明文密鑰、SaaS 的外部端點不是 https，都會讓 `helm lint`、`helm template` 與 `helm install` 直接失敗，不會裝出壞掉的版本。

## SaaS 與地端的差異

| 欄位 | `values-saas.yaml` | `values-onprem.yaml` | 依據 |
| --- | --- | --- | --- |
| `global.deploymentMode` | `saas` | `onprem` | 決策 1 |
| `ingress.exposure` | `tunnel`：入口 gateway 只有 ClusterIP、聽 80 的 HTTP，由主機上的 cloudflared 轉入，主機不開埠；NetworkPolicy 只放行 `ingress.tunnel.allowedSources`（本機） | `direct`：入口 gateway 以 `ingress.direct.serviceType`（預設 LoadBalancer）在客戶網路開 443，以 `ingress.direct.tlsSecret` 終止外層 TLS | 決策 27 |
| 可信的來源 IP（由 `ingress.exposure` 推導，不能單獨設定） | `CF-Connecting-IP` | 入口 gateway 附加在 X-Forwarded-For 最右段的位址 | 決策 6、27 |
| `qdrant.mode` | `external`：Qdrant Cloud | `bundled`：叢集內自架 | 決策 19 |
| `objectStorage.*` | 託管的 S3 或 R2 | 客戶的 S3 相容儲存（細節未定） | 決策 20 |
| `smtp.*` | 我們的郵件服務 | 客戶的 SMTP | 決策 18 |
| `ai.*` | 雲端 API | 客戶的 OpenAI 相容端點，例如 vLLM、Ollama | 決策 15、16、26 |
| `observability.sentry.enabled` | `true`，送 sentry.io | `false`，客戶有自己的 Sentry 時可開 | 決策 24 |
| OTLP 轉送（欄位隨 Laysi/trustiot-v2#73 的 collector 加入） | 不轉送 | 可轉送到客戶的 SIEM | 決策 22 |

兩種型態相同的部分：Postgres（含 Apache AGE）與 ClickHouse 都在叢集內（`bundled`）；MQTT broker 不直接對外，閘道器一律以 443 的 WebSocket 經入口 gateway 與 mqtt-bridge 連到 broker；cookie 驗證、例外路徑與標頭規則相同（見「入口 gateway」）；chart 產生的 Service 一律是 ClusterIP（測試強制），唯一對外的是 Istio 依 Gateway 產生的入口 Service。SaaS 在 Cloudflare 無法使用時，可改用 `ingress.exposure: direct` 直接對外，閘道器只改 DNS（docs/roadmap/risks.md）。

## 元件與開關

加入元件的 issue 要同時更新這張表。

| 元件 | 開關 | 2026-10-10 的狀態 | 後續 issue |
| --- | --- | --- | --- |
| Rust 服務（見下一節） | `services.<name>.enabled` | 通用樣板完成；映像還沒出，預設全關 | 各服務的 issue 出映像時改成開 |
| 入口 gateway（Istio ambient、Gateway API、ext_authz） | `ingress.*` | 完成：Gateway、路由、cookie 驗證、例外路徑、`/internal` 拒絕與 NetworkPolicy；服務的 pod 加入 ambient mesh。Istio 本身與 ext_authz provider 由 `deploy/istio` 安裝。服務的 cookie 路由隨各服務加入 `ingress.routes` | Laysi/trustiot-v2#62；真身分服務的端到端驗證在 Laysi/trustiot-v2#77 |
| 服務間授權（AuthorizationPolicy、NetworkPolicy） | `mesh.*` | 完成：每個服務自己的 ServiceAccount、依 `mesh.calls` 產生的 AuthorizationPolicy、namespace 預設拒絕的 NetworkPolicy；不部署 waypoint。服務之間的呼叫隨各服務加入 `mesh.calls` | Laysi/trustiot-v2#63；代表使用者呼叫的授權矩陣在 Laysi/trustiot-v2#106 |
| Postgres（CloudNativePG 加 AGE） | `postgres.mode` | `external` 已接到平台設定；`bundled` 的 operator 與 Cluster 尚未加入 | Laysi/trustiot-v2#57、Laysi/trustiot-v2#58 |
| migration Job | — | 尚未加入 | Laysi/trustiot-v2#60 |
| ClickHouse | `clickhouse.mode` | `bundled`：單節點 StatefulSet（見「叢集內的 ClickHouse」）；`external` 已接到平台設定。schema、帳號與 row policy 由 migration Job 以 SQL 套用 | Laysi/trustiot-v2#60（migration Job）、Laysi/trustiot-v2#72（admin 密碼的 Secret） |
| MQTT broker（Mosquitto） | — | 尚未加入 | Laysi/trustiot-v2#84 |
| Qdrant | `qdrant.mode` | `external` 已接到平台設定；`bundled` 尚未加入 | Laysi/trustiot-v2#194 |
| 物件儲存、SMTP、AI 端點 | `objectStorage.*`、`smtp.*`、`ai.*` | 一律外部，設定已接到平台設定 | Laysi/trustiot-v2#133、Laysi/trustiot-v2#169、Laysi/trustiot-v2#139 |
| OpenTelemetry collector 與 OTLP 轉送 | — | 尚未加入；轉送目的地的欄位隨 collector 一起加 | Laysi/trustiot-v2#73 |
| Grafana | — | 尚未加入 | Laysi/trustiot-v2#183 |
| Sentry | `observability.sentry.*` | 開關、環境名稱與 DSN 已接到每個服務 | Laysi/trustiot-v2#184 |
| 密鑰 | 各個 `*Secret` 欄位、`services.<name>.credentials` | values 只存 Secret 名稱與 key；Sentry DSN 給每個服務，其他依賴的帳密只給列出它的服務，外部資料庫的 CA 掛進每個服務 | Laysi/trustiot-v2#72 決定 Secret 怎麼進叢集 |

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
| `ingress.tunnel.allowedSources` | `[10.42.0.1/32]` | `tunnel` 時唯一能連入口 pod 的來源（IPv4 CIDR，至少 /8）：本機。k3s 的主機以 cni0 連 pod，位址是節點 pod CIDR 的第一個位址（單節點預設安裝為 10.42.0.1）；其他 Kubernetes 改成 cloudflared 與 kubelet 的實際來源 |
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
| NetworkPolicy `<release>-ingress` | `tunnel`：只放行 `ingress.tunnel.allowedSources`，叢集內其他 pod 連不到入口；`direct`：只開 443 與 kubelet 檢查就緒用的 15021 |

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

與身分服務的約定（Laysi/trustiot-v2#61 實作）：入口以 `<pathPrefix><原始路徑>` 呼叫身分服務，`pathPrefix` 是 `/internal/ext-authz`，只帶 `cookie`（另有 Host、方法、路徑與 Authorization）；有效時回 200 並一定帶 `x-internal-identity`，入口以它覆寫用戶端帶來的同名標頭；無效時回 401，入口把回應原樣交給用戶端。身分服務無法連線、出錯或超過 0.5 秒時入口回 503（`statusOnError`）。這些設定在 istiod 的 provider，見 `deploy/istio/README.md`。

服務的 pod 帶 `istio.io/dataplane-mode: ambient` 加入 ambient mesh，入口到服務、服務之間由 ztunnel 加 mTLS；`podLabels` 不能改這個 label。服務之間的授權見下一節。

叢集上的 e2e 測試（cookie、例外路徑、標頭與其底線寫法、`Host` 帶埠號、28 種路徑繞過、`/internal` 的 11 種斜線寫法、身分服務停機）在 `tests/e2e/`，用法見該目錄的 README。

### 服務間授權（mesh）

決策 27 的服務間 mTLS 與授權：ztunnel 替每條連線加 mTLS，呼叫端的身分是它的 ServiceAccount（每個服務一個，`<release>-<服務>`）。「誰可以呼叫誰」只有一份清單，由兩層各自執行，任何一層單獨存在都擋得住清單以外的呼叫：

| 欄位 | 預設值 | 說明 |
| --- | --- | --- |
| `mesh.trustDomain` | `cluster.local` | mesh 的 SPIFFE trust domain（Istio 的 `meshConfig.trustDomain`）；呼叫端身分寫成 `<trustDomain>/ns/<namespace>/sa/<ServiceAccount>` |
| `mesh.calls` | `mesh.calls.ingest` 與 `mesh.calls.telemetry-query` 都是 `[clickhouse]` | 誰可以呼叫誰：`<呼叫端>: [<被呼叫端>, ...]`。呼叫端是 `services` 的 key；被呼叫端是 `services` 的 key，或叢集內元件 `clickhouse`（`clickhouse.mode: bundled` 時）。不認得的名稱讓安裝失敗；服務未啟用或元件沒有部署時略過這條，所以可以先列還沒出映像的服務。預設值是決策 14 與 docs/architecture/deployment.md 寫明的 ClickHouse 用戶端：資料接收寫入、時序查詢以每租戶唯讀帳號讀取。各服務的 issue 加入它要呼叫的內部 API |

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
| NetworkPolicy `<release>-<服務或元件>` | 同上，每個一個：只從同一批呼叫端的 pod（與入口 gateway 的 pod）放行 TCP 15008，也就是 ztunnel 的 HBONE 埠，mesh 內的連線都從這裡進來，不直接連應用程式的埠；另放行 `169.254.7.127/32`，ambient 下 kubelet 的健康檢查由 istio-cni SNAT 成這個位址 |

- 兩層的分工：AuthorizationPolicy 驗證 mTLS 憑證裡的身分，ztunnel 對每條代理的連線檢查；NetworkPolicy 只看來源 pod 的 label 與 IP，擋的是新建立的 TCP 連線。拿掉 NetworkPolicy 時，清單外的呼叫仍被 ztunnel 重置；拿掉 AuthorizationPolicy 時，清單外的服務連不到對方的 15008，非 mesh 的 pod 連不到任何埠。被拒時呼叫端看到的是連線重置或拒絕，不是 HTTP 403（docs/feasibility/2026-10-10-edge-poc.md）。2026-10-10 在實驗機實測兩個方向（docs/test-reports/2026-10-10-63-mesh-authz.md）。
- NetworkPolicy 這一層的限制，主要防線是 AuthorizationPolicy：
  - mesh 內的連線都經 15008，所以 NetworkPolicy 只有在 15008 依呼叫端的 pod 限定時才是 mesh 內的第二層；只放行「任何來源的 15008」（2026-10-10 PoC 的寫法）對 mesh 內的呼叫端沒有作用，只擋非 mesh 的 pod。
  - NetworkPolicy 不切斷已建立的連線，而 ztunnel 會保留兩個 pod 之間的 HBONE 連線重複使用。之後才收緊或加上的 NetworkPolicy，對既有連線上的呼叫不起作用，要等連線關閉（實測恢復 NetworkPolicy 後 90 秒內仍持續通過）；AuthorizationPolicy 的變更則立即套用到新的呼叫。
  - NetworkPolicy 依 label 選來源，能在 namespace 裡建 pod 的人可以複製服務的 label 通過它；mTLS 身分來自 ServiceAccount，只有 AuthorizationPolicy 擋得住（實測的 imposter）。
- NetworkPolicy 只管連入（`Ingress`），不限制連出：外部依賴（Qdrant Cloud、S3、SMTP、AI 端點、外部資料庫）以主機名稱指定，NetworkPolicy 無法依主機名稱放行。
- 不部署 waypoint：L4 的身分已足夠。服務之間需要 L7 規則（例如依路徑或方法）時才為該服務加 waypoint，並把它的 AuthorizationPolicy 改成以 `targetRefs` 指向 Service、在 ztunnel 層放行 waypoint 的 ServiceAccount；代表使用者呼叫的授權（內部身分 token）由服務自己驗證，不靠 waypoint（Laysi/trustiot-v2#106）。
- 叢集測試 `tests/e2e/mesh.e2e.ts` 以真實連線驗證清單內放行、清單外被拒，以及兩層各自生效，用法見該目錄的 README。
- 叢集內的 ClickHouse 走同一套規則：只有 `mesh.calls` 列出的 `ingest` 與 `telemetry-query` 連得到它，其他服務與 pod 都被拒，kubelet 的 `/ping` 探測經 `169.254.7.127/32` 照常通過。還沒有的用戶端加入時要列進來：migration Job（Laysi/trustiot-v2#60）要在 `trustiot.mesh.plan` 加上 Job 當呼叫端（目前呼叫端只能是服務），OpenTelemetry collector（Laysi/trustiot-v2#73）與 Grafana（Laysi/trustiot-v2#183）同樣。ClickHouse 的政策只以 chart 測試驗證，2026-10-10 的實驗機測試沒有部署它（見測試報告）。
- 預設拒絕套用到 namespace 內所有 pod，包括之後加入的叢集內元件（broker、Postgres 等）：在 mesh 內的元件加進 `trustiot.mesh.components`，由同一份清單產生政策；不在 mesh 內的元件自帶 NetworkPolicy，只放行實際的用戶端（見「擴充 chart」）。chart 測試檢查每個 Deployment 與 StatefulSet 的 pod 都有自己的 NetworkPolicy 與 AuthorizationPolicy。

### serviceDefaults 與 services.\<name\>

`serviceDefaults` 是每個服務的預設值，`services.<name>` 可以覆寫其中任何欄位。合併方式：map 逐個 key 遞迴合併，list 與單一值整個取代，`false`、`0` 與空字串也算覆寫；`probes` 底下的每個 probe 整個取代；`env` 接在 `serviceDefaults.env` 後面。

| 欄位 | 預設值 | 說明 |
| --- | --- | --- |
| `serviceDefaults.image.registry` | `ghcr.io/laysi/trustiot-v2` | 映像是 `<registry>/<服務名稱>` |
| `serviceDefaults.image.repository` | `""` | 完整的 repository，設定時取代 `<registry>/<服務名稱>` |
| `serviceDefaults.image.tag` | `""` | 空白時用 `Chart.yaml` 的 `appVersion`（服務一起發版，決策 4） |
| `serviceDefaults.image.digest` | `""` | `sha256:` 加 64 個十六進位字元；設定時以 digest 固定映像 |
| `serviceDefaults.image.pullPolicy` | `IfNotPresent` | `Always`、`IfNotPresent` 或 `Never` |
| `serviceDefaults.replicas` | `1` | 副本數 |
| `serviceDefaults.ports` | `http` 8080 | 容器埠清單，欄位見下表；Service 開同樣的埠號 |
| `serviceDefaults.env` | `[]` | 每個服務都加的環境變數（Kubernetes `EnvVar`）；每個服務都會拿到，所以不能讀 Secret（`valueFrom.secretKeyRef`）；名稱的限制同 `services.<name>.env` |
| `serviceDefaults.resources` | requests `cpu: 20m`、`memory: 32Mi`；limits `cpu: "1"`、`memory: 256Mi` | 資源需求與上限；schema 要求 `limits.memory` |
| `serviceDefaults.podSecurityContext` | `runAsNonRoot: true`、`seccompProfile: RuntimeDefault` | 不固定 `runAsUser`，讓 OpenShift 的 restricted-v2 SCC 指派 UID。只接受 restricted Pod Security Standard 允許的欄位與值：`runAsNonRoot` 只能是 `true`，seccomp 與 AppArmor 只能是 `RuntimeDefault` 或 `Localhost`，不能設 sysctls |
| `serviceDefaults.securityContext` | `allowPrivilegeEscalation: false`、`readOnlyRootFilesystem: true`、`capabilities.drop: [ALL]` | 同樣只接受 restricted 允許的設定：不能提權、不能 privileged、`capabilities.drop` 一定含 `ALL` 且不能加 capability、`procMount` 只能是 `Default` |
| `serviceDefaults.probes.startup` | `GET /healthz`，每 2 秒，最多 30 次 | 設成 `{}` 停用 |
| `serviceDefaults.probes.liveness` | `GET /healthz`，每 10 秒，逾時 2 秒，失敗 3 次 | 必填 |
| `serviceDefaults.probes.readiness` | `GET /readyz`，每 5 秒，逾時 2 秒，失敗 2 次 | 必填 |
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
| `services.<name>.ports` | 整個取代埠清單；埠名與埠號不能重複，探針只能指向清單內的埠名 |
| `services.<name>.env` | 接在 `serviceDefaults.env` 後面，名稱不能重複。`TRUSTIOT_` 後面接英數字、`SENTRY_`、`OTEL_` 開頭的名稱（不分大小寫）保留給 chart，避免覆寫平台設定與 chart 給的密鑰；`TRUSTIOT__`（兩個底線）開頭的名稱不在保留範圍。名稱像密鑰（含 SECRET、PASSW、PASSPHRASE、PWD、TOKEN、API_KEY、DSN、PRIVATE、CREDENTIAL，或以 `_KEY` 結尾，不分大小寫）的變數只能用 `valueFrom.secretKeyRef`，不能用字面值或 ConfigMap 等其他來源；每一項只能有 `value` 或一個 `valueFrom` 來源。任何變數都不能讀 values 指定的依賴帳密與 Sentry DSN（同一個 Secret 的同一個 key），依賴帳密要改列在 `credentials` |
| `services.<name>.credentials` | 只給這個服務的依賴帳密，可列 `qdrant-read`、`qdrant-write`、`object-storage`、`smtp`、`ai`，變數見「密鑰的環境變數」；不能放在 `serviceDefaults`，避免每個服務都拿到。`ai` 只能給 `ai-gateway`（決策 28）；`qdrant-read` 與 `qdrant-write` 只能給 `knowledge`，因為 Qdrant 查詢的租戶條件由知識庫服務加上（docs/architecture/rag.md） |
| `services.<name>.resources` | 逐欄覆寫資源設定 |
| `services.<name>.podSecurityContext` | 逐欄覆寫；限制同 `serviceDefaults` |
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

### 外部依賴

所有外部依賴由 values 指定，託管或自架只差在 values（可搬移規則 2）。`bundled` 代表由這份 chart 部署在叢集內；`external` 代表用叢集外的服務。

| 欄位 | 預設值 | 說明 |
| --- | --- | --- |
| `postgres.mode` | `bundled` | `bundled`：CloudNativePG（Laysi/trustiot-v2#58）；`external`：自行維運、含 Apache AGE 的 Postgres |
| `postgres.external.host` | `""` | `external` 時必填；主機名稱或 IPv4 |
| `postgres.external.port` | `5432` | |
| `postgres.external.database` | `trustiot` | 資料庫名稱 |
| `postgres.external.sslMode` | `verify-full` | 只接受 `verify-full`：一律驗證伺服器憑證（清單 M3） |
| `postgres.external.caSecret.name` | `""` | `external` 時必填；簽發伺服器憑證的 CA 所在的 Secret |
| `postgres.external.caSecret.key` | `ca.crt` | |
| `clickhouse.mode` | `bundled` | `bundled`：叢集內自管（Laysi/trustiot-v2#59）；`external`：ClickHouse Cloud 等，只當備案（決策 14） |
| `clickhouse.bundled.image.repository` | `docker.io/clickhouse/clickhouse-server` | |
| `clickhouse.bundled.image.tag` | `26.8` | 26.8 LTS；實際版本由 digest 決定 |
| `clickhouse.bundled.image.digest` | `sha256:110d3d1f…b40c` | 必填（版本固定），multi-arch index 的 digest，含 arm64；必須與 `scripts/lib/stack.ts` 和 `services/ingest/tests/common/mod.rs` 相同（測試比對） |
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
| `clickhouse.external.caSecret.name` | `""` | 私有 CA 簽發時才設定 |
| `clickhouse.external.caSecret.key` | `ca.crt` | |
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

URL 欄位都不接受 `user:password@` 這種帶帳密的寫法。

## 叢集內的 ClickHouse

`clickhouse.mode: bundled`（預設，SaaS 與地端相同）部署 `templates/clickhouse.yaml`：

- 一個副本的 StatefulSet `<release>-clickhouse`，資料在 PVC `data-<release>-clickhouse-0`（`/var/lib/clickhouse`）；StatefulSet 刪除或縮容時保留 PVC。ClusterIP Service 只開 HTTP 8123 與 native 9000。
- 伺服器設定只有一個 ConfigMap（`<release>-clickhouse-config`，掛成 `config.d`）：監聽位址、關掉 MySQL／PostgreSQL 相容埠與 interserver 埠、日誌只寫 stdout。設定變更時 pod 自動重啟。
- 帳號、角色、row policy、settings profile 與 quota 一律不寫在設定檔（決策 14；ClickHouse Cloud 不能改設定檔）：由 migration Job（Laysi/trustiot-v2#60）以 bootstrap 管理帳號套用 `services/ingest/migrations/clickhouse/` 的 SQL，存在 ClickHouse 自己的 access storage（PVC 上的 `access/`）。唯一由映像產生的帳號是 bootstrap 管理帳號：映像的 entrypoint 依 `CLICKHOUSE_USER`、`CLICKHOUSE_PASSWORD_FILE`（掛載的 Secret）與 `CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1` 寫進 `users.d`（記憶體 emptyDir），並移除沒有密碼的 `default` 帳號。
- 根目錄唯讀、以 UID 101 非 root 執行、不能提權、drop 所有 capability；探針是 `GET /ping`。pod 帶 `istio.io/dataplane-mode: ambient` 加入 ambient mesh，服務連 ClickHouse 由 ztunnel 加 mTLS。

磁碟區大小沒有預設值，每個部署在安裝實例的覆寫檔設定 `clickhouse.bundled.storage.size`：每日筆數 × 保存天數 × 約 3.1 bytes（2026-10-09 實測，30 天 52 億筆佔 15.2 GiB）× 1.5（merge 時新舊 part 並存與系統表的餘裕）。例如每天 1 億筆、保存 400 天約 190 GB（遙測表的保存期限暫定 400 天，Laysi/trustiot-v2#92 定案）。磁碟用量的告警與擴充見清單 I2（硬碟水位告警）。

安裝前要先有 `clickhouse.bundled.adminSecret` 指定的 Secret（如何進叢集由 Laysi/trustiot-v2#72 決定），例如：

```sh
kubectl -n trustiot create secret generic trustiot-clickhouse-admin --from-literal=password="$(openssl rand -hex 32)"
```

## chart 與服務的約定

服務照下列約定實作，就能直接用通用樣板部署（docs/architecture/deployment.md 的服務設計規則）：

- **設定**：ConfigMap `<release>-platform` 以 `envFrom` 交給每個服務，另外每個服務有 `OTEL_SERVICE_NAME=<服務名稱>`。`TRUSTIOT_` 後接英數字、`SENTRY_` 與 `OTEL_` 開頭的變數只由 chart 設定，`env` 不能覆寫，服務讀到的平台設定一定是 values 決定的值。沒有設定的項目不會出現，服務可以分辨「沒設定」與「空字串」。平台設定變更時 pod 會自動滾動更新。
- **密鑰**：不放進 ConfigMap，以 `valueFrom.secretKeyRef` 從 values 指定的 Secret 取用。Sentry 開啟時每個服務有 `SENTRY_DSN`；其他依賴的帳密只給在 `services.<name>.credentials` 列出它的服務（見後面「密鑰的環境變數」），Secret 名稱空白的項目不注入。外部 Postgres 與 ClickHouse 的 CA 憑證以唯讀方式掛在 `/etc/trustiot/<postgres|clickhouse>-ca/ca.crt`。服務自己的資料庫帳號（每個服務一個角色，決策 4）隨 Postgres 與 migration 的 issue 加入（Laysi/trustiot-v2#58、Laysi/trustiot-v2#60）。
- **埠與探針**：HTTP 監聽 8080（埠名 `http`）；`GET /healthz` 供 startup 與 liveness，`GET /readyz` 供 readiness。
- **SIGTERM**：pod 刪除時先等 `preStopSleepSeconds`（預設 5 秒），再送 SIGTERM；服務收到後 readiness 立即回失敗、處理完進行中的請求、主動關閉長連線，在 `drainSeconds`（預設 30 秒）內結束，之後才會被 SIGKILL。服務是容器內的 PID 1，要自己處理 SIGTERM。
- **執行環境**：根目錄唯讀，只有 `/tmp` 可寫；以非 root 執行，映像的 `USER` 要是數字 UID，並能以 OpenShift 指派的任意 UID 執行；沒有 ServiceAccount token；只排到 arm64 節點。

平台設定的環境變數：

| 變數 | 來源 | 何時出現 |
| --- | --- | --- |
| `TRUSTIOT_DEPLOYMENT_MODE` | `global.deploymentMode` | 一律 |
| `TRUSTIOT_APP_HOST`、`TRUSTIOT_GATEWAY_HOST`、`TRUSTIOT_OTA_HOST` | `hosts.*` | 一律 |
| `TRUSTIOT_CLIENT_IP_SOURCE` | 由 `ingress.exposure` 推導：`cf-connecting-ip` 或 `x-forwarded-for-rightmost` | 一律 |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE` | `postgres.external.*` | `postgres.mode` 為 `external` |
| `TRUSTIOT_CLICKHOUSE_URL` | `external`：`clickhouse.external.url`；`bundled`：`http://<release>-clickhouse.<namespace>.svc:8123`（叢集內明文 HTTP，pod 之間由 Istio ambient 的 ztunnel 加上 mTLS） | 一律 |
| `TRUSTIOT_QDRANT_URL` | `qdrant.external.url` | `qdrant.mode` 為 `external` |
| `TRUSTIOT_S3_ENDPOINT`、`_REGION`、`_BUCKET`、`_FORCE_PATH_STYLE` | `objectStorage.*` | 各自有值時；`_FORCE_PATH_STYLE` 隨 bucket |
| `TRUSTIOT_SMTP_HOST`、`_PORT`、`_SECURITY`、`_FROM` | `smtp.*` | 有 host 時（`_FROM` 有值時） |
| `TRUSTIOT_AI_CHAT_BASE_URL`、`TRUSTIOT_AI_EMBEDDING_BASE_URL`、`TRUSTIOT_AI_RERANK_BASE_URL` | `ai.*.baseUrl` | 各自有值時 |
| `TRUSTIOT_POSTGRES_CA_FILE`、`TRUSTIOT_CLICKHOUSE_CA_FILE` | 掛載的 CA 憑證路徑 | 對應的依賴為 `external` 且設定了 `caSecret.name` |
| `SENTRY_ENVIRONMENT` | `observability.sentry.environment` | Sentry 開啟時（`SENTRY_DSN` 來自 Secret，見上） |

`bundled` 模式的叢集內端點，由部署該元件的 issue 加進這張表（ClickHouse 見 `TRUSTIOT_CLICKHOUSE_URL`）。服務連 ClickHouse 的帳號與密碼隨 migration Job 與各服務加入（Laysi/trustiot-v2#60、Laysi/trustiot-v2#91、Laysi/trustiot-v2#119）。

密鑰的環境變數（以 `valueFrom.secretKeyRef` 注入，不在 ConfigMap）：

| 變數 | 來源 | 哪些服務拿到 |
| --- | --- | --- |
| `SENTRY_DSN` | `observability.sentry.dsnSecret` | Sentry 開啟時每個服務 |
| `TRUSTIOT_QDRANT_READ_API_KEY` | `qdrant.external.credentialsSecret` 的 `readApiKeyKey` | `knowledge` 的 `credentials` 含 `qdrant-read`，且 `qdrant.mode` 為 `external` |
| `TRUSTIOT_QDRANT_WRITE_API_KEY` | `qdrant.external.credentialsSecret` 的 `writeApiKeyKey` | `knowledge` 的 `credentials` 含 `qdrant-write`，且 `qdrant.mode` 為 `external` |
| `TRUSTIOT_S3_ACCESS_KEY_ID`、`TRUSTIOT_S3_SECRET_ACCESS_KEY` | `objectStorage.credentialsSecret` | `credentials` 含 `object-storage` |
| `TRUSTIOT_SMTP_USERNAME`、`TRUSTIOT_SMTP_PASSWORD` | `smtp.credentialsSecret` | `credentials` 含 `smtp` |
| `TRUSTIOT_AI_CHAT_API_KEY`、`TRUSTIOT_AI_EMBEDDING_API_KEY`、`TRUSTIOT_AI_RERANK_API_KEY` | `ai.*.apiKeySecret` | `ai-gateway` 的 `credentials` 含 `ai`（決策 28） |

## 擴充 chart

- **新增服務**：在 `values.yaml` 的 `services` 加一個 key，並同步 docs/architecture/deployment.md 的服務表（測試比對兩邊）。服務特有的埠、資源、探針與 Secret 寫在該 key 底下。
- **服務之間的呼叫**：服務要呼叫另一個服務的內部 API 時，在服務的 issue 把這條呼叫加進 `values.yaml` 的 `mesh.calls`；沒列的呼叫會被重置。服務間授權的規則集中在 `templates/_mesh.tpl` 的 `trustiot.mesh.plan`，AuthorizationPolicy 與 NetworkPolicy 都由它產生，不要為單一服務另外寫政策。
- **對外的路由**：服務要經入口對外時，在服務的 issue 把路由加進 `values.yaml` 的 `ingress.routes`（預設需要 cookie）。不要新增例外路徑：決策 27 只允許登入、閘道器憑證換發、MQTT 轉接與健康檢查四種，要加第五種必須先改決策。入口的規則集中在 `templates/_ingress.tpl` 的 `trustiot.ingress.plan`；可信的來源 IP 沿用 `trustiot.clientIpSource`，不要另外加欄位。
- **新增叢集內元件**（broker、Postgres、ClickHouse、Qdrant、collector、Grafana）：放在自己的 `templates/<元件>.yaml`，以 `*.mode` 或開關控制；Service 只能是 ClusterIP；namespace 預設拒絕所有連入：元件在 mesh 內時（pod 帶 `istio.io/dataplane-mode: ambient`）把它加進 `templates/_mesh.tpl` 的 `trustiot.mesh.components`、用戶端列進 `mesh.calls`，AuthorizationPolicy 與 NetworkPolicy 由 chart 產生；不在 mesh 內時自帶 NetworkPolicy，只對實際的用戶端開它的埠，並讓 kubelet 的健康檢查到得了；在 `values.schema.json` 加欄位定義、在本文件加欄位說明（測試檢查 `values.yaml` 每個欄位都在本文件），並更新「元件與開關」與平台設定的表。
- **Secret**：values 只能出現 `{name, key}` 形式的引用；schema 的 `additionalProperties: false` 會擋下放明文的新欄位。服務要拿新的依賴帳密時，在 `_helpers.tpl` 的 `trustiot.credentialCatalog` 加一列（變數名稱、Secret、可以拿的服務）、在 schema 的 `credentials` 列舉加上名稱，並更新「密鑰的環境變數」表；自訂 `env` 自動不能讀這個帳密。叢集內元件自己產生的帳密 Secret（例如 CloudNativePG 的資料庫帳號，Laysi/trustiot-v2#58）也要加進 `trustiot.dependencySecretRefs`。新的平台變數用 `TRUSTIOT_` 開頭，自動受保留規則保護。
- **測試**：規則寫進 `tests/chart.test.ts`。`tests/values/` 的兩個安裝實例範例與 `smoke.yaml`（以 traefik/whoami 當替身服務）也供 nightly（Laysi/trustiot-v2#69）在叢集上安裝使用；叢集測試是 `tests/e2e/ingress.e2e.ts`（入口）與 `tests/e2e/mesh.e2e.ts`（服務間授權），共用 `tests/e2e/cluster.ts`，nightly 可直接沿用。`bun test` 的 kubeconform 已帶 CRD schema 目錄，Gateway API 與 Istio 的資源可以直接驗證。兩個實例範例會部署叢集內 ClickHouse（`clickhouse.mode: bundled`），在叢集上安裝前要先建立 `trustiot-clickhouse-admin` Secret（見「叢集內的 ClickHouse」）；叢集測試用 `tests/e2e/values-e2e.yaml` 改成 `external`，不部署 ClickHouse。`tests/` 不會打包進 chart（`.helmignore`）。

## OpenShift 與其他 Kubernetes

- Pod 符合 restricted Pod Security Standard，且不固定 UID，可以在 OpenShift 的 restricted-v2 SCC 下執行。
- 預設只排到 arm64 節點；客戶用 x86 主機時，要先加出 amd64 映像（決策 5）再調整 `serviceDefaults.nodeSelector`。
- chart 需要叢集已裝 Istio ambient 與 Gateway API CRD，並設好 ext_authz provider。k3s 用 `global.platform=k3s`，OpenShift 用 `global.platform=openshift` 或 OpenShift Service Mesh 3，步驟見 `deploy/istio/README.md`。
- 入口 gateway 的 pod 由 Istio 產生，以 UID 1337 執行，符合 restricted Pod Security Standard（2026-10-10 在 restricted namespace 實測）；OpenShift 的 restricted-v2 SCC 會指派其他 UID，目前沒有 OpenShift 環境可測。
- `ingress.tunnel.allowedSources` 的預設值是 k3s 單節點的本機位址；其他 CNI 要改成 cloudflared 與 kubelet 實際的來源位址。
