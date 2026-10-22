# contract

服務之間的契約（[決策 4](../../docs/architecture/decisions.md)）：服務只透過本 crate 的型別、事件與有型別的 client 溝通，契約一改，依賴它的服務就編譯失敗，不會等到執行時才出錯。`services/ai-gateway` 不得依賴本 crate（決策 28），所以這裡不放 AI 閘道需要的任何東西。

| 模組 | 內容 |
| --- | --- |
| `error` | M1 錯誤格式：`ErrorBody`、`ErrorCode`、`MessageKey`、`FieldError` |
| `id` | `TenantId`、`UserId`、`EventId`、`DeviceModelId`：新 ID 一律 UUIDv7（[可搬移規則 3](../../docs/architecture/deployment.md)） |
| `event` | `Event` trait 與事件信封 `EventEnvelope` |
| `events` | 事件本身，每個產生端服務一個子模組；目前有 `events::identity::TenantCreated`（`identity.tenant_created`，Laysi/trustiot-v2#65），以及設備型號的 `events::assets::DeviceModelCreated`、`DeviceModelUpdated`、`DeviceModelDeleted`（`assets.device_model_{created,updated,deleted}`，Laysi/trustiot-v2#99；型號本身是 `protocol::config::DeviceModel`，閘道器設定用的同一個格式） |
| `identity` | 內部身分標頭 `x-internal-identity`、claims `InternalClaims`、遮蔽內容的 `InternalToken` |
| `client` | 內部 API 的 `Endpoint` 與有型別的 client `InternalClient<S>` |
| `api` | 內部 API 本身，每個提供 API 的服務一個子模組；目前有 `api::identity::ExchangeToken`（身分服務的 token 交換，Laysi/trustiot-v2#106） |
| `service` | `ServiceName` 與型別層級的服務標記（`service::Identity` 等） |
| `openapi` | `SharedSchemas`：每個服務規格共用的 OpenAPI schema（M1 錯誤格式） |
| `json` | 不含文件內容的 JSON 錯誤 `JsonError`：serde 的訊息會引用值，日誌不得含個資（M2） |

測試：`cargo test -p contract`。OpenAPI 與事件的固定樣本在 `tests/fixtures/`。

## 錯誤格式（M1）

每個服務的每個錯誤回應都是同一種 JSON。回應不放給使用者看的文字：前端用 `message_key` 查使用者語言的翻譯，用 `details` 的值填入訊息。

```json
{
  "code": "validation_failed",
  "message_key": "error.validation_failed",
  "field_errors": [
    {"field": "/name", "message_key": "device_model.name_invalid", "details": {"max": 40}}
  ]
}
```

| 欄位 | 必填 | 說明 |
| --- | --- | --- |
| `code` | 是 | 下表的代碼之一，前端依它決定怎麼處理 |
| `message_key` | 是 | 翻譯鍵；預設是 `error.<code>`，需要更具體的訊息時換成領域的鍵，例如 `gateway.already_bound` |
| `details` | 否 | 訊息要填入的值，例如 `{"max": 40}`；空的時候不出現 |
| `field_errors` | 否 | 被拒絕的請求欄位：`field` 是指向請求 body 的 JSON Pointer（RFC 6901），加上該欄位的 `message_key` 與 `details` |

| 代碼 | HTTP | 意義與前端處理 |
| --- | --- | --- |
| `invalid_request` | 400 | 請求格式錯誤：不是 JSON、型別不對、缺欄位 |
| `validation_failed` | 422 | 格式正確但違反規則；`field_errors` 指出欄位 |
| `unauthenticated` | 401 | session 或內部身分 token 缺少、無效或過期；前端導回登入 |
| `permission_denied` | 403 | 身分有效但沒有權限 |
| `tenant_suspended` | 403 | 租戶已停權（B1）；前端只顯示停權說明頁 |
| `quota_exceeded` | 403 | 方案上限或配額用完（B2、B3）；前端提供升級，重試無效 |
| `not_found` | 404 | 資源不存在，或呼叫者的租戶看不到 |
| `conflict` | 409 | 與目前狀態衝突，例如重複或同時修改 |
| `rate_limited` | 429 | 請求太多，稍後重試 |
| `internal` | 500 | 非預期的失敗；不帶任何細節，原因只進日誌（不外洩底層錯誤，Laysi/trustiot#113） |
| `unavailable` | 503 | 依賴的服務停止、過載或本服務正在關閉，稍後重試 |
| `timeout` | 504 | 依賴的服務沒在逾時內回應，稍後重試 |

