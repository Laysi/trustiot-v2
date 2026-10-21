# MS0 完成條件與證明它們的測試

MS0 完成時，[里程碑](milestones.md) 規定每個完成條件都寫成自動測試，MS0 照這個規定完成，下表每個條件都有自動測試。2026-10-21 改成的規定（至少實測一次、自動化另開 P2 issue）只適用於之後的完成條件。本文件把 MS0 的完成條件（里程碑表 MS0 列，加上經入口的驗證冒煙測試）逐條對到證明它的測試、測試在哪裡執行，以及它是否證明整個條件。2026-10-15 由 Laysi/trustiot-v2#77 整理；之後改動或搬移下列測試時，同一個變更要更新本表。

## 執行的地方

| 簡稱 | 什麼時候跑 | 說明 |
| --- | --- | --- |
| CI `rust` | 每個 PR 與 main 的每次 push（路徑相關時） | `.github/workflows/ci.yml`：`cargo test --workspace`。需要容器的測試在 CI 裡設了 `CI`，沒有容器時失敗而不是略過 |
| CI `helm` | 同上 | `bun test deploy/chart`：helm lint、kubeconform、chart 的規則 |
| CI `workflow-policy` | 每個 PR 與 main 的每次 push | `bun run --cwd scripts test`：`scripts/` 的單元測試，nightly 的檢查邏輯以假的叢集測 |
| nightly | 每晚，以及帶 `smoke` label 的 PR | `.github/workflows/nightly.yml`：k3d 叢集裝 Gateway API、Istio ambient、CloudNativePG，兩份 values 各部署一次（`scripts/nightly/smoke.ts`），細節見 [CI 與發佈](../architecture/ci-release.md) 的「nightly.yml」 |
| lab | 手動，需要 lab 的鎖 | lab 的 k3s 上的叢集測試（`deploy/chart/tests/e2e/README.md`）；不是任何條件唯一的證據 |

## 對照表

