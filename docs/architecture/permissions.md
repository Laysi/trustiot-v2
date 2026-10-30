# 權限矩陣（A2）

每個服務的客戶端 API（`openapi()` 列出、經入口呼叫的操作）要求呼叫者是誰。功能重建清單 A2 要求「權限只在後端判斷」，Laysi/trustiot-v2#104 要求權限矩陣寫進文件、每一列都有 API 層級的測試；本文件是那份矩陣（Laysi/trustiot-v2#584）。

本文件不是手寫後就放著的說明：每個服務在 `tests/permissions.rs` 以 `crates/testkit` 的 `permissions` 模組宣告自己每個操作的要求，測試同時對照三方，任何一方不一致就失敗：

- 服務的 OpenAPI 文件：每個操作都要宣告，宣告的每個操作都要在文件裡。服務新增路由時，沒決定誰能呼叫就合併不了。另外 `crates/testkit` 的單元測試 `the_matrix_covers_every_operation_of_the_api_spec` 對照整份 `api/openapi.json`：其中每個操作都要剛好列在本文件的一節，本文件列的每個操作也都要在其中，所以新的服務登記到 `tools/openapi` 而沒有接上矩陣，同樣合併不了（Laysi/trustiot-v2#585）。
- 本文件：該服務一節的表格要與宣告逐列相同（操作、要求、已知落差）。只改本文件或只改程式都會讓測試失敗。
- 服務本身：以真的服務程序與資料庫，對每個操作用下方「探測」列出的每種身分各送一次請求，檢查回應。

做法與 [chart 的呼叫矩陣](../../deploy/chart/README.md)（由 `deploy/chart/tests/chart.test.ts` 對照）相同。各服務怎麼接上、探測的細節在 [crates/testkit 的 README](../../crates/testkit/README.md)「權限矩陣」。

服務的 handler 以 `crates/service-kit` 的共用檢查判斷要求（Laysi/trustiot-v2#585）：`UserIdentity::require(Requirement::…, "<動作>")` 在 handler 一開始、讀 body 與資料庫之前呼叫，拒絕時回 403 `permission_denied`（`cache-control: no-store`，不論原因都是同一個回應），並在 INFO 記一行 `refused to <動作>`，帶使用者、租戶、原因（`no_tenant`、`unrecognized_role`、`role_too_low`、`not_platform_admin`、`exchanged_token`）與換發 token 的服務，不記 token。store 在開交易前的第二道檢查用不記日誌的 `UserIdentity::meets`。「已登入」不需要檢查：只有驗證過的 token 才有 `UserIdentity`。做法見 [crates/service-kit 的 README](../../crates/service-kit/README.md)。

## 要求

| 要求 | 誰可以呼叫 | 依據 |
| --- | --- | --- |
| 公開 | 任何人，不需要 token | 登入（決策 27 明列不需要 cookie 的路徑） |
| 已登入 | 任何登入的使用者，有沒有租戶都可以；其他服務換發的 token（帶 `act`，Laysi/trustiot-v2#106）也可以 | |
| 已登入，本人 | 使用者自己經入口的請求；帶 `act` 的 token 一律 403 | 使用者自己的帳號（session、偏好設定），不讓其他服務代為讀寫 |
| 租戶 viewer、租戶 editor、租戶 admin | token 的租戶裡至少有這個角色（admin 包含 editor，editor 包含 viewer）；換發的 token 也可以 | A2；角色在 token 裡，由身分服務簽發時從成員資格讀出 |
| 平台管理員，本人 | 平台管理員自己經入口的請求；帶 `act` 的 token 一律 403，任何租戶角色都是 403 | Laysi/trustiot-v2#104 2026-10-23 的決定：平台管理員的動作只接受本人的請求 |

平台管理員對租戶資料沒有隱含權限（Laysi/trustiot-v2#104 的決定 4）：要求租戶角色的操作，平台管理員也要有該租戶的成員資格，探測會確認沒有租戶的平台管理員得 403。

拒絕一律由 token 決定、在服務讀取 body 或資料庫之前：401 `unauthenticated` 是沒有或無效的 token，403 `permission_denied` 是 token 有效但要求不符。探測要求狀態碼與錯誤代碼都相符：其他檢查回的 403（例如跨站請求的拒絕）不算這裡的拒絕。

## 探測

`Suite::run` 對每個操作依它的要求送出下列請求。請求不帶 body，路徑參數填新的隨機 UUID；「通過授權」指回應不是 401 也不是 403（通常是 body 不合格的 400、不存在的 404，或空的結果），所以探測不會寫入任何資料。

