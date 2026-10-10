# 入口：mesh 路徑正規化、底線標頭與帶埠號的 Host（2026-10-11）

對應 issue：Laysi/trustiot-v2#255（修正 [2026-10-10 獨立 e2e 驗證](2026-10-10-62-e2e-verify.md) 的 F1–F3）。原始資料在 [data/2026-10-11-255-path-normalization/](data/2026-10-11-255-path-normalization/)。

## 目的

1. 證明 `//internal/...`、`/%2finternal/...` 這類寫法帶有效 cookie 時，修正前會到達全包路由的後端，修正後入口回 403、後端收不到（F1）；修正靠的是 mesh 的 `pathNormalization: DECODE_AND_MERGE_SLASHES`，不是 chart。
2. 底線寫法的 `x_internal_identity` 等標頭到不了後端（F2）。
3. 例外路徑在 `Host` 帶埠號時照樣是例外，而且只在它自己的主機（F3）。
4. repo 既有的 `ingress.e2e.ts` 在 SaaS 與地端兩份 values 全部通過。
5. 測完 lab 的 istiod mesh 設定與測試前逐位元組相同，`poc` 與 `edge` 不受影響。

## 環境

- **受測 commit**：分支 `fix/255-ingress-path-normalization` 的 `032acf2`（以 main `8931a63` 為基底）。「修正前」用 main `8931a63` 的 `deploy/chart`（`git archive` 取出到 `~/.cache/trustiot-v2-255/before`），只換上本分支的 `ingress.e2e.ts`。
- **本機**：x86_64 Fedora；bun 1.4.2、helm 4.3.0、kubectl 1.36.5，皆由 `.prototools` 鎖定。
- **lab**：`trustiot-mac-m1`（aarch64），k3s v1.36.5+k3s1，Istio 1.30.5 ambient；istiod、ztunnel、istio-cni 的映像 digest 見 [environment-lab.txt](data/2026-10-11-255-path-normalization/environment-lab.txt)。
- **身分服務替身**：在 lab 以 podman 由 repo 的 `identity-double/` 建置，`localhost/trustiot-e2e/identity-double:255@sha256:063a3d3594e37a3ef7c0c224a2ac2c0579000de9b3ecea1825f9233b486bf0e5`；其他後端是 `values-e2e.yaml` 固定 digest 的 traefik/whoami。
- **namespace**：`e2e-255`（Pod Security `restricted`）。執行身分是該 namespace 的 ServiceAccount `e2e-255-runner`（RoleBinding `admin`，另加只在該 namespace 有效、允許 Gateway、HTTPRoute、AuthorizationPolicy 的 Role），token 3 小時，kubeconfig 只放在本機暫存目錄，測完刪除；API server 經 `ssh -L` 連線。
- **mesh 設定**：依 `deploy/chart/tests/e2e/README.md`，以 k3s 的 `HelmChartConfig istiod` 疊加。內容由 [make-hcc.ts](data/2026-10-11-255-path-normalization/scripts/make-hcc.ts.txt) 從 repo 的 `deploy/istio/istiod-values.yaml` 產生：provider `trustiot-e2e-255` 是 repo 的 provider 設定改指向 `e2e-255` 的替身，lab 原有的 provider `identity`（PoC）原樣重複寫入；兩個版本只差 `pathNormalization`（[不含](data/2026-10-11-255-path-normalization/lab-istiod-helmchartconfig-without-normalization.yaml)、[含](data/2026-10-11-255-path-normalization/lab-istiod-helmchartconfig.yaml)）。沒有修改 `~/stack/platform/istio.yaml`。
- **請求路徑**：測試經 `E2E_SSH_JUMP=trustiot-mac-m1` 由 lab 主機連入口的 ClusterIP，來源與 cloudflared 相同，會經過入口的 NetworkPolicy。
- **時間**：台北時間 2026-10-11 02:09–02:17（UTC 2026-10-10 18:09–18:17）。lab 鎖 UTC 18:09 取得、18:17 釋放。

## 指令

lab 準備（lab 主機上）：

```sh
sudo k3s kubectl -n istio-system get cm istio -o yaml > ~/e2e-255/mesh-before-255.yaml   # 備份
sudo k3s kubectl -n istio-system get cm istio -o jsonpath='{.data.mesh}' | sha256sum
# 建替身映像並匯入 containerd、建 namespace 與 RBAC：同 deploy/chart/tests/e2e/README.md
```

產生 `HelmChartConfig`（本機）：

```sh
bun make-hcc.ts deploy/istio/istiod-values.yaml <lab 的 mesh 設定> e2e-255 without > hcc-without.yaml
bun make-hcc.ts deploy/istio/istiod-values.yaml <lab 的 mesh 設定> e2e-255 with > hcc-with.yaml
```

每次 e2e 以 [run-e2e.sh](data/2026-10-11-255-path-normalization/scripts/run-e2e.sh.txt) 執行，內容等於：