| # | 完成條件 | 證明的測試 | 跑在 | 證明整個條件 |
| --- | --- | --- | --- | --- |
| 1 | 服務骨架經 API 閘道對外，身分服務與資產服務以事件互通 | nightly smoke 的 API 檢查（`scripts/nightly/api.ts` 的 `checkTenantApi`，Laysi/trustiot-v2#65）：從這個 commit 建出的身分與資產服務，經入口 gateway 以平台管理者的 cookie `POST /api/identity/v1/tenants` 得 201，身分服務在同一個交易寫 `identity.tenant_created`，資產服務消費後，新租戶成員的 cookie 經入口 `GET /api/assets/v1/tenant` 得 200 且是同一個租戶。服務層：`services/assets/tests/tenant_created.rs` 的 `every_new_tenant_gets_one_row_whatever_is_replayed`（重播不重複）、`services/identity/tests/tenants.rs` 的 `platform_administrators_create_tenants_with_their_event`（列與事件同一個交易）。檢查邏輯：`scripts/nightly/api.test.ts` | nightly（兩份 values）；CI `rust`；CI `workflow-policy` | 是。MS0 的服務骨架就是身分與資產兩個服務，之後的服務由各自的 issue 加進 API 檢查 |
| 2 | 未經 AuthorizationPolicy 允許的服務呼叫被拒絕 | `deploy/chart/tests/e2e/mesh.e2e.ts`（Laysi/trustiot-v2#63）：真實連線分五個階段，第 4 階段只留 AuthorizationPolicy 時清單外的呼叫全被拒，冒用 assets 標籤但 ServiceAccount 不同的 pod（NetworkPolicy 依標籤放行）也被拒；第 3 階段兩層都拿掉時同樣的呼叫成功，證明被拒是政策造成的。2026-10-15 起 nightly 每晚以 SaaS 型態跑一次（`scripts/nightly/e2e.ts`，Laysi/trustiot-v2#77）。渲染規則：`deploy/chart/tests/chart.test.ts` 的「service-to-service authorization (decision 27)」 | nightly（SaaS）；lab；CI `helm` | 是。呼叫端是身分服務替身的映像（服務的映像沒有 shell，無法從 pod 內發出呼叫）；AuthorizationPolicy 只看呼叫端 pod 的 ServiceAccount，與映像無關 |
| 3 | 身分服務停掉時入口一律拒絕，明列的例外除外；以路由清單測試證明每條路由都要經過驗證 | 停機：`ingress.e2e.ts` 的「identity service down」（身分服務 scale 到 0 時需要 cookie 的路徑有無 cookie 都回 503，例外路徑 `/certs/renew`、`/mqtt` 照常 200，恢復後回到 401）與「slow identity service」（逾時回 503）。路由清單：`chart.test.ts` 的「route list, …」七個案例以 `deploy/chart/tests/ingress-authz.ts` 照 gateway 的語意（Gateway API 的路由選擇、Istio AuthorizationPolicy 的比對）評估渲染出的每條 HTTPRoute 與 cookie 檢查：每個會被路由到後端的請求（主機帶或不帶埠號）都經過 cookie 檢查，唯一的例外是決策 27 列的公開路徑、而且只在它自己的主機；路由清單包含與例外重疊的前綴與其他主機上的同名路徑，也包含 nightly 與兩個叢集測試實際安裝的路由。評估本身的測試在 `ingress-authz.test.ts`（漏掉 `:*`、例外漏到別的主機、沒列的公開路由都會被抓到）。fail closed：`chart.test.ts` 的「the mesh extension provider in deploy/istio fails closed and matches the chart」 | nightly（兩份 values）；CI `helm` | 是。停機測試用身分服務替身：入口在身分服務停機時怎麼回應由 provider 的 `failOpen: false` 與 `statusOnError` 決定，nightly 的 provider 是由 `deploy/istio/istiod-values.yaml` 複製的（`scripts/nightly/platform.ts`）。登入與 `/healthz` 是身分服務自己的例外路徑，它停機時這兩條也無法回應；不依賴它的例外（憑證換發、MQTT）照常 |
| 4 | 偽造、過期或受眾不符的內部身分被拒絕 | `crates/service-kit/tests/identity.rs` 的 `only_requests_with_a_valid_token_reach_the_handler`；`identity_adversarial.rs`（`algorithm_confusion_is_rejected`、`every_single_character_edit_of_a_valid_token_is_rejected`、`lifetime_and_leeway_boundaries`、`claims_are_typed_and_required`、`the_identity_header_is_read_once_and_nowhere_else`、`http_layer_accepts_only_one_well_formed_header` 等）；`services/identity/tests/token_claims.rs`（`a_token_signed_with_another_key_or_under_another_kid_is_rejected`、`audience_and_issuer_are_exact`、`expiry_issued_at_and_skew_boundaries`）；`services/assets/tests/tenant_created.rs`（沒有 token、受眾不符 401）。經入口：nightly 的偽造身分標頭檢查（第 9 列） | CI `rust`；nightly | 是 |
| 5 | 未設定租戶的交易讀不到任何列 | `crates/service-kit/tests/tenant_db.rs` 的 `without_a_tenant_no_rows_are_visible_or_writable`、`the_tenant_setting_does_not_leak_across_pooled_connections`、`the_service_role_cannot_bypass_row_level_security`；`crates/testkit` 的跨租戶套件（決策 29），身分與資產服務的 `tests/cross_tenant.rs` 都接上 | CI `rust`（需要容器，CI 裡沒有容器就失敗） | 是 |
| 6 | 同一份 chart 搭配兩份 values 都部署得起來 | nightly smoke 對 chart 目錄裡每個 `values-<shape>.yaml`（`discoverShapes`，少一個實例覆寫檔就失敗）做 `helm upgrade --install --wait --wait-for-jobs`、每個 workload 就緒、CloudNativePG Cluster 健康、Gateway `Programmed`、沒有壞掉的 pod、`helm uninstall` 乾淨。渲染：`chart.test.ts` 的 helm lint 與兩個 Kubernetes 版本的 kubeconform，兩種型態各一次 | nightly；CI `helm` | 是 |
| 7 | migration 以 Job 執行 | nightly smoke 的「the migration Job completed before any service started」與「the migration Job generated every password into its Secret」（Laysi/trustiot-v2#60）；`chart.test.ts` 的「migration Job (Laysi/trustiot-v2#60)」（每個服務 pod 以 init container 等 Job、Job 的權限）；`tools/migrate/tests/`（`fresh_database.rs` 的 `a_fresh_database_gets_every_migration_once`、`concurrent_runs.rs`、`passwords.rs`、`wait_for_job.rs`、`registry.rs` 的 `the_chart_binds_every_login_role_of_the_product` 等）。升級、rollback 與輪替：lab 的 `postgres.e2e.ts` | nightly；CI `rust`、`helm`；lab | 是 |
| 8 | main 上的 CI 全綠 | `ci.yml` 在 main 的每次 push 跑全部 job（必要檢查的清單見 [CI 與發佈](../architecture/ci-release.md)），nightly 每晚在 main 上跑 | main 的 ci.yml 與 nightly | 是，以合併 Laysi/trustiot-v2#77 之後 main 的那次執行為證據，不是一個測試檔 |
| 9 | 經入口的驗證冒煙測試（有效 cookie、沒有 cookie、偽造身分標頭）加進 nightly 並通過 | 真實服務（`scripts/nightly/api.ts`）：有效 cookie（管理者 403、成員 200 且是自己的租戶）、沒有 cookie 401；偽造身分標頭（`checkIngressEdges`，Laysi/trustiot-v2#77）：以這次部署自己的簽章金鑰簽出一個新租戶 viewer 的 token，先經 `kubectl port-forward` 直接送給資產服務確認它會被接受（200），再經入口送：沒有 cookie 時 401，帶管理者的 cookie 時 403（資產服務只看到管理者自己的 token）。替身（`ingress.e2e.ts`）：偽造的標頭與三種底線寫法到不了後端，例外路徑上也被剝除 | nightly（兩份 values） | 是 |