`rate_limited`、`unavailable`、`timeout` 可以原樣重試（`ErrorCode::is_retryable`），其餘重試也不會成功。

規則：

- `details` 與 `field_errors` 不放給使用者看的文字、內部錯誤文字、SQL 或個資。`internal` 一律不帶：`with_detail` 與 `with_field_error` 對它不生效。
- 訊息鍵：兩段以上以 `.` 分隔、每段由 `a-z`、`0-9`、`_` 組成，最長 128 bytes，第一段是領域（例如 `device_model.name_invalid`）。寫成常數（`MessageKey::new` 是 `const fn`）或 `message_key!`，格式錯誤就無法編譯；執行時收到的鍵用 `MessageKey::parse`。每個訊息鍵在前端都要有中英翻譯（前端骨架建立後，Laysi/trustiot-v2#123）。
- 先用既有代碼加上更具體的訊息鍵。代碼代表前端的處理方式（重新登入、顯示停權頁、提供升級、稍後重試、顯示欄位錯誤）；只有前端需要新的處理方式時才新增代碼。
- 新增代碼：在 `ErrorCode` 加變體，補上 `ALL`、`as_str`、`http_status`、`default_message_key`（編譯器會指出漏掉的 `match`），更新本頁的代碼表，再以 `CONTRACT_UPDATE_FIXTURES=1 cargo test -p contract --test error_format` 重新產生 `tests/fixtures/openapi/m1-error-schemas.json` 並在審查時檢查差異。
- 既有代碼不刪除、不改名：服務分批更新時，舊版前端與其他服務仍依賴它。
- 用戶端忽略不認得的欄位，所以 `ErrorBody` 只會增加選填欄位。
- OpenAPI：`ErrorBody`、`ErrorCode`、`FieldError`、`MessageKey` 都實作 `utoipa::ToSchema`，由 `openapi::SharedSchemas` 匯出；`tools/openapi` 把它併進 `api/openapi.json`，服務的規格可以直接以 `#/components/schemas/ErrorBody` 描述錯誤回應。改了這些型別後要重新產生 `api/openapi.json`（`cargo run -p openapi`），否則 `cargo test --workspace` 與 CI 失敗。

## 內部 API

服務之間的同步呼叫用 HTTP/JSON（決策 4），只做讀取；跨服務的寫入一律走事件。每個內部 API 是一個實作 `Endpoint` 的型別，由 `InternalClient<S>` 呼叫；client 只接受 `Endpoint::Service` 為 `S` 的 endpoint，呼叫錯服務無法編譯。

新增內部 API：

1. 放在 `src/api/<服務>.rs`，請求與回應型別寫在同一個檔案；服務的第一個內部 API 建立這個檔案，並在 `src/api.rs` 加上 `pub mod`。
2. `ROUTE` 以 `/internal/` 開頭，各段只用 `a-z`、`0-9`、`-`、`_`，路徑參數寫成 `{name}`（與 axum 0.8 的路徑語法相同）。提供 API 的服務直接用同一個 `ROUTE` 字串註冊路由，兩邊不會分歧。入口 gateway 不轉送 `/internal/`（決策 27）。
3. `METHOD` 只有 `Get`（只靠路徑，不帶 body）與 `Post`（需要 JSON body 的查詢，例如批次查詢）。`Post` 的 endpoint 必須覆寫 `body()` 回傳 `Some`；`Get` 帶 body 或 `Post` 沒有 body 的呼叫，client 在送出前就以 `invalid_request` 拒絕。
4. 每個 endpoint 都要有測試：`check_route(ROUTE)` 通過，且 `path_params()` 的數量等於路徑參數的數量。路徑參數由 client 逐段百分比編碼，空字串、`.`、`..` 會被拒絕。
5. 相容性：服務分批更新時新舊版本並存，所以請求與回應只能增加選填欄位（`Option` 或 `#[serde(default)]`），不改名、不刪除、不改型別，也不用 `deny_unknown_fields`。不相容的變更改開新的 `ROUTE`，等所有呼叫端更新後再移除舊的。