| 呼叫者 | 公開 | 已登入 | 已登入，本人 | 租戶角色 R | 平台管理員，本人 |
| --- | --- | --- | --- | --- | --- |
| 沒有 token | 通過授權 | 401 | 401 | 401 | 401 |
| 偽造的 token（測試行程另產生的金鑰與金鑰 ID 簽的，代表不經入口、直接送到 pod 的偽造身分標頭） | | 401 | 401 | 401 | 401 |
| 給其他服務的 token（`aud` 不是本服務） | | 401 | 401 | 401 | 401 |
| 已登入、沒有租戶 | | 通過授權 | 通過授權 | 403 | 403 |
| 已登入、沒有租戶，換發的 token | | 通過授權 | 403 | | |
| 沒有租戶的平台管理員 | | | | 403 | |
| 租戶 viewer、editor、admin | | | | 低於 R 的 403，其餘通過授權 | 403 |
| 租戶裡本版本不認得的角色（`Unrecognized`） | | | | 403 | 403 |
| 租戶 R，換發的 token | | | | 通過授權 | |
| 平台管理員，換發的 token | | | | | 403 |
| 平台管理員 | | | | | 通過授權 |

## 已知落差

已知落差是程式目前的行為與上述決定不符、另外追蹤修正的操作。表格的「已知落差」欄寫明落差，該列的探測照目前的行為檢查；修正後探測會失敗，提醒修正的人同時刪掉宣告的 `accepts_exchanged_tokens` 與這一格。

- `接受換發的 token`：要求「本人」的操作目前接受帶 `act` 的 token。

## 不在矩陣裡的端點

- `/internal/...` 與服務各自的其他連接埠（例如身分服務的 ext_authz 檢查與 token 換發、certs 的 CA 公開資料與 MQTT broker 憑證簽發）：不在 `openapi()`，入口不轉送，只限叢集內的指定服務呼叫，由 mesh 的 AuthorizationPolicy 把關，誰可以呼叫誰列在 [chart 的呼叫矩陣](../../deploy/chart/README.md)。
- 閘道器的憑證換發 `/certs/renew`（Laysi/trustiot-v2#81）：閘道器以 TPM 簽章證明身分，不用使用者 token，不在 `openapi()`；測試在 `services/certs/tests/renewal.rs`。

## identity

`services/identity/tests/permissions.rs`。

| 操作 | 要求 | 已知落差 |
| --- | --- | --- |
| `POST /auth/login` | 公開 | |
| `DELETE /api/identity/v1/sessions` | 已登入，本人 | |
| `DELETE /api/identity/v1/sessions/current` | 已登入 | |
| `GET /api/identity/v1/sessions/current` | 已登入，本人 | |
| `PUT /api/identity/v1/sessions/current/tenant` | 已登入，本人 | |
| `GET /api/identity/v1/preferences` | 已登入，本人 | |
| `PATCH /api/identity/v1/preferences` | 已登入，本人 | |
| `GET /api/identity/v1/event-log` | 租戶 admin | |
| `POST /api/identity/v1/tenants` | 平台管理員，本人 | |
| `PUT /api/identity/v1/platform/tenants/{tenant_id}/members` | 平台管理員，本人 | |
| `GET /api/identity/v1/platform/tenants/{tenant_id}/event-log` | 平台管理員，本人 | |
| `GET /api/identity/v1/platform/event-log` | 平台管理員，本人 | |
| `GET /api/identity/v1/platform/ai/model-roles` | 平台管理員，本人 | |
| `PUT /api/identity/v1/platform/ai/model-roles/{role}` | 平台管理員，本人 | |
| `DELETE /api/identity/v1/platform/ai/model-roles/{role}` | 平台管理員，本人 | |

- 登出（`DELETE .../sessions/current`）撤銷的是請求 cookie 的那個 session，沒有 cookie 得 401；探測帶一個格式正確、但不屬於任何 session 的 cookie。
- 兩條平台操作紀錄路由在 Laysi/trustiot-v2#584 改為拒絕換發的 token（之前接受）。
- `POST /api/identity/v1/tenants` 與 `DELETE /api/identity/v1/sessions` 在 Laysi/trustiot-v2#760 改為只接受本人的請求（2026-10-30 決定；之前都接受換發的 token）：建立租戶是平台管理員的動作，登出所有裝置會結束使用者所有的 session，都不讓其他服務代為進行。兩者在解析 body 與讀資料庫之前拒絕，不寫入任何資料；`tests/permissions.rs` 的 `an_exchanged_token_creates_no_tenant_and_revokes_no_session` 證明沒有建立租戶、沒有撤銷 session。

## certs

`services/certs/tests/permissions.rs`。

| 操作 | 要求 | 已知落差 |
| --- | --- | --- |
| `POST /api/certs/v1/workstation-certificates` | 平台管理員，本人 | |
| `POST /api/certs/v1/workstation-certificates/{serial}/revoke` | 平台管理員，本人 | |
| `POST /api/certs/v1/gateway-registrations` | 平台管理員，本人 | |
| `POST /api/certs/v1/gateways/{hardware_id}/revoke` | 平台管理員，本人 | |