```sh
KUBECONFIG=<暫存> E2E_NAMESPACE=e2e-255 E2E_SHAPE=<saas|onprem> E2E_AUTHZ_PROVIDER=trustiot-e2e-255 \
E2E_IDENTITY_IMAGE=localhost/trustiot-e2e/identity-double:255@sha256:063a3d3594e37a3ef7c0c224a2ac2c0579000de9b3ecea1825f9233b486bf0e5 \
E2E_IMAGE_PULL_POLICY=Never E2E_SSH_JUMP=trustiot-mac-m1 E2E_REPORT_DIR=<資料目錄> \
bun test --timeout 600000 --reporter=junit --reporter-outfile=<資料目錄>/e2e-<shape>.junit.xml ./deploy/chart/tests/e2e/ingress.e2e.ts
```

順序（UTC 2026-10-10）：

| 時間 | 動作 |
| --- | --- |
| 18:10:32 | 套用 `hcc-without.yaml`（只加 provider） |
| 18:10:55–18:11:50 | **修正前**：main 的 chart、SaaS（`before-saas/`） |
| 18:12:06–18:13:03 | **只有 chart 修正**：本分支的 chart、mesh 沒有路徑正規化、SaaS（`chart-only-saas/`） |
| 18:13:08 | 套用 `hcc-with.yaml`（加上 `DECODE_AND_MERGE_SLASHES`），18:13:22 mesh 設定已更新 |
| 18:13:29–18:14:27 | **修正後**：本分支、SaaS（`after-saas/`） |
| 18:14:31–18:15:32 | **修正後**：本分支、地端（`after-onprem/`） |
| 18:15:38 | 路徑正規化生效中，檢查 PoC 的 `edge`（[poc-edge-during.txt](data/2026-10-11-255-path-normalization/poc-edge-during.txt)） |
| 18:15:43 | 刪除 `HelmChartConfig istiod`，helm-controller 以原本的值重裝 istiod |
| 18:16:00–18:16:45 | 刪除 `e2e-255`，比對 mesh 設定與 `poc`、`edge`（[lab-restore.txt](data/2026-10-11-255-path-normalization/lab-restore.txt)） |

## 結果

### 總表

| 執行 | chart | mesh 路徑正規化 | 結果 | 資料 |
| --- | --- | --- | --- | --- |
| 修正前 | main `8931a63` | 無（`BASE`） | 61 項：44 通過、**17 失敗**（全部是新加的案例） | [before-saas/](data/2026-10-11-255-path-normalization/before-saas/) |
| 只有 chart 修正 | 本分支 | 無 | 61 項：50 通過、**11 失敗**（`/internal` 的 11 種斜線寫法） | [chart-only-saas/](data/2026-10-11-255-path-normalization/chart-only-saas/) |
| 修正後（SaaS） | 本分支 | `DECODE_AND_MERGE_SLASHES` | **61 項全部通過** | [after-saas/](data/2026-10-11-255-path-normalization/after-saas/) |
| 修正後（地端） | 本分支 | `DECODE_AND_MERGE_SLASHES` | **61 項全部通過** | [after-onprem/](data/2026-10-11-255-path-normalization/after-onprem/) |

61 項 = 既有的 44 項 + 本次新加的 17 項（`/internal` 斜線寫法 11 項、底線標頭 5 項、`Host` 帶埠號 1 項）。

### F1：`/internal` 的斜線寫法（有效 cookie）

| 寫法 | 修正前 | 只有 chart 修正 | 修正後 SaaS | 修正後地端 |
| --- | --- | --- | --- | --- |
| `//internal`、`//internal/tenants`、`///internal`、`//./internal`、`/.//internal/tenants`、`/x/..//internal/tenants` | 307（通過 DENY 到達全包路由的後端，whoami 整理路徑後重導向到 `/internal/...`） | 307 | **403** | **403** |
| `/%2finternal`、`/%2finternal/tenants`、`/%2Finternal`、`/%2f%2finternal`、`/./%2finternal` | 200，**到達需要 cookie 的後端** | 200，到達後端 | **403** | **403** |

沒有 cookie 時 11 種寫法在四次執行都是 401。2026-10-10 的驗證另外量過 `MERGE_SLASHES`：雙斜線擋得住，`%2f` 的寫法仍到達後端（[mesh-pathnormalization-experiment.txt](data/2026-10-10-62-e2e-verify/mesh-pathnormalization-experiment.txt)），所以選 `DECODE_AND_MERGE_SLASHES`。

既有的 28 種 `BYPASS_CASES`（無 cookie）在修正前後都是 401 ×27、404 ×1，到達需要 cookie 的後端 0 次、偽造值回顯 0 次；路徑正規化沒有讓任何寫法反而變成可以繞過。

### F2：底線寫法的身分標頭

每個請求同時帶 `x_internal_identity`、`x-internal_identity`、`x_internal-identity` 三個標頭，值是偽造的。