呼叫：

- `InternalClient::<service::Identity>::new(base_url, timeout)`：逾時是必填參數，必須大於 0 且不超過 60 秒（`MAX_TIMEOUT`），涵蓋連線到讀完 body。`base_url` 只能是 `http://主機[:埠]`，也就是對方的 Kubernetes Service；服務間的 mTLS 由 ztunnel 提供，所以不帶 TLS。啟動時每個對方服務建一個，之後重複使用。
- client 只用 HTTP/1.1、不讀環境變數的 proxy、不跟隨重導、不自動重試；要重試由呼叫端明確決定。成功回應的 body 上限預設 16 MiB（`with_max_response_bytes` 可改）。
- `call` 要帶受眾為對方服務的內部身分 token，放在 `x-internal-identity` 標頭。自己收到的 token 受眾是自己，不能轉送：代表使用者呼叫時，先以 token 交換（下一節）換一張受眾是對方的 token。服務不自己組這兩步，而是用 `crates/service-kit` 的 `OnBehalf::call`（決策 27，Laysi/trustiot-v2#106）。

### token 交換（`api::identity::ExchangeToken`）

`POST /internal/token-exchange`，在身分服務 token 交換自己的埠（預設 8082，chart 的 Service `<release>-identity-exchange`、埠名 `token-exchange`），本文 `{"audience": "<服務>"}`，`x-internal-identity` 帶呼叫端自己收到的 token。身分服務照驗證端的規則檢查這張 token（只是不限受眾），再確認它是入口 gateway 的請求簽出的（沒有 `act`），而且呼叫端（token 的受眾）可以代表使用者呼叫 `audience`：清單由 chart 從 `mesh.calls` 推得（`TRUSTIOT_ON_BEHALF_CALLS`，見 [chart README](../../deploy/chart/README.md) 的「服務間授權（mesh）」）。只換一跳：換出的 token 帶 `act`，不能再拿來換，多跳的鏈見 Laysi/trustiot-v2#509。這是一個不改變任何狀態的 `Post`：受眾放在本文，不留下記錄。

| 結果 | 狀態 | 本文 |
| --- | --- | --- |
| 換到 | 200 | `ExchangedToken` `{"token"}`：同一個使用者、租戶、角色與平台管理員旗標，受眾是 `audience`，`act` 是呼叫端，`iat` 是現在，到期是原 token 的到期與 60 秒後取早者 |
| 沒有 token、token 無效或過期、1 秒內就到期 | 401 | `unauthenticated` |
| 呼叫端不能代表使用者呼叫 `audience`，或 token 本身是換來的（帶 `act`） | 403 | `permission_denied` |
| 本文不是這個請求，或服務名稱不認得（含 `ai-gateway`，它不在 `ServiceName`） | 400 | `invalid_request` |
| 還沒有公鑰或簽章金鑰 | 503 | `unavailable` |

回應一律 `cache-control: no-store`。`ExchangedToken` 只能反序列化，沒有 `Serialize`，token 照樣是遮蔽內容的 `InternalToken`。經 `OnBehalf` 呼叫時，交換失敗是 `OnBehalfError::Exchange`：回給上游的代碼只有 `timeout`、`unavailable` 照傳，其他（含 403 與 401）一律 `internal`，因為那是部署的呼叫清單或本服務的問題，不是使用者的錯，同時記一筆 ERROR（呼叫端、受眾、身分服務回的代碼）。

每種失敗都是 `CallError`，`code()` 給出要回給上游的 M1 代碼，`to_error_body()` 給出整個 body：