- 出廠登錄另外要求產線工作站的憑證與簽章（D1，`services/certs/README.md`）；矩陣只管使用者的身分，工作站簽章的拒絕在 `tests/registration.rs`。
- 閘道器憑證的撤銷（Laysi/trustiot-v2#636）只看平台管理員的身分，不屬於任何租戶；被拒絕的呼叫不撤銷任何憑證、不發 CRL、不動 broker，`tests/gateway_revocation.rs` 比對帳本與各 CA 的 CRL 編號。
- 探測前後比對工作站憑證與出廠登錄的帳本，任何呼叫者都沒有寫入資料（Laysi/trustiot-v2#584 的留言：viewer 與 editor 呼叫簽發與撤銷得 403、不寫入任何資料）。

## assets

`services/assets/tests/permissions.rs`。

| 操作 | 要求 | 已知落差 |
| --- | --- | --- |
| `GET /api/assets/v1/tenant` | 租戶 viewer | |
| `GET /api/assets/v1/device-models` | 租戶 viewer | |
| `POST /api/assets/v1/device-models` | 租戶 editor | |
| `POST /api/assets/v1/device-models/import` | 租戶 editor | |
| `GET /api/assets/v1/device-models/{model_id}` | 租戶 viewer | |
| `PUT /api/assets/v1/device-models/{model_id}` | 租戶 editor | |
| `DELETE /api/assets/v1/device-models/{model_id}` | 租戶 editor | |
| `GET /api/assets/v1/device-models/{model_id}/export` | 租戶 viewer | |
| `GET /api/assets/v1/devices` | 租戶 viewer | |
| `POST /api/assets/v1/devices` | 租戶 editor | |
| `GET /api/assets/v1/devices/{device_id}` | 租戶 viewer | |
| `PUT /api/assets/v1/devices/{device_id}` | 租戶 editor | |
| `DELETE /api/assets/v1/devices/{device_id}` | 租戶 editor | |
| `GET /api/assets/v1/platform/gateways` | 平台管理員，本人 | |
| `GET /api/assets/v1/platform/gateways/{hardware_id}` | 平台管理員，本人 | |
| `PUT /api/assets/v1/platform/gateways/{hardware_id}/binding` | 平台管理員，本人 | |

- `GET /api/assets/v1/tenant` 在 Laysi/trustiot-v2#585 之前只檢查有沒有租戶，帶本版本不認得的角色（`Unrecognized`）的 token 也能讀；現在要求租戶 viewer。
- 探測前後比對型號與其資料點、設備、閘道器綁定與 outbox 的筆數，任何呼叫者都沒有寫入資料。
- 綁定閘道器的 `GatewayBinder` 在開交易前再檢查一次平台管理員本人（`meets`，見 `services/assets/src/gateways/binding.rs`）。

## gateway-ops

`services/gateway-ops/tests/permissions.rs`。

| 操作 | 要求 | 已知落差 |
| --- | --- | --- |
| `GET /api/gateway-ops/v1/gateways` | 租戶 viewer | |

- 列表以服務角色在呼叫者租戶的交易裡讀（決策 29），只看得到該租戶的綁定、在線狀態與設定版本；跨租戶測試在 `services/gateway-ops/tests/cross_tenant.rs`（Laysi/trustiot-v2#112）。
- 探測前後比對閘道器綁定與在線狀態的筆數，任何呼叫者都沒有寫入資料。探測用的服務程序連不到 broker（API 不需要 broker）。

## telemetry-query

`services/telemetry-query/tests/permissions.rs`。

| 操作 | 要求 | 已知落差 |
| --- | --- | --- |
| `GET /api/telemetry/v1/devices/{device_id}/series` | 租戶 viewer | |
| `GET /api/telemetry/v1/devices/{device_id}/latest` | 租戶 viewer | |
| `POST /api/telemetry/v1/statistics` | 租戶 viewer | |
| `POST /api/telemetry/v1/usage` | 租戶 viewer | |

- 在 Laysi/trustiot-v2#585 之前這四個操作只檢查有沒有租戶，而且先驗證參數或 body、再檢查租戶：帶 `Unrecognized` 角色的 token 能查詢，沒有租戶的呼叫者送錯參數得到 400 而不是 403。現在角色檢查在最前面，`statistics` 與 `usage` 的 body 也在檢查之後才讀。
- 探測不需要 ClickHouse：被拒的呼叫者在讀任何東西之前就得 403；放行的呼叫者沒帶參數或 body 得 400，`latest` 在 Postgres 找不到這個隨機租戶的帳號得 404，都在查 ClickHouse 之前。測試把 ClickHouse 位址指向一個只計算連線數的本機埠，探測後斷言沒有任何連線。
- 助理服務換發的 token 也能呼叫（租戶 R 的換發 token 探測通過授權）：助理的遙測工具以使用者的身分查詢（Laysi/trustiot-v2#142）。