| 路徑 | 修正前 | 修正後（兩種型態） |
| --- | --- | --- |
| 需要 cookie 的 `/api/whoami`（有效 cookie） | 200，後端收到三個偽造值（`X-Internal_identity`、`X_internal-Identity`、`X_internal_identity`） | 200，後端 0 個底線寫法，`x-internal-identity` 恰好 1 個且是身分服務給的 |
| 例外路徑 `/certs/renew`、`/mqtt`、`/auth/login`、`/healthz` | 200，偽造值到達後端 | 200，偽造值不見 |

### F3：`Host` 帶埠號

| `Host` | 路徑 | 修正前 | 修正後 SaaS（`:80`） | 修正後地端（`:443`） |
| --- | --- | --- | --- | --- |
| gateway 主機加埠 | `/mqtt` | 401 | 200（mqtt-bridge） | 200 |
| gateway 主機加埠 | `/certs/renew` | 未測（前一項已失敗） | 200 | 200 |
| gateway 主機加 `:8443` | `/mqtt` | 未測 | 200 | 200 |
| app 主機加埠 | `/healthz`、`/auth/login` | 未測 | 200 | 200 |
| app 主機加埠 | `/mqtt`、`/certs/renew`、`/api/whoami`（無 cookie） | 未測 | 401 | 401 |
| gateway 主機加埠 | `/auth/login` | 未測 | 404 | 404 |
| ota 主機加埠 | `/mqtt` | 未測 | 404 | 404 |

修正前的測試在第一個斷言（`gw…:80` 的 `/mqtt` 期望 200、實得 401）就停止，同一個測試裡後面的請求沒有送出。修正後例外只在它自己的主機成立，帶埠號的 app 主機照樣要 cookie；沒有任何請求到達需要 cookie 的後端或回顯偽造值。

### 其他

- 身分服務延遲 3 秒：SaaS 503 於 537–558 ms（修正前）、541–560 ms（修正後），地端 561–578 ms，逾時行為不受路徑正規化影響。
- 路徑正規化生效期間，PoC 的 `edge`（`trustiot-poc.laysi.io`、`poc.trustiot.test`）`/healthz` 200、`/api/whoami` 401，與平常相同。

### 還原

[lab-restore.txt](data/2026-10-11-255-path-normalization/lab-restore.txt)：刪除 `HelmChartConfig` 後，`istio` ConfigMap 的 `mesh` 與 `meshNetworks` 和測試前的備份以 `cmp` 比對**逐位元組相同**（`mesh` 的 sha256 前後都是 `11507d117465e92da63f14204fc2c19837df210957b181730a46a0ce91ae76be`，`meshNetworks` 都是 `7b2c40b5001e85e00a7ee8ebfc7bcfea574e72f8906e1503bc73e5a2cb0eeaed`）。istiod、ztunnel、istio-cni、`edge` 的 pod 都沒有重啟；`poc` 的 8 個 pod 都在 Running；`edge` 仍 Programmed，`/healthz` 200、`/api/whoami` 401。namespace `e2e-255`、替身映像（containerd 與 podman）、`~/e2e-255` 都已刪除，`~/e2e.lock` 已釋放；本機的 kubeconfig 已刪除。mesh 設定被改動的期間是 UTC 18:10:32–18:16:00，其中路徑正規化生效約 2.5 分鐘（18:13:08–18:15:43）。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 依重現步驟，`//internal/tenants` 與 `/%2finternal/tenants` 帶有效 cookie 時入口回 403、後端收不到；e2e 回歸案例修正前失敗、修正後通過，在 lab 實測 | **通過** | F1 表：修正前 307／200（到達後端），修正後兩種型態都 403；`before-saas` 11 項失敗、`after-*` 全部通過；`chart-only-saas` 證明修正來自 mesh 設定 |
| 既有的 `ingress.e2e.ts` 在 SaaS 與地端全部通過 | **通過** | `after-saas`、`after-onprem` 各 61 項全過（含既有 44 項） |
| `x_internal_identity` 到不了後端 | **通過** | F2 表：修正前三種底線寫法都到達後端，修正後 0 個 |
| 帶埠號 Host 的處理方式已決定並寫入文件；若改成接受，有 e2e 測試 | **通過** | 決定接受（`deploy/chart/README.md` 的「`Host` 帶埠號」、決策 27）；F3 表的 e2e 案例兩種型態都通過 |
| `deploy/istio/README.md` 說明路徑正規化的設定與影響 | **通過** | 該檔「路徑正規化」一節 |

## 未涵蓋

- 真正的身分服務（Laysi/trustiot-v2#61）、前端與閘道器韌體的 MQTT WebSocket：本次只用替身。`DECODE_AND_MERGE_SLASHES` 對它們的影響只做了分析（路徑參數只有 UUID，見 `deploy/istio/README.md`），沒有實測；閘道器實際送出的 `Host` 格式由 Laysi/trustiot-v2#77 的端到端驗證確認。
- 大小寫不同的 `/INTERNAL` 仍會落到全包路由（路徑比對區分大小寫）；這不是正規化的範圍，後端不得自行做大小寫正規化（README 已寫明）。
- OpenShift Service Mesh 3：沒有環境，`pathNormalization` 放進 `Istio` 資源的寫法只依文件。