| 失敗 | `CallErrorKind` | M1 代碼 |
| --- | --- | --- |
| 逾時內沒有完整回應 | `timeout` | `timeout` |
| 連不上：名稱解析失敗、拒絕連線 | `connect` | `unavailable` |
| 連線被重置，或回應完成前被關閉；ztunnel 依 AuthorizationPolicy 拒絕時就是這樣，不是 HTTP 403（[可行性測試](../../docs/feasibility/2026-10-10-edge-poc.md) Istio 段第 6 點） | `connection_reset` | `unavailable` |
| 其他傳輸錯誤，例如 HTTP 格式錯誤 | `transport` | `unavailable` |
| 對方回 M1 錯誤 | `error_status` | 對方的代碼原樣傳遞；但 `unauthenticated` 改成 `internal`：被拒絕的是本服務帶的 token，不是使用者的 session，重新登入沒有用 |
| 對方回非 M1 的 408、504 | `error_status` | `timeout` |
| 對方回非 M1 的 429、502、503 | `error_status` | `unavailable` |
| 對方回其他非 M1 狀態，含 3xx | `error_status` | `internal` |
| 成功回應不符合回應型別，或超過大小上限 | `invalid_response` | `internal` |
| 請求無法建立：路徑參數或 token 不合法、body 無法序列化、`Get` 帶 body 或 `Post` 沒有 body | `invalid_request` | `internal` |

client 每次失敗都記一筆日誌，欄位有 `peer`、`route`（路由樣板，不含參數值）、`kind`、`status`、`peer_code`、`code`、`timeout_ms`、`cause`。層級依結果而定：傳輸類失敗（`timeout`、`connect`、`connection_reset`、`transport`）與對應到 `unavailable` 或 `timeout` 的對方回應記 warn；對方回應原樣傳遞的其他代碼（例如 `not_found`，呼叫端自己處理）記 debug；對應到 `internal` 的失敗（請求或回應不合法、對方的內部錯誤、本服務的 token 被拒）記 error。token、請求與回應 body、路徑參數值都不進日誌。

## 事件

跨服務的寫入一律走事件（決策 4）：產生端與業務資料同一筆交易寫進 outbox，消費端冪等處理。outbox 的寫入、投遞與重播在 `crates/outbox`（Laysi/trustiot-v2#54）。

信封 `EventEnvelope`：

| 欄位 | 說明 |
| --- | --- |
| `id` | UUIDv7，每個事件唯一；消費端以它去重 |
| `type` | 事件型別 `Event::TYPE` |
| `version` | payload 的版本 `Event::VERSION`，從 1 開始 |
| `tenant_id` | 事件所屬的租戶；只有全平台事件是 `null` |
| `occurred_at` | 變更發生的時間，UTC，精確到毫秒 |
| `producer` | 產生事件的服務 `Event::PRODUCER`；與型別的產生端不符時拒絕解碼 |
| `payload` | 事件本身，一定是 JSON 物件 |
| `actor` | 選填（2026-10-21，Laysi/trustiot-v2#130）：誰做了這個變更，操作紀錄的「誰」。`{"kind": "user", "user_id"}` 是經 API 操作的人（內部身分 token 的 `sub`，平台管理者也是），`{"kind": "service", "service"}` 是服務自己的工作（排程、事件消費端）；沒有設定時不出現。之後新增的 `kind` 在舊版解碼成 `Actor::Unrecognized`，這版解碼不了的其他 `actor`（不認得的 `service`、格式錯的 `user_id`）也一樣，不會讓整個信封解碼失敗；只能新增 `kind`、不能改名；身分服務的事件紀錄遇到不認得的 `kind` 會重試、等它升級，所以新增 `kind` 時先升級身分服務。`Actor::Unrecognized` 只用於解碼，序列化會失敗，所以寫不進 outbox。`service` 不是產生端自己時，事件紀錄當作沒有 `actor` 記錄（WARN）。產生端以 `EventEnvelope::with_actor` 或 `outbox::publish_as` 設定 |

payload 經身分服務的事件紀錄給租戶的管理員看，並隨租戶的資料一直保存（[決策 34](../../docs/architecture/decisions.md)），所以 payload 一律不放：帳密、密鑰、一次性 token 或能取得存取權的連結（邀請、重設密碼、註冊）；只供內部的商業資料；ID 以外的個資，指一個人只用 `UserId`，不寫姓名、email 或電話。

消費端忽略信封與 payload 裡不認得的欄位，所以兩者都只增加選填欄位。

新增事件：

1. 在 `src/events/<產生端服務>.rs` 實作 `Event`（第一個事件建立該檔案，並在 `src/events.rs` 加上模組）：`TYPE` 是 `<領域>.<事件>`，小寫英數與底線（例如 `identity.tenant_created`），全系統唯一，永不改名、不重用，格式錯誤會在第一次呼叫 `EventEnvelope::new` 的地方編譯失敗；`VERSION = 1`；`PRODUCER` 是唯一會產生它的服務。
2. payload 必須序列化成 JSON 物件，不用 `deny_unknown_fields`。
3. 新增固定樣本 `tests/fixtures/events/<TYPE>/v1.json`（完整信封），並在 `tests/events.rs` 加一個呼叫 `decode_fixtures::<該事件>()` 的測試。

