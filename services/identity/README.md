# identity（身分服務）

使用者、租戶成員與 server-side session 的擁有者，也是入口 gateway 的 cookie 檢查（ext_authz）：把 session cookie 換成一個短效、簽過章的內部身分 token，交給請求要去的服務（決策 13、27；[部署與可搬移性](../../docs/architecture/deployment.md)「身分與租戶」）。

目前是 MS0 的骨架（Laysi/trustiot-v2#61）：

- 有自己的 schema、角色與 RLS，session 存在伺服器端。
- 有 ext_authz 端點與內部身分簽發。
- 有 token 交換：服務代表使用者呼叫另一個服務時，拿自己收到的 token 換一張受眾是對方的 token（見 [token 交換](#token-交換代表使用者呼叫)，Laysi/trustiot-v2#106，2026-10-21）。
- **email 驗證碼登入、登出與登出所有裝置**（Laysi/trustiot-v2#103，2026-10-21，見[登入](#登入)）：這是建立 session 的唯一方式，沒有密碼登入、公開註冊，任何版本都沒有假登入；驗證碼經 chart `smtp.*` 的 relay 寄出。使用者與 email 目前由擁有者角色寫入（[測試工具怎麼建立 session](#測試工具怎麼建立-session)），沒有管理使用者的 API。
- 一個對外的 HTTP API：平台管理者建立租戶，同一個交易寫出 `identity.tenant_created` 事件（見[租戶](#租戶)，Laysi/trustiot-v2#65，2026-10-14）；平台管理者設定既有使用者在租戶裡的角色，同一個交易寫出 `identity.member_role_changed` 事件（見[成員](#成員)，Laysi/trustiot-v2#583，2026-10-23）；平台管理者停權（記錄原因）與恢復租戶，同一個交易寫出帶狀態快照與版本的 `identity.tenant_state_changed` 事件（見[租戶狀態](#租戶狀態)，Laysi/trustiot-v2#776，2026-10-30）；平台管理者設定租戶的方案與 Enterprise 的覆寫值，同一個交易寫出帶權益快照與版本的 `identity.tenant_entitlements_changed` 事件，租戶成員讀自己租戶的權益（見[方案權益](#方案權益)，Laysi/trustiot-v2#111，2026-11-01）；規格在 `api/openapi.json`（`tools/openapi`）。
- 平台管理者讀取、修改與重設 AI 閘道的模型角色：身分服務檢查權限之後轉給 AI 閘道的管理 API，每次變更寫出全平台事件 `identity.ai_model_role_changed`（見[AI 模型角色](#ai-模型角色)，Laysi/trustiot-v2#608，2026-10-29）。
- 已登入的使用者讀取目前的 session（使用者、目前的租戶與角色、所屬的租戶）、把 session 切換到自己所屬的另一個租戶，並讀取與修改自己的偏好（介面語言、AI 回答語言、時區、主題）（Laysi/trustiot-v2#497，2026-10-23，見[目前的 session 與切換租戶](#目前的-session-與切換租戶)與[偏好設定](#偏好設定)）。
- 映像見下方[映像](#映像)；chart 的 `services.identity.enabled` 預設仍是 `false`。nightly 的 smoke 從 Laysi/trustiot-v2#65 起啟用它與資產服務，經入口建立租戶；其餘啟用工作在 Laysi/trustiot-v2#77。

## 與入口的約定（ext_authz）

Istio 的 ext_authz provider（`deploy/istio/istiod-values.yaml`）對每個需要 cookie 的請求，以原始的方法、Host、`Cookie`、`Origin` 與 `Sec-Fetch-Site` 呼叫 `/internal/ext-authz<原始路徑>`。`/internal` 不會被入口路由，用戶端到不了這個端點。服務只讀方法、`Cookie`、`Origin`、`Sec-Fetch-Site`、`Host` 與路徑；請求帶的其他標頭（包含任何寫法的 `x-internal-identity`）都不影響結果。

| 回應 | 情況 | 入口 |
| --- | --- | --- |
| 403 `permission_denied`，訊息 key `security.cross_site_request` | 跨站請求（見下方[跨站請求](#跨站請求csrf)），在讀 cookie、查 session 之前判斷，所以沒有 session 時也是這個回應 | 原樣交給用戶端 |
| 200，剛好一個 `x-internal-identity`，`cache-control: no-store` | session 有效，原始請求的主機與路徑對得到一條 cookie 路由；session 目前的租戶被停權或已刪除時，只有路由的服務是身分服務自己（受眾 `identity`）才是這個回應，token 帶 `tenant_suspended`（見[停權時的每次請求檢查](#停權時的每次請求檢查)） | 以這個值覆寫用戶端帶來的同名標頭，轉給後端 |
| 401 `unauthenticated` | 沒有 session cookie、有兩個以上、格式不對；session 不存在、過期、已撤銷或使用者已刪除 | 原樣交給用戶端 |
| 403 `permission_denied` | session 有效，但路由表對不到這個請求 | 原樣交給用戶端 |
| 403 `tenant_suspended` | session 有效、對得到路由，但 session 目前的租戶被停權或已刪除，而路由的服務不是身分服務；不簽 token | 原樣交給用戶端 |
| 503 `unavailable` | 資料庫出錯、0.4 秒內查不完，或同時在查的請求太多（見[連線池](#連線池)） | 把 5xx 當成錯誤，回自己的 503（`statusOnError`） |

- 200 只能由剛簽好的 token 組成（程式裡的 `Decision::Allow` 沒有 token 就建不出來）。原因：ext_authz 回 200 卻沒帶 `x-internal-identity` 時，入口會把用戶端自己帶的同名標頭轉給後端（[2026-10-10 PoC](../../docs/feasibility/2026-10-10-edge-poc.md)）。
- 拒絕的回應一律不帶身分標頭，body 是 `contract::ErrorBody`，也帶 `cache-control: no-store`。
- 401 與 403 以 INFO 記一行 `denied the ext_authz check`，`reason` 是 `cross_site_request`（另帶 `header`：判斷依據是 `sec-fetch-site` 還是 `origin`）、`no_session_cookie`、`repeated_session_cookie`、`malformed_session_cookie`、`no_active_session`、`no_route` 或 `tenant_suspended`；日誌不寫 cookie、session 雜湊、token 或 `Origin` 的值。
- 每個檢查最多等 0.4 秒（`CHECK_DEADLINE`），比入口的 0.5 秒短，所以入口一定拿得到服務自己的回答；到了就回 503（fail closed）。查詢本身在背景跑完，開到一半的資料庫連線留給下一個檢查（見[連線池](#連線池)）。

### 跨站請求（CSRF）

SPA 與 API 同源（決策 2），SPA 自己送的請求一律是同源；別的網站可以讓瀏覽器帶著 session cookie 送請求，所以 cookie 檢查在查 session 之前先擋下跨站請求（`src/csrf.rs`，Laysi/trustiot-v2#123，2026-10-21）：

| 請求帶的標頭 | 放行的條件 |
| --- | --- |
| `Sec-Fetch-Site`（目前的瀏覽器都會帶） | 剛好一個值，是 `same-origin`；GET 與 HEAD 另外接受 `none`（使用者自己輸入網址或開書籤）。`same-site`（同一個網站的其他主機，例如閘道器的主機名稱）也算跨站 |
| 沒有 `Sec-Fetch-Site`、有 `Origin`，方法不是 GET 或 HEAD | 剛好一個值，等於 `https://` 加上 `Host` 的主機與埠號（沒寫埠號時是 443）；`http://` 只接受 `localhost` 與 `127.0.0.1`（本機開發，預設埠 80）。`Origin: null`、帶路徑或帳號、埠號不合法的值都算跨站 |
| 兩個都沒有 | 一律放行：不是瀏覽器，或太舊的瀏覽器 |

- 入口要把 `Origin` 與 `Sec-Fetch-Site` 交給身分服務（`includeRequestHeadersInCheck`）。istiod 還是舊設定時，這兩個標頭不會送來，檢查一律放行而且沒有任何紀錄；升級步驟見 [deploy/istio/README.md](../../deploy/istio/README.md)。
- 這是第一道防線，[租戶](#租戶)的「以 cookie 認證的寫入一律要求 JSON 的 `Content-Type`」與 cookie 的 `SameSite` 照樣適用。
- 不需要 cookie 的例外路徑（例如 Laysi/trustiot-v2#103 的登入）不經 ext_authz，這個檢查管不到：那些處理常式要自己呼叫 `csrf::cross_site`。登入已經這樣做（見[登入](#登入)的 CSRF 說明）。

### token

JWS compact serialization，格式與驗證規則在 [`crates/service-kit` 的「內部身分」](../../crates/service-kit/README.md)：

- 保護標頭剛好是 `{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"<kid>"}`，Ed25519 簽章。
- payload 是 `contract::InternalClaims`：`iss` 一律 `identity`；`sub` 是使用者；`aud` 是請求要去的服務；`iat` 是簽發時間，`exp = iat + 60`；`tenant` 是 session 目前的租戶與使用者在那裡的角色（沒有選租戶、或已不是成員時省略）；`platform_admin` 是平台管理者（A2）；`sid` 是 session 的 ID（Laysi/trustiot-v2#551 起），只有身分服務用它把 [run 委派](#agent-執行期間的委派)綁在 session 上，其他服務忽略它，換出的 token 不帶；`tenant_suspended` 只在 `tenant` 的租戶被停權或已刪除時出現（值是 `true`，Laysi/trustiot-v2#777），只有給身分服務自己的 token 會帶（見[停權時的每次請求檢查](#停權時的每次請求檢查)）。session ID 不是 credential：cookie 裡的是另一個隨機值，資料庫只存它的雜湊。
- 效期 60 秒（`TOKEN_LIFETIME_SECS`）：token 在效期內是受眾服務的 bearer credential，所以要短；輪替也要等驗證端接受的最長效期（300 秒）。

角色與租戶的狀態每個請求都從 `memberships` 與 `tenants` 重新讀（同一個查詢、同一個租戶交易）：改角色、移除成員、停權或恢復，下一個請求就生效，不必等 session 過期。

### 受眾（`aud`）

token 只給請求要去的服務。chart 把 `ingress.routes` 換成實際主機名稱，以 `TRUSTIOT_INGRESS_ROUTES` 交給身分服務（JSON 陣列 `[{host, path, match, service}]`，最多 512 條）；身分服務以 Gateway API 的規則比對原始請求：

- 主機不分大小寫，忽略埠號與結尾的 `.`。
- 先找 `Exact`，再找最長、以路徑段為界的 `PathPrefix`（`/api` 對得到 `/api` 與 `/api/x`，對不到 `/apix`）；`/` 對得到所有路徑。
- 對不到就回 403。路由表與入口不一致時只會多拒絕，不會放寬權限，因為 token 只有它的受眾接受。
- `ai-gateway` 不驗證內部身分（決策 28），chart 不允許它有 cookie 路由，設定裡出現它，服務拒絕啟動。

## token 交換（代表使用者呼叫）

決策 27（2026-10-21，Laysi/trustiot-v2#106）：服務收到的 token 只對自己有效，要以同一個使用者呼叫另一個服務時，向這裡換一張受眾是對方的 token。契約是 `contract::api::identity::ExchangeToken`，呼叫端用 `crates/service-kit` 的 `OnBehalf`；回應表在 [contract README](../../crates/contract/README.md) 的「token 交換」。程式在 `src/exchange.rs`。

token 交換在自己的埠（`exchange.listen_addr`，預設 `0.0.0.0:8082`，chart 的埠名 `token-exchange`），API 埠沒有這個路徑。chart 給這個埠自己的 Service、AuthorizationPolicy 與 NetworkPolicy（`<release>-identity-exchange`，呼叫端連 `http://<release>-identity-exchange:8082`）：mesh 讓代表使用者呼叫的服務只連得到這個埠，入口 gateway 只連得到 API 埠，所以它們碰不到 API 埠上的 ext_authz 檢查（[chart README](../../deploy/chart/README.md) 的「服務間授權（mesh）」）。這個埠由 `crates/service-kit` 的 `Context::serve_port` 提供，handler 逾時、panic 處理、請求日誌與收尾都和 API 埠相同。

`POST /internal/token-exchange`，`x-internal-identity` 帶呼叫端自己收到的 token，本文 `{"audience": "<服務>"}`（最多 1 KiB）。依序：

1. 以 `ExchangeVerifier` 驗證 token：與每個服務的驗證端相同（簽章、`kid`、`typ`、發行者、效期與時間），只是受眾可以是任何服務。沒有公鑰 503，其他失敗 401。先驗 token 再讀本文，沒有有效 token 的請求看不出本文對不對。
2. 本文不是這個請求、服務名稱不認得（`ai-gateway` 不在 `ServiceName`）400。
3. token 帶 `act`，也就是本身是換來的：403 `permission_denied`。只換一跳。
   - token 帶 `tenant_suspended`：403 `tenant_suspended`（Laysi/trustiot-v2#777）。這種 token 只有身分服務自己拿得到，它不代表使用者呼叫別的服務；這一步是第二道防線。
4. 「token 的受眾 → `audience`」不在呼叫清單上 403 `permission_denied`。ztunnel 不把呼叫端的身分交給應用程式，所以呼叫端以 token 的受眾認定。以某個服務為受眾的 token 不只那個服務拿得到，清單上可以呼叫它的服務也換得到一張，所以清單只套用在入口 gateway 的請求簽出、沒有 `act` 的 token 上：A 可以呼叫 B、B 可以呼叫 C 時，A 拿換來的 B token 換不到 C 的（第 3 步）。代價是 B 以換來的 token 被呼叫時，不能再代表同一個使用者呼叫 C；多跳的鏈要先能驗證呼叫端的工作負載，記在 Laysi/trustiot-v2#509。
5. 簽一張新 token：`sub`、`tenant`、`platform_admin` 與原 token 相同，`aud` 是 `audience`，`act` 是 `{"sub": "<原 token 的受眾>"}`（RFC 8693 的 actor），`iat` 是現在，`exp` 是原 token 的 `exp` 與現在加 60 秒取早者；剩不到 1 秒時不簽，回 401。沒有可以簽章的金鑰 503。

成功回 200 `{"token": "<token>"}`，所有回應都帶 `cache-control: no-store`。換 token 不查資料庫，所以停權之前簽出的 token 在剩下的效期（最多 60 秒）內仍換得到。日誌：換到記 DEBUG（呼叫端與受眾）；401、503 記 WARN，欄位是原因（驗證端的 `reason`、`expires_too_soon`、`no_keys`、`no_signing_key`）；403 記 WARN，`reason` 是 `already_exchanged`（帶呼叫端、`act` 的服務與受眾）或 `not_listed`（帶呼叫端與受眾，通常是 `mesh.calls` 少了這條呼叫）；`tenant_suspended` 的 403 記 INFO（帶呼叫端與受眾）；400 記 INFO。token、使用者與本文都不進日誌。

還沒處理、記在 Laysi/trustiot-v2#509 的：多跳呼叫要的呼叫端工作負載驗證；入口 gateway 把 session cookie 轉給身分服務以外的後端；`ExchangeVerifier` 是 `crates/service-kit` 的公開型別；token 交換沒有並行上限，每次呼叫都換一張新的 token、不重用；金鑰撤銷還在傳播時，不拒絕以已撤銷世代的金鑰簽的 token（`TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION` 只管簽章）。

呼叫清單是 `TRUSTIOT_ON_BEHALF_CALLS`（JSON `{"<呼叫端>": ["<被呼叫端>", ...]}`），chart 從 `mesh.calls` 推得：兩個啟用的服務之間的呼叫，`ai-gateway` 兩個方向都不在（[chart README](../../deploy/chart/README.md) 的「服務間授權（mesh）」與「呼叫矩陣」）。沒設時是空的，什麼都不換；不是 JSON、名稱不認得或含 `ai-gateway` 時拒絕啟動，訊息只寫變數名稱與原因，不引用設定值。啟動時 INFO `serving the token exchange` 寫出清單的呼叫數。

背景工作沒有使用者的請求，不換 token（[service-kit README](../../crates/service-kit/README.md) 的「背景工作沒有使用者身分」）。

## agent 執行期間的委派

Laysi/trustiot-v2#551（2026-10-23 決定，決策 27 的補充）：AI 助理的主 agent 回答一個問題時，可能跑上好幾分鐘，每次工具呼叫都要以使用者的身分呼叫其他服務，但請求帶來的 token 只活 60 秒，token 交換換出的 token 也不會比它晚到期。這時持有者（目前只有 `assistant`，`delegation::HOLDERS`，加一個要改程式並審查，不是設定）在請求的 token 還有效時建立一個 run 委派，之後每次呼叫前用它換一張 60 秒的 token。契約是 `contract::api::identity::CreateRunDelegation` 與 `ExchangeRunDelegation`，呼叫端用 `crates/service-kit` 的 `RunDelegations`；程式在 `src/delegation.rs`。兩個路徑都在 token 交換的埠（8082），mesh 與 NetworkPolicy 和 token 交換相同。

**建立**：`POST /internal/run-delegations`，`x-internal-identity` 帶入口 gateway 的請求簽給持有者的 token，本文 `{"run_id", "key", "lifetime_secs"}`（最多 1 KiB）。`key` 是持有者為這次 run 產生的 Ed25519 公鑰（base64url），私鑰只留在持有者的行程裡。依序：token 驗不過 401、沒有公鑰 503；本文不對、效期不在 1–900 秒、`key` 不是可用的 Ed25519 公鑰（含 small-order 點）400；token 帶 `act` 403（只換一跳）；token 的受眾不是持有者 403；token 沒有 `sid`（#551 之前簽的）401；token 帶 `tenant_suspended` 403 `tenant_suspended`（不查資料庫）；session 已撤銷、過期或使用者已刪除 401，使用者已不是 token 租戶的成員 403，租戶現在被停權或已刪除 403 `tenant_suspended`（Laysi/trustiot-v2#777），資料庫失敗 503。成功回 200 `{"delegation", "expires_at"}`。

**換 token**：`POST /internal/run-delegations/token`，`x-internal-identity` 帶委派，本文 `{"audience", "iat", "proof"}`，`proof` 是 run 私鑰對 `run_proof_message(委派, audience, iat)` 的簽章，`iat` 要在現在的 ±30 秒內（`RUN_PROOF_WINDOW_SECS`）。依序：委派的 MAC 驗不過 401、還沒讀到簽章金鑰檔 503；過期、`iat` 在未來超過 5 秒、或效期超過 900 秒（即使 MAC 正確）401；本文不對 400；proof 過期或驗不過 401；持有者已不在 `HOLDERS` 403；呼叫清單沒有「持有者 → `audience`」403；**每次都重查**：session 不再有效 401，使用者已不是委派租戶的成員 403，委派的租戶被停權或已刪除 403 `tenant_suspended`（Laysi/trustiot-v2#777；成員資格與租戶狀態是同一個查詢），資料庫失敗 503。成功回 200 `{"token"}`：和 token 交換換出的形狀相同（`sub`、委派的租戶與**現在**的角色、現在的 `platform_admin`、`act` 是持有者、沒有 `sid`），`iat` 是現在，`exp` 是現在加 60 秒與委派到期取早者。被呼叫端不用改任何東西。

**格式**：委派是 `trd1.<payload>.<mac>`，payload 是 `DelegationClaims`（`sub`、`sid`、`tenant`、`holder`、`run`、`key`、`iat`、`exp`）的 JSON，MAC 是 HMAC-SHA-256，金鑰是當下簽章的那把 Ed25519 私鑰對標籤 `trustiot-run-delegation-key:v1` 的 HMAC-SHA-256（和驗證碼的金鑰同樣的導出方式、不同的標籤，`src/issuer.rs`）。只有身分服務有簽章金鑰，所以只有它做得出、驗得了委派；它不是 JWS，任何服務的驗證端與 token 交換都不接受它。驗證時接受簽章金鑰檔裡每一把沒被撤銷的金鑰導出的 MAC，所以換簽章金鑰不影響進行中的 run；調 `revokeBeforeGeneration` 撤銷一代時，以那代簽的委派立即失效。委派不存在資料庫，換 token 只讀 session 與成員資格。

**上限**：

- 委派最長 900 秒（`RUN_DELEGATION_MAX_LIFETIME_SECS`，15 分鐘），持有者可以要更短的。到期後換不到 token，run 要以使用者的新請求重新開始。900 秒不超過舊金鑰從換金鑰到被移除的最短時間（`keyPropagationSeconds` 預設 600 加 305 秒）；`keyPropagationSeconds` 設得比 595 秒短時，換金鑰後的下一次重啟可能讓剛開始的 run 提早結束（換 token 回 401），不會延長任何委派。
- 換出的 token 一張 60 秒，每次呼叫換一張，不重用。
- 只換一跳：換出的 token 帶 `act`，兩個路徑與 token 交換都不接受帶 `act` 的 token。
- 只到呼叫清單（`mesh.calls`）上持有者可以呼叫的服務。

**撤銷**：登出、登出所有裝置、session 被撤銷、session 過期或使用者被刪除之後，下一次換 token 就回 401；使用者被移出委派的租戶，下一次回 403；租戶被停權，下一次回 403 `tenant_suspended`，恢復之後同一個委派（還沒到期時）又換得到；角色改變時下一張 token 帶新角色、平台管理者旗標也重讀。已經換出的 token 在剩下的效期（最多 60 秒）內仍有效，和入口簽的 token 相同。

**威脅模型**：

- 委派外洩（日誌、存下來的 run、錯誤報告）：沒有 run 私鑰換不到 token。持有者以外的服務拿到委派也一樣（每個呼叫端都連得到這個埠，ztunnel 不把呼叫端身分交給應用程式，所以呼叫端以 proof 認定）。
- proof 被截取：proof 綁定委派、受眾與 `iat`，只能在 ±30 秒內、對同一個受眾重播。這需要同時拿到委派與 mesh mTLS 裡的流量，代價和直接截取換出的 60 秒 token 相同。
- 竄改委派（換掉公鑰、持有者、租戶、延長期限）：MAC 驗不過；未知欄位一律拒絕。
- 持有者行程被控制：可以在進行中的 run 期間代表那些使用者呼叫清單上的服務，和它拿著請求 token 時能做的相同，只是時間多到委派的上限；撤銷 session 或把使用者移出租戶會在下一次呼叫生效。不擋的部分記在 Laysi/trustiot-v2#509（呼叫端工作負載驗證）。

**連線池**：查 session 與成員資格用自己的連線池（`delegation::POOL_CONNECTIONS` 4 條，取得連線最多等 5 秒，每次查詢最多 5 秒，同時最多 64 個在等，多的立即回 503），和 ext_authz 的分開：大量換 token 不會讓每個請求都要的 cookie 檢查等連線。成員資格是租戶資料，以委派裡的租戶開 RLS 交易讀（`BackgroundTenantDb`，只在這個查詢裡用）；那個租戶來自身分服務自己簽的委派，建立時查過成員資格，不是請求給的。

**日誌**：建立記 INFO（持有者、run、效期）；換到記 DEBUG（持有者、受眾、run）；session 不再有效、委派到期、使用者不再是成員、租戶被停權（`reason` 是 `tenant_suspended`）記 INFO（run 照預期結束）；其他 401、403 記 WARN（原因如 `invalid_delegation`、`invalid_proof`、`stale_proof`、`not_listed`）；503 記 WARN。委派、proof、公鑰、token 與使用者都不進日誌。

## session

- cookie 名稱 `__Host-trustiot-session`（`__Host-` 前綴：瀏覽器只接受 `Secure`、`Path=/`、沒有 `Domain` 的這個 cookie，子網域種不進來）。值是 32 bytes 的隨機數，base64url 不補 `=`，43 個字元；只接受唯一的標準寫法。
- 資料庫只存值的 SHA-256（`sessions.token_hash`）：資料表外洩也拿不到可用的 cookie。cookie 缺少、重複或格式不對時直接回 401，不查資料庫。
- 有效的 session：`token_hash` 相符、`revoked_at` 是 NULL、`expires_at > now()`（以資料庫的時鐘判斷），而且使用者還在。
- 期限（`src/session.rs`）：登入時 `expires_at` 是 12 小時後（`SESSION_IDLE_LIFETIME`，checklist A1）；ext_authz 檢查到一個有效 session、而延長能多出至少 10 分鐘（`SESSION_EXTEND_STEP`）時，在背景把它延到 `min(now() + 12 小時, created_at + 7 天)`，所以使用中的 session 不會過期，但最長 7 天（`SESSION_ABSOLUTE_LIFETIME`）一定要重新登入。延長不擋檢查的回應，同時最多 8 個（`EXTENSIONS_IN_FLIGHT`），只用連線池當下閒置的連線（見[連線池](#連線池)），沒有閒置的連線就跳過；失敗只記 WARN 與 SQLSTATE，跳過或失敗都由下一個請求再試。
- cookie 是 `__Host-trustiot-session=<值>; Max-Age=604800; Path=/; Secure; HttpOnly; SameSite=Lax`：`Max-Age` 等於最長期限，實際期限由資料庫決定；`HttpOnly` 讓頁面的 JavaScript 讀不到；`SameSite=Lax` 讓別的網站發出的子請求與 POST 不帶它（從別的網站點連結進來的頂層 GET 仍帶，使用者才不必重新登入）。
- 資料庫的 trigger `identity.sessions_guard`（`migrations/postgres/0004_login.sql`）擋下兩件欄位權限表達不了的事：已撤銷的 session 不能復原或改撤銷時間，任何 session 都不能超過建立後 7 天。它擋的是服務的 bug，不是被入侵的服務：能建立 session 的角色本來就能建新的。

## 登入

使用者以寄到 email 的驗證碼登入（Laysi/trustiot-v2#103；決策 13；checklist A1；`src/login.rs`）。這是建立 session 的唯一方式：沒有密碼登入，沒有公開註冊（自助註冊在 MS6），任何版本都沒有假登入（見[沒有假登入](#沒有假登入)）。只有 `users.email` 有值的使用者能登入；地址以 ASCII 小寫、去掉前後空白比對與儲存，屬於個人資料，不寫日誌。

兩步都是 `POST /auth/login`：這是入口唯一不需要 cookie、轉給身分服務的路徑（決策 27，chart 的 `ingress.publicPaths.login`，必須維持 `/auth/login`），body 是 JSON，以 `step` 區分：

1. `{"step": "send_code", "email": "..."}`：立刻回 202 `{"code_length": 8, "expires_in_secs": 600, "resend_after_secs": 60}`，不論有沒有使用者用這個地址；查使用者、產生與寄出驗證碼在背景的 worker 做（佇列 64 個，同時寄 4 封，每封 60 秒內），所以回應的內容與時間都看不出帳號是否存在。`verify_code` 的兩種失敗（這個地址沒有可用的驗證碼、驗證碼不對）在資料庫做的事不同：驗證碼不對要記一次嘗試並提交，沒有可用的驗證碼什麼都不寫；而只有有帳號的地址才拿得到驗證碼，所以先替一個地址要一封驗證碼、再比較兩種失敗的回應時間，就能看出這個地址有沒有帳號。因此兩種 401 都至少花 0.5 秒（`REFUSAL_FLOOR`，從查驗證碼開始算，交易結束後才等，不佔連線），遠高於資料庫正常時兩者的耗時；資料庫慢到超過這個時間時才可能再分得出來。
2. `{"step": "verify_code", "email": "...", "code": "12345678", "redirect_to": "/devices?page=2"}`：驗證碼對了，就在同一個交易建立 session，回 200 `{"redirect_to": ...}` 與 `Set-Cookie`（屬性見 [session](#session)）。`redirect_to` 可省略（預設 `/`）。

| 請求 | 回應 |
| --- | --- |
| 跨站請求（判斷規則與 ext_authz 相同，見[跨站請求](#跨站請求csrf)），在其他所有檢查之前判斷 | 403 `permission_denied`，訊息 key `security.cross_site_request`；什麼都沒有改變 |
| 有 query string、`Content-Type` 不是 `application/json`、body 超過 4096 bytes，或 body 不是指名這兩步之一的 JSON 物件（多了任何欄位，例如 `password`，或是 JSON 陣列，也算） | 400 `invalid_request` |
| `email` 不是地址、`code` 不是 8 位數字、`redirect_to` 不是本站的路徑 | 422 `validation_failed`，欄位 `/email`（`login.email_invalid`）、`/code`（`login.code_invalid`）、`/redirect_to`（`login.redirect_invalid`）；不算一次嘗試 |
| `send_code` | 202 |
| `send_code`，但沒有設定 SMTP relay、relay 的設定不能用（見[寄信（SMTP）](#寄信smtp)），或佇列滿了 | 503 `unavailable` |
| `verify_code`，這個地址沒有可用的驗證碼，或驗證碼不對 | 401 `unauthenticated` |
| `verify_code`，驗證碼對了 | 200 與 `Set-Cookie`，`Cache-Control: no-store` |
| 資料庫出錯，或 `verify_code` 時還沒讀到簽章金鑰 | 503 `unavailable`，什麼都沒有改變 |

驗證碼的規則（[密鑰與權限](../../docs/architecture/secrets.md)「一次性 token 與驗證碼」）：

- **數值**：8 位數字，以作業系統的 CSPRNG 產生（拒絕取樣，每個數字機率相同）；**10 分鐘有效**（`CODE_LIFETIME`）；**每個驗證碼最多 5 次嘗試，含正確的那一次**（`MAX_ATTEMPTS`）：第 5 次嘗試之後，不論對錯，那個驗證碼都作廢，要重新寄。同一個使用者同時最多一個可用的驗證碼：寄新的會作廢舊的，部分唯一索引 `login_codes_one_open_per_user` 讓同時寄出的兩個只有一個成立。
- **寄送限制**：同一個地址 60 秒內最多一封（`RESEND_INTERVAL`）、每小時 5 封、每天 20 封；超過時一樣回 202，但不寄、不存。同一個使用者的發碼在交易內以 advisory lock 排隊（服務角色不能鎖 `users` 的列），所以同時送出的請求也是一個一個對照限制，只寄一封。所以猜一個地址每天最多 100 次，猜中機率 10⁻⁶。這是每個地址的限制，擋不住對很多地址各試幾次；以來源 IP 為單位的速率限制在入口（Laysi/trustiot-v2#113）。反過來，任何人都能用掉一個地址的寄送額度、猜錯用掉它的嘗試次數，讓那個使用者一段時間內登入不了；這種鎖定的防護（寄送與猜測分開計算等）在 Laysi/trustiot-v2#508。
- **只存帶金鑰的雜湊**：資料庫只存 HMAC-SHA-256（用途字串、地址、驗證碼），金鑰由身分服務的簽章金鑰導出（`src/issuer.rs` 的 `Issuer::code_key`：以私鑰對固定標籤做 HMAC，不能反推私鑰），只在身分服務掛載的 Secret 裡、不在資料庫。所以驗證碼只對寄到的那個地址有效，而只讀得到資料庫的人（備份、SQL 注入）無法離線窮舉 10⁸ 個組合；沒有 HMAC 時，他可以先替任何地址要一個驗證碼，再從資料表算出來登入。新的驗證碼用這一刻簽章的金鑰（每個副本選的相同），驗證時試簽章金鑰檔裡每一把未撤銷的金鑰，所以金鑰輪替前寄出的驗證碼在舊金鑰移除前（新金鑰開始簽章後至少 `keyPropagationSeconds` 加 305 秒）仍然有效，之後就要重新寄；撤銷一代金鑰也作廢它的驗證碼。還沒讀到簽章金鑰時不寄也不驗（`verify_code` 回 503，不算一次嘗試）。超過 24 小時的列每小時刪除一次。
- **原子消耗**：一個 `UPDATE ... RETURNING` 同時把嘗試次數加一、在雜湊相符時寫入 `consumed_at`，session 在同一個交易建立；同時送出同一個驗證碼只有一個成功。
- **不放網址、不進日誌**：驗證碼只出現在 POST body 與信件內文。帶 query string 的請求不讀 body 就回 400；信件不含連結，主旨不含驗證碼；日誌只記使用者 ID，不記地址、驗證碼與 cookie，在 TRACE 等級也一樣。
- **寄不出去時**：那個驗證碼立即作廢，記 ERROR（只有使用者 ID 與 SMTP 的步驟與回應碼）。

`redirect_to` 只接受本站的路徑（`src/login.rs` 的 `redirect_target`）：`/` 開頭、第二個字元不是 `/`、只有可見的 ASCII 字元而且沒有 `\`、最長 2048 bytes。其他一律 422，例如 `https://evil.example`（含本站主機的完整 URL 也拒絕）、`//evil.example`（另一個主機）、`/\evil.example`（瀏覽器把 `\` 當 `/`）、`/<tab>/evil.example`（瀏覽器刪掉 tab 後變成 `//`）、`javascript:`、非 ASCII。

回應的 `redirect_to` 是本站的路徑，前端要以前端路由（client-side routing）導過去；不要交給先解碼再轉址的伺服器：`/%2F%2Fevil.example` 通過上面的檢查，解碼後卻是 `//evil.example`。

登入成功時：

- 瀏覽器原本帶著的 session（如果有）一併撤銷，新的 session 一律用新產生的 cookie 值。
- 新 session 的目前租戶沿用使用者最近一個有租戶的 session（之後由使用者切換，見[目前的 session 與切換租戶](#目前的-session-與切換租戶)）；角色仍是每個請求從 `memberships` 讀，已不是成員就不帶租戶。

**CSRF**：

- 登入是公開路徑，不經 ext_authz，所以處理常式第一件事自己呼叫 `csrf::cross_site`（[跨站請求](#跨站請求csrf)的規則，2026-10-22）：兩步都一樣，在讀 body、查資料庫、計算嘗試次數與寄送限制、產生或寄出驗證碼之前判斷；跨站請求回 403 `permission_denied`，訊息 key `security.cross_site_request`，與 ext_authz 的回應相同，也帶 `Cache-Control: no-store`，以 INFO 記一行 `refused a login request`（`reason` 是 `cross_site_request`，`header` 是 `sec-fetch-site` 或 `origin`），不記地址、驗證碼、cookie 與 `Origin` 的值。沒有這個檢查時，別的網站可以讓瀏覽器替某個地址要驗證碼、用掉它的寄送額度與嘗試次數，或讓瀏覽器登入攻擊者的帳號（login CSRF）。
- 入口原樣轉送 `Host`、`Origin` 與 `Sec-Fetch-Site`：登入的路由（chart 的 `ingress.publicPaths.login`）以 `hosts.app` 比對主機，只改 `x-forwarded-proto` 與移除 `x-internal-identity`，不改寫 `Host`，所以身分服務拿到的是對外的主機名稱（可能帶 `:443`，比對時與沒寫埠號相同）；cloudflared 也照原樣轉送 `Host`，否則路由本身就對不到。本機以 Vite 轉送時（`changeOrigin: false`）`Host` 是 Vite 的位址，瀏覽器送的是 `Sec-Fetch-Site: same-origin`。
- 登出的兩個端點經 ext_authz，跨站的登出在那裡就被擋下，不再重複檢查。
- 此外登入要求 `Content-Type: application/json`，登出是 `DELETE`：別的 origin 的網頁要送這兩種請求都得先經 CORS preflight，而入口與服務都不允許 preflight（見[租戶](#租戶)的 CSRF 說明）；cookie 另設 `SameSite=Lax`。

### 登出

兩個端點都經過入口的 cookie 檢查（受眾 `identity`），入口把 `Cookie` 標頭原樣轉給身分服務（`src/logout.rs`）：

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| `DELETE /api/identity/v1/sessions/current` 沒有可用的 session cookie | 401 `unauthenticated` |
| `DELETE /api/identity/v1/sessions/current` | 204，撤銷這個 cookie 的 session（必須屬於 token 的使用者），清除 cookie |
| `DELETE /api/identity/v1/sessions`（登出所有裝置），token 是別的服務換來的（帶 `act`） | 403 `permission_denied`，什麼都沒有撤銷，不清除 cookie：只接受使用者本人的請求（Laysi/trustiot-v2#760，2026-10-30 決定），否則任何能代使用者呼叫的服務都能替使用者登出所有裝置；INFO `refused to log out everywhere`，帶 `user_id`、`reason` 與呼叫的服務 |
| `DELETE /api/identity/v1/sessions`（登出所有裝置） | 204，撤銷這個使用者所有還沒結束的 session（包括這個），清除 cookie |
| 資料庫出錯 | 503 `unavailable`，什麼都沒有撤銷 |

ext_authz 每個請求都查資料庫，所以被撤銷的 session 下一個請求就回 401，在每個裝置上都一樣。已經簽出的內部身分 token 在它的 60 秒效期內仍被受眾服務接受（見 [token](#token)）。

### 沒有假登入

不做假登入（2026-10-21 決定，寫在決策 13）：本機開發以 `bun scripts/dev.ts seed` 建立 session（[測試工具怎麼建立 session](#測試工具怎麼建立-session)），不需要它；它是一旦在正式環境被打開就能任意登入的開關。驗收條件「正式版本不含假登入、本機版本沒開設定時 cookie 也開不了」以「不存在」滿足，由兩個測試證明：

- 編譯期：這個 crate 沒有 Cargo feature（`cargo metadata` 列出的 feature 是空的，所以也沒有 optional dependency 帶來的 feature；`Cargo.toml` 沒有 `[features]`、`optional = true` 與 `build =`），沒有 build script；原始碼去掉空白後，除了單元測試的 `cfg(test)` 以外沒有任何 `cfg`、`cfg_attr`、`cfg!`、`debug_assertions`、`option_env!` 或 `include!`，`env!` 只讀套件版本，所以正式版本與本機版本的登入程式相同（`tests/login.rs` 的 `no_build_switch_can_add_a_fake_login`）。
- 路由：本機建置的 binary 只有 OpenAPI 列出的十一個端點（登入、兩種登出、建立租戶、事件紀錄的三個唯讀端點、讀取目前的 session、切換它的租戶、讀取與修改偏好）；密碼登入、註冊、假登入常用的路徑與方法都是 404 或 405，`/auth/login` 拒絕其他 `step` 與多餘欄位，帶著各種「開發登入」cookie 也一樣（`there_is_no_password_login_sign_up_or_fake_login`）。

### 寄信（SMTP）

`src/mail.rs` 是只做這件事的 SMTP 客戶端（沒有引入郵件套件）：

- **一律 TLS**：`TRUSTIOT_SMTP_SECURITY` 是 `tls`（一連線就 TLS，通常是 465）或 `starttls`（587）。`starttls` 時 relay 必須提供 `STARTTLS`，在升級之前只送 `EHLO` 與 `STARTTLS`，不提供就不寄，絕不退回明文；relay 回應 `STARTTLS` 時多送的位元組視為注入（CVE-2011-0411）而中止。憑證必須鏈到映像內建的 CA（`/etc/ssl/certs/ca-certificates.crt`），或 `TRUSTIOT_SMTP_CA_FILE` 指定的 CA（只信任它），主機名稱或 IP 必須相符。
- 有帳密時以 `AUTH PLAIN` 登入（只在 TLS 之內）；relay 沒提供 `AUTH PLAIN` 就不寄。
- 信件是 UTF-8 純文字（base64），中文在前、英文在後；寄件者是 `TRUSTIOT_SMTP_FROM`（chart 的 `smtp.from`），收件者是驗證過格式的 ASCII 地址，所以標頭無法被注入。
- 錯誤訊息只有 SMTP 的步驟與回應碼，不含 relay 回應的文字、地址與內容。
- **設定不能用時照常服務**：設了 `TRUSTIOT_SMTP_HOST`，但其他設定缺少或不合規則（`_PORT` 是 0、沒有 `_SECURITY`、`_FROM` 空白或不是 `名稱 <地址>`、帳密只設了一個、`TRUSTIOT_SMTP_AUTH_REQUIRED` 是 `true` 卻沒有帳密），或 CA 檔讀不到、沒有可用的憑證時，服務照常啟動，啟動時記一行 ERROR（設定名稱與原因，不含值；CA 檔的錯誤寫出路徑），之後 `send_code` 回 503，直到修正設定、重啟服務。不結束程序，是因為每個帶 cookie 的請求都要經過身分服務的 ext_authz：寄信的設定錯了，不該讓整個平台的請求都失敗。chart 的 schema 另外要求設了 `smtp.host` 就要有 `smtp.from`，在安裝前擋下最常見的漏設。`_PORT` 不是數字、`_SECURITY` 不是 `starttls` 或 `tls` 時讀不了設定，跟其他讀不了的設定一樣以 exit code 1 結束（chart 的 schema 已先擋下這兩種值）。
- SaaS 暫用 Resend 的 SMTP relay（2026-10-19，[密鑰與權限](../../docs/architecture/secrets.md)的 SMTP 帳密）；Resend 主控台看得到寄出的信件內容（含驗證碼），所以主控台成員只限維運人員。

## 租戶

身分服務擁有租戶清單（`identity.tenants`）。平台管理者（A2）以 `POST /api/identity/v1/tenants` 建立租戶（`src/tenants.rs`）：

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 不是平台管理者（租戶的 `admin` 也不是），或 token 帶 `act` | 403 `permission_denied`，在解析 body 與讀資料庫之前判斷，什麼都沒有建立 |
| `Content-Type` 不是 `application/json`（可帶 `; charset=utf-8` 之類的參數），或沒有 `Content-Type` | 400 `invalid_request` |
| body 不是帶字串 `name` 的 JSON 物件 | 400 `invalid_request` |
| `name` 去掉前後空白後是空的、超過 100 個字元，或含 Unicode 一般類別 Cc（控制字元）、Cf（格式字元：零寬字元、雙向文字的覆寫與隔離、BOM）、Zl、Zp（行與段落分隔符）的字元 | 422 `validation_failed`，欄位 `/name`，訊息 key `tenant.name_invalid`，`{"max": 100}` |
| 資料庫出錯或拒絕交易 | 503 `unavailable`，什麼都沒有建立 |

| 建立了 | 201，`{"tenant_id", "name", "created_at"}` |

建立租戶只接受平台管理者本人的請求（Laysi/trustiot-v2#104 2026-10-23 的決定，Laysi/trustiot-v2#760 起程式照做）：其他服務換發、帶 `act` 的 token 一律 403，與[成員](#成員)相同，INFO 記 `refused to create a tenant`，帶 `user_id` 與 `exchanged`。[權限矩陣](../../docs/architecture/permissions.md)沒有這一列的已知落差。

- 呼叫路徑和其他 API 一樣經過入口：cookie 由這個服務自己的 ext_authz 換成內部身分 token，路由 `/api/identity` 的受眾是 `identity`；平台管理者不必屬於任何租戶，所以 token 沒有 `tenant` 也可以建立。給其他服務的 token（受眾不是 `identity`）回 401。
- **以 cookie 認證的寫入一律要求 JSON 的 `Content-Type`**（防 CSRF）：入口把 cookie 換成呼叫者的身分，而別的 origin 的網頁也能讓瀏覽器帶著 cookie 送出 POST，但沒有 CORS preflight 時只能用表單（`application/x-www-form-urlencoded`、`multipart/form-data`）或 `text/plain`。要求 `application/json` 會讓跨 origin 的請求必須先經 preflight，而入口與服務都沒有 CORS 設定、不允許任何 preflight，所以這種網頁無法用管理者的 cookie 建立租戶。日後若加 CORS，不可對外部 origin 允許帶 cookie 的請求。之後每個以 cookie 認證的寫入 API 都照這個規則；登入與登出（Laysi/trustiot-v2#103）也照這個規則，cookie 另設 `SameSite=Lax`（見[登入](#登入)）。
- 新租戶的 ID 由服務產生（UUIDv7），租戶的列與 `identity.tenant_created`（`crates/contract` 的 `events::identity::TenantCreated`，aggregate `tenant`/租戶 ID，事件帶租戶，`actor` 是建立它的平台管理者）在**同一個新租戶的 RLS 交易**裡寫入（`outbox::publish_as`），一起 commit 或一起放棄：有租戶就一定有事件，沒有租戶就沒有事件（`tests/tenants.rs` 撤銷寫 outbox 的權限後證明 503 且沒有留下列）。其他服務從這個事件建立自己的租戶資料，第一個是[資產服務](../assets/README.md)。
- 成功時記一行 INFO `created a tenant`，帶 `user_id` 與 `tenant_id`；租戶名稱可能是公司名，不寫日誌，事件也不帶名稱（要名稱的服務以後經 API 取得）。

## 租戶狀態

每個租戶有一個狀態（Laysi/trustiot-v2#776，清單 B1，`src/tenant_state.rs`）：`active`（正常）、`suspended`（停權，一定帶原因：`security_or_abuse` 資安或濫用、`non_payment` 欠費、`contract_terminated` 合約終止）或 `deleted`（合約終止後刪除，Laysi/trustiot-v2#109；這裡只定義狀態，沒有 API 設定它）。平台管理者（A2）以 `PUT /api/identity/v1/platform/tenants/{tenant_id}/state` 停權或恢復，以 `GET` 同一個路徑讀目前的狀態。body 是 `{"state": "suspended", "reason": "<原因>"}` 或 `{"state": "active"}`；兩者都回 `{"tenant_id", "state", "reason", "state_version", "state_changed_at", "status"}`，`status` 是後台要顯示的狀態（見[停權的處理中](#停權的處理中)）。

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 不是平台管理者（租戶的 `admin` 也不是），或 token 帶 `act` | 403 `permission_denied`，在讀 body 與資料庫之前判斷（body 不讀進記憶體） |
| 租戶 ID 不是 UUID | 400 `invalid_request` |
| `PUT`：`Content-Type` 不是 `application/json`（可帶參數），或沒有 `Content-Type` | 400 `invalid_request` |
| `PUT`：body 超過 4096 位元組（`MAX_BODY_BYTES`），或不是帶字串 `state`、字串或 `null` 的 `reason` 的 JSON 物件 | 400 `invalid_request` |
| `PUT`：`state` 不是 `active` 或 `suspended`（`deleted` 也不行）；停權沒有三種原因之一，或恢復帶了原因 | 422 `validation_failed`，欄位 `/state`（`tenant.state_invalid`）、`/reason`（`tenant.suspension_reason_invalid`） |
| 沒有這個租戶 | 404 `not_found`，什麼都沒寫 |
| `PUT`：租戶已因合約終止停權或已刪除，而請求會改變它 | 409 `conflict`，訊息 key `tenant.state_final`；什麼都沒寫，版本不變 |
| 資料庫出錯或拒絕交易 | 503 `unavailable`，什麼都沒寫 |
| `GET` | 200，目前的狀態 |
| `PUT`：狀態或原因改變了 | 200，新的狀態（版本加一），並寫出事件 |
| `PUT`：租戶本來就是這個狀態與原因 | 200，原本的狀態；什麼都不寫、不發事件 |

2026-10-30 的決定（Laysi/trustiot-v2#107 拆分時）：

- **版本**：`state_version` 在建立租戶時是 1（`identity.tenant_created` 就代表「正常、沒有原因、版本 1」），之後狀態或原因每改變一次剛好加一；停權中換原因也升版本，同狀態同原因不寫入。`state_changed_at` 是進入這個狀態的時間（由 trigger 在改變時以資料庫的 `clock_timestamp()` 設定，沒改變時保持原值，任何寫入者都設不了別的時間；從沒改過時是建立時間），只給人看，順序以版本為準。
- **事件帶快照**：每次改變在同一個交易寫 `identity.tenant_state_changed`（`crates/contract` 的 `events::identity::TenantStateChanged`），payload 是改變後的 `{state, reason, state_version}`；信封的租戶是這個租戶，`actor` 是平台管理者，aggregate 是 `tenant` 加租戶 ID（與 `identity.tenant_created` 同一個，所以依寫入順序送達）。消費端只保留版本較大的快照（[crates/outbox](../../crates/outbox/README.md) 的「通知模式」），從頭重播就能重建每個租戶的狀態，不必呼叫這個服務。狀態與原因的列舉都有 `Unrecognized`，舊版本的消費端解得開新值，要當成不是 `active` 處理。事件層把它記進該租戶的操作紀錄，後台以 `GET /api/identity/v1/platform/tenants/{tenant_id}/event-log` 看得到。
- **合約終止與 `deleted` 不能恢復**（清單 B1「合約終止不恢復」）：已因 `contract_terminated` 停權的租戶不能恢復、也不能換成其他原因，只能（由 Laysi/trustiot-v2#109）變成 `deleted`；只有合約終止的租戶會變成 `deleted`，並保留原因 `contract_terminated`；`deleted` 不再改變。
- 只接受平台管理者本人的請求，與[成員](#成員)相同。
- 每次請求的停權檢查見下方[停權時的每次請求檢查](#停權時的每次請求檢查)（Laysi/trustiot-v2#777），後台的「處理中」見[停權的處理中](#停權的處理中)（Laysi/trustiot-v2#779）；certs 的訂閱在 Laysi/trustiot-v2#778，下游的撤銷與踢線在 Laysi/trustiot-v2#108。

實作：

- 交易以**路徑的租戶**開（`BackgroundTenantDb`，見[資料庫](#資料庫)），以 `SELECT ... FOR NO KEY UPDATE` 鎖住租戶的列（更新不改鍵，這就是更新本身要的鎖），所以同一個租戶的請求依序執行，每個請求都以前一個留下的狀態判斷：同時送出的相同停權只產生一個新版本、一個事件。交易開頭設 `lock_timeout` 5 秒（`LOCK_TIMEOUT`），等鎖超過就回 503。讀取也在路徑租戶的交易裡，不上鎖。
- **資料庫也擋**（`0008_tenant_state.sql`）：`state` 與 `suspension_reason` 有 CHECK（`active` 沒有原因、`suspended` 一定有原因、`deleted` 的原因一定是 `contract_terminated`）；trigger `tenants_state_guard` 對任何寫入者（包括服務角色）拒絕「狀態或原因改變時版本不是剛好加一、沒改變時版本變了」、「離開合約終止或 `deleted`（合約終止只能變成 `deleted`）」與「不是合約終止的租戶變成 `deleted`」，以 42501 拒絕，並由它設定 `state_changed_at`。服務角色只多了 `state`、`suspension_reason`、`state_version` 三欄的 `UPDATE`；它的 `INSERT` 縮成 `tenant_id`、`name` 兩欄，所以新租戶一定從「正常、版本 1」開始。
- 每次改變記一行 INFO `changed a tenant's state`，帶平台管理者的 `user_id`、`tenant_id`、前後的狀態與原因、新版本；拒絕記 INFO `refused to change a tenant's state` 或 `refused to read a tenant's state`，帶 `user_id` 與 `exchanged`。
- 路由：與其他身分服務的 API 一樣在 `/api/identity` 的 cookie 路由底下，跨站請求由 ext_authz 擋下，服務另外要求 JSON 的 `Content-Type`（見[租戶](#租戶)）。

### 停權時的每次請求檢查

停權與刪除在下一個請求就生效（Laysi/trustiot-v2#777，清單 B1「每次請求檢查停權」）：`suspended` 與 `deleted` 一樣處理，以下都稱「停權」。

- **ext_authz 讀狀態**：`PgSessionStore` 讀成員角色的同一個查詢、同一個租戶 RLS 交易，也讀租戶的 `state`（`src/session.rs` 的 `MEMBERSHIP`），沒有快取。路由的服務不是身分服務時回 403 `tenant_suspended`、不簽 token；是身分服務時照常簽，token 帶 `tenant_suspended`，讓身分服務自己的端點決定。
- **服務端**：`crates/service-kit` 的 `Requirement::Tenant` 遇到帶 `tenant_suspended` 的 token 回 403 `tenant_suspended`，和 `permission_denied` 分開，在比對角色之前判斷（[service-kit README](../../crates/service-kit/README.md) 的「角色檢查」）。身分服務裡需要租戶的端點（例如租戶的事件紀錄）因此回 403 `tenant_suspended`；`OwnRequest` 與 `OwnPlatformAdmin` 不看這個 claim。
- **run 委派**：建立與每次換 token 都回 403 `tenant_suspended`（見[agent 執行期間的委派](#agent-執行期間的委派)）；`RunDelegations` 與 `OnBehalf` 把這個代碼照傳給使用者。token 交換拒絕帶這個 claim 的 token。
- **停權時照常可用**：讀目前的 session（每個租戶帶 `state`）、切換租戶、登出、登出所有裝置與偏好設定；平台管理者的平台端點在身分服務裡照常可用，所以目前租戶被停權的平台管理者仍能恢復它（`tests/tenant_suspension.rs` 實測）。
- **代價**：
  - 平台管理者的目前租戶被停權時，其他服務（例如 assets 的閘道器綁定、certs 的出廠登錄）的平台端點也回 403 `tenant_suspended`，因為 ext_authz 不簽給它們 token；要先切換到另一個租戶（或恢復這個租戶）才能用。只屬於這個租戶的平台管理者在恢復之前用不了它們；租戶已刪除時，要由 Laysi/trustiot-v2#109 處理成員資格。
  - 已經簽出的內部身分 token 在剩下的效期內（最多 60 秒，`TOKEN_LIFETIME_SECS`，驗證端另有 5 秒時鐘誤差）仍被受眾服務接受，也仍換得到 token 交換的 token；run 委派每次換 token 都重查，不受這一條影響。
  - 沒有 `tenants` 列的成員資格（只有直接寫表的測試與本機資料會有，建立租戶一定寫這一列）當作 `active`：停權要有這一列，狀態 API 對沒有列的租戶回 404。資料庫裡出現這版不認得的狀態時，ext_authz 回 503（fail closed）。
- **日誌**：ext_authz 的拒絕記在 `denied the ext_authz check`，`reason` 是 `tenant_suspended`；服務端的拒絕記在 service-kit 的 `refused to <動作>`，`reason` 是 `tenant_suspended`。

### 停權的處理中

停權時後台先顯示「處理中」，certs 確認已踢線、CRL 已生效才顯示「已停權」（清單 B1，Laysi/trustiot-v2#779，`src/state_applied.rs`）。[租戶狀態](#租戶狀態)的兩個 API 的回應都帶 `status`（`TenantStatus`）：

| `state` | certs 確認過的版本 | `status` |
| --- | --- | --- |
| `active` | 不看 | `active`：恢復後立即是正常，不等任何確認 |
| `suspended`、`deleted` | 小於 `state_version`（包括從沒確認過） | `processing` |
| `suspended` | 大於或等於 `state_version` | `suspended` |
| `deleted` | 大於或等於 `state_version` | `deleted` |

- **確認事件**：certs 套用了 `identity.tenant_state_changed` 的某個版本之後寫 `certs.tenant_state_applied`（`crates/contract` 的 `events::certs::TenantStateApplied`），payload 只有 `state_version`，租戶在信封上，aggregate 是 `tenant_state` 加租戶 ID，`actor` 是 certs 自己。停權的「套用」是該租戶閘道器的憑證已進 broker 載入的 CRL、閘道器已被踢線；發出它的是 Laysi/trustiot-v2#108。一個確認涵蓋它的版本與所有更舊的版本，所以 certs 可以跳過中間的版本。
- **接收端**：身分服務另開一個訂閱 `identity.tenant-state-applied.certs`（`state_applied::SUBSCRIPTION`，與事件紀錄的 `identity.event-log.certs` 分開，兩者互不等待），讀 certs 的 outbox（`CONSUMES` 已經有 `certs`，migration Job 已授權平台角色讀它），只處理 `certs.tenant_state_applied`，其他 certs 事件略過（事件紀錄照常記下它們，包括這個確認，所以操作紀錄看得到 certs 何時套用了哪個版本）。handler 在事件的租戶交易裡以服務角色寫 `identity.tenants.certs_applied_version`：只在新的版本比存的大、而且比租戶目前的 `state_version` 舊，或就是目前的版本而且 certs 寫下確認的時間（outbox 列的 `stored_at`）晚於租戶進入這個版本的時間（`state_changed_at`）時寫入（INFO `certs applied a tenant's state`）。兩個時間都是資料庫的時鐘。
- **只往大的方向寫**：晚到、重複或重播的舊確認什麼都不改（DEBUG），所以舊版本的確認不會把較新的停權標成已停權。資料庫也擋（`0009_tenant_state_applied.sql`）：trigger `tenants_certs_applied_guard` 對任何寫入者拒絕把它改小（42501），CHECK `tenants_certs_applied_not_ahead` 不讓它超過 `state_version`；服務角色只多了這一欄的 `UPDATE`。
- **不合理的確認進死信**：沒有租戶、版本小於 1、身分服務沒有這個租戶，或版本比租戶目前的還大（身分服務從沒寫過這個版本），都記一行 WARN（事件 ID、租戶、版本）並進死信（`outbox_dead_letter`），不寫入：版本超前的確認若被存下，之後真的停權到那個版本時會在 certs 看到之前就顯示「已停權」。這種確認的死信在租戶之後真的到了那個版本時以 `replay_from` 重新處理，也不會被接受：它寫在租戶進入那個版本之前，所以再進死信（WARN `certs confirmed a tenant state version before the identity service wrote it`）。比這個 build 新的事件版本照常重試，等身分服務升級。
- **2026-10-30 的決定：下游一直沒有確認時一直是 `processing`**，不設逾時、不自動改成「已停權」。理由：「已停權」代表閘道器確實已斷線，沒有確認就顯示它是假的安全感；畫面要顯示等了多久，以 `state_changed_at` 算（進入這個狀態的時間）。certs 卡住時由它讀身分服務事件的訂閱的落後量告警（Laysi/trustiot-v2#778），身分服務這一端的訂閱落後由 `outbox.consumer.lag.age` 告警。在 Laysi/trustiot-v2#108 開始發出確認之前，每個停權都會一直是 `processing`。
- **既有的租戶**：`certs_applied_version` 從 0 開始（從沒確認過），所以這個 migration 之前就停權的租戶也顯示 `processing`，直到 certs 確認。
- 沒有給平台角色帳密時（事件紀錄關閉），這個訂閱也不會跑，停權一直是 `processing`：只會少顯示「已停權」，不會多顯示。

## 方案權益

每個租戶有一個方案、合約的覆寫值與一份權益快照（Laysi/trustiot-v2#111，清單 B3，`src/entitlements.rs`）。方案是 `starter`、`business`、`enterprise`、`enterprise_on_prem` 之一；權益的欄位、各方案的預設值與各服務的檢查點見 [docs/product/plans.md](../../docs/product/plans.md)「權益清單」，預設值寫在 `Plan::defaults`，單元測試確認它與文件的表一致。

- **快照與版本**：權益是方案預設套上覆寫值的結果，由這個服務在寫入時算好、連同版本存進 `identity.tenants`（`entitlements`、`entitlements_version`）。方案、覆寫值或快照每改變一次，版本剛好加一，並在同一個交易寫 `identity.tenant_entitlements_changed`（`crates/contract` 的 `events::identity::TenantEntitlementsChanged`，payload 是 `{plan, entitlements, entitlements_version}`；信封的租戶是這個租戶，aggregate 是 `tenant_entitlements` 加租戶 ID，與租戶狀態的 `tenant` 分開，所以只訂閱租戶狀態的消費端看不到它）。消費端只保留版本較大的快照（[crates/outbox](../../crates/outbox/README.md) 的「通知模式」），在自己的伺服器端檢查，不在請求路徑上呼叫這個服務；還沒收到快照的租戶一律當成沒有權益。方案的列舉有 `Unrecognized`，檢查只看快照的欄位，不看方案名稱。
- **新租戶**：建立租戶的交易同時寫 Starter 第 1 版與它的事件（actor 是建立租戶的平台管理員），所以新租戶一建立就有權益。
- **既有租戶**：`0010_entitlements.sql` 之前建立的租戶（以及換版期間舊副本建立的租戶）是版本 0、沒有快照。每個副本在啟動時與之後每 10 分鐘（`BACKFILL_PERIOD`）補一次：以平台角色列出版本 0 的租戶（一次最多 500 個，`BACKFILL_BATCH`，用事件紀錄的平台角色連線池），再對每個租戶在它自己的交易裡鎖住列、確認仍是版本 0，寫入方案的快照第 1 版與事件，actor 是身分服務本身（`{"kind": "service", "service": "identity"}`）。多個副本同時補也只會產生一個第 1 版。版本 0 的租戶在 API 上顯示目前方案的預設值與版本 0。沒有平台角色的帳密時不補（與事件紀錄一樣記 WARN）。
- **方案預設值改變時不追溯**：新版本改了某個方案的預設值，既有租戶的快照不會自己改變（每次改變都要有事件，同一個版本在所有服務看到的值才一樣）；平台管理員對該租戶再設定一次同一個方案，就換成新的值、版本加一。
- **覆寫只給 Enterprise**：只有 `enterprise` 與 `enterprise_on_prem` 可以有覆寫值，覆寫值優先於方案預設；其他方案帶覆寫值回 422，資料庫的 CHECK 也擋。
- **底線**：權益只擋新增與功能使用，不擋資料寫入、不刪資料；降級後超過上限的成員、閘道器與規則都保留，只是不能再新增。這個服務自己的寫入（成員角色、租戶狀態）都不看權益。

平台管理員（A2）以 `GET`／`PUT /api/identity/v1/platform/tenants/{tenant_id}/entitlements` 讀取與設定租戶的方案與覆寫值。`PUT` 的 body 是 `{"plan": "<方案>", "overrides": {...}, "expected_version": <版本>}`，方案與覆寫值整個取代，`overrides` 可省略、`null` 或 `{}`；`expected_version` 必填，是呼叫端讀到的 `entitlements_version`，租戶的版本已經不同時回 409、什麼都不寫，所以兩位平台管理員依同一個版本修改時，後送的人不會默默蓋掉前一個人的覆寫值。body 不接受其他欄位，打錯成 `override` 會回 400，不會被當成沒有覆寫值而清空合約的覆寫值。回應是 `{"tenant_id", "plan", "overrides", "entitlements", "entitlements_version", "entitlements_changed_at"}`。租戶成員（至少 viewer，換發的 token 也可以）以 `GET /api/identity/v1/entitlements` 讀自己租戶的 `{"tenant_id", "plan", "entitlements", "entitlements_version"}`，給畫面顯示升級牆；不含覆寫值。

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 平台的路由：不是平台管理員，或 token 帶 `act` | 403 `permission_denied`，在讀 body 與資料庫之前判斷 |
| `GET /api/identity/v1/entitlements`：沒有租戶或角色不認得 | 403 `permission_denied`；租戶停權或已刪除時 403 `tenant_suspended` |
| 租戶 ID 不是 UUID | 400 `invalid_request` |
| `PUT`：`Content-Type` 不是 `application/json`、body 超過 8192 位元組（`MAX_BODY_BYTES`），或不是帶字串 `plan`、物件或 `null` 的 `overrides`、至少 0 的整數 `expected_version` 且沒有其他欄位的 JSON 物件 | 400 `invalid_request` |
| `PUT`：不認得的方案 | 422 `validation_failed`，欄位 `/plan`（`entitlements.plan_invalid`） |
| `PUT`：覆寫值不是已知權益與正確型別的物件 | 422，欄位 `/overrides`（`entitlements.overrides_invalid`） |
| `PUT`：不是 Enterprise 方案卻帶覆寫值 | 422，欄位 `/overrides`（`entitlements.overrides_not_allowed`） |
| `PUT`：覆寫值超出範圍 | 422，欄位 `/overrides/<名稱>`（`entitlements.override_out_of_range`，`details` 有 `min`、`max`） |
| 沒有這個租戶 | 404 `not_found`，什麼都沒寫 |
| `PUT`：租戶的版本不是 `expected_version`（即使這次請求不會改變任何東西） | 409 `conflict`（`entitlements.version_conflict`，`details` 有 `current_version`），什麼都沒寫 |
| 資料庫出錯或拒絕交易 | 503 `unavailable`，什麼都沒寫 |
| `GET` | 200 |
| `PUT`：方案、覆寫值或快照改變了 | 200，新的版本，並寫出事件 |
| `PUT`：都沒有改變 | 200，原本的內容；什麼都不寫、不發事件 |

實作：

- 設定以**路徑的租戶**開交易（`BackgroundTenantDb`），以 `SELECT ... FOR NO KEY UPDATE` 鎖住租戶的列，交易開頭設 `lock_timeout` 5 秒（`LOCK_TIMEOUT`），所以同一個租戶的請求依序執行；鎖住之後才比對 `expected_version`，比對與寫入之間不會有別的變更：同時依同一個版本送出的請求只有一個成功、產生一個新版本與一個事件，其他回 409。租戶成員的讀取以 token 的租戶開交易（`TenantDb`），在 RLS 下只看得到自己的租戶。
- **資料庫也擋**（`0010_entitlements.sql`）：`plan` 只能是四個方案之一；覆寫值與快照一定是 JSON 物件；快照與 `entitlements_changed_at` 剛好在版本 0 時是 NULL；不是 Enterprise 方案的覆寫值一定是空的。trigger `tenants_entitlements_guard` 對任何寫入者（包括服務角色）拒絕「方案、覆寫值或快照改變時版本不是剛好加一、沒改變時版本變了」（42501），並以資料庫時間設定 `entitlements_changed_at`。服務角色只多了 `plan`、`entitlement_overrides`、`entitlements`、`entitlements_version` 四欄的 `UPDATE`；它的 `INSERT` 仍只有 `tenant_id`、`name`，所以新租戶一定從 Starter、版本 0 開始，由建立的交易升到版本 1。
- 每次改變記一行 INFO `changed a tenant's entitlements`，帶平台管理員的 `user_id`、`tenant_id`、前後的方案、是否有覆寫值與新版本；補上第一版時記 INFO `gave a tenant its first entitlements`；補的一輪失敗記 WARN，下一輪再試。
- 拒絕的回應（給各服務的檢查點用）：`contract::entitlements::feature_refusal` 與 `limit_refusal`，都是 403 `quota_exceeded`，訊息 key 分別是 `entitlement.feature_not_in_plan` 與 `entitlement.limit_reached`。
- AI 月額度不在快照裡（單位與數字隨價格表訂），由 Laysi/trustiot-v2#110 以新增的選填欄位加入。

## 成員

平台管理者（A2）以 `PUT /api/identity/v1/platform/tenants/{tenant_id}/members` 設定既有使用者在租戶裡的角色（`src/members.rs`，Laysi/trustiot-v2#583）：使用者還不是成員時建立成員資格，已經是成員時改角色。body 是 `{"email", "role"}`，`role` 是 `viewer`、`editor` 或 `admin`。

2026-10-23 的決定（Laysi/trustiot-v2#104 的留言）：

- 平台管理者對租戶的資料沒有隱含的權限：要看或改一個租戶的資料，要有明確的成員資格，就是這支 API 寫的那一筆，而且留在操作紀錄。平台管理者也可以用它設定自己的角色（展示環境的展示者就是這樣進入展示租戶）。
- 只設定既有使用者，不建立使用者：舊系統曾因「加成員時默默建立帳號」出問題（Laysi/trustiot#46）。建立使用者與邀請在 Laysi/trustiot-v2#105。
- 只接受平台管理者本人的請求：經其他服務換發、帶 `act` 的 token（例如 AI 助理代表平台管理者呼叫）一律 403，與 certs 的工作站憑證相同。identity 其他的平台路徑由 Laysi/trustiot-v2#584 統一。
- 租戶管理者自己改角色、移除成員與「至少一位 admin」的原子檢查在 Laysi/trustiot-v2#105；這支 API 不檢查租戶是否還有 admin。

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 不是平台管理者（租戶的 `admin` 也不是），或 token 帶 `act` | 403 `permission_denied`，在讀 body 與資料庫之前判斷（body 不讀進記憶體） |
| 租戶 ID 不是 UUID | 400 `invalid_request` |
| `Content-Type` 不是 `application/json`（可帶參數），或沒有 `Content-Type` | 400 `invalid_request` |
| body 超過 4096 位元組（`MAX_BODY_BYTES`），或不是帶字串 `email` 與字串 `role` 的 JSON 物件 | 400 `invalid_request` |
| `email` 不是地址、`role` 不是三種角色之一 | 422 `validation_failed`，欄位 `/email`（`member.email_invalid`）、`/role`（`member.role_invalid`），兩個都錯時兩個都列 |
| 沒有這個租戶 | 404 `not_found`，什麼都沒寫 |
| 沒有使用者用這個 email | 422 `validation_failed`，欄位 `/email`，`member.email_unknown`；什麼都沒寫，也不建立使用者 |
| 資料庫出錯或拒絕交易 | 503 `unavailable`，什麼都沒寫 |
| 使用者原本不是成員 | 201，`{"tenant_id", "user_id", "role", "previous_role": null}` |
| 使用者原本是成員 | 200，`previous_role` 是原本的角色；角色本來就是這個時什麼都不寫、不發事件 |

- 不存在的租戶回 404、不存在的 email 回 422：租戶是路徑上的資源，email 是 body 的欄位，前端可以把錯誤標在欄位上。能分辨某個 email 有沒有帳號的只有平台管理者本人，他們本來就管理使用者；其他人在讀任何資料之前就拿到 403，看不出 email 是否存在。
- email 照登入的規則正規化（去前後空白、ASCII 小寫）後比對 `users.email`。
- 交易以**路徑的租戶**開（`BackgroundTenantDb`，見[資料庫](#資料庫)），先確認租戶存在、找到使用者，再以 `SELECT ... FOR UPDATE` 鎖住成員資格，所以同一筆成員資格的兩個請求依序執行，每個事件記的「原本的角色」都正確。成員資格還不存在時以 `INSERT ... ON CONFLICT DO NOTHING` 建立；同時有另一個請求先建立了，就等它 commit 後鎖住再改。交易開頭設 `lock_timeout` 5 秒（`LOCK_TIMEOUT`）：等鎖超過就回 503，不長時間佔住租戶連線池僅有的兩條連線。
- 成員資格與事件 `identity.member_role_changed`（`crates/contract` 的 `events::identity::MemberRoleChanged`）在同一個交易寫入（`outbox::publish_as`）：payload 是 `user_id`、`previous_role`（新成員是 `null`）與 `role`，不含 email；信封的租戶是成員資格的租戶，`actor` 是平台管理者；aggregate 是 `membership` 加 `<租戶 ID>:<使用者 ID>`，同一筆成員資格的事件依序送達。事件層把它記進該租戶的操作紀錄，租戶的 `admin` 以 `GET /api/identity/v1/event-log` 看得到。
- **何時生效**：ext_authz 每個請求都在那個租戶的 RLS 交易裡重讀角色，沒有快取，所以使用者下一個請求就帶新角色。已經簽出的 token 最多還活 60 秒（`TOKEN_LIFETIME_SECS`，驗證端另有 5 秒時鐘誤差），它在這段期間仍帶舊角色；run 委派每次換 token 也重讀角色。使用者的 session 要以這個租戶為目前的租戶（`active_tenant_id`），請求才帶這個租戶；使用者以 `PUT /api/identity/v1/sessions/current/tenant` 切換（見[目前的 session 與切換租戶](#目前的-session-與切換租戶)，Laysi/trustiot-v2#497）。
- 每次變更記一行 INFO `set a member's role`，帶平台管理者的 `user_id`、`tenant_id`、`member_user_id` 與兩個角色；拒絕記 INFO `refused to set a member's role`，帶 `user_id` 與 `exchanged`。不寫 email。
- 路由：與其他身分服務的 API 一樣在 `/api/identity` 的 cookie 路由底下（chart 的路由以路徑前綴比對，不分方法），跨站請求由 ext_authz 擋下（PUT 不是 GET 或 HEAD，`Sec-Fetch-Site` 或 `Origin` 不符時 403），服務另外要求 JSON 的 `Content-Type`（見[租戶](#租戶)）。

## AI 模型角色

平台管理者（A2）讀取、修改與重設 AI 閘道的模型角色（`src/ai_model_roles.rs`，Laysi/trustiot-v2#608）：身分服務只檢查權限、跨站請求與請求的形狀，再轉給 AI 閘道的管理 API；模型清單的規則（供應商、協定、版本）由 AI 閘道判斷（[services/ai-gateway 的「管理 API」](../ai-gateway/README.md#管理-api)，Laysi/trustiot-v2#606）。管理埠只有身分服務連得到（chart 的 Service `<release>-ai-gateway-admin`，Laysi/trustiot-v2#607）。

| 路徑 | 轉給 AI 閘道 |
| --- | --- |
| `GET /api/identity/v1/platform/ai/model-roles` | `GET /internal/admin/v1/roles`：每個角色（生效的模型、設定的模型、來源、`version`、沒有使用的清單、最後一次變更的時間與操作者）與每個供應商 |
| `PUT /api/identity/v1/platform/ai/model-roles/{role}`，body `{"models": [...], "expected_version": <讀到的版本>}` | `PUT /internal/admin/v1/roles/{role}`，body 只有這兩個欄位 |
| `DELETE /api/identity/v1/platform/ai/model-roles/{role}?expected_version=<讀到的版本>` | `DELETE /internal/admin/v1/roles/{role}?expected_version=<讀到的版本>`：回到設定的模型 |

- **誰能用**：只有平台管理者本人的請求，與[成員](#成員)相同。租戶的 `admin`、沒有租戶的使用者、經其他服務換發、帶 `act` 的 token 都 403，讀取也是；在讀 body、送出任何東西之前判斷。換模型會讓所有租戶的 AI 助理改用另一個供應商或模型，所以沒有租戶層級的權限。
- **跨站請求**：修改與重設由 ext_authz 擋下跨站請求（`PUT`、`DELETE` 不是 GET 或 HEAD），handler 再檢查一次，免得入口哪天不再轉這兩個標頭（見[跨站請求](#跨站請求csrf)）；兩者都要求 JSON 的 `Content-Type`（可帶參數），`DELETE` 沒有 body 也要。表單送不出這個型別，前端的 API client 每個寫入都帶（`frontend/src/api/client.ts`）。
- **轉送什麼**：方法、路徑、`expected_version`、`ai-gateway-actor: user:<token 的使用者 ID>` 與服務自己序列化的 body。使用者請求的標頭一律不轉送：cookie、`x-internal-identity`、`Authorization`、用戶端自己帶的 `ai-gateway-actor` 都到不了 AI 閘道；body 裡多餘的欄位也不轉送。AI 閘道的變更紀錄與角色的 `updated_by` 記的就是這個 actor。
- **自己的 HTTP client**（`src/ai_model_roles/admin_client.rs`，hyper，不與 ext_authz 共用任何東西）：同時最多 4 個呼叫（`MAX_IN_FLIGHT`），再多的立刻回 503、不送出；連線 2 秒（`CONNECT_TIMEOUT`），整個呼叫含讀回應 10 秒（`CALL_TIMEOUT`），回應最多讀 256 KiB。不保留連線、不重送：身分服務的一個請求最多把變更送到 AI 閘道一次。入口會重試 503（Istio 的預設，HTTPRoute 沒有另設重試），所以結果不明的變更一律不回 503（見下表），被入口重試的只有「什麼都沒送」的 503。沒有 TLS：位址是叢集內的 Service，連線由 ztunnel 的 mTLS 保護。AI 閘道卡住時最多佔住 4 個請求，每個請求都要經過的 ext_authz 照常回答（`tests/ai_model_roles.rs` 實測）。
- **設定**：`TRUSTIOT_AI_GATEWAY_ADMIN_URL` 只接受 `http://<host>[:<port>]`（可帶結尾的 `/`）。沒有設時記 WARN、不能用時記 ERROR（不引用值），服務照常啟動，只有這三條路由回 503。

| 情況 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 不是平台管理者，或 token 帶 `act` | 403 `permission_denied`，什麼都沒送 |
| 跨站的修改或重設 | 403 `permission_denied`，訊息 key `security.cross_site_request`，什麼都沒送 |
| 修改或重設的 `Content-Type` 不是 JSON；body 超過 16 KiB、不是 JSON 物件、`models` 不是字串陣列或 `expected_version` 不是 0 以上的整數；`DELETE` 沒有 0 以上的整數 `expected_version` | 400 `invalid_request`，什麼都沒送 |
| `models` 不是 1 到 8 個 | 422 `validation_failed`，欄位 `/models`，`ai_model_role.invalid_models`，什麼都沒送 |
| 角色名稱不可能存在（不是小寫字母開頭、只有小寫字母、數字與 `_` 的 1 到 32 個字元） | 404 `not_found`，`ai_model_role.not_found`，什麼都沒送 |
| 沒有設定或不能用的 `TRUSTIOT_AI_GATEWAY_ADMIN_URL`；同時已有 4 個呼叫；連不上 | 503 `unavailable`，什麼都沒送，沒有改變（入口重試也無害） |
| AI 閘道回 404 `role_not_found` | 404 `not_found`，`ai_model_role.not_found` |
| AI 閘道回 409 `version_conflict` | 409 `conflict`，`ai_model_role.version_conflict`，`details.current_version` 是現在的版本 |
| AI 閘道回 422 `invalid_models`、`unknown_provider`、`protocol_mismatch` | 422 `validation_failed`，欄位 `/models`，`ai_model_role.invalid_models`、`ai_model_role.unknown_provider`、`ai_model_role.protocol_mismatch` |
| AI 閘道回 2xx 與讀得懂的角色 | 200，AI 閘道回應的內容（`GET` 是所有角色與供應商） |
| 送出之後：逾時、連線中斷、5xx（包含 AI 閘道的 503 `unavailable`：資料庫失敗時變更可能已經 commit）、讀不懂的回應（未知的錯誤碼、錯誤碼與狀態不符、讀不懂的 2xx） | 讀取：503 `unavailable`。修改與重設：504 `timeout`，訊息 key `ai_model_role.outcome_unknown`：變更可能已經生效，前端要重新讀取角色再決定要不要重試。不用 503，因為入口會重試 503，變更會再送一次（與 [service-kit](../../crates/service-kit/README.md) 的 `http.request_timeout` 逾時回 504 同一個理由）；服務自己也不重送 |
| AI 閘道拒絕服務自己組的請求（`invalid_request`、`invalid_actor`、`request_too_large`、`not_found`）：程式錯誤或版本不符 | 503 `unavailable`，記 ERROR；AI 閘道什麼都沒改 |

只有 HTTP 狀態決定成功與否：非 2xx 不論 body 寫什麼都是失敗。回應一律 `cache-control: no-store`。

- **事件**：成功而且版本改變的修改或重設寫一筆全平台事件 `identity.ai_model_role_changed`（`crates/contract` 的 `events::identity::AiModelRoleChanged`）：payload 是 `role`、`action`（`set` 或 `reset`）、`version`（AI 閘道給的新版本）與 `models`（之後生效的模型）；沒有租戶，`actor` 是平台管理者；aggregate 是 `ai_model_role` 加角色名稱。以平台角色經 `PlatformDb`（INFO `platform database access`，purpose `ai-model-role-changed`）寫進身分服務的 outbox，用事件紀錄的平台角色連線池，消費端收進 `platform_event_log`，後台以 `GET /api/identity/v1/platform/event-log` 看得到（見[事件層與操作紀錄](#事件層與操作紀錄)）。沒有存下模型的角色再重設，版本不變，不寫事件。
- **事件寫不進去時**：變更已經在 AI 閘道生效，它的變更紀錄也有，所以照樣回 200，並記 ERROR `could not write identity.ai_model_role_changed`（使用者 ID、角色、版本與錯誤類別），表示操作紀錄少了這一筆。原因可能是資料庫出錯、5 秒內寫不完（`EVENT_TIMEOUT`）或沒有平台角色的帳密。AI 閘道回應之後，事件在自己的 task 裡寫，從這時起請求斷線也會寫完；回應在寫完（或放棄）之後才送出。
- **事件的順序**：同一個角色的事件不一定依版本的順序寫入。兩位管理者幾乎同時變更時，後拿到新版本的請求可能先寫事件，所以 `aggregate_seq` 與收到事件的順序都不代表變更的順序；消費端與讀操作紀錄的人要依 payload 的 `version` 排序（AI 閘道的版本只會往上）。
- **操作紀錄可能少一筆**：AI 閘道的呼叫還在進行時，用戶端斷線、關機的 drain 截斷請求，或結果不明（504 `ai_model_role.outcome_unknown`），變更都可能已經在 AI 閘道生效，身分服務卻沒有寫事件。以 AI 閘道的變更紀錄為準；兩邊的對帳之後另開 issue 處理。
- **日誌**：變更記 INFO `changed an AI model role`（使用者 ID、操作、角色、新版本）；權限與跨站的拒絕記 INFO；AI 閘道的回應依嚴重度記 INFO（404、409、422）、WARN（AI 閘道的 503、同時呼叫太多）或 ERROR（其他），只寫狀態碼與種類，不寫回應的 body、模型、cookie 或 token。

## 目前的 session 與切換租戶

已登入的使用者讀取自己這個 session，並把它切換到自己所屬的另一個租戶（Laysi/trustiot-v2#497，`src/current_session.rs`）。兩個端點都經入口的 cookie 檢查（受眾 `identity`），和其他 API 一樣：

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 兩個端點，token 是別的服務換來的（帶 `act`） | 403 `permission_denied`：session、所屬的租戶與 email 只給使用者自己的請求 |
| `GET /api/identity/v1/sessions/current` | 200 `{"user_id", "email", "platform_admin", "active_tenant", "tenants"}`（五個欄位都一定出現，`email`、`active_tenant` 與租戶的 `name` 可能是 `null`），`Cache-Control: no-store` |
| `PUT /api/identity/v1/sessions/current/tenant`，跨站（[跨站請求](#跨站請求csrf)的規則，cookie 檢查之後再判斷一次） | 403 `permission_denied`，訊息 key `security.cross_site_request` |
| `PUT …/tenant`，`Content-Type` 不是 `application/json`、body 超過 1024 bytes，或不是 `{"tenant_id": "<uuid>"}`（多的欄位、重複的欄位、陣列都不行） | 400 `invalid_request` |
| `PUT …/tenant`，token 沒有 `sid`、沒有可用的 session cookie，或 cookie 的 session 不是 token 簽出時的那個 session、不屬於 token 的使用者、已經結束 | 401 `unauthenticated`，什麼都沒改 |
| `PUT …/tenant`，使用者不是那個租戶的成員（租戶存在與否都一樣） | 403 `permission_denied`，訊息 key `session.tenant_not_member`，session 維持原本的租戶 |
| `PUT …/tenant`，使用者所屬的租戶 | 200，回應同 `GET`（切換之後的樣子）；這個 session 的下一個請求起以那個租戶、以使用者在那裡的角色執行 |
| 資料庫出錯、讀成員資格等了 5 秒還輪不到（見[連線池](#連線池)），或平台角色沒有帳密 | 503 `unavailable`，什麼都沒改 |

- **內容從哪裡來**：使用者、平台管理者旗標，以及目前的租戶與角色，取自請求的 token（剛由 cookie 檢查從 session 與成員資格簽出）；租戶名稱在那個租戶的 RLS 交易裡讀。`active_tenant` 是 `null` 表示 session 不在任何租戶：還沒選過，或使用者已不是那個租戶的成員。`tenants` 是使用者所有的成員資格，依名稱排序；`email` 是 `null` 表示使用者不以 email 登入。`role` 是 `viewer`、`editor` 或 `admin`。`active_tenant` 與 `tenants` 的每個租戶都帶 `state`（`active`、`suspended` 或 `deleted`，Laysi/trustiot-v2#777，[租戶狀態](#租戶狀態)），畫面以它說明為什麼其他功能不能用（Laysi/trustiot-v2#780）；停權的原因不在這裡，只有平台管理者讀得到。租戶沒有 `tenants` 的列時是 `active`，資料庫裡出現這版不認得的狀態時回 503。
- **停權時照常可用**：讀這個 session、切換租戶（包括切到被停權的租戶，切過去之後其他服務一樣回 403 `tenant_suspended`）、登出與偏好設定都不需要租戶，停權不影響它們（見[停權時的每次請求檢查](#停權時的每次請求檢查)）。
- **列出所屬的租戶要跨租戶讀**：`memberships` 是租戶資料，服務角色一次只看得到一個租戶，而連線檢查不讓服務角色有以使用者為條件的 policy 或 `SECURITY DEFINER` 函式（決策 29）。所以這裡照決策 29 跨租戶操作的作法，以平台角色 `identity_platform` 讀：`0007_preferences_and_tenant_switch.sql` 給它 `memberships` 與 `tenants` 的 `SELECT` 與只有 `FOR SELECT` 的 `platform_access` policy，沒有寫入權限；查詢在 `PlatformDb` 的 `READ ONLY` 交易裡、條件只有已驗證的 token 的使用者，不取自請求的本文、路徑或查詢字串（`PlatformActor::service`，用途 `session-tenants`，啟動時以 INFO 記一行 `platform database reader`；每個交易記 DEBUG，service-kit 的那一行只有用途，這裡另記一行 `reading a user's memberships with the platform role` 帶 `user_id`，不帶 email）。用的是事件紀錄的平台角色連線池，同時最多 2 個（見[連線池](#連線池)）。沒有給平台角色帳密時這組 API 回 503（啟動時記 WARN），chart 一律給。取捨：平台角色因此看得到每個租戶的成員資格與名稱（它原本就讀得到每個租戶的事件紀錄）；另一種作法是由 trigger 維護一張不受 RLS 保護的「使用者→租戶」投影表，等於把成員資格搬到 RLS 之外，沒有採用。
- **切換**：目標必須在上面讀出的成員資格裡，然後把請求帶來的那個 session 的 `active_tenant_id` 指向它：以 cookie 的雜湊與 token 的 `sid`（cookie 檢查簽出 token 的那個 session）一起找，而且要屬於 token 的使用者、還沒撤銷或過期（資料庫的時鐘）。使用者其他的 session、別人的 session 都不受影響；同一個使用者的另一個 session 的 cookie 配上這個 token 也改不到（401）。服務角色只多了 `sessions.active_tenant_id` 一欄的 `UPDATE`（0004 的 trigger 不管這一欄：撤銷或過期的 session 指向哪個租戶都還是結束的）。
- **指標本身不給權限**：cookie 檢查每個請求都在那個租戶的 RLS 交易裡重讀 `memberships`（決策 13），所以切換之後才被移除的成員資格（即使只晚一瞬間）讓 session 不在任何租戶，角色也一律是使用者當下在那裡的角色。平台管理者不是成員就不能切換到那個租戶。
- **沒有探測租戶的管道**：不是成員的租戶與不存在的租戶回一樣的 403，回應不帶租戶的任何資料。
- **agent 的 run 委派不跟著切換**：委派綁的是建立時的租戶（見[agent 執行期間的委派](#agent-執行期間的委派)），切換 session 的租戶之後，進行中的 run 仍在原本的租戶、以使用者在那裡當下的角色執行，直到委派到期、session 結束或使用者不再是成員。
- **CSRF**：除了 cookie 檢查已經擋下跨站請求，處理常式在讀 body 之前以同一套規則（`csrf::cross_site`）再判斷一次，免得入口哪天沒把標頭交給檢查；寫入一律要求 JSON 的 `Content-Type`（見[租戶](#租戶)的 CSRF 說明）。
- **只給使用者自己的請求**：別的服務換來的 token（`act`）讀不到也改不了使用者的 session、所屬的租戶、email 與偏好（2026-10-23，安全審查的 F6：原本允許讀，代表使用者呼叫身分服務的服務就能拿到使用者所有租戶的名稱與 email）。以後有服務需要使用者的偏好（例如 AI 回答的語言），另開以它為對象的內部 API，不放寬這裡。
- **日誌**：成功記 INFO `switched the session's tenant`（`user_id`、原本的租戶 `from`、`tenant_id`），拒絕記 INFO `refused to switch tenants`（`reason`：`cross_site_request`、`exchanged_token`、`no_session_claim`、`no_session_cookie` 等、`not_a_member`、`no_active_session`）；換來的 token 讀 session 記 INFO `refused to read the current session`（`reason` 是 `exchanged_token`，帶呼叫的服務）；不記租戶名稱、email 與 cookie。
- **第一次登入**：新 session 沿用最近一個有租戶的 session 的租戶，第一次登入的使用者沒有，所以 `active_tenant` 是 `null`，要由畫面讓使用者選一次：SPA 在使用者只屬於一個租戶時自動切換，屬於多個時讓他選，一個都沒有時說明原因（Laysi/trustiot-v2#124，`frontend/src/session/TenantGate.tsx`）。

## 偏好設定

使用者讀取與修改自己的偏好（Laysi/trustiot-v2#497，`src/preferences.rs`）。偏好屬於使用者，不是租戶資料：存在 `identity.preferences`（每個改過偏好的使用者一列，沒有 RLS），每個查詢都以 token 的使用者為條件，請求無法指定別的使用者。

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| `GET` 或 `PATCH`，token 是別的服務換來的（帶 `act`） | 403 `permission_denied` |
| `GET /api/identity/v1/preferences` | 200 `{"language", "ai_language", "time_zone", "theme"}`，`Cache-Control: no-store`；沒改過的使用者是預設值 |
| `PATCH`，跨站、不是 JSON 的 `Content-Type`、body 超過 1024 bytes | 同上一節的 `PUT …/tenant`（403 或 400） |
| `PATCH` 的 body 不是只含這四個欄位、值都是字串的 JSON 物件（未知欄位、`null`、陣列都不行） | 400 `invalid_request` |
| `PATCH` 有值不在允許清單 | 422 `validation_failed`，每個不合的值一個欄位錯誤：`/language`（`preferences.language_invalid`）、`/ai_language`（`preferences.ai_language_invalid`）、`/time_zone`（`preferences.time_zone_invalid`）、`/theme`（`preferences.theme_invalid`）；整個請求都不套用 |
| `PATCH` | 200，改完之後的全部偏好；沒給的欄位維持原值，`{}` 什麼都不改 |
| `PATCH`，使用者已經不存在 | 401 `unauthenticated` |
| 資料庫出錯 | 503 `unavailable`，什麼都沒改 |

| 欄位 | 允許的值 | 預設 |
| --- | --- | --- |
| `language`：介面語言 | `auto`、`zh-TW`、`en`（大小寫要相同） | `auto`：跟隨瀏覽器 |
| `ai_language`：AI 回答的語言 | 同上 | `auto`：以提問的語言回答 |
| `time_zone`：時區 | `auto`，或 IANA 時區資料庫的名稱，拼法要和資料庫相同（`Asia/Taipei`，不是 `asia/taipei`；`Asia/Calcutta` 這類相容的舊名也可以；`Factory` 不行） | `auto`：跟隨瀏覽器 |
| `theme`：主題 | `system`、`light`、`dark` | `system`：跟隨作業系統 |

- 時區的名稱清單編進服務（`jiff-tzdb` crate 的資料，2026-10-23 是 tzdata 2026e），所以每個副本接受的名稱都一樣，不依映像裡的 `/usr/share/zoneinfo`；升級這個 crate 才會有新的時區名稱。資料庫的 CHECK 重複一次允許清單（語言、主題的清單，時區只檢查 `auto` 或 1–64 bytes、以 `/` 分段的 ASCII 名稱），服務的程式錯了也存不進別的值；單元測試確認服務接受的每個時區名稱都通過資料庫的 CHECK，預設值與清單和 migration 相同。
- 服務角色對 `preferences` 有 `SELECT`、只限 `user_id` 的 `INSERT`（一律先以預設值建立那一列）與四個偏好欄位的 `UPDATE`；不能改一列屬於誰、不能刪除，使用者被刪除時一起刪除（外鍵 `ON DELETE CASCADE`）。同一個使用者同時的兩個修改依序取得那一列的鎖，各自只改自己給的欄位。修改的 `UPDATE` 以使用者為條件，服務另外確認它剛好改到一列（使用者自己的那一列），否則整個交易不提交、回 503（縱深防禦，2026-10-23，安全審查的 F2）。
- 寫入的 CSRF 規則與上一節相同，別的服務換來的 token 讀寫都不行（見上一節的「只給使用者自己的請求」）。成功記 INFO `changed preferences`，只寫哪幾個欄位改了（布林值），不寫值。
- SPA 登入後讀寫這個 API，以伺服器的值為準；瀏覽器只留一份副本，給登入前與伺服器回應前的畫面使用（Laysi/trustiot-v2#124，[frontend/README.md](../../frontend/README.md)「偏好設定、租戶與工作階段」）。

## 事件層與操作紀錄

平台的事件層與操作紀錄（清單 K2，[決策 34](../../docs/architecture/decisions.md)，Laysi/trustiot-v2#130）由身分服務保存（`src/event_log.rs`）：

- **怎麼寫進來**：每個服務照舊把事件寫進自己的 outbox，與業務資料同一筆交易（`crates/outbox`），所以變更 commit 才有事件，回滾就沒有。人經 API 做的操作用 `outbox::publish_as` 帶 `actor`（`{"kind": "user", "user_id"}`，取自內部身分 token），服務自己的工作帶 `{"kind": "service", "service"}`（[crates/contract 的事件信封](../../crates/contract/README.md)）。
- **怎麼收進來**：身分服務對 `src/schema.rs` 的 `CONSUMES` 列出的每個產生端各跑一個 outbox 消費端（訂閱 `identity.event-log.<schema>`），以平台角色 `identity_platform` 讀產生端的 `outbox_event`，再以服務角色在事件的租戶交易裡寫一列：租戶事件寫 `identity.event_log`（RLS），全平台事件（信封沒有 `tenant_id`）寫 `identity.platform_event_log`。目前 `CONSUMES` 是 `identity` 自己、`assets`（設備型號的 `assets.device_model_*`，2026-10-22 起，Laysi/trustiot-v2#99）與 `certs`（出廠登錄的 `certs.gateway_registered`，全平台事件，Laysi/trustiot-v2#79），每個產生端的事件以信封的租戶寫進同一張表，同樣受 RLS 限制；新的服務開始寫 outbox 時，把它的 schema 加進 `CONSUMES`（migration Job 依此授權平台角色讀那張表），就會開始收。
- **只收一次**：主鍵是事件 ID（租戶表是 `(tenant_id, event_id)`），寫入用 `ON CONFLICT DO NOTHING`，重播或重複投遞不會多一列；inbox 也先擋一次。
- **只收產生端自己的事件**：信封的 `producer`（`-` 換成 `_`）必須等於讀取的 schema，因為只有那個服務的角色寫得進它的 outbox（決策 29）；不符的是偽造或壞掉的事件，進死信（`outbox_dead_letter`），不寫進紀錄。版本小於 1、`payload` 不是 JSON 物件、aggregate 序號小於 1 的事件同樣進死信，免得表的 CHECK 讓它無限重試。`actor` 是服務、但不是產生端自己的事件照常寫入，只是不記 `actor`，並記一行 WARN（事件 ID、產生端、它聲稱的服務，不含 payload）：服務的 `actor` 只代表自己的工作，而這一列仍記著驗證過的產生端，進死信反而讓紀錄缺一筆。這版不認得的 `actor` 種類（較新的產生端寫的）則重試、不寫成「沒有 actor」也不進死信，與 `DecodeError::NewerVersion` 相同：訂閱等身分服務升級（lag 告警），升級後照常寫入，紀錄不會永久缺少是誰做的。
- **訂閱卡在重試時**：消費端的 lag 與 `outbox.consumer.lag.age` 一直上升、日誌一直出現 `the envelope names an actor kind this build does not know`，就是卡在這版記不下的 `actor`。正常做法是升級身分服務，訂閱會自己往下走。不能等升級時，以 `outbox::replay_from` 把訂閱 `identity.event-log.<schema>` 移到那個事件的位置（`Delivery::position` 或 `outbox_event` 的 `<tx>:<id>`，[crates/outbox 的「重播」](../../crates/outbox/README.md#重播)），下一次輪詢從它之後開始讀；這個事件就暫時不在紀錄裡，記下它的位置，升級後以 `position.just_before()` 與 `Replay::SkipProcessed` 重讀，寫入有 `ON CONFLICT DO NOTHING`，已記的事件不會多一列。
- **append-only**：`identity_app` 對 `event_log` 只有 `SELECT`、`INSERT`，對 `platform_event_log` 只有 `INSERT`；`identity_platform` 兩張都只有 `SELECT`。誰都沒有 `UPDATE`、`DELETE`、`TRUNCATE`，只有擁有者（migration Job 的 `identity_owner`）能改，服務的程式錯誤或被入侵都改不了歷史（`tests/event_log.rs` 以兩個角色實測 42501）。
- **保留**：紀錄不自動刪；租戶的紀錄只在合約終止、刪除租戶資料時一起刪（之後的工作），隨 Postgres 備份與還原（M8）。各服務的 `outbox_event` 與 `outbox_inbox` 因此只是投遞的緩衝：每個訂閱都讀過、再過一段重播用的保留期之後可以刪，刪除工作在 Laysi/trustiot-v2#521（[crates/outbox](../../crates/outbox/README.md) 的「限制與後續」）。
- **讀取有時間上限**：每筆讀取交易一開始設 `statement_timeout` 10 秒（`READ_STATEMENT_TIMEOUT`，消費端的寫入不設），超過時 Postgres 取消查詢、API 回 503，讀取不會一直佔著與消費端共用的連線。分頁的位置以 `(occurred_at, event_id) <` 列比較寫成索引的範圍（`event_log_by_time`、`platform_event_log_by_time`），往後翻頁不必掃過比它新的每一列。
- **不影響 ext_authz**：事件紀錄用自己的兩個連線池（服務角色 `SERVICE_CONNECTIONS` 4 條、平台角色 `CONSUMES` 數加 2 條，最少 0 條、用到才開；平台角色的那個也給列出使用者所屬的租戶用，同時最多 2 條，見[連線池](#連線池)），都不算進 readiness：平台角色登不進去或紀錄落後時，每個請求都要經過的 ext_authz 不受影響。落後由消費端的 `outbox.consumer.lag.age` 告警。沒有給平台角色的帳密時紀錄關閉（WARN，API 回 503），事件留在各服務的 outbox，給了帳密之後從頭補收；chart 一律給。

查詢 API，最新的在前，依 `occurred_at` 再依事件 ID 排序：

| 路徑 | 誰 | 範圍 |
| --- | --- | --- |
| `GET /api/identity/v1/event-log` | token 的租戶的 `admin` | 那個租戶的事件，服務角色在租戶的 RLS 交易裡讀 |
| `GET /api/identity/v1/platform/tenants/{tenant_id}/event-log` | 平台管理者本人的請求（A2，後台）：帶 `act` 的 token 403（Laysi/trustiot-v2#584 起） | 那個租戶的事件，平台角色經 `PlatformDb` 讀（INFO `platform database access`，purpose `event-log-read`） |
| `GET /api/identity/v1/platform/event-log` | 平台管理者本人的請求 | 全平台事件 |

參數都可省略：`from`（含）與 `to`（不含）是 RFC 3339 時間，`from` 要早於 `to`；`limit` 1–500，預設 100；`cursor` 是上一頁回的 `next_cursor`。回應 `{"events": [...], "next_cursor"}`，每個事件有 `event_id`、`type`、`version`、`producer`、`tenant_id`（全平台事件是 `null`）、`occurred_at`（產生端的時間）、`recorded_at`（存進紀錄的資料庫時間）、`aggregate_type`、`aggregate_id`、`aggregate_seq`、`actor`（`null` 表示產生端沒說）、`payload`（產生端寫的原樣）；最後一頁的 `next_cursor` 是 `null`。不存在的租戶 ID 回空的一頁。

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 不是上表的「誰」（租戶的 `editor`、沒有租戶的使用者、平台路徑上的租戶 `admin`、平台路徑上其他服務換發的 token） | 403 `permission_denied`，在讀參數與資料庫之前判斷 |
| 參數格式不對、`from` 不早於 `to`、租戶 ID 不是 UUID | 400 `invalid_request` |
| 資料庫出錯，或紀錄關閉 | 503 `unavailable` |

payload 可能含租戶的資料，所以回應一律 `cache-control: no-store`，日誌只寫錯誤類別。

## 資料庫

schema `identity`，DDL 在 `migrations/postgres/`，由 migration Job（Laysi/trustiot-v2#60）以 migration 角色依檔名順序套用；執行期程式碼不建表、不改表（`tools/repo-check` 檢查）。檔案清單在 `src/schema.rs`，Job 與服務的 readiness 編譯同一份：資料庫缺少這一版預期的檔案（或套用了別的版本）時 `/readyz` 回 503，日誌寫出檔案（Laysi/trustiot-v2#402，[service-kit 的 migration 檢查](../../crates/service-kit/README.md#migration-檢查migrations)）。角色照 [`crates/service-kit` 的角色與 policy 範本](../../crates/service-kit/README.md)：

| 角色 | 權限 |
| --- | --- |
| `identity_owner`（migration 角色） | 擁有 schema 與所有表 |
| `identity_app`（服務角色） | `identity.users`、`identity.memberships`、`identity.sessions` 的 `SELECT`；`identity.memberships` 的 `INSERT`（只限 `tenant_id`、`user_id`、`tenant_role` 三欄）與 `tenant_role` 一欄的 `UPDATE`（設定成員角色，`0005_member_roles.sql`；不能刪除成員資格，不能改租戶、使用者與建立時間，RLS 只放行交易的租戶）；`identity.sessions` 的 `INSERT`（只限 `id`、`token_hash`、`user_id`、`active_tenant_id`、`expires_at` 五欄：建立時間一律是資料庫的預設 `now()`，7 天上限才從真正的建立時間算起）與 `expires_at`、`revoked_at` 兩欄的 `UPDATE`（登入、延長、登出；不能刪除、不能改使用者與建立時間，trigger 另擋超過 7 天與撤銷後復原）與 `active_tenant_id` 一欄的 `UPDATE`（切換租戶，Laysi/trustiot-v2#497）；`identity.preferences` 的 `SELECT`、只限 `user_id` 的 `INSERT` 與 `language`、`ai_language`、`time_zone`、`theme` 四欄的 `UPDATE`；`identity.login_codes` 的 `SELECT`、`INSERT`、`DELETE` 與 `attempts`、`consumed_at`、`invalidated_at` 三欄的 `UPDATE`；`identity.tenants` 的 `SELECT`、只限 `tenant_id`、`name` 兩欄的 `INSERT`（建立租戶，`0008_tenant_state.sql` 起；之前是整張表）與 `state`、`suspension_reason`、`state_version` 三欄的 `UPDATE`（停權與恢復，Laysi/trustiot-v2#776；trigger 另擋版本不是剛好加一、離開合約終止或 `deleted` 與不是合約終止就刪除，並設定 `state_changed_at`，見[租戶狀態](#租戶狀態)），以及 `certs_applied_version` 一欄的 `UPDATE`（certs 確認套用的版本，`0009_tenant_state_applied.sql`，Laysi/trustiot-v2#779；trigger 另擋改小，CHECK 擋超過 `state_version`，見[停權的處理中](#停權的處理中)），與 `plan`、`entitlement_overrides`、`entitlements`、`entitlements_version` 四欄的 `UPDATE`（方案權益，`0010_entitlements.sql`，Laysi/trustiot-v2#111；trigger 另擋版本不是剛好加一，並設定 `entitlements_changed_at`，見[方案權益](#方案權益)）；outbox 的寫入權限（`outbox_event` 的 `INSERT`、`outbox_aggregate` 的 `SELECT`、`INSERT`、`UPDATE`，由 migration Job 授權，見 [crates/outbox](../../crates/outbox/README.md)）；兩張表都有 RLS，它只碰得到目前租戶的事件與聚合。事件紀錄：`event_log` 的 `SELECT`、`INSERT`（RLS），`platform_event_log` 的 `INSERT`，outbox 消費端的表（Job 授權） |
| `identity_platform`（平台角色） | 事件紀錄的消費端以它讀 `CONSUMES` 每個產生端的 `outbox_event`（Job 授權），後台以它讀 `event_log`（`platform_access` policy，只有 `FOR SELECT`）與 `platform_event_log` 的 `SELECT`；列出使用者所屬的租戶時以它讀 `memberships` 與 `tenants`（`platform_access` policy，只有 `FOR SELECT`，Laysi/trustiot-v2#497，見[目前的 session 與切換租戶](#目前的-session-與切換租戶)）；沒有寫入權限，也碰不到 `sessions` 與 `preferences`。chart 綁它的密碼（`trustiot.migrate.platformLogins`，2026-10-21 起，Laysi/trustiot-v2#130） |

| 表 | 內容 | RLS |
| --- | --- | --- |
| `users` | 使用者 ID、平台管理者旗標、登入用的 `email`（唯一，ASCII 小寫，可為 NULL：不能以 email 登入） | 無：使用者跨租戶，不是租戶資料 |
| `login_codes` | 驗證碼的雜湊、使用者、建立與到期時間、嘗試次數、消耗與作廢時間（見[登入](#登入)） | 無：登入時還沒有 session 與租戶，不是租戶資料 |
| `memberships` | `(tenant_id, user_id)` 與角色 `tenant_role`（`viewer`、`editor`、`admin`；不叫 `role`，因為 `tools/sql-check` 把 `UPDATE` 這個名稱的欄位當成切換資料庫角色而拒絕） | 照 `crates/service-kit/sql/tenant-table.sql` 的 `tenant_isolation`，另有只給 `identity_platform` 的 `platform_access FOR SELECT` |
| `sessions` | `token_hash`、使用者、`active_tenant_id`、建立、到期與撤銷時間 | 無 |
| `tenants` | 租戶 ID（整個主鍵）、名稱（1–100 個字元）、建立時間；狀態 `state`、停權原因 `suspension_reason`、版本 `state_version`、進入目前狀態的時間 `state_changed_at`（`0008_tenant_state.sql`，見[租戶狀態](#租戶狀態)）；certs 確認已套用的最大版本 `certs_applied_version`（0 表示還沒有，`0009_tenant_state_applied.sql`，見[停權的處理中](#停權的處理中)）；方案 `plan`、覆寫值 `entitlement_overrides`、權益快照 `entitlements`、版本 `entitlements_version` 與時間 `entitlements_changed_at`（`0010_entitlements.sql`，見[方案權益](#方案權益)） | 照 `tenant-table.sql` 的 `tenant_isolation`，另有只給 `identity_platform` 的 `platform_access FOR SELECT` |
| `preferences` | 使用者（主鍵，外鍵到 `users`，`ON DELETE CASCADE`）、`language`、`ai_language`、`time_zone`、`theme`，各有 CHECK（`0007_preferences_and_tenant_switch.sql`，見[偏好設定](#偏好設定)） | 無：偏好屬於使用者，不是租戶資料 |
| `outbox_event`、`outbox_aggregate` | 這個服務寫出的事件與每個聚合的序號（由 Job 建立，見 [crates/outbox](../../crates/outbox/README.md)）；`tenant` 聚合屬於該租戶 | outbox 範本的 policy（`outbox-event.sql`、`outbox-aggregate.sql`） |
| `event_log` | 租戶事件的紀錄（`0003_event_log.sql`，見[事件層與操作紀錄](#事件層與操作紀錄)）：信封的欄位、aggregate、`actor_user_id` 或 `actor_service`、`payload`、`recorded_at`；主鍵 `(tenant_id, event_id)` | `tenant_isolation`，另有只給 `identity_platform` 的 `platform_access FOR SELECT` |
| `platform_event_log` | 全平台事件的紀錄，欄位同上但沒有 `tenant_id` | 無：不是租戶資料 |
| `platform_admin_bootstrap` | 第一位平台管理員建立過的標記：最多一列（`singleton`、使用者 ID、時間，`0006_platform_admin_bootstrap.sql`），只有 `identity_owner` 碰得到（見[第一位平台管理員](#第一位平台管理員)） | 無：不是租戶資料 |
| `outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version` | 事件紀錄的消費端的進度、已處理與放棄的事件 ID（由 Job 建立，因為 `CONSUMES` 不是空的） | 無：不是租戶資料 |
| `trustiot_migrate_applied` | migration Job 在這個 schema 套用過的檔案（component、檔名、SHA-256），由 Job 寫入；`identity_app` 只能 `SELECT`，給 readiness 的 migration 檢查用 | 無：不是租戶資料 |

為什麼 `sessions` 不是租戶資料：查 session 時還不知道租戶（只有 cookie），使用者也可以切換目前的租戶，所以 session 屬於使用者，沒有 `tenant_id`。它記的租戶叫 `active_tenant_id`，不叫 `tenant_id`，連線檢查與 `tools/sql-check` 才不會把它當成租戶欄位。角色一律在那個租戶的 RLS 交易裡從 `memberships` 讀。

讀角色用 `BackgroundTenantDb`，這是 kit 規則的例外：kit 只把它交給背景工作，handler 只拿 `TenantDb`。這裡的租戶來自剛從資料庫讀出的 session 列，不來自請求，所以由 `PgSessionStore` 私下持有 `BackgroundTenantDb`，只在 `lookup` 裡使用，ext_authz handler 拿不到它。

建立租戶是第二個例外：新租戶的交易沒有請求可以提供租戶（token 是平台管理者的，沒有那個租戶）。`TenantStore` 有自己的連線池（服務角色 2 條連線，`POOL_CONNECTIONS`，和 session 查詢的連線池分開，大量建立租戶不會佔走 ext_authz 等著用的連線），私下持有它的 `BackgroundTenantDb`；唯一的方法 `create` 自己產生租戶 ID，所以請求無法選擇交易的租戶，handler 也拿不到 `BackgroundTenantDb`。同一個連線池給 handler 的 `TenantDb`（`TenantStore::connect_lazy_shared`）交給使用者自己的帳號 API 共用：它只能以請求 token 的租戶開交易。

設定成員角色是第三個例外（Laysi/trustiot-v2#583），也是唯一以**請求指定、但 token 沒有的租戶**開交易的地方：平台管理者的 token 沒有那個租戶，交易的租戶取自路徑。`MemberStore` 由 `TenantStore::members` 產生，共用同一個連線池與 `BackgroundTenantDb`（兩者都是平台管理者少量的寫入，不另外佔每個副本的連線數），並私下持有它；唯一的方法 `set_role` 在開交易之前先確認是平台管理者本人的請求（`is_own_platform_admin`：平台管理者，而且 token 沒有 `act`），handler 在讀 body 之前也先判斷一次，所以其他人無法讓服務在任何租戶開交易。交易裡只做三件事：確認租戶存在、以 email 找使用者（`users` 不是租戶資料）、寫那一筆成員資格與事件；RLS 讓它碰不到其他租戶的列。

讀取與設定租戶狀態是同一類的例外（Laysi/trustiot-v2#776）：`TenantStateStore` 由 `TenantStore::states` 產生，共用同一個連線池與 `BackgroundTenantDb` 並私下持有；兩個方法 `read`、`set` 在開交易之前先確認是平台管理者本人的請求（`Requirement::OwnPlatformAdmin`），handler 在讀 body 之前也先判斷一次。交易裡只讀、鎖、改路徑租戶的那一列並寫事件。certs 的確認（Laysi/trustiot-v2#779）不是例外：它和事件紀錄的消費端一樣，在事件信封的租戶交易裡寫，租戶由 outbox 的消費端設定，不取自請求。

## 連線池

開一條新的資料庫連線（TCP、TLS、SCRAM、service-kit 的 session 檢查）在負載高的節點或較慢的網路上可能超過檢查的 0.4 秒。2026-10-11 的獨立驗證（[報告](../../docs/test-reports/2026-10-11-61-verify.md)的 F1）發現：原本取得連線的逾時也是 0.4 秒，開到一半的連線跟著檢查一起被取消，這種環境下連線池永遠補不到連線；readiness 又只要一條連線，就緒之後的第一批檢查仍會 503。現在的作法（`src/session.rs`）：

- **檢查不等新連線**：連線池常駐 3 條連線（`WARM_CONNECTIONS`）。一個檢查同時最多用兩條（session 查詢的連線還在歸還時，成員資格的交易已經在另一條開始），保溫工作再用一條，所以一連串的檢查不必開新連線。
- **readiness 跟著連線池**：背景的保溫工作（`PgSessionStore::keep_warm`）每 2 秒一輪，每輪確認 3 條不同的連線都還能用：
  1. 先把這一輪開始時閒置的每條連線取出來，跑一次 `SELECT pg_backend_pid()` 再還回去。取的方式（sqlx 的 `try_acquire`）不先 ping，所以斷掉的連線不會被悄悄重開，而是由這一輪發現。連線池的閒置連線先進先出，還回去的排到最後，所以開始時閒置的每一條，不是被這一輪、就是被檢查（sqlx 取用前會先 ping）取到過。
  2. 再一條一條取，直到 3 條不同的連線（以 backend PID 區分）都回答過：連線池不到 3 條就同時開缺的條數，連線都在檢查手上或還在開，就每 50 ms 再看一次。

  保溫工作同時只拿一條，而且還回去之後才拿下一條，所以檢查要用的兩條一直空著。一輪確認了 3 條，`/readyz` 才回 200，所以入口只把檢查送給連線已經開好的副本。發現斷掉的連線時，readiness 當下回 503，那條連線關掉，由連線池的 `min_connections` 在背景補開，等這一輪確認了 3 條新的才回 200。資料庫連不上、帳密錯、session 檢查拒絕，或一輪超過 10 秒（`WORK_LIMIT`）時，這一輪失敗，readiness 回 503。readiness 改變時記一行日誌（開好 INFO；失敗或發現斷線 WARN 附錯誤），不含密鑰。
- **保溫的負擔**：每個副本每輪對每條閒置連線做一次 `SELECT pg_backend_pid()`，sqlx 還連線時再 ping 一次（一來一回，不執行查詢）。連線池是常駐的 3 條時，每個副本每秒約 1.5 個查詢加 1.5 次 ping（原本每輪各 1 次），連線池因尖峰多開的連線也每輪各一次，直到 30 分鐘的壽命到期。這些都是不讀表的查詢，對資料庫可以忽略。保溫工作不多開連線：同時最多拿一條，只在連線池不到 3 條時才開新的。
- **開連線不受檢查期限限制**：取得連線（含開新連線）的逾時是 5 秒（`CONNECT_TIMEOUT`）。每次查詢在自己的 task 裡跑：檢查到 0.4 秒就回 503，但 task 不被取消，開到一半的連線會開完，留給下一個檢查。同時在跑的查詢每條連線最多 4 個（`IN_FLIGHT_PER_CONNECTION`，預設 10 條連線就是 40 個），超過就直接回 503；每個查詢與每輪保溫最多 10 秒（`WORK_LIMIT`），資料庫卡住時 task 與連線不會一直被佔住。
- **連線的壽命**：沿用 sqlx 的預設：存活超過 30 分鐘的連線在歸還時關閉，閒置超過 10 分鐘的由連線池的定期清理關閉；替補的連線重新經過 service-kit 的 session 檢查，由連線池的 `min_connections`（3）在背景開，不在檢查裡開。不把壽命設成無限，是為了讓資料庫角色或 policy 改壞時，最多 30 分鐘內就會被 session 檢查擋下。
- **延長 session 不搶檢查的連線**：ext_authz 的連線池只按檢查與保溫的需要計算，所以背景的 session 延長（見 [session](#session)）在檢查自己的查詢之後才開始，而且只用當下閒置的連線（`try_acquire`，不等、不開新連線）；沒有閒置的連線就跳過這次（記 DEBUG），由下一個請求再延長。延長的步距是 10 分鐘、閒置期限是 12 小時，跳過幾次不會讓使用中的 session 過期。

剩下的情況：資料庫換主或斷線時舊連線全部失效。從斷線到下一輪保溫（最多 2 秒），readiness 還是 200，這段期間遇到死連線的檢查要重開連線，新連線超過 0.4 秒時回 503（不帶身分標頭）。保溫工作一發現斷線就讓 readiness 回 503，3 條新連線開好、確認過才回 200（service-kit 會沿用 readiness 的結果最多 1 秒，所以 `/readyz` 看到的改變最多晚 1 秒）。檢查放棄後還在跑的查詢會跑到自己的連線開好、查完為止，換主時湧入的檢查因此會讓連線池暫時多開幾條；這些查詢跑完之前，新的檢查仍可能要等連線。

`tests/ext_authz.rs` 以每條新連線延遲 1 秒的代理重現（Laysi/trustiot-v2#341）：

- `a_slow_database_connect_never_fails_a_check_after_ready`：就緒後的檢查全部 200、沒有開新連線。切斷所有連線並讓代理暫停放行新連線：檢查回 503 且不帶身分標頭，不再送檢查，readiness 也自己轉為 503。代理恢復後 readiness 回到 200，之後的檢查全部 200、沒有開新連線。
- `a_keep_warm_round_replaces_every_dropped_connection`：切斷所有連線後不送任何檢查，保溫工作自己發現斷線（WARN），換掉全部 3 條（代理多接到至少 3 條連線），再記 INFO、readiness 回 200；之後的檢查全部 200、沒有開新連線。原本一輪只 ping 一條連線的作法換掉一條就繼續回報就緒，readiness 不會失敗，通不過這個測試。

**每個副本的連線數**：ext_authz 之外的工作各有自己的小連線池，一種請求的尖峰用不到另一種要的連線：

| 連線池 | 誰用 | 連線數 |
| --- | --- | --- |
| ext_authz（`PgSessionStore`） | ext_authz 檢查、保溫工作、session 延長 | `TRUSTIOT__DATABASE__MAX_CONNECTIONS`，預設 10 |
| 租戶（`tenants::POOL_CONNECTIONS`） | 租戶 API、設定成員角色、租戶狀態的讀取與停權、恢復、方案權益的讀取、設定與補上第一版（見[方案權益](#方案權益)）；使用者自己的帳號（讀取目前的 session、切換租戶、偏好，見[目前的 session 與切換租戶](#目前的-session-與切換租戶)與[偏好設定](#偏好設定)）的查詢 | 2 |
| 登入請求（`login::POOL_CONNECTIONS`） | `verify_code`、登出、登出所有裝置 | 4 |
| 寄信（`login::SEND_POOL_CONNECTIONS`） | 發驗證碼的 worker（發碼、作廢沒寄出的驗證碼）、每小時刪除舊的驗證碼 | 2 |
| run 委派（`delegation::POOL_CONNECTIONS`） | 建立 run 委派與每次換 token 時查 session 與成員資格（見[agent 執行期間的委派](#agent-執行期間的委派)） | 4 |
| 事件紀錄的服務角色（`event_log::SERVICE_CONNECTIONS`） | 事件紀錄的消費端與停權確認的消費端寫入（每個消費端一條，`event_log::consumers()`）、租戶的事件紀錄 API | 5 |
| 事件紀錄的平台角色（`event_log::platform_connections()`） | 消費端讀各產生端（目前三個：`identity`、`assets`、`certs`）的 outbox、停權確認的消費端讀 `certs` 的 outbox（見[停權的處理中](#停權的處理中)）、後台的事件紀錄 API、列出使用者所屬的租戶、列出還沒有權益快照的租戶（每 10 分鐘一次，見[方案權益](#方案權益)）、寫 AI 模型角色的變更事件（以 `identity_platform` 連線，見[事件層與操作紀錄](#事件層與操作紀錄)、[目前的 session 與切換租戶](#目前的-session-與切換租戶)與[AI 模型角色](#ai-模型角色)） | 消費端數（`CONSUMES` 數加 1）加 2，目前 6 |

寄信的連線池與登入請求的分開，所以大量的 `send_code` 不會讓 `verify_code` 與登出等連線、逾時回 503。worker 同時寄 4 封，但每封只在發碼的短交易裡用一條連線，寄信的期間不佔連線，所以 2 條就夠；等連線超過 5 秒（`ACQUIRE_TIMEOUT`）的那一封記 ERROR、不寄，使用者再要一次。

使用者自己的帳號（Laysi/trustiot-v2#497）不另開連線池（2026-10-23，安全審查的 F3：原本另開的 4 條讓只有身分服務換版時，所有連線池加總剛好等於 Postgres 可用的連線數）：服務角色的查詢用租戶 API 的連線池，平台角色讀成員資格用事件紀錄的平台角色連線池。後者有每個消費端各一條、另外 2 條，讀成員資格同時最多 2 個（`current_session::MEMBERSHIP_READS`，等超過 5 秒回 503），所以大量的登入使用者不會讓消費端等連線。代價是大量讀取目前 session 或偏好的請求可能讓平台管理者建立租戶或設定成員角色的請求等連線、逾時回 503，後台讀事件紀錄也和讀成員資格共用那 2 條；平台管理者重試即可，限流在 Laysi/trustiot-v2#113。

預設設定下每個副本最多 33 條：27 條以 `identity_app`、6 條以 `identity_platform` 連線（這兩個角色沒有個別的連線數上限）。`connections_per_replica` 從各連線池的常數算出這兩個數字，是唯一的加總：單元測試確認這裡寫的相同；`tests/chart_connections.rs` 確認 chart 的連線預算（`trustiot.postgres.connectionBudgets` 的 `identity`：`database.maxConnections` 3–100、其他連線池 23 條）在每個 `database.maxConnections` 都等於它，並列出 `start` 開連線池的每個呼叫，所以連線池變大、多一個連線池或多呼叫一次既有的建構函式，而預算沒跟著改時，測試失敗。每多一個消費端（`CONSUMES` 的產生端或另一個訂閱），兩個角色各多一條：2026-10-23 加入 `certs`（Laysi/trustiot-v2#79）後由 30 條變成 31 條（當時服務角色的連線池沒有跟著加）；2026-10-30 加入停權確認的消費端（Laysi/trustiot-v2#779）後是 33 條。所有服務、所有副本的連線池加總要放得進 Postgres 的 `max_connections`（chart 的 `postgres.bundled.parameters.max_connections`，預設值在 `deploy/chart/values.yaml`），換版時新舊副本會同時存在：chart 以所有服務一起滾動計算加總，超過就渲染失敗（Laysi/trustiot-v2#575，見 [chart 的 README](../../deploy/chart/README.md)「連線預算」）。

## 簽章金鑰

金鑰在叢集內產生、公開與輪替，沒有人建立、看到或複製它（Laysi/trustiot-v2#404；設計、比較過的做法與每個失敗情況在 [docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「內部身分簽章金鑰」，操作在[密鑰 runbook](../../docs/runbooks/secrets.md)）。

- **誰產生**：身分服務 pod 的 init container `signing-keys`（migration 映像的 `migrate identity-signing-keys`，`tools/migrate/src/signing_keys.rs`，格式與排程規則在 `crates/identity-keys`）。它在每次身分服務的 pod 啟動時執行（依金鑰年齡輪替的 CronJob `<release>-identity-key-rotation` 也定期執行同一個指令，Laysi/trustiot-v2#429），以身分服務 ServiceAccount 的 projected token 讀寫 Secret `<release>-identity-signing-keys`（私鑰）與 ConfigMap `<release>-internal-identity-keys`（公鑰的 JWK Set，每個服務都掛）。這個服務自己的容器沒有 token，chart 強制 `automountServiceAccountToken: false`。
- **怎麼讀**：chart 把 Secret 以唯讀 volume 掛進這個服務（`/etc/trustiot/identity-signing-keys/signing-keys.json`，0440），路徑以 `TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE` 傳入；公鑰檔與其他服務相同，路徑是 `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`。兩個檔案都在啟動時讀，之後每 5 秒重讀（`src/issuer.rs` 的 `Issuer::watch`），所以輪替不必重啟這個服務；kubelet 更新掛載的檔案之前，它照舊用手上的金鑰。檔案不見或不能用時保留上一組可用的金鑰，記 WARN（只寫原因）；讀進來的文字用後清零，錯誤訊息與 `Debug` 輸出只有 kid。某一次重讀 panic 時記 ERROR `periodic job panicked; it runs again next period`（`job` 是 `identity-signing-keys-reload` 或 `internal-identity-keys-reload`），下個週期照常重讀（[service-kit 的「每副本的週期性工作」](../../crates/service-kit/README.md#每副本的週期性工作periodic)）。
- **用哪一把簽**：每個 token 簽章當下，從簽章金鑰檔裡選 `signs_from` 已到、代不小於 `TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION`、而且 kid 與公鑰完全相同地列在這個服務自己看到的公鑰檔裡的金鑰中，`signs_from` 最晚的那一把（`crates/identity-keys` 的 `SigningKeys::signing_key`）。每個副本以同樣的規則讀同樣的資料，所以同時換金鑰；換的時候記一行 INFO（`signing internal identity tokens`，寫出 kid）。一把都沒有時不就緒（readiness 檢查 `signing_key`），ext_authz 檢查回 503，不會簽出服務不接受的 token。
- **兩邊不一致時**：簽章金鑰與公鑰檔的 kid 或公鑰不同，持續超過 `TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS`（chart 的 `internalIdentity.keyPropagationSeconds`，預設 600）時記 ERROR，寫出差在哪幾個 kid 與處理方式（`kubectl rollout restart deployment/<release>-identity` 讓 init container 依 Secret 重寫 ConfigMap）；恢復一致時記 INFO。
- **輪替逾期時**：`TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS`（chart 的 `internalIdentity.maxKeyAgeSeconds`，輪替 CronJob 不渲染時是 0）不是 0，而正在簽章的金鑰的 `published_at` 已超過它加 `TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS` 再加 2 天時，記一次 ERROR（`the internal identity signing key is older than internalIdentity.maxKeyAgeSeconds allows …`，寫出 kid 與公開時間）：輪替的 CronJob 沒在跑或一直失敗。換了金鑰之後才會再記（Laysi/trustiot-v2#429）。

### 金鑰輪替

調大 chart 的 `internalIdentity.keyGeneration` 再升級：身分服務滾動重啟，init container 加一把新金鑰並先公開公鑰，`keyPropagationSeconds` 之後這個服務的每個副本同時改用新金鑰簽章，舊金鑰在它簽的 token 全部過期之後的下一次啟動時移除。私鑰外洩時調 `internalIdentity.revokeBeforeGeneration`：代比它小的金鑰立即移除，新金鑰立即開始簽章，這個服務也從環境變數拿到它，不再用被撤銷的金鑰簽章。其他服務不重啟，它們每 5 秒重讀公鑰檔（[`crates/service-kit` 的「公鑰與輪替」](../../crates/service-kit/README.md)）。步驟與確認方式在[密鑰 runbook](../../docs/runbooks/secrets.md) 的「內部身分簽章金鑰」。

## 設定

設定由 `service_kit::config` 載入，`TRUSTIOT_` 後接英數字的平台設定只由部署決定（kit README 的「設定」）。

| 變數 | 說明 |
| --- | --- |
| `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` | 平台設定：每個服務共用的公鑰檔（JWK Set）；只有列在這裡的金鑰會被拿來簽章 |
| `TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE` | 只給身分服務：掛載的簽章金鑰檔路徑 |
| `TRUSTIOT_IDENTITY_REVOKE_BEFORE_GENERATION` | 只給身分服務：代比它小的金鑰不簽章，預設 0（chart 的 `internalIdentity.revokeBeforeGeneration`） |
| `TRUSTIOT_IDENTITY_KEY_PROPAGATION_SECONDS` | 只給身分服務：簽章金鑰與公鑰檔不一致多久之後記 ERROR，120–86400，預設 600（chart 的 `internalIdentity.keyPropagationSeconds`） |
| `TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS` | 只給身分服務：輪替 CronJob 的金鑰年齡上限，0 到 31536000，預設 0（不檢查）；正在簽章的金鑰超過它加 propagation 再加 2 天時記 ERROR（chart 的 `internalIdentity.maxKeyAgeSeconds`，CronJob 不渲染時是 0） |
| `TRUSTIOT_INGRESS_ROUTES` | 只給身分服務：cookie 路由，決定受眾 |
| `TRUSTIOT_ON_BEHALF_CALLS` | 只給身分服務：token 交換的呼叫清單，JSON `{"<呼叫端>": ["<被呼叫端>", ...]}`，chart 從 `mesh.calls` 推得；沒設時是空的（見 [token 交換](#token-交換代表使用者呼叫)） |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 平台設定：Postgres 位置；只接受 `verify-full`，`_CA_FILE` 必填，是唯一信任的根憑證 |
| `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` | 服務角色 `identity_app` 的帳密：chart 以 `identity_app` 設定帳號，密碼來自 Secret `<release>-identity-password` 的 `identity_app` key；migration Job 產生這個密碼並以它設定角色 |
| `TRUSTIOT__DATABASE__PLATFORM_USERNAME`、`TRUSTIOT__DATABASE__PLATFORM_PASSWORD` | 平台角色 `identity_platform` 的帳密，給事件紀錄與列出使用者所屬的租戶：chart 以 `identity_platform` 設定帳號，密碼來自同一個 Secret 的 `identity_platform` key。兩個一起給或都不給；帳號不能是服務角色。都不給時事件紀錄關閉（見[事件層與操作紀錄](#事件層與操作紀錄)），目前的 session 與切換租戶的 API 回 503（見[目前的 session 與切換租戶](#目前的-session-與切換租戶)） |
| `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | 每個副本 ext_authz 連線池的連線數上限，3–100（至少放得下常駐的 3 條），預設 10；租戶 API、登入與寄信另有自己的連線池（2、4、2 條，見[連線池](#連線池)），事件紀錄與 run 委派也有自己的小連線池，使用者自己的帳號共用租戶 API 與事件紀錄的連線池，都不佔 ext_authz 的連線 |
| `TRUSTIOT__EXCHANGE__LISTEN_ADDR` | token 交換的監聽位址，預設 `0.0.0.0:8082`（chart 的埠 `token-exchange`）；埠被占用時不啟動 |
| `TRUSTIOT_AI_GATEWAY_ADMIN_URL` | 平台設定：AI 閘道的管理 API，`http://<host>[:<port>]`；chart 只給身分服務，指向 Service `<release>-ai-gateway-admin`（Laysi/trustiot-v2#607）。沒有設時照常啟動，記 WARN；不合規則時也照常啟動，記 ERROR；兩種情況 AI 模型角色的 API 都回 503（見[AI 模型角色](#ai-模型角色)） |
| `TRUSTIOT_SMTP_HOST`、`_PORT`、`_SECURITY`、`_FROM` | 平台設定：寄驗證碼的 relay（chart 的 `smtp.host`、`port`、`security`、`from`）。沒有 `TRUSTIOT_SMTP_HOST` 時照常啟動，但記 WARN、`send_code` 回 503；設了它，其他三個就必填，`_SECURITY` 是 `starttls` 或 `tls`，`_FROM` 是 `名稱 <地址>` 或地址，只能一行、ASCII 地址。缺少或不合規則時也照常啟動，記 ERROR、`send_code` 回 503（見[寄信（SMTP）](#寄信smtp)） |
| `TRUSTIOT_SMTP_AUTH_REQUIRED` | 平台設定：`true` 表示 relay 需要帳號，chart 在 `smtp.credentialsSecret.name` 有值時設定；預設 `false`。是 `true` 卻沒有帳密時照常啟動，記 ERROR、`send_code` 回 503，不會不帶帳號寄信 |
| `TRUSTIOT_SMTP_USERNAME`、`TRUSTIOT_SMTP_PASSWORD` | relay 的帳密，兩個一起設或都不設；chart 在 `services.identity.credentials` 預設列 `smtp`，從 `smtp.credentialsSecret` 給（Secret 名稱空白時不給）。引用是 `optional: true`：Secret 不存在時 pod 照常啟動、沒有這兩個變數，由 `TRUSTIOT_SMTP_AUTH_REQUIRED` 報出，而不是讓 pod 停在 `CreateContainerConfigError`、連 ext_authz 都起不來 |
| `TRUSTIOT_SMTP_CA_FILE` | 選用：relay 憑證的 CA 檔（PEM），設了就只信任它；不設時用映像內建的 `/etc/ssl/certs/ca-certificates.crt`。chart 還沒有對應的欄位：地端客戶的 relay 用私有 CA 時要補上 |

啟動後由保溫工作在背景開連線；readiness（健康埠與 API 埠的 `/readyz`）在常駐的 3 條連線開好、而且有一把可以簽章的金鑰之後才通過（見[連線池](#連線池)與[簽章金鑰](#簽章金鑰)），資料庫連不上或沒有可以簽章的金鑰時 ext_authz 回 503、readiness 回 503。token 交換只在自己的埠（預設 8082）。健康埠（預設 8081）只有 `/healthz` 與 `/readyz`，ext_authz 端點只在 API 埠（8080）。設定（SMTP 的設定除外，見[寄信（SMTP）](#寄信smtp)）或路由不能用時以 exit code 1 結束，不開始服務；金鑰檔還沒有內容或不能用時照常啟動，只是不就緒。

## 映像

`deploy/images/identity/image.toml` 宣告這個服務的映像（2026-10-11，Laysi/trustiot-v2#308）：release.yml 把 `identity` 交叉編譯成 `aarch64-unknown-linux-musl` 的靜態執行檔，放進共用的 `deploy/images/rust-binary.Containerfile`（distroless static、UID 65532、`ENTRYPOINT` 是 `/usr/local/bin/identity`），發佈為 `ghcr.io/laysi/trustiot-v2/identity:<版本>`，只有 linux/arm64（[deploy/images/README.md](../../deploy/images/README.md)）。

- 映像裡沒有 shell、私鑰與設定：金鑰由 chart 掛載（身分服務 pod 的 init container 以 migration 映像產生），其餘都是環境變數（見上方[設定](#設定)）。
- 啟動不需要資料庫：設定與路由可用時就開始服務，健康埠的 `/healthz` 回 200，`/readyz` 等連線池開好、有可以簽章的金鑰才回 200（2026-10-11 的驗證時，金鑰還是啟動時讀一次的 PEM 檔）。2026-10-11 在 arm64（lab Mac M1 的 podman）以發佈流程建出的映像、當場產生的金鑰與連不到的資料庫驗證過：`/healthz` 200、`/readyz` 503、沒有 cookie 的 ext_authz 檢查 401（[測試報告](../../docs/test-reports/2026-10-11-308-identity-image.md)）。
- nightly 的 k3d 以 `scripts/nightly/service-image.ts` 的 `loadServiceImage(platform, "identity")` 建出同樣的映像並匯入節點（輸入沒變時從快取載入）；smoke 以它部署身分服務，當作入口的 ext_authz，並經入口建立租戶（Laysi/trustiot-v2#65，`scripts/nightly/api.ts`）。

本機試建：`bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run --output plan.json`，再 `bun scripts/release/build.ts --plan plan.json --out <目錄>`（需要 podman、skopeo 與 `proto install` 裝好的工具，見 [scripts/README.md](../../scripts/README.md)）。

## 本機啟動

本機開發環境（`bun scripts/dev.ts`，[scripts/README.md](../../scripts/README.md) 的「身分服務」）備好了身分服務要的東西（Laysi/trustiot-v2#336）：

- `up`：以 migration Job（`tools/migrate`，不改程式）建立 `identity_owner`、`identity_app` 與 `identity` schema 並套用 `migrations/postgres/`（Job 套用整個產品，`identity_platform` 與資產服務的角色也會建立並綁密碼，與 chart 相同）；產生 Ed25519 簽章私鑰（`<狀態目錄>/identity/signing-key.pem`，0600，沿用已有的），再從它寫出叢集裡 init container 寫的同樣兩個檔案：簽章金鑰檔 `signing-keys.json` 與公鑰檔 `jwks.json`（都是 0600，每次 `up` 重寫）。
- `seed`：建立開發使用者、`admin` 成員資格與一筆 session（寫法照下一節），印出 cookie。
- `env`：印出 `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`、`TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE`（兩個路徑，不是金鑰）、`TRUSTIOT_INGRESS_ROUTES`（`localhost` 的每個路徑給 `assets`）與 `TRUSTIOT_DEV_IDENTITY_DATABASE_{USERNAME,PASSWORD}`、`TRUSTIOT_DEV_IDENTITY_PLATFORM_DATABASE_{USERNAME,PASSWORD}`（事件紀錄的平台角色）。

```sh
bun scripts/dev.ts up && bun scripts/dev.ts seed
set -a; eval "$(bun scripts/dev.ts env)"; set +a
TRUSTIOT__DATABASE__USERNAME="$TRUSTIOT_DEV_IDENTITY_DATABASE_USERNAME" \
TRUSTIOT__DATABASE__PASSWORD="$TRUSTIOT_DEV_IDENTITY_DATABASE_PASSWORD" \
TRUSTIOT__DATABASE__PLATFORM_USERNAME="$TRUSTIOT_DEV_IDENTITY_PLATFORM_DATABASE_USERNAME" \
TRUSTIOT__DATABASE__PLATFORM_PASSWORD="$TRUSTIOT_DEV_IDENTITY_PLATFORM_DATABASE_PASSWORD" \
  cargo run -p identity
curl -i -H 'Host: localhost' -H 'Cookie: __Host-trustiot-session=<seed 印出的值>' \
  http://127.0.0.1:8080/internal/ext-authz/api/anything
```

`/readyz`（健康埠 8081）回 200 之後，有效的 cookie 回 200 與一個 `x-internal-identity`，無效的 cookie 回 401，不在路由表的主機回 403。本機沒有入口 gateway，所以呼叫的人自己帶 `Host` 與 `Cookie`。`TRUSTIOT_DEV_IDENTITY_DATABASE_PASSWORD` 是 Job 為 `identity_app` 產生的密碼，每次 `up` 重新產生。

## 測試工具怎麼建立 session

正式的建立流程是[登入](#登入)。沒有 SMTP relay 的地方（本機開發、nightly 的 smoke、大部分的測試），以 migration 角色直接寫表（`tests/ext_authz.rs` 的 `add_session`；`bun scripts/dev.ts seed`；nightly 的 smoke 以資料庫的 superuser 做同樣的事，`scripts/nightly/api.ts`；它的平台管理員則以 chart 的 CronJob 照 runbook 建立，見下方[第一位平台管理員](#第一位平台管理員)）：

1. 產生 32 bytes 的隨機數，base64url（不補 `=`）就是 cookie `__Host-trustiot-session` 的值。
2. 寫入 `identity.sessions`：`token_hash` 是那 32 bytes（不是 base64 文字）的 SHA-256，另填 `user_id`、`active_tenant_id`、`expires_at`（最多建立後 7 天，trigger 拒絕更長的）。

使用者（含 `email`）與成員資格也是這樣寫入：服務角色不能寫 `users` 與 `memberships`。第一位平台管理員的正式流程是 migration 映像的 `migrate bootstrap-platform-admin`，操作者在叢集內執行一次（Laysi/trustiot-v2#582，見下方[第一位平台管理員](#第一位平台管理員)）；其他使用者與成員邀請在 Laysi/trustiot-v2#105。

## 第一位平台管理員

部署的第一位平台管理員（A2）不經這個服務的任何 HTTP 路由建立：操作者以 chart 的 CronJob `<release>-platform-admin-bootstrap` 建一個 Job，經 `kubectl attach -i` 把地址寫進它的 stdin，Job 以 migration 角色執行 `migrate bootstrap-platform-admin`（[tools/migrate](../../tools/migrate/README.md)，步驟、結束碼與復原在 [docs/runbooks/platform-admin.md](../../docs/runbooks/platform-admin.md)）。

- 一個交易裡：以 `identity_owner` 依地址找使用者（`FOR UPDATE`）、在 `identity.platform_admin_bootstrap` 寫下唯一的一列（主鍵是固定為 `true` 的 `singleton`，`0006_platform_admin_bootstrap.sql`）、把已有的使用者設為平台管理員或建立新使用者，再以 outbox 寫 `identity.platform_admin_granted`（`crates/contract` 的 `events::identity::PlatformAdminGranted`，aggregate `user`/使用者 ID，沒有租戶、沒有 `actor`，payload 只有 `user_id` 與 `via: "bootstrap"`）。事件紀錄的消費端把它收進 `platform_event_log`。
- 一列已經存在時（之前成功過，或同時執行的另一次先 commit），不改任何東西、以結束碼 3 結束，日誌寫出當時那位的 `user_id` 與時間。這一列不跟著使用者刪除或取消平台管理員而消失（沒有外鍵），所以「只有一次」在所有平台管理員都被移除之後仍然成立；要再建立一位，以 superuser 刪除這一列（runbook 的「復原」）。
- 地址以 `src/email.rs` 的同一份規則正規化（去掉前後空白、ASCII 小寫、`is_address`），`tools/migrate` 編譯同一個檔案，所以建立的 `users.email` 一定是登入比對的寫法。地址不寫進日誌、事件與錯誤訊息。
- 平台管理員不會自動成為任何租戶的成員：登入之後以 `POST /api/identity/v1/tenants` 建立租戶。
- `identity_app` 與 `identity_platform` 對 `platform_admin_bootstrap` 沒有任何權限。

## 測試

在 repo 根目錄執行：

```sh
CARGO_BUILD_JOBS=2 cargo test -p identity
```

`tests/ext_authz.rs` 需要 podman 或 docker：起一個帶 TLS 的暫時 Postgres（與 `crates/service-kit` 的測試共用同一份支援程式），每個測試建自己的資料庫、套用 migration、以 `identity_app` 啟動真的 binary（它與 `tests/adversarial.rs` 只套用 `0001_identity.sql`、`0002_tenants.sql`、`0004_login.sql` 與 `0008_tenant_state.sql`：cookie 檢查讀成員角色時也讀租戶的狀態，並代替 Job 寫入這一版每個檔案的紀錄，`tests/support/mod.rs` 的 `record_release_migrations`，readiness 的 migration 檢查才會通過；沒有給平台角色的帳密，所以事件紀錄關閉，ext_authz 不受影響）。`tests/tenants.rs`、`tests/members.rs`、`tests/login.rs`、`tests/event_log.rs`、`tests/platform_admin.rs`、`tests/ai_model_roles.rs`、`tests/cross_tenant.rs`（[crates/testkit](../../crates/testkit/README.md) 的跨租戶負向測試）、`tests/permissions.rs`（權限矩陣）、`tests/tenant_suspension.rs`（停權）與 `tests/entitlements.rs`（方案權益）用 testkit 的 TLS Postgres，schema 由 migration Job 的程式套用（`testkit::product`）。沒有容器時都略過，設了 `CI` 時改為失敗。

| 測試 | 驗證 |
| --- | --- |
| `src/session.rs` | cookie 的解析（混在其他 cookie 與多個 `Cookie` 標頭裡、缺少、重複、非標準寫法），`Debug` 不顯示 secret 與雜湊；`Set-Cookie` 與清除 cookie 的屬性；最長期限與 migration 的 trigger 一致 |
| `src/email.rs` | 地址的格式（拒絕換行、空白與非 ASCII 地址）與正規化（去掉前後空白、ASCII 小寫），`Debug` 不顯示地址；`tools/migrate` 編譯同一個檔案 |
| `src/login.rs` | 驗證碼的格式與產生（每一位數字都出現、`Debug` 不顯示）、驗證碼雜湊是帶金鑰的 HMAC 並綁定金鑰、用途與地址（與 OpenSSL 算出的值比對）、`redirect_to` 接受與拒絕的寫法、請求的解析（`password` 等多餘欄位、未知的 `step`、重複的鍵都拒絕）、信件只在內文帶驗證碼 |
| `src/mail.rs` | 寄件者的格式、標頭的編碼、信件的組成與 dot-stuffing、SMTP 回應的解析、錯誤訊息只有步驟與回應碼、`Debug` 不顯示信件 |
| `tests/login.rs` | 真的服務、產品 schema 與測試起的假 SMTP relay（`tests/support/smtp.rs`，測試 CA 簽的 TLS，地址都在 example.com）：完整流程（202、經 STARTTLS 與 AUTH 寄出、200 與 cookie 屬性、12 小時的 session、通過 ext_authz；資料表裡的不是不帶金鑰的 SHA-256），重放、過期、猜錯 5 次、別的地址的驗證碼、新驗證碼作廢舊的都被拒，4 個同時送出只有一個成功；網址帶驗證碼回 400 且不算嘗試；TRACE 等級的日誌與回應都沒有驗證碼、地址、cookie 與 relay 密碼；`redirect_to` 指到別的網域都 422 且不算嘗試；登出與登出所有裝置後下一個請求 401、別人的 session 不受影響、重新登入撤銷舊 session；角色變更與移除成員在下一個請求生效；寄送限制與不存在的地址（同樣的 202、不寄），同一個地址 8 個同時送出只寄一封；relay 不提供 STARTTLS、STARTTLS 後注入、憑證不是設定的 CA 簽的都不寄並作廢驗證碼；沒有 relay 時 503，設定不完整（含 `TRUSTIOT_SMTP_AUTH_REQUIRED` 是 `true` 卻沒有帳密，chart 的 Secret 不存在時就是這樣）時照常服務、記 ERROR 且不印出值、`send_code` 回 503；延長期限與 7 天上限；兩步的跨站請求（`Sec-Fetch-Site: cross-site`、`same-site`、沒有 `Sec-Fetch-Site` 時不符的 `Origin`）都回 403 `security.cross_site_request` 且什麼都沒改變（沒有驗證碼列、沒有寄信、不算嘗試、沒有新 session、瀏覽器原本的 session 不被撤銷，日誌只有標頭名稱），跨站的登出被 cookie 檢查擋下，同源的請求（`Sec-Fetch-Site: same-origin` 或相符的 `Origin`）照常登入；路由測試（只有 OpenAPI 列出的端點，沒有密碼、註冊與假登入，cookie 開不了）；編譯期檢查（沒有 Cargo feature、optional dependency、build script 與依建置設定或建置環境改變的程式） |
| `src/issuer.rs` | token 的標頭與 claims 剛好是約定的格式、只有受眾驗得過、過期後被拒；`kid` 的判斷；公鑰沒部署時拒絕；錯誤與 `Debug` 不含私鑰；驗證碼的金鑰與 run 委派的 MAC 金鑰由簽章金鑰導出（換簽章金鑰就換、兩者不同、切換前的仍可驗證、撤銷的代與沒有金鑰時沒有）；ext_authz 簽的 token 帶 session 的 `sid` |
| `src/delegation/tests.rs` | run 委派（Laysi/trustiot-v2#551），以假的 store 控制 session 與成員資格：10 分鐘前建立的委派（請求的 token 早已過期）仍換得到受眾正確、被呼叫端接受的 60 秒 token，`act` 是 assistant、沒有 `sid`，每次建立與換 token 都查一次 store；委派先到期時 token 跟著到期；session 結束後換 token 與建立都 401；過期、在未來簽的、效期超過 900 秒（MAC 正確也一樣）的委派 401，900 秒剛好可以，超過上限的效期不簽；角色改變時下一張 token 帶新角色與管理者旗標，移出租戶後 403；換出的 token（兩種來源）不能建立委派、不能當委派換、也不能在 token 交換再換，委派不被任何服務的驗證端與 token 交換接受；只有清單上的受眾與 `HOLDERS` 的服務換得到；冒用與重播：別的私鑰、給別的受眾、別的委派、別的時間的 proof，±30 秒外的 proof，空的、不是 base64url、長度不對的 proof，換掉公鑰、別的服務金鑰、改過 MAC、改前綴的委派，撤銷的代，重複或缺少的標頭都 401；換簽章金鑰後舊金鑰簽的委派仍可用；壞 token、沒有 `sid`、壞本文、small-order 公鑰、超過 1 KiB 400 或 401，`GET` 405；沒有公鑰、沒有簽章金鑰、資料庫失敗 503；未知欄位的 payload 被拒；以 service-kit 的 `RunDelegations` 端到端呼叫：被呼叫端看到同一個使用者與 `act`，清單外 `permission_denied`，session 結束後 `unauthenticated` |
| `tests/ext_authz.rs` 的 run 委派 | Laysi/trustiot-v2#551，真的服務與 Postgres：ext_authz 給 assistant 的 token 建立委派，在 token 交換的埠換到 certs 接受的 60 秒 token（同一個使用者、`act` 是 assistant），清單外 403，別的私鑰 401；以服務自己的測試金鑰簽一個 10 分鐘前建立的委派，仍換得到 token；改角色後下一張帶新角色，刪除成員資格後 403、加回後恢復；撤銷 session 後兩個委派都 401；日誌有建立的 INFO，沒有委派與 token |
| `src/audience.rs` | `Exact` 優先與最長 `PathPrefix`、主機比對；不能用的路由設定拒絕啟動且不引用設定值 |
| `src/csrf.rs` | 跨站請求的判斷：`Sec-Fetch-Site` 的每個值、重複與合併的值，`Origin` 的主機、埠號、scheme 與格式，沒有標頭時放行 |
| `src/exchange/tests.rs` | token 交換（Laysi/trustiot-v2#106）：清單上的呼叫端換到同一個使用者、租戶、角色的 token，到期不晚於原 token；重播：受眾不是驗證端的 token 被每個服務拒絕，過期的 token 換不到；只有清單上的組合換得到；換來的 token 帶 `act`，即使清單上有對應的呼叫也不能再換（只換一跳）；缺少、重複、偽造的 token 401，token 先於本文檢查，各種壞本文 400，`GET` 405，沒有公鑰或簽章金鑰 503；清單的解析錯誤不引用設定值；以 `OnBehalf` 端到端呼叫：被呼叫端看到的是同一個使用者 |
| `src/ext_authz/tests.rs` | 以假的 session store 驗證回應表的每一列、跨站請求在查 session 之前回 403（查詢次數不變）、偽造的身分標頭不影響 200、store 失敗或卡住時 0.4 秒內回 503、各種方法與路徑形狀、只服務這個端點 |
| `tests/ext_authz.rs` | 真的服務與 Postgres：有效 session 拿到一個通過 service-kit 驗證的 token，偽造的同名標頭不會出現在回應；401 的各種情況（含過期、撤銷、使用者刪除）；跨站的寫入不論 session 有效與否都回 403 `security.cross_site_request`，同源的照常放行；改角色與移除成員在下一個請求生效；服務角色只能照登入的規則寫 session（建立、延長到 7 天內、撤銷，不能刪除、轉給別人、復原撤銷或關掉 trigger）、不能寫使用者與成員，RLS 擋住其他租戶的成員資料；私鑰、session secret 與 token 不出現在日誌與回應；健康埠只有 `/healthz`、`/readyz`；資料庫連不上時回 503；開新連線比檢查期限慢時，就緒後的檢查不失敗、斷線後自動恢復；金鑰與設定不能用時拒絕啟動（含只給平台角色帳密的一半、平台角色就是服務角色） |
| `tests/ext_authz.rs` 的 token 交換 | Laysi/trustiot-v2#106，真的服務：以 `TRUSTIOT_ON_BEHALF_CALLS` 啟動，在 token 交換自己的埠把 ext_authz 給 assets 的 token 換成給 certs 的，certs 的驗證端接受、是同一個使用者與租戶、到期不晚於原 token，assets 與 alerts 的驗證端拒絕它；清單外的組合 403、沒有或偽造的 token 401；清單含 `ai-gateway`、不是 JSON、以 `TRUSTIOT__PLATFORM__ON_BEHALF_CALLS` 覆寫時拒絕啟動 |
| `tests/adversarial.rs` | 驗證者另寫的攻擊測試（真的服務與 Postgres）：cookie 解析的各種變形、以資料庫時鐘判斷到期與撤銷、資料庫被鎖、連線被殺、帳密錯誤時回 503 而不放行、成員資料的租戶隔離、各種日誌等級都不含密鑰、金鑰載入、健康埠、跨重啟的金鑰輪替 |
| `tests/token_claims.rs` | 驗證者另寫：竄改、改簽、改受眾、改時間的 token 都過不了 service-kit 的驗證器，輪替期間舊 token 在效期內仍有效（不需要資料庫） |
| `src/preferences.rs` | 偏好的允許清單（語言與主題大小寫要相同、時區是 `auto` 或 tzdb 的名稱、`Factory` 與路徑形狀的值被拒）、服務接受的每個時區名稱都通過 migration 的 CHECK、預設值與清單和 migration 相同、`PATCH` 的 body（`null`、未知欄位、陣列、重複的欄位被拒）、每個不合的欄位各一個錯誤 |
| `src/current_session.rs` | 角色的顯示、回應的欄位、切換的 body 只能是一個帶 `tenant_id` 的 JSON 物件；API 文件把 session 與租戶的每個欄位都列為 `required`（`email`、`active_tenant`、`name` 可能是 `null` 但一定出現）；讀成員資格的上限加上消費端的連線不超過事件紀錄的平台角色連線池 |
| `src/lib.rs` | 從各連線池的常數算出每個副本以兩個角色最多開幾條連線，和[連線池](#連線池)寫的數字與表格的每一列相同 |
| `tests/chart_connections.rs` | chart 的連線預算（`deploy/chart/templates/_postgres.tpl`，Laysi/trustiot-v2#575）：`database.maxConnections` 的範圍就是服務接受的 3–100，範圍內的每個值，chart 算的每副本連線數都等於 `connections_per_replica`；程式裡每個 `.max_connections(...)`（`testkit::chart::pool_size_calls`）與 `start` 開連線池的每個呼叫（`connect_lazy*`、`start_event_log`）都是列出的那些（不需要資料庫） |
| `src/tenants.rs` | 租戶名稱的規則（去空白、長度以字元計、控制字元、格式字元與行／段落分隔符）；只接受 JSON 的 `Content-Type`（可帶參數，表單、`text/plain`、相近的型別與兩個 `Content-Type` 都拒絕） |
| `tests/tenants.rs` | 真的服務與 migration Job 套用的產品 schema：平台管理者的 session 經這個服務的 ext_authz 換成 token 後建立租戶得到 201，列、`identity.tenant_created`、Starter 第 1 版的權益快照與 `identity.tenant_entitlements_changed`（actor 同一位平台管理者）在同一個交易（同一個 `xmin`）；撤銷寫 outbox 的權限或寫權益欄位的權限時都回 503 且沒有留下列；不是平台管理者 403（租戶的 `admin` 也是）、沒有 token 401、平台管理者給資產服務的 token 401、沒有 `Content-Type` 或是表單與 `text/plain` 400、壞的 body 400、壞的名稱 422，都不建立租戶；`application/json; charset=utf-8` 201；日誌不含租戶名稱 |
| `tests/platform_admin.rs` | Laysi/trustiot-v2#582，真的服務、migration Job 套用的產品 schema 與假 SMTP relay：`migrate::platform_admin::bootstrap`（`migrate bootstrap-platform-admin` 的程式）以大小寫混雜、帶空白的地址建立第一位平台管理員，`platform_event_log` 收到剛好一筆 `identity.platform_admin_granted`（payload 只有 `user_id` 與 `via`、沒有 `actor`、沒有地址），沒有任何成員資格；這個使用者以驗證碼登入、經 ext_authz 換到 token，`POST /api/identity/v1/tenants` 201；再執行一次回報同一個使用者、什麼都不改；服務的日誌沒有地址。指令本身（結束碼、同時執行、復原）在 `tools/migrate/tests/platform_admin.rs` |
| `src/members.rs` | 角色只接受 `viewer`、`editor`、`admin` 三種寫法（`unrecognized`、大小寫不同都拒絕），與 token、事件、資料表的寫法相同；body 的每個錯誤欄位都列出，body 的 `Debug` 不印 email；只有平台管理者本人的請求（沒有 `act`）可以設定，租戶的 `admin` 不行；OpenAPI 列出每個回應 |
| `tests/members.rs` | Laysi/trustiot-v2#583，真的服務與 migration Job 套用的產品 schema：平台管理者設定之後，成員下一個經 ext_authz 的請求帶新角色（新成員 201、改角色 200、同樣的角色 200 且什麼都不寫）；每次變更在成員資格的租戶寫一筆 `identity.member_role_changed`（actor 是平台管理者、原本與新的角色、沒有 email），租戶 `admin` 讀到的操作紀錄有這兩筆，另一個租戶沒有；租戶 `admin`、別的租戶的 `admin`、沒有租戶的使用者、平台管理者經其他服務換發的 token 都 403，對不存在的租戶、壞的 body 與超過上限的 body 也是 403（先於讀取），成員資格、使用者與 outbox 都不變；不存在的租戶 404、不存在的 email 422，不寫入、不建立使用者；壞的租戶 ID、非 JSON 的 `Content-Type`、壞的 body 與超過 4096 位元組的 body 400，壞的 email 與角色 422；同時兩個請求建立同一個成員資格，一個 201、一個 200，事件前後相接；服務角色在租戶 A 的交易裡不能新增租戶 B 的成員資格、看不到也改不了它，也不能改成員資格的租戶、使用者、建立時間或刪除；日誌有設定與拒絕、沒有 email |
| `src/tenant_state.rs` 與 `src/state_applied.rs` | Laysi/trustiot-v2#776、Laysi/trustiot-v2#779：狀態與原因的寫法與事件、migration 的 CHECK 相同；body 的規則；合約終止與 `deleted` 不能改變；`status` 的規則（`active` 不看確認，停權與刪除在確認的版本小於 `state_version` 時是 `processing`）；OpenAPI 列出每個回應、`TenantStatus` 的四個值；確認的訂閱讀的是 `CONSUMES` 裡的 `certs`、名稱與事件紀錄的訂閱不同 |
| `src/entitlements.rs` | Laysi/trustiot-v2#111：方案名稱與 migration 的 CHECK、事件的方案相同；每個方案的預設值都在範圍內、較高的方案不少於較低的、SaaS 方案的可查期間不超過保存期限；[方案與權益](../../docs/product/plans.md)「權益清單」的表與 `Plan::defaults` 逐格相同；覆寫值優先於方案預設，只存設定的欄位；`PUT` 的 body 必須有整數 `expected_version`、不接受未知欄位；不認得的方案、未知或型別錯誤的覆寫值、非 Enterprise 的覆寫值與超出範圍的覆寫值各回自己的欄位與 key；存的列讀不懂時拒絕；OpenAPI 列出每個回應 |
| `tests/entitlements.rs` | Laysi/trustiot-v2#111，真的服務與 migration Job 套用的產品 schema：服務啟動前以兩欄寫入的舊租戶（版本 0）在啟動後得到 Starter 第 1 版，事件的 actor 是身分服務；新租戶是 Starter 第 1 版，平台與成員的 API 都讀得到；升級、相同請求（三種寫法）不寫入、Enterprise 帶覆寫值（覆寫值優先，成員也看到）、拿掉覆寫值、再加回，事件逐筆是完整快照與版本 1–5、actor 是平台管理者；依舊版本送出的變更（內容不同、內容相同、版本比目前大）都回 409 `entitlements.version_conflict` 與 `current_version`，列與 outbox 都不變；有 5 個成員的租戶降到上限 3 的 Starter 後成員都還在、仍能改成員的角色；成員只讀得到自己的租戶，換發的 token 也可以；4 個依同一個版本同時送出的相同升級只有一個 200，其餘 409，只產生一個版本與一個事件；租戶的 viewer 與 admin、換發的平台管理者 token 403，沒有 token 與給別的服務的 token 401，沒有租戶 403、停權的租戶 403 `tenant_suspended`，不存在的租戶 404，壞的租戶 ID、非 JSON、超過 8192 位元組與壞的 body（包括沒有或不合法的 `expected_version`、打錯成 `override` 與其他未知欄位）400，不認得的方案、壞的覆寫值、非 Enterprise 的覆寫值、超出範圍 422，都什麼都沒改；superuser 也改不了 `entitlements_changed_at`；服務角色只能照規則改四欄（改時間、指定方案或版本的 INSERT 42501，版本不是剛好加一 42501，非 Enterprise 的覆寫值、不認得的方案、拿掉快照、覆寫值不是物件 23514），在 A 的交易讀不到也改不了 B；日誌有變更與補上第一版 |
| `tests/state_applied.rs` | Laysi/trustiot-v2#779，真的服務、migration Job 套用的產品 schema，`certs_app` 以 `outbox::publish_as` 在租戶交易裡寫 `certs.tenant_state_applied` 當 certs 的替身：停權後讀到 `processing`，收到同版本的確認後 `suspended`；版本 1、前一個停權、恢復的版本等較舊的確認都不會結束較新的停權；恢復在回應與之後的讀取都立即是 `active`；確認亂序到達（4、2、3）時存的版本停在 4；版本超前、小於 1、不存在的租戶進死信且什麼都不改（超前的確認之後真的停權到那個版本仍是 `processing`，以 `replay_from` 重新處理死信也一樣，certs 之後寫的確認才讓它變成 `suspended`）；全平台的 certs 事件被略過、不進死信；事件紀錄記下每個確認（actor 是 certs）；`identity_app` 把確認的版本改小與 superuser 改小都 42501、超過 `state_version` 23514、在 A 的交易改不到 B 的列 |
| `src/ai_model_roles/tests.rs` | AI 模型角色（Laysi/trustiot-v2#608）：只有 HTTP 狀態決定成功（2xx 而 body 讀不懂、非 2xx 而 body 像成功都不是成功）；AI 閘道的每個錯誤碼對應的 M1 回應，修改與重設結果不明時是 504 `timeout` 帶 `ai_model_role.outcome_unknown`（不是入口會重試的 503）、讀取是 503；錯誤碼與狀態不符或未知的錯誤碼不當成那個錯誤；日誌依嚴重度、只寫種類；AI 閘道的角色轉成 API 的型別（時間解析成 UTC）；body 只能是帶 `models` 與 `expected_version` 的 JSON 物件；只有平台管理者本人的請求通過（租戶 `admin` 與帶 `act` 的 token 不行）；跨站與非 JSON 的修改被拒；不可能存在的角色名稱不轉送；OpenAPI 列出三條路由、每個回應，回應的每個欄位都是 `required` |
| `src/ai_model_roles/admin_client.rs` | 管理 API 的 client：只接受 `http://<host>[:<port>]`（其他 scheme、帳密、路徑、query、fragment 都拒絕，錯誤不引用值）；連不上與同時呼叫已滿時什麼都沒送；不合格的 actor 與 `/internal/admin/` 以外的路徑不送出；超過上限的回應當作沒有回應 |
| `tests/ai_model_roles.rs` | Laysi/trustiot-v2#608，真的服務、migration Job 套用的產品 schema，與測試起的假 AI 閘道管理 API（記下收到的每個請求，照 AI 閘道的規則回答，或依步驟回指定的狀態、或一直不回）：沒有 token 401，租戶 `admin`、沒有租戶的使用者、帶 `act` 的 token 在三條路由都 403，假 AI 閘道什麼都沒收到；平台管理者讀取、修改、重設得到假 AI 閘道的內容，假 AI 閘道收到的 `ai-gateway-actor` 是 token 的使用者（用戶端偽造的被換掉），沒有 cookie、token、`Authorization`，body 只有兩個欄位；協定不符、未知的供應商、重複的模型 422 各帶自己的 key，什麼都沒改；舊版本 409 帶現在的版本，沒有的角色 404；每次變更在 outbox 寫一筆沒有租戶、actor 是平台管理者的事件，後台的操作紀錄看得到，重設沒有存模型的角色不寫；跨站（ext_authz 與 handler 都擋）、非 JSON 的 `Content-Type`、0 或 9 個模型、超過 16 KiB、負的版本、不可能存在的角色名稱都在送出前拒絕；500、502、504、AI 閘道的 503、未知的錯誤碼、讀不懂的 2xx 時，修改與重設都回 504 `ai_model_role.outcome_unknown`（從不回 503）、讀取回 503，每個寫入剛好送一次、不寫事件；撤銷平台角色寫 outbox 的權限時變更仍回 200 並記 ERROR；假 AI 閘道不回時 4 個變更等著、第 5 個立刻 503 不送出、ext_authz 照常回 200，等著的在 client 的時間上限之後回 504 `ai_model_role.outcome_unknown`；沒有設定與連不上的位址三條路由都 503；日誌沒有 cookie、token 或偽造的 actor |
| `tests/migrations.rs` | 真的服務與 migration Job 套用的產品 schema（Laysi/trustiot-v2#402）：Job 寫的紀錄就是 `src/schema.rs` 預期的清單（outbox 的兩個範本與 `migrations/postgres/` 的每個檔案），服務 ready 並記 INFO；以 superuser 拿掉 `0002_tenants.sql` 的紀錄時，新的行程 503，ERROR 日誌的 `missing` 寫出它，已經通過的行程維持 ready；紀錄回來後同一個行程轉為 ready；`outbox-aggregate.sql` 的 SHA-256 不同時 503，`changed` 寫出它；`identity_app` 讀得到自己的紀錄、讀不到 `trustiot_migrate.applied` 與 `assets` 的紀錄，也不能新增、修改、刪除自己的紀錄 |
| `tests/account.rs` | 真的服務與產品 schema（含平台角色），每個請求先經這個服務的 ext_authz（Laysi/trustiot-v2#497）：目前的 session 顯示使用者、目前租戶與角色、依名稱排序的所屬租戶，沒有別人與非所屬租戶的資料；切換到所屬租戶後回應、資料庫與下一個請求的 token 都是新租戶與那裡的角色（角色變更也立即生效：直接改資料表，或由平台管理者經[成員](#成員)的 API 設定），同一使用者的另一個 session 與別人的 session 不變；切到存在但不屬於自己、與不存在的租戶回一樣的 403，session 不變；token 與 cookie 不是同一個 session、別人的 cookie 或 token、沒有 cookie 都改不到任何 session；切換之後被移除成員資格時 session 不在任何租戶；檢查之後才被撤銷的 session 401；兩個寫入的跨站請求被 cookie 檢查擋下（`cross-site`、`same-site`、別的 `Origin`），帶著 token 送到處理常式也再擋一次（含 `Sec-Fetch-Site: none` 與重複的標頭）；別的服務換來的 token（`act`）讀寫都 403、沒有 `sid` 的 token 不能切換、非 JSON 的 body 400、沒有 token 或給別的服務的 token 401，都什麼都沒改，而同源的 JSON 請求成功；大小上限以補了空白的合法 body 測：剛好 1024 bytes 200，1025 bytes 與多 1100 bytes 的 400（或連線在讀完之前被關掉）且什麼都沒改；檢查之後才過期（資料庫的時鐘，沒有撤銷）的 session 401 且不變；偏好的預設值、部分修改、`{}`、另一個使用者先存了自己的偏好，之後每次修改他的那一列都不變、16 種不在允許清單的值 422 且同一請求的合法欄位也不套用、`null`、未知欄位、陣列與想指定別的使用者的 body 400、使用者已刪除 401；`identity_app` 只能改 session 的 `active_tenant_id` 與偏好的四欄、CHECK 擋下清單外的值、沒有租戶時仍看不到成員資格與租戶，`identity_platform` 只能跨租戶讀成員資格與租戶名稱、不能寫、碰不到 session 與偏好（42501）；日誌沒有租戶名稱、email、cookie 與偏好的值 |
| `tests/tenant_suspension.rs` | Laysi/trustiot-v2#777，真的服務、migration Job 套用的產品 schema，路由有身分服務、assets 與 assistant：停權之前每個 session 都通過；平台管理者經 API 停權之後，同一個 session 的下一個請求在 cookie 檢查就對 assets 與 assistant 回 403 `tenant_suspended`（GET 與 POST、管理者與平台管理者都一樣，沒有身分標頭），給身分服務的 token 帶 `tenant_suspended`，租戶的事件紀錄回 403 `tenant_suspended`，run 委派的建立與換 token 都回 403 `tenant_suspended`；讀 session（租戶帶 `suspended`）、修改偏好、切換到另一個租戶（那裡照常通過）、登出照常可用，切回被停權的租戶後又被擋下；目前租戶被停權的平台管理者讀得到狀態並恢復它；恢復之後下一個請求都通過，停權前建立的委派又換得到 token；`deleted` 的租戶同樣被擋下、session 顯示 `deleted`、切換到別的租戶照常可用；日誌有 `tenant_suspended` 的拒絕、沒有 credential |
| `tests/cross_tenant.rs` | 產品 schema（`testkit::product`）上，服務角色讀不到也改不了其他租戶的 `memberships`、`tenants`（每個租戶一列）、`outbox_event` 與 `outbox_aggregate`（讀、無租戶讀、更新、搬移、插入五種探測；`outbox_aggregate` 在 2026-10-15 前豁免，Laysi/trustiot-v2#413 起照租戶表測）與 `event_log`（種子是 `assets` 的設備型號事件：每個產生端的事件都在這張表、受同一個 RLS），服務角色碰得到的每個關聯都已宣告：`users`、`sessions`、`login_codes`、`trustiot_migrate_applied`、`platform_event_log` 與消費端的表不是租戶資料而豁免；真的服務上，一個租戶的 `admin` 呼叫 `GET /api/identity/v1/event-log` 只看得到自己租戶的事件、沒有租戶時看不到任何事件；`GET /api/identity/v1/sessions/current` 以租戶的 token 只顯示自己租戶的名稱、沒有租戶時什麼都不顯示（Laysi/trustiot-v2#497）；`GET /api/identity/v1/entitlements` 只回自己租戶的權益（Laysi/trustiot-v2#111）；`preferences` 是使用者的設定、不是租戶資料而豁免；建立租戶、登入、兩個登出與偏好的 API 不讀租戶資料，切換租戶只回呼叫者自己所屬的租戶（同 `GET /api/identity/v1/sessions/current`），只動呼叫者自己的 session，探測的 token 指不到它（由 `tests/account.rs` 涵蓋）、平台路徑（兩個事件紀錄、設定成員角色與方案權益的讀取與設定）只給平台管理者，都豁免，設定成員角色的隔離由 `tests/members.rs` 測 |
| `src/event_log.rs` | 查詢參數（時間轉成 UTC、`from` 不早於 `to`、`limit` 的範圍、壞的 cursor）、cursor 到微秒來回不失真、`actor` 寫成哪個欄位（不認得的種類重試）、訂閱名稱；兩條平台路徑只放行平台管理者本人的請求：assistant 或 assets 換發的平台管理者 token 與租戶 `admin` 都 403（Laysi/trustiot-v2#584） |
| `tests/permissions.rs` | [權限矩陣](../../docs/architecture/permissions.md)（Laysi/trustiot-v2#584）：`openapi()` 的每個操作都宣告了要求，宣告與 OpenAPI 文件、矩陣文件的 `## identity` 一節逐列相同（不需要資料庫）；真的服務與產品 schema 上，每個操作以沒有 token、偽造的 token、給別的服務的 token、每種租戶角色、沒有租戶、平台管理者、換發的 token 各呼叫一次，只有要求放行的呼叫者通過授權，其餘剛好 401 或 403；建立租戶接受換發 token 的已知落差照目前的行為檢查，修正後失敗 |
| `tests/event_log.rs` | 真的服務與產品 schema，事件紀錄打開（Laysi/trustiot-v2#130）：建立租戶的操作在紀錄裡剛好一筆、`actor` 是平台管理者；寫 outbox 被拒時 503、沒有租戶也沒有紀錄，之後的操作照常收進來，紀錄數等於 outbox 的租戶事件數；已在紀錄裡的事件再投遞不多一列、訂閱不停；信封的 `producer` 不是讀取的 schema 時進死信；全平台事件進 `platform_event_log`；`assets` outbox 的設備型號事件（`assets.device_model_created`、`_updated`、`_deleted`）以 `identity.event-log.assets` 收進該租戶的紀錄，`producer`、aggregate 與 `actor` 照原樣，另一個租戶的 `admin` 看不到；`identity_app` 與 `identity_platform` 對兩張表的 `UPDATE`、`DELETE`、`TRUNCATE` 都是 42501，平台角色不能寫、服務角色不能讀全平台紀錄；租戶 `admin` 依時間範圍與分頁讀到自己租戶的紀錄（最新在前、每筆一次），另一個租戶看不到，`viewer`、沒有租戶、沒有 token 與壞參數被拒；平台管理者依租戶與時間讀任何租戶、讀全平台紀錄，不存在的租戶回空頁，租戶 `admin` 讀平台路徑 403；後台讀取記 `platform database access`，日誌沒有 payload |