## 指標的兩項確認（Laysi/trustiot-v2#255 留下的問題）

- **閘道器送出的 `Host` 格式**：入口把客戶端送的 `Host` 原樣交給 cookie 檢查，帶埠號也一樣。`ingress.e2e.ts` 的「the cookie check gets the client's Host as sent」：身分服務替身把 ext_authz 請求的 `Host` 與路徑放進它回的身分標頭，測試比對 `Host` 不帶埠號、帶 listener 的埠與帶 8443 三種，都與送出的相同。真的身分服務以 `services/identity/src/audience.rs` 去掉埠號再找路由；nightly 的 API 檢查以成員的 cookie 帶 `Host: <app>:<listener 埠>` 與 `<app>:8443` 都拿到 200 與自己的租戶。所以閘道器的韌體不必處理 `Host`（[deploy/chart/README.md](../../deploy/chart/README.md)「`Host` 帶埠號」）。
- **路徑正規化下的行為**：cookie 檢查與後端看到的都是 mesh 正規化之後的路徑。`ingress.e2e.ts` 的「the cookie check and the backend get the normalized path」七種寫法（`//`、`/./`、`..`、`%2f`、`%2F`）都讓替身看到 `/api/whoami`；nightly 的 API 檢查以六種寫法呼叫資產服務都拿到 200，其中 `/api/identity/../assets/v1/tenant` 以身分服務自己的前綴開頭，代表身分服務以正規化後的路徑決定 token 的受眾（否則資產服務會以受眾不符回 401）。前端與閘道器的 MQTT WebSocket 在 MS0 還不存在，由實作它們的 issue（MS1 的 MQTT 轉接、MS2 的 SPA）沿用這兩條測試的做法確認。

## 新增檢查的成本

nightly 只多了兩件事，沒有多一個叢集、也沒有新的映像建置：

- API 檢查每份 values 多了 11 個請求（其中 1 個直接送資產服務），加一條到資產服務的 `kubectl port-forward`；入口 e2e 每份 values 多了 8 個測試，共 10 個請求；
- 服務間授權 e2e 每晚一次（SaaS 型態），用的是入口 e2e 已經建好並匯入的身分服務替身映像。

實際多花的時間記在 [Laysi/trustiot-v2#77 的測試報告](../test-reports/2026-10-15-77-ms0-acceptance.md)。