演進：

- 相容的變更只有一種：加選填欄位（`Option` 或 `#[serde(default)]`）。`VERSION` 不變，舊消費端會忽略新欄位；想保留新形狀的樣本時另加 `v<N>-<說明>.json`。
- 其他變更都不相容（改名、刪除、改型別、選填改必填、意義改變）：`VERSION` 加 1，舊版的結構留成 `<事件>V<N>`，在 `decode_payload` 裡轉成目前的型別（範例見 `tests/events.rs` 的 `example.widget_renamed`），並新增 `v<N+1>.json`。
- 固定樣本只增不改、不刪：測試要求每個版本都有樣本，且每個樣本都能被目前的程式解碼，所以做了不相容的變更卻沒升版，舊樣本就會解碼失敗。
- 消費端收到比自己新的版本時得到 `DecodeError::NewerVersion`（`is_retryable()` 為 true）：不要丟棄，等消費端更新後重試。其他解碼錯誤重試也不會成功。
- 發佈順序：先讓所有消費端能解碼新版本，再讓產生端寫新版本。初期所有服務一起發版，滾動更新期間收到的新版事件會以 `NewerVersion` 重試，直到消費端更新完成。

## 內部身分

入口 gateway 對每個需要 session 的請求向身分服務換一個短效的內部身分 token，放在 `x-internal-identity` 標頭交給後端：需要 cookie 的路由由身分服務的值覆寫這個標頭，例外路由將它剝除，所以用戶端無法自己帶（決策 27）。token 的格式、驗證、公鑰的發放與輪替見 [`crates/service-kit` 的內部身分一節](../service-kit/README.md#內部身分identity)（Laysi/trustiot-v2#52），簽發在[身分服務](../../services/identity/README.md)；本 crate 只定義雙方共用的標頭與 claims。

| claim | 說明 |
| --- | --- |
| `iss` | 簽發者，一律是 `identity` |
| `sub` | 使用者 ID |
| `aud` | 唯一可以接受這個 token 的服務；各服務拒絕受眾不是自己的 token，token 無法拿去別的服務重播 |
| `iat`、`exp` | 簽發與到期時間，Unix 秒；token 是短效的，效期由身分服務決定 |
| `tenant` | 使用者目前所在的租戶 `{"id", "role"}`；還沒選或沒有加入任何租戶時是 `null`，需要租戶的端點拒絕這種 token |
| `platform_admin` | 是否為平台管理員（A2），與租戶角色無關；缺少時視為 `false` |
| `act` | 選填，只有 token 交換換出的 token 有：`{"sub": "<服務>"}`，替使用者呼叫 `aud` 的服務（RFC 8693 的 actor，`Actor`）。入口 gateway 的請求簽出的 token 沒有它；身分服務不再換帶 `act` 的 token（Laysi/trustiot-v2#106） |

- claim 的名稱沿用 JWT（RFC 7519），claims 可以直接當 JWT payload。
- 驗證端忽略不認得的 claim，所以新 claim 只能是選填。
- 租戶角色 `viewer` ⊂ `editor` ⊂ `admin`，用 `TenantRole::includes` 判斷；新版身分服務發出這版不認得的角色時解析成 `unrecognized`，不給任何權限。
- `InternalToken` 的 `Debug` 只印 `InternalToken(<redacted>)`，也沒有 `Display` 與 `Serialize`；token 是憑證，不得進日誌（M2）。

## 相依套件

- `reqwest` 關閉預設功能，不含 TLS 與 HTTP/2：服務間是網格內的明文 HTTP/1.1，mTLS 由 ztunnel 提供。
- `hyper` 只用來辨識「回應完成前連線就被關閉」：這種失敗的錯誤鏈裡沒有 I/O 錯誤，只有 `hyper::Error`。
- 時間用 `chrono`，OpenAPI 用 `utoipa` 6，ID 用 `uuid`（v7）。
