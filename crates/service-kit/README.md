# service-kit

雲端服務的共用骨架，實作[部署文件](../../docs/architecture/deployment.md)「讓 k8s 只是打包工作的服務設計規則」：設定只來自環境變數或設定檔、health 與 readiness、收到 SIGTERM 正常收尾、JSON 日誌與 OTLP 匯出（日誌、trace、指標）、排程工作以 Postgres advisory lock 互斥。每個服務的 `main` 都透過 `service_kit::run` 啟動。

| 模組 | 內容 |
| --- | --- |
| `config` | 環境變數與 TOML 設定檔、啟動時驗證（`Validate`）；錯誤訊息不含設定值 |
| `health` | `/healthz`、`/readyz`（獨立的健康埠與 API 埠）與 readiness 檢查 |
| `shutdown` | SIGTERM、SIGINT 收尾，`Shutdown` 狀態 |
| `http` | 請求 span 與存取日誌、handler 逾時、panic 轉成 500 |
| `telemetry` | JSON 日誌、trace id、OTLP 匯出（日誌、trace、指標）、敏感欄位遮蔽 |
| `job_lock` | 排程工作同一時間只在一個副本執行 |
| `postgres` | Postgres 連線一律 `verify-full`；新連線先檢查角色與 RLS 設定，不安全就拒用 |
| `identity` | 驗證內部身分 token、取得請求的使用者身分（`contract` feature） |
| `tenant` | 以 RLS 限定單一租戶的交易，租戶取自已驗證的身分；平台角色的跨租戶交易（`contract` feature） |

完整的範例服務在 `src/bin/example-service.rs`，kit 的整合測試以它做為真的行程與容器來測試。本機執行：

```sh
TRUSTIOT__GREETING=hello TRUSTIOT__HTTP__LISTEN_ADDR=127.0.0.1:8080 \
  cargo run -p service-kit --bin example-service
```

## 用法

```rust
use std::process::ExitCode;

use axum::Router;
use serde::Deserialize;
use service_kit::config::Validate;
use service_kit::{BoxError, Context, ServiceInfo};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    database_url: String,
}

impl Validate for Config {}

#[tokio::main]
async fn main() -> ExitCode {
    service_kit::run(ServiceInfo::new("assets", env!("CARGO_PKG_VERSION")), start).await
}

async fn start(context: Context<Config>) -> Result<Router, BoxError> {
    let pool = sqlx::PgPool::connect_lazy(&context.config().database_url)?;
    let ping = pool.clone();
    context.health().add_check("postgres", move || {
        let pool = ping.clone();
        async move { sqlx::query("SELECT 1").execute(&pool).await.map(|_| ()) }
    });
    Ok(Router::new() /* 服務的路由 */)
}
```

（這個例子只示範 `run` 的流程；服務實際連 Postgres 要經 [`postgres` 與 `tenant`](#租戶隔離tenant)，連線才會驗證伺服器憑證、租戶資料才受 RLS 保護。）

`run` 依序：

1. 讀取並驗證設定；有錯就記一行 ERROR、exit code 1，不啟動。
2. 啟動日誌與 trace，安裝 SIGTERM、SIGINT handler。
3. 呼叫 `start` 取得服務的 router；回傳錯誤時 exit code 1。啟動期間收到 SIGTERM 就直接以 exit code 0 結束。
4. 監聽 `http.listen_addr`（API，加上 health 路由與 kit 的 layer）與 `http.health_listen_addr`（只有 health 路由），readiness 開始回 200。任一個位址被占用就 exit code 1，不啟動。
5. 收到 SIGTERM 後依[收尾](#收尾sigterm)結束，回傳 exit code。

`Context` 提供 `config()`（已驗證的服務設定）、`health()`（註冊 readiness 檢查）、`shutdown()`（收尾狀態）與 `spawn(task)`（收尾時會等待的背景工作）。

## 設定

- 每個設定鍵有點分隔的路徑，例如 `http.listen_addr`。用環境變數 `TRUSTIOT__HTTP__LISTEN_ADDR` 設定（前綴之後每段轉大寫，以 `__` 連接），或寫在 `TRUSTIOT_CONFIG_FILE` 指定的 TOML 檔；兩者都有時以環境變數為準。值為空字串的變數視同未設定。
- 頂層的 `http`、`shutdown`、`telemetry` 屬於 kit（`config::KIT_SECTIONS`），其他頂層鍵交給服務自己的設定型別。設定型別加上 `#[serde(deny_unknown_fields)]`，拼錯的鍵才會在啟動時被擋下；沒有自己設定的服務用 `config::NoConfig`。
- 環境變數的值都是字串，kit 依欄位型別轉成數字或布林；清單用逗號分隔，例如 `TRUSTIOT__CORS__ORIGINS=https://a.example,https://b.example`。元素本身含逗號的清單寫在設定檔。
- 時間長度欄位加 `#[serde(deserialize_with = "service_kit::config::duration")]`，值寫成 `5s`、`500ms`、`1m 30s`。`duration` 不設上限（humantime 接受 `500000000000y` 這類值）：服務自己的時間欄位要在 `Validate` 設合理上限，免得 `Instant + Duration` 溢位 panic。
- 缺少必要的鍵、型別錯誤、未知的鍵，以及 `Validate::validate` 回傳的 `Invalid`，都會讓服務在接流量之前以 exit code 1 停止。錯誤訊息只寫鍵、環境變數名稱與來源，不寫設定值；服務自己的 `Invalid` 訊息也不能含設定值。
- 不帶產品名稱前綴的服務（AI 閘道，[決策 28](../../docs/architecture/decisions.md)）用 `ServiceInfo::with_env_prefix` 換掉 `TRUSTIOT`；平台設定的前綴也跟著換，部署時要對應。
- 不要用 `Debug` 印出整個設定：連線字串等設定可能含密碼。

平台設定：Helm chart 以單一底線的 `TRUSTIOT_<NAME>` 變數把平台設定交給每個服務（例如 `TRUSTIOT_POSTGRES_HOST`，清單見 [chart 的 README](../../deploy/chart/README.md)「chart 與服務的約定」）。kit 把它們放進設定的 `platform` 表，鍵是去掉前綴後轉小寫的名稱：

```rust
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    platform: Platform,
}

/// 不加 deny_unknown_fields：每個服務都會拿到全部的平台設定。
#[derive(Debug, Deserialize)]
struct Platform {
    deployment_mode: String,       // TRUSTIOT_DEPLOYMENT_MODE，必填
    postgres_host: Option<String>, // TRUSTIOT_POSTGRES_HOST，可省略
}
```

- 設定型別沒有 `platform` 欄位時，kit 不會把平台設定交給它，所以加了 `deny_unknown_fields` 的型別不會因平台設定而失敗。
- 平台設定和其他鍵一樣在啟動時驗證；缺少或無效時，錯誤訊息寫出對應的變數，例如 `TRUSTIOT_POSTGRES_HOST`。
- 平台設定只由部署設定：`TRUSTIOT__PLATFORM__*` 會讓服務拒絕啟動，`services.<name>.env` 因此改不了平台設定。本機開發可以直接設 `TRUSTIOT_<NAME>`，或寫在設定檔的 `[platform]` 表（環境變數優先）。
- `TRUSTIOT_CONFIG_FILE` 是 kit 的設定檔路徑，不是平台設定。它落在 chart 保留給自己的名稱範圍內，chart 也不掛載設定檔，所以在 k8s 上設定一律來自環境變數；設定檔用於本機開發與 chart 以外的執行方式。
- `OTEL_*` 與 `SENTRY_*` 由 chart 設定。kit 的 `service.name` 一律用程式裡的 `ServiceInfo` 名稱，與 chart 的 `OTEL_SERVICE_NAME`（服務的 key）相同。

kit 的設定與預設值（環境變數以 `TRUSTIOT` 前綴為例）：

| 鍵 | 環境變數 | 預設 | 說明 |
| --- | --- | --- | --- |
| `http.listen_addr` | `TRUSTIOT__HTTP__LISTEN_ADDR` | `0.0.0.0:8080` | API 的監聽位址；health 端點也在這個埠，見[Health 與 readiness](#health-與-readiness) |
| `http.health_listen_addr` | `TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR` | `0.0.0.0:8081` | 健康埠：只有 `/healthz`、`/readyz`，其他路徑一律 404；chart 的探針連這個埠（埠名 `health`）。埠號不能與 `http.listen_addr` 相同（`0` 表示各自取空的埠） |
| `http.request_timeout` | `TRUSTIOT__HTTP__REQUEST_TIMEOUT` | `30s` | handler 產生回應標頭的時間上限，逾時回 504；串流的 body 不受限。用 504 而不是 503，是因為入口會重試 503，非冪等的請求可能被執行兩次；必須大於 0，上限 1 小時 |
| `shutdown.drain_delay` | `TRUSTIOT__SHUTDOWN__DRAIN_DELAY` | `5s` | 收到 SIGTERM 後照常服務多久，等入口、ztunnel 與 kube-proxy 不再送來新請求；上限 1 小時 |
| `shutdown.drain_timeout` | `TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT` | `20s` | 停止接受連線後，進行中的請求與背景工作結束的時間上限；必須大於 0，上限 1 小時 |
| `telemetry.log_filter` | `TRUSTIOT__TELEMETRY__LOG_FILTER` | `info` | 預設等級加上以逗號分隔的 `target=level`，例如 `info,sqlx=warn`；OpenTelemetry SDK 的診斷日誌（target 以 `opentelemetry` 開頭）不論怎麼設，DEBUG 與 TRACE 都不輸出，見[日誌與 trace](#日誌與-trace)的 OTLP 匯出一節 |
| `telemetry.otlp_endpoint` | `TRUSTIOT__TELEMETRY__OTLP_ENDPOINT` | 未設定（不匯出） | OTLP/HTTP 接收端的 base URL（日誌、trace、指標都送到這裡），例如 `http://otel-collector:4318`；只接受 `http://`（mesh 已加密這一段），不得含帳密或 query |

時間長度的上限（`service_kit::http::MAX_REQUEST_TIMEOUT`、`service_kit::shutdown::MAX_DRAIN`）：`http.request_timeout`、`shutdown.drain_delay`、`shutdown.drain_timeout` 各自不得超過 1 小時（剛好 1 小時可以），而且 `drain_delay + drain_timeout` 合計也不得超過 1 小時。超過就在啟動時以設定錯誤拒絕（錯誤訊息寫出設定名稱與環境變數，不寫設定值）。沒有上限時，極端值（例如 `500000000000y`）會讓 SIGTERM 後的時間運算溢位而 panic（`drain_timeout`），或讓服務永遠不退出（`drain_delay`）。

## Health 與 readiness

- `GET /healthz`：行程能處理 HTTP 就回 200 `{"status":"ok"}`。不檢查依賴，依賴故障時才不會讓每個 pod 都被重啟。
- `GET /readyz`：啟動完成、尚未收到 SIGTERM、且每個檢查都通過時回 200 `{"status":"ready"}`；否則回 503，`status` 是 `starting`、`draining` 或 `unavailable`。
- `Health::add_check(name, check)` 註冊依賴檢查，例如資料庫 ping。每個檢查最多 800 ms，結果快取 1 秒，同時進來的 probe 共用同一輪檢查；檢查開始失敗與恢復時各記一行日誌，錯誤文字不能含祕密。
- 兩個端點不記存取日誌、不受 handler 逾時限制，回應帶 `Cache-Control: no-store`。兩個埠共用同一份狀態，同一時間回同樣的結果。
- 健康埠（`http.health_listen_addr`，預設 8081）只提供這兩個端點：其他路徑一律 404（例如 `/v1/...`、`/internal/...`、`/healthz/`），這兩個路徑的其他 method 回 405。kubelet 的探針連這個埠：Istio ambient 下它來自 169.254.7.127、不經 ztunnel，從節點發出的其他連線（例如其他 namespace 的 `hostNetwork` pod）也一樣，ztunnel 不驗身分也不套用 AuthorizationPolicy。chart 的 NetworkPolicy 因此只對探針連的埠放行這個位址；探針改連健康埠後，節點上的行程碰不到服務的 API，包括只靠 AuthorizationPolicy 限定呼叫端的 `/internal/`（Laysi/trustiot-v2#286，[chart README](../../deploy/chart/README.md)「節點來源」）。
- API 埠保留這兩個端點，理由：入口 gateway 的公開路徑 `/healthz`（外部監控，`ingress.publicPaths.healthCheck`）經 mesh 轉給身分服務的 API 埠；本機開發環境（`scripts/dev.ts`）與 chart 以外的執行方式只開 API 埠也能檢查健康。這兩個端點不帶資料，mesh 內連得到 API 埠的呼叫端本來就經 AuthorizationPolicy 授權，所以保留不會多開放什麼；節點來源連不到 API 埠，是由探針只連健康埠來保證，不是靠 API 埠拿掉端點。
- 收尾時兩個埠的 readiness 一致：收到 SIGTERM 立刻都回 503（`draining`），`/healthz` 照常回 200。drain delay 結束後 API 埠停止接受連線；健康埠繼續回應（readiness 503、liveness 200）直到行程結束，kubelet 看到的是正在收尾的 pod，而不是被拒的連線。送出遙測資料之前健康埠關閉：readiness 一律回 503、停止接受連線並關閉閒置的連線，進行中的探針照常拿到回應（收到訊號後 readiness 不執行檢查，立即回應）。健康埠意外停止時，API 埠也跟著停止，服務以 exit code 1 結束。

## 收尾（SIGTERM）

服務在容器內是 PID 1，kernel 不會替 PID 1 執行訊號的預設動作：沒有 handler 的話 SIGTERM 會被忽略，pod 撐到寬限期結束才被 SIGKILL（[可行性測試](../../docs/feasibility/2026-10-10-edge-poc.md)）。kit 自己處理 SIGTERM 與 SIGINT：

1. 收到第一個訊號，readiness 立刻回 503（`draining`），pod 開始從 Service 的 endpoints 移除；`Shutdown::requested` 完成。服務在 `shutdown.drain_delay` 內照常處理請求。
2. drain delay 結束：停止接受新連線並關閉閒置的 keep-alive 連線，`Shutdown::stopping` 完成。進行中的請求與 `Context::spawn` 的背景工作要在 `shutdown.drain_timeout` 內結束：全部結束就以 exit code 0 結束。
3. 逾時就中止剩下的工作：還在執行的 handler 被丟棄，它的請求回 504（不回 503，入口才不會重試執行到一半的非冪等請求）；背景工作在下一個 await 被丟棄；最多再等 0.5 秒，沒停下的連線一併放棄，exit code 1。送出遙測資料時 handler 與背景工作都已停下（已送出標頭的串流 body 除外，見下方長連線的規則）。
4. 收尾期間再收到一次訊號，立刻以 exit code 1 結束。
5. 結束前送出還在緩衝的日誌與 trace，最多約 2.5 秒。drain 之後最多再需要 3 秒（中止 0.5 秒加遙測資料 2.5 秒）。

部署與實作規則：

- Helm chart 刪除 pod 時先等 `preStopSleepSeconds`（預設 5 秒），讓入口與 ztunnel 先移除這個 pod，再送 SIGTERM，之後給服務 `drainSeconds`（預設 30 秒）才送 SIGKILL。`drain_delay + drain_timeout` 再加 3 秒（中止逾時的工作與送出遙測資料）不能超過 `drainSeconds`：kit 的預設值合計 28 秒，在 30 秒內；`tests/chart_contract.rs` 對照 chart 的預設值檢查。調高 `drain_timeout` 的服務（例如持有長連線）要同時調高它在 chart 的 `drainSeconds`。
- 有 preStop 等待時，`drain_delay` 與它重疊，只是多等幾秒；沒有 preStop 的環境（chart 設 `preStopSleepSeconds: 0`、chart 以外的執行方式）就靠 `drain_delay` 讓流量先移走，所以預設保留 5 秒。
- 映像的 `ENTRYPOINT` 用 exec 形式直接執行服務，不要包在 shell 裡，訊號才會送到服務本身。
- chart 的執行環境是唯讀的根目錄、任意的非 root UID、沒有 capability；kit 不寫檔案，`tests/container.rs` 以相同限制執行範例服務。
- 背景工作用 `Context::spawn` 啟動，並在 `Shutdown::stopping` 完成時結束。
- WebSocket、SSE 等長連線的 handler 要等 `Shutdown::stopping()`，主動關閉連線（WebSocket 送 close、SSE 結束串流），讓用戶端立刻改連新的 pod。SSE 的回應還算進行中的請求，kit 會等它結束，但回應標頭送出後的串流 body 不在逾時中止的範圍內，drain 逾時時要到行程結束才被切斷；升級後的 WebSocket 連線 kit 不會等，行程結束時才由 kernel 關閉。

## 日誌與 trace

日誌寫到 stdout，每行一個 JSON 物件，欄位順序固定：

```json
{"timestamp":"2026-10-10T08:00:00.123456Z","level":"INFO","service":"assets","target":"service_kit::http","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736","span_id":"5f1c3e2a9b7d4e60","message":"request completed","fields":{"http.request.method":"POST","http.route":"/v1/devices/{id}","http.response.status_code":200,"duration_ms":3.2}}
```

- `trace_id`、`span_id`：事件在 span 內才有。每個請求在 `http.request` span 內處理；請求帶 W3C `traceparent` 時延續呼叫端的 trace。
- 每個請求一行 `request completed`（5xx 是 ERROR 等級的 `request failed`），只記 method、路由樣板（不記實際路徑與 query）、狀態碼與耗時；不記標頭與 body。health 端點不記。
- handler panic 時回 500，並記一行 ERROR `thread panicked`，含位置與 panic 訊息，所以 panic 與 `expect` 的訊息不能放資料。handler 超過 `http.request_timeout` 時回 504，記一行 WARN。兩者的回應 body 見 [contract feature](#contract-feature)。
- `telemetry.log_filter` 調整等級。span 不受它影響，一律在 INFO 以上建立，等級調到 `warn` 時，warn 日誌仍帶 trace id。

規則（[觀測文件](../../docs/architecture/observability.md)、功能重建清單 M2）：

1. 不記錄 request 或 response 的 body、標頭、聊天內容、憑證與個資；需要時記大小或數量，例如 `length`。
2. 值放在欄位，不要寫進訊息字串（例如 `info!("login {email}")`），欄位名稱要如實描述內容。
3. `#[tracing::instrument]` 一律加 `skip_all`，需要的欄位用 `fields(...)` 列出；否則每個參數都會以 `Debug` 格式記錄。
4. SQL 用綁定參數，不要把值組進 SQL 字串：sqlx 會以 DEBUG 等級記錄 SQL 文字，慢查詢以 WARN 記錄。
5. 欄位名稱的遮蔽只是最後防線：名稱像憑證、個資或內容的欄位（`password`、`token`、`authorization`、`cookie`、`email`、`phone`、`body`、`content`、`prompt` 等，完整清單見 `src/telemetry/redact.rs`），值會換成 `[REDACTED]`，stdout、OTLP 的日誌屬性與 span 屬性都一樣。寫進訊息字串的值、以無害名稱記錄的值都攔不到。

OTLP 匯出：

- 設定 `telemetry.otlp_endpoint` 後，日誌送到 `<url>/v1/logs`、trace 送到 `<url>/v1/traces`（OTLP/HTTP protobuf，批次送出），指標送到 `<url>/v1/metrics`（每 `OTEL_METRIC_EXPORT_INTERVAL` 毫秒一次，預設 60 秒），resource 帶 `service.name` 與 `service.version`。預設關閉；只設 `OTEL_EXPORTER_OTLP_ENDPOINT` 不會開啟匯出，端點以 kit 的設定為準。
- 指標：開啟匯出時 kit 設定全域的 meter provider，服務與共用 crate 以 `opentelemetry::global::meter` 建立儀表（例如 `crates/outbox` 的 lag）。在 `Telemetry::init` 之前建立的儀表不會匯出，所以在 `start` 裡或之後建立；關閉匯出時儀表不做任何事。收尾時送出最後一次讀數。
- 其他 OpenTelemetry SDK 的標準環境變數仍然有效，例如取樣的 `OTEL_TRACES_SAMPLER` 與批次的 `OTEL_BSP_*`；在 chart 上 `OTEL_*` 只由 chart 設定。
- SDK 自己的診斷日誌（target 以 `opentelemetry` 開頭：`opentelemetry`、`opentelemetry_sdk`、`opentelemetry-otlp`、`opentelemetry-http`）只寫 stdout，不匯出，避免匯出失敗時自我循環。
- 端點的路徑可能帶 token，所以 SDK 的診斷日誌**只輸出 INFO、WARN、ERROR**，`telemetry.log_filter` 設成 `debug` 或 `trace`、或寫 `opentelemetry-otlp=trace` 這類針對 SDK 的指令，都不會讓它輸出更多：SDK 在 DEBUG 以下，每次匯出失敗都會記錄完整 URL（含路徑），還有 collector 回應的 body。啟動日誌也只記「匯出是否開啟」，不記端點。核對過鎖定版本（`Cargo.lock` 的 0.33.1）的 INFO 以上診斷，沒有 URL；升級 SDK 後，`cargo test -p service-kit --test otlp` 的 canary 測試會檢查這件事。其他 target（例如 `reqwest`、`hyper_util`）在 `debug` 與 `trace` 下只記主機與連接埠，不含路徑，同一組測試也涵蓋。端點的 query 與帳密本來就會被設定驗證拒絕，錯誤訊息不含原值。仍然不要把 token 放在端點路徑，token 應由 mesh 或 collector 端處理。

## 排程工作（`job_lock`）

每個副本排同樣的工作。執行前先取 Postgres 的 session 層級 advisory lock：拿到鎖的副本執行，其他副本跳過這一輪。

```rust
use std::time::Duration;
use service_kit::job_lock::JobLock;

let lock = JobLock::new(pool.clone(), "assets", "purge-expired");
context.spawn(lock.run_periodically(
    Duration::from_secs(300),
    context.shutdown().clone(),
    move || {
        let pool = pool.clone();
        async move { purge_expired(&pool).await }
    },
));
```

- `run_periodically` 每個週期試一次，直到 `Shutdown::stopping`；失敗記 WARN，panic 記 ERROR（見下）。`run_exclusive(job)` 只試一次，拿不到鎖回 `Ok(None)`；`try_acquire()` 取得 `JobLockGuard`，自行 `release()`。
- 工作 panic 時，`run_periodically` 不結束排程：panic 被攔下，記一行 ERROR `scheduled job panicked; it runs again next period`（只有工作名稱，不含 panic 訊息），鎖照一般流程明確釋放，下一個週期照常再跑。這一行 ERROR 是告警的依據。panic hook 另外記的那行 `thread panicked` 仍含位置與 panic 訊息，所以 panic 訊息不能放資料（見[日誌與 trace](#日誌與-trace)）。工作的 closure 在 panic 後會被再次呼叫，它共用的狀態（例如被 poison 的 `Mutex`）要自己處理。`panic = "abort"` 的建置不受影響，行程仍會結束。
- 為什麼選「排程繼續」而不是「readiness 失敗或結束行程」：
  1. 每個副本都排同一個工作，工作的 panic 多半來自資料或邏輯，所有副本會一起 panic。readiness 失敗會讓整個服務退出 Service 的 endpoints，退出行程會讓所有 pod 進入 CrashLoopBackOff；一個背景工作的缺陷就變成整個服務的 API 中斷。
  2. 重啟不會修好資料造成的 panic，只會換來更長的重試間隔與重啟風暴；排程繼續則維持原本的週期重試，資料修好後自動恢復。
  3. 工作必須冪等（規則 1），重跑同一輪是安全的；panic 前持有的鎖已釋放，別的副本也可以接手。
  4. 原本「默默停止」的問題來自沒有訊號。現在每次 panic 都有一行 ERROR，可以用日誌告警。
- 鎖跟著資料庫 session：副本當機或斷線時 session 結束，鎖跟著釋放。guard 被 drop（panic、取消）時關閉連線，鎖也一併釋放。
- 工作執行期間每 5 秒 ping 一次 session（`with_heartbeat` 可調，不能是 0；不論設多少，每個鎖逾時內至少 ping 兩次）。session 斷掉，或超過鎖逾時（`with_timeout`，預設 10 秒，範圍 100 ms 到 60 秒，超出範圍會 panic）沒有確認 session 還活著時，工作在下一個 await 被取消並回 `JobLockError::Lost`，因為別的副本可能已經接手。確認的時間點是最後一次有回應的查詢送出的時間。
- 取鎖與釋放鎖等資料庫也最多等鎖逾時：`try_acquire` 在時間內拿不到 pool 的連線回 `JobLockError::Database`（`sqlx::Error::PoolTimedOut`），鎖的查詢沒有回應回 `Lost`；`release` 沒有回應回 `Lost`，guard 關閉連線；工作 panic 後的明確釋放也一樣。`run_periodically` 在釋放之前就記下工作的結果（完成、失敗的 WARN、panic 的 ERROR），所以釋放失敗時這些日誌也不會漏掉，之後另記一行 `scheduled job lock failed`。

### 鎖的保證有時間界限

server 端可以在持有者不知情時結束鎖的 session：網路分割時封包被丟棄、連線沒有被重設，同時 failover、idle 逾時、keepalive 判定或管理者以 `pg_terminate_backend` 結束了 session。server 一結束 session 就釋放鎖，別的副本可以立刻取得；持有者這邊的 TCP 要十幾分鐘才放棄，所以只能靠鎖逾時察覺。

- 網路分割時，持有者最晚在最後一次確認後的一個鎖逾時判定 `Lost` 並停止工作，所以兩個副本同時執行同一個工作的時間最多一個鎖逾時（預設 10 秒），另加計時器與排程的延遲（正常負載下是毫秒級）。
- 分割但 server 沒有結束 session 時，持有者一樣在鎖逾時內停止工作；鎖留在 server 上，直到 server 察覺 session 已死（TCP keepalive 或逾時設定），這段期間沒有副本執行這個工作。
- 連線被乾淨關閉或重設時，下一次 ping 就失敗，不必等逾時。
- 鎖逾時越長越能容忍資料庫偶爾變慢，但重疊的時間也越長；越短則資料庫一慢就取消工作，留到下一輪再跑。

規則：

1. 工作必須冪等：鎖只在上述時間界限內防止同時執行，也不防止重複執行。網路分割時兩個副本可能同時執行最多一個鎖逾時，工作要能承受。
2. 工作要經常 await：取消只發生在 await，長時間不 await 的運算（CPU 密集或阻塞呼叫）在察覺 `Lost` 後仍會跑完那一段，超出上述時間界限。
3. 連線必須是 session 層級：直連 Postgres，或經 session mode 的 pooler；不能經 transaction mode 的 pooling，否則鎖會落在別的 session。
4. key 由 `lock_key(service, job)` 以 64 bit FNV-1a 從固定命名空間、服務名稱與工作名稱算出。這個算法永遠不能改，否則滾動更新期間新舊 pod 算出不同 key，會同時執行同一個工作。改服務或工作的名稱也等於換一把鎖。
5. 工作執行期間佔用 pool 的一條連線。

## 內部身分（`identity`）

入口 gateway 驗過 cookie 後，身分服務為每個請求簽一個短效的內部身分 token，放在 `x-internal-identity` 標頭交給後端（[決策 27](../../docs/architecture/decisions.md)、功能重建清單 A4）。每個服務自己驗證簽章、簽發者、受眾與效期，只信簽章，不信任何其他關於呼叫者的資訊。本模組在 `contract` feature 後面；簽發 token 是[身分服務](../../services/identity/README.md)的工作。

```rust
use axum::extract::FromRef;
use axum::routing::get;
use axum::Router;
use contract::ServiceName;
use serde::Deserialize;
use service_kit::identity::{IdentityKeys, IdentityVerifier, UserIdentity};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    platform: Platform,
}

#[derive(Debug, Deserialize)]
struct Platform {
    internal_identity_keys: IdentityKeys, // TRUSTIOT_INTERNAL_IDENTITY_KEYS，必填
}

#[derive(Clone)]
struct AppState {
    identity: IdentityVerifier,
    // 服務的其他狀態
}

impl FromRef<AppState> for IdentityVerifier {
    fn from_ref(state: &AppState) -> Self {
        state.identity.clone()
    }
}

async fn list_devices(user: UserIdentity) -> String {
    // 只有驗證通過的請求會進到這裡
    format!("{:?}", user.tenant_id())
}

fn routes(config: &Config) -> Router {
    let identity =
        IdentityVerifier::new(ServiceName::Assets, config.platform.internal_identity_keys.clone());
    Router::new()
        .route("/v1/devices", get(list_devices))
        .with_state(AppState { identity })
}
```

- `IdentityVerifier::new(受眾, 公鑰)`：受眾是服務自己的 `ServiceName`，寫在程式裡，不從設定讀。
- handler 加上 `UserIdentity` 參數就要求這個請求帶有效的 token；沒通過的請求不會進 handler，直接回 401。`UserIdentity` 提供 `user_id()`、`tenant()`、`tenant_id()`、`is_platform_admin()` 與完整的 `claims()`。
- router 的 state 必須能以 `FromRef` 取出 `IdentityVerifier`，忘了放 verifier 的 handler 無法編譯。
- 需要 token 但不經 axum 的地方用 `IdentityVerifier::verify_headers(&HeaderMap)` 或 `verify(&str)`。
- 範例服務的 `GET /v1/whoami` 是可以執行的例子。

### token 格式

JWS compact serialization（RFC 7515），payload 是 contract 的 `InternalClaims`（JSON）：

| 部分 | 規定 |
| --- | --- |
| 標頭 | 只能有 `alg`、`typ`、`kid` 三個參數，都是字串：`{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"<key id>"}`。`crit`、`jku`、`jwk`、`x5u`、`b64` 等其他參數，或同一參數出現兩次，一律拒絕 |
| `alg` | 只接受 `EdDSA`（RFC 8037），而且公鑰只能是 Ed25519。演算法由驗證端固定，不看 token 選演算法：`none`、HMAC（拿公鑰當密鑰）、RSA、ECDSA 都在碰到公鑰之前就被拒絕 |
| `typ` | `internal-identity+jwt`（不分大小寫）。明確標示 token 種類（RFC 8725 3.11 節），同一把金鑰簽的其他 JWT 不會被當成內部身分 |
| `kid` | 選公鑰，必須是設定裡的某一把；不認得就拒絕，不會改用別把試 |
| 簽章 | Ed25519 嚴格驗證（`verify_strict`）：拒絕非標準形式的簽章與小階點，同一個 token 無法改寫出另一個有效簽章 |
| 編碼 | base64url、不補 `=`；`+`、`/`、補位與非零的尾端位元都拒絕，每個 token 只有一種寫法 |
| 位置 | 只放在 `x-internal-identity` 標頭，且只能出現一次；不讀 `Authorization`、cookie 或 query |
| 長度 | 最多 8192 bytes（`InternalToken::MAX_LEN`），超過的在解碼前就拒絕 |

驗證順序：長度、三段格式、標頭、`kid`、簽章，簽章通過後才解析 claims。claims 的檢查：

| 檢查 | 規定 |
| --- | --- |
| `iss` | 必須是 `identity` |
| `aud` | 必須是驗證端自己；token 拿去別的服務會被拒絕 |
| 效期 | `exp - iat` 必須在 1 到 300 秒之間（`MAX_LIFETIME_SECS`）。身分服務實際簽 60 秒（2026-10-10 可行性測試）；上限限制 token 外洩後的可用時間，也決定輪替要等多久 |
| 到期 | 現在時間早於 `exp + 5 秒` |
| 生效 | `iat` 與 `nbf`（有的話）不晚於現在時間 `+ 5 秒`：身分服務不需要送 `nbf`，送了就照它檢查 |
| claims 格式 | 缺欄位、型別不對、`aud` 是陣列、時間不是整數秒（`"nbf": null` 也算）、同一個 claim 出現兩次，一律拒絕；不認得的 claim 忽略（contract 的規則） |

5 秒（`LEEWAY_SECS`）是容許的時鐘誤差。節點都要跑 NTP；誤差超過 5 秒時，每個 token 都會被當成過期或尚未生效。

### 拒絕

每種失敗都回同一個回應：401，body 是 M1 的 `{"code":"unauthenticated","message_key":"error.unauthenticated"}`，呼叫端看不出是哪一項檢查失敗。服務記一行 WARN `rejected the request's internal identity`，欄位 `reason` 寫出原因；token、簽章與公鑰都不進日誌。

| `reason` | 情況 |
| --- | --- |
| `missing` | 沒有 `x-internal-identity` 標頭，或值是空的 |
| `repeated` | 標頭出現不只一次 |
| `oversized` | 超過 8192 bytes |
| `malformed` | 不是三段 base64url、有 token 不能有的字元、簽章不是 64 bytes |
| `malformed_header` | 標頭不是剛好 `alg`、`typ`、`kid` 三個字串 |
| `unsupported_algorithm` | `alg` 不是 `EdDSA` |
| `wrong_type` | `typ` 不是 `internal-identity+jwt` |
| `unknown_key` | `kid` 不在設定的公鑰裡 |
| `bad_signature` | 簽章不符 |
| `malformed_claims` | 簽章正確但 payload 不是 `InternalClaims` |
| `wrong_issuer` | `iss` 不是 `identity` |
| `wrong_audience` | `aud` 是別的服務 |
| `invalid_lifetime` | `exp - iat` 不在 1 到 300 秒之間 |
| `expired` | 已過 `exp + 5 秒` |
| `not_yet_valid` | `iat` 或 `nbf` 晚於現在時間 `+ 5 秒` |

### 公鑰與輪替

決定（2026-10-10，Laysi/trustiot-v2#52）：公鑰由部署以設定交給每個服務，不由身分服務在執行時發布。

- 身分服務的公鑰寫成 JWK Set（RFC 7517），放在平台設定 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`，服務設定裡是 `platform.internal_identity_keys`。範例：`{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"2026-10-a","x":"<32 bytes 的 base64url>"}]}`。
- 每把公鑰的 `kty` 必須是 `OKP`、`crv` 必須是 `Ed25519`、`kid` 是 1 到 64 個可見 ASCII 字元且不重複；`alg`、`use`、`key_ops` 可以省略，有寫就必須是 `EdDSA`、`sig`、含 `verify` 的清單；一組最多 8 把。含私鑰（`d`）、小階點、格式不對的公鑰，服務都拒絕啟動，錯誤訊息只寫第幾把與原因，不寫值。
- 平台設定只由部署決定（`TRUSTIOT__PLATFORM__*` 會被拒絕，chart 的 `services.<name>.env` 也不能設 `TRUSTIOT_` 後接英數字的名稱），所以單一服務的設定無法換掉公鑰。
- 公鑰不是祕密，放在 chart 的平台設定 ConfigMap；私鑰只在身分服務的 Secret。chart 的欄位是 `internalIdentity.publicKeys`（組成 JWK Set 寫進 ConfigMap）與 `internalIdentity.signingKeySecret`（只掛進身分服務），見 [chart README](../../deploy/chart/README.md) 的「內部身分」。
- 本機開發：直接設 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`，或寫在設定檔的 `[platform]` 表，值是 JWK Set 的 JSON 字串。

為什麼不由身分服務發布（例如服務在執行時抓 JWKS 端點）：

1. 驗證不多一個執行期依賴：身分服務停機時，已經發出去的 token 照樣能驗證，服務啟動也不必等身分服務。
2. 不多一個信任來源：公鑰只來自部署設定，冒充身分服務或改寫抓取回應都換不了公鑰；也沒有「看到不認得的 `kid` 就重抓」可以被濫用來放大流量。
3. 地端與離線環境一樣運作，沒有快取與重試的狀態要處理。

代價是換公鑰要滾動重啟所有服務。平台設定變更時 chart 本來就會滾動更新 pod，服務也一起發版（決策 4），而輪替一年只有幾次，所以可以接受。

輪替步驟（新舊兩把 key 以 `kid` 區分，例如 `2026-10-a` 換成 `2027-04-a`）：

1. **加新公鑰**：產生新的金鑰對，把新公鑰加進 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`（新舊兩把並存）並部署。確認每個服務都已滾動完成（每個 Deployment 的 `kubectl rollout status` 都完成），身分服務仍用舊私鑰簽章。
2. **換簽章私鑰**：把身分服務的簽章私鑰換成新的（Secret；`kid` 由身分服務啟動時從公鑰找出，見[身分服務的金鑰輪替](../../services/identity/README.md#金鑰輪替)），身分服務滾動完成後只簽新 `kid`。步驟 1 沒有全部完成就換，還沒拿到新公鑰的服務會以 `unknown_key` 拒絕新 token。
3. **移除舊公鑰**：身分服務滾動完成後至少等 `300 + 5` 秒（效期上限加時鐘誤差），舊 token 全部過期，再從 `TRUSTIOT_INTERNAL_IDENTITY_KEYS` 移除舊公鑰並部署，最後刪除舊私鑰。

私鑰外洩時：立即同時做步驟 1 到 3，不等舊 token 過期。滾動期間部分請求會被拒絕（401），換取外洩的金鑰馬上失效。

### 背景工作沒有使用者身分

`UserIdentity` 只能由 `IdentityVerifier` 從通過檢查的 token 建立，handler 透過 extractor 拿到它。它沒有其他建構方式、沒有 `Default`、沒有 `Deserialize`，`ServiceIdentity` 也不能轉成它，所以背景工作、排程工作與事件消費這些沒有請求的程式拿不到使用者身分，只能以 `ServiceIdentity`（服務自己）執行：

```rust
use contract::ServiceName;
use service_kit::identity::ServiceIdentity;

let actor = ServiceIdentity::new(ServiceName::Assets); // 或 verifier.service_identity()
context.spawn(purge_expired(actor));
```

這由 `src/identity.rs` 的 compile-fail doctest 證明：以欄位、`From`、`Into`、`Deserialize`、`Default` 建立 `UserIdentity` 都無法編譯。不要把 handler 拿到的 `UserIdentity` 移進 `Context::spawn` 或 `tokio::spawn`：token 60 秒就過期，背景工作要做的事以服務自己的身分執行。背景工作代表服務呼叫其他服務的完整流程在 Laysi/trustiot-v2#106。

## 租戶隔離（`tenant`）

帶租戶資料的表一律啟用 RLS，policy 拿 `tenant_id` 比對交易的租戶設定 `trustiot.tenant_id`；kit 在交易開頭設定一次，值取自已驗證的內部身分（[決策 29](../../docs/architecture/decisions.md)）。查詢漏寫租戶條件也只看得到該交易的租戶，交易外的查詢看不到任何租戶資料。`tenant` 在 `contract` feature 後面；連線與檢查在 `postgres`，不需要 contract。

```rust
use service_kit::identity::UserIdentity;
use service_kit::postgres::{PostgresConfig, SslMode};
use service_kit::tenant::{TenantDb, TenantDbError};
use sqlx::postgres::PgPoolOptions;

#[derive(Debug, serde::Deserialize)]
struct Platform {
    postgres_host: String,                    // TRUSTIOT_POSTGRES_HOST
    postgres_port: u16,                       // TRUSTIOT_POSTGRES_PORT
    postgres_database: String,                // TRUSTIOT_POSTGRES_DATABASE
    postgres_sslmode: SslMode,                // TRUSTIOT_POSTGRES_SSLMODE，只接受 verify-full
    postgres_ca_file: std::path::PathBuf,     // TRUSTIOT_POSTGRES_CA_FILE，必填：唯一信任的 CA
}

fn tenant_db(platform: &Platform, username: String, password: String) -> Result<TenantDb, service_kit::BoxError> {
    let config = PostgresConfig {
        host: platform.postgres_host.clone(),
        port: platform.postgres_port,
        database: platform.postgres_database.clone(),
        ssl_mode: platform.postgres_sslmode,
        ca_file: platform.postgres_ca_file.clone(),
        username, // 服務角色的帳密，由 Laysi/trustiot-v2#58、Laysi/trustiot-v2#60 決定怎麼交給服務
        password,
    };
    Ok(TenantDb::connect_lazy(config.connect_options(), PgPoolOptions::new().max_connections(10))?)
}

async fn device_names(db: &TenantDb, user: &UserIdentity) -> Result<Vec<String>, TenantDbError> {
    let mut tx = db.begin(user).await?; // 租戶取自 user；沒有租戶回 NoTenant
    let names = sqlx::query_scalar("SELECT name FROM devices ORDER BY name") // 不寫租戶條件
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(names)
}
```

- 健康檢查用 `db.ping()` 註冊（範例服務的 `postgres` 檢查）。完整可執行的例子是範例服務的 `GET /v1/items`。
- 連到 Postgres 的唯一方式是 `TenantDb`、`PlatformDb` 或 `postgres::connect_lazy`；不要自己建 `PgPool`，否則沒有伺服器驗證與連線檢查。

### 交易

| 呼叫 | 用在 | 租戶從哪來 |
| --- | --- | --- |
| `TenantDb::begin(&UserIdentity)` | handler | 請求的內部身分 token（已驗證）；token 沒有租戶時回 `TenantDbError::NoTenant`，端點回 403 `permission_denied` |
| `BackgroundTenantDb::begin(TenantId, purpose)` | 排程工作、事件消費 | 工作本身：事件帶的租戶，或平台角色查詢找出的租戶。以 DEBUG 記下服務、租戶與 `purpose` |
| `PlatformDb::begin(PlatformActor, purpose)` | 平台後台、跨租戶的工作 | 不設定租戶；平台角色只看得到表上有 `platform_access` policy 的資料。以 INFO 記一行 `platform database access`（actor、`purpose`），操作本身的紀錄在 Laysi/trustiot-v2#130 |

- 交易的第一個陳述就是 `SELECT set_config('trustiot.tenant_id', $1, true)`：等同 `SET LOCAL`，但值以參數綁定。交易結束（commit、rollback、drop、出錯）設定就消失，連線回到 pool 時不帶租戶。
- `TenantTransaction` 沒有改租戶的方法，`TenantDb` 也沒有以任意租戶開交易的 handler 用方法；`src/tenant.rs` 的 compile-fail doctest 證明這些寫法無法編譯。SQL 裡設定租戶或角色由 `tools/sql-check` 擋下，只有 `src/tenant.rs` 例外。
- 背景工作沒有使用者身分（見[內部身分](#背景工作沒有使用者身分)），所以決定：代表單一租戶的工作用 `BackgroundTenantDb` 開交易，租戶來自工作本身；要找出「哪些租戶有工作」這種跨租戶查詢，用 `PlatformDb` 與 `PlatformActor::service`，會留紀錄。
- `BackgroundTenantDb` 是能指定租戶的能力，只有啟動時的 `TenantDb::connect_lazy_with_background(options, pool, ServiceIdentity)` 會一併產生（與 `TenantDb` 共用 pool）。把 `TenantDb` 放進 router 的 state、`BackgroundTenantDb` 只交給背景工作：handler 手上的 `TenantDb` 沒有任何接受租戶的方法，無法把請求裡的租戶拿去開交易（compile-fail doctest 證明）。唯一的例外是身分服務的 session 查詢：查 session 時請求還沒有內部身分，租戶取自剛從資料庫讀出的 session 列，不取自請求，所以由它的 session store 私下持有 `BackgroundTenantDb`，handler 拿不到（見 [services/identity](../../services/identity/README.md) 的「資料庫」）。
- `PlatformActor::admin(&UserIdentity)` 只有平台管理者（A2）拿得到；`PlatformActor` 沒有其他建構方式。

### outbox（`crates/outbox`）

- 產生端在同一個 `&mut *tx` 呼叫 `outbox::publish`，事件與業務資料一起 commit 或 rollback；事件的租戶用 `tx.tenant_id()`。
- `outbox_event` 的 RLS 照 [`sql/outbox-event.sql`](sql/outbox-event.sql)，由產生端的 migration 在 `install_producer` 之後執行：
  1. 服務角色只有 INSERT，`tenant_isolation` 的 `WITH CHECK` 讓它只寫得進目前租戶的事件；不給 SELECT（`publish` 不用 `RETURNING`）。
  2. 全平台事件（`tenant_id` 為 NULL）決定（2026-10-10）：允許，但只由產生端的平台角色經 `PlatformDb` 寫（`platform_events` policy：`FOR INSERT ... WITH CHECK (tenant_id IS NULL)`），會留紀錄；平台角色寫不了任何租戶的事件，服務角色寫不了全平台事件。平台角色另需 `outbox_aggregate` 的權限（outbox README「資料庫權限」中產生端的那幾行）。
  3. 投遞要讀所有租戶的事件，所以消費端以**它的平台角色**讀（`consumer_read` policy：`FOR SELECT TO <消費端平台角色> USING (true)`），不能用服務角色：開放給服務角色的跨租戶 policy 會讓它的連線被拒（`PermissivePolicy`）。handler 的寫入則在消費端服務角色的租戶交易裡。
- 目前 `outbox::Consumer` 讀取與 handler 交易共用一個 pool，`Handler::begin` 也還沒有設定租戶的 kit 介面；兩者要配合上一點調整（讀取用平台角色的 pool、handler 交易用 `BackgroundTenantDb` 的 pool 並由 kit 設定租戶），列為後續工作。

### 角色與 policy 範本

每個服務三個角色：

| 角色 | 用途 | 限制 |
| --- | --- | --- |
| migration 角色（例 `assets_owner`） | 跑 migration，擁有 schema 與表 | 不給服務在執行期使用；表的擁有者不受 RLS 限制，所以 migration 可以跨租戶回填 |
| 服務角色（例 `assets_app`） | `TenantDb`、`BackgroundTenantDb` | 不是 superuser、沒有 `BYPASSRLS`、不擁有任何表、不是擁有者角色的成員、沒有 `TRUNCATE`、不能建暫存表；套用到它的 policy 只能是租戶條件 |
| 平台角色（例 `assets_platform`） | `PlatformDb` | 同服務角色，但可以有以角色名稱明列給它的 policy（`platform_access`、`platform_events`、`consumer_read`）；不給 `BYPASSRLS`，所以沒開放的表一列都看不到 |

每個資料庫建好後，擁有者先執行一次 [`sql/database.sql`](sql/database.sql)：撤銷 `PUBLIC` 預設有的 `TEMPORARY`。暫存表不受 RLS 限制，而且跟著 session 活到交易之後，pool 重用連線時，一個租戶交易複製進暫存表的列，下一個交易（任何租戶）都讀得到。

帶租戶資料的表照 [`sql/tenant-table.sql`](sql/tenant-table.sql) 寫 migration：`tenant_id uuid NOT NULL` 排在主鍵與索引的最前面；`ENABLE ROW LEVEL SECURITY`；`tenant_isolation` policy 對所有角色，`USING` 與 `WITH CHECK` 一字不差地用 `postgres::TENANT_CONDITION`（連線檢查比對 Postgres 存下的寫法 `TENANT_CONDITION_AS_STORED`，任何放寬，例如加上 `OR ...`，都會被拒）；需要時才加以名稱給平台角色的 `platform_access`；只 grant 需要的權限。檢視表一律 `WITH (security_invoker = true)`；物化檢視、foreign table 與讀租戶表的 `SECURITY DEFINER` 函式不能保護，不要讓服務角色讀得到或執行（函式建立後 `REVOKE EXECUTE ... FROM PUBLIC`）。`tests/tenant_db.rs` 直接執行這兩個檔案，範本就是測過的版本。

### 連線（`postgres`）

- 伺服器憑證一律驗證（清單 M3）：`postgres::SslMode` 只接受 `verify-full`，宣告成 `TRUSTIOT_POSTGRES_SSLMODE` 的型別，其他值（`disable`、`require`、`verify-ca`⋯）讓服務在接流量前以 exit code 1 停止，錯誤訊息不含設定的值。`postgres::connect_lazy`、`TenantDb::connect_lazy`、`PlatformDb::connect_lazy` 也拒絕 `sslmode` 不是 `verify-full` 或走 Unix socket 的連線設定。
- 信任範圍（Laysi/trustiot-v2#272）：伺服器憑證必須鏈到 `TRUSTIOT_POSTGRES_CA_FILE`（`PostgresConfig::ca_file`，chart 掛在 `/etc/trustiot/postgres-ca/ca.crt`）裡的 CA，**只有**這些 CA；公開根憑證（webpki-roots）與系統的憑證庫都不信任。所以公開 CA 為資料庫主機名稱簽發的憑證也會被拒，資料庫用公開 CA 簽的憑證時（例如某些託管 Postgres），要把那個 CA 放進 CA 檔。
  - `ca_file` 是必填欄位，把 `TRUSTIOT_POSTGRES_CA_FILE` 宣告成不是 `Option` 的 `PathBuf`：沒有設定就以 exit code 1 停止，不退回公開根憑證。檔案在每次建立新連線時讀取，換 CA 不必重啟；檔案可以放多張 CA（換 CA 的過渡期）。
  - 做法：sqlx 的 rustls 連線器一律先放 `webpki_roots::TLS_SERVER_ROOTS` 再加上設定的 CA，沒有可以關掉的設定或 feature（其他 TLS feature 改載入系統憑證庫）。根目錄 `Cargo.toml` 以 `[patch.crates-io]` 把 `webpki-roots` 換成空清單的 [`crates/webpki-roots-empty`](../webpki-roots-empty/src/lib.rs)，信任清單因此只剩設定的 CA；沒有帶 root 憑證的連線設定（例如自己組的 `PgConnectOptions`）一律連不上。這是暫時做法：sqlx 提供自訂 rustls 設定或不帶預設根憑證的 feature 後，依 Laysi/trustiot-v2#304 改用官方做法並移除這個替換。
  - 這個替換對整個 workspace 生效，所以其他 crate 不能依賴 `webpki-roots`（需要公開根憑證的 client 請改用系統憑證庫或 `webpki-root-certs`）。`tests/postgres_trust.rs` 檢查 `Cargo.lock`：`webpki-roots` 是這個空 crate、只有 sqlx 用它、sqlx 沒有載入 `rustls-native-certs` 等其他信任來源；升級 sqlx 時這項檢查失敗，代表替換失效，要先處理再合併。
- 每個新連線在交給 pool 之前執行 `postgres::check_session`，有問題就關閉連線並記一行 ERROR `refused a database session that row-level security would not protect`（`problem` 欄位寫原因）。sqlx 會一直重試到 pool 的 acquire timeout，之後 `begin`、`ping` 回 `TenantDbError::UnsafeSession`，readiness 失敗。檢查項目（`UnsafeSession`）：

| 問題 | 原因 |
| --- | --- |
| `BypassesRls` | 角色本身或它所屬的角色是 superuser 或有 `BYPASSRLS` |
| `TenantPreset` | 連線一開始就有 `trustiot.tenant_id`（角色或資料庫預設值、連線參數） |
| `OwnsRelation` | 角色擁有、或屬於擁有某張表或檢視的角色 |
| `UnprotectedTable` | 角色能用、有 `tenant_id` 欄的表沒啟用 RLS（含 partition） |
| `PermissivePolicy` | 套用到角色（`PUBLIC` 或它所屬的角色）的 permissive policy 不是一字不差的租戶條件；平台角色的連線只放行以平台角色本身名稱給它、而且沒給 `PUBLIC` 的 policy，經由它所屬的群組角色取得的不放行 |
| `UnprotectedView` | 角色能用、讀租戶資料（直接或經其他檢視）的檢視表不是 `security_invoker`，不論有沒有 `tenant_id` 欄 |
| `Unprotectable` | 角色能用、含租戶資料的物化檢視或 foreign table |
| `DefinerFunction` | 角色能執行別的角色擁有的 `SECURITY DEFINER` 函式（extension 與系統 schema 的函式除外）：函式以擁有者的權限與 RLS 可見範圍執行，擁有者可能是表的擁有者、superuser 或有跨租戶 policy 的平台角色 |
| `TemporaryTables` | 角色能在資料庫建暫存表（`TEMPORARY` 權限；Postgres 預設給 `PUBLIC`） |
| `TruncateAllowed` | 角色對租戶表有 `TRUNCATE`（不受 RLS 限制） |

- 檢查依欄位名稱 `tenant_id` 判斷哪些是租戶表（再沿著檢視的相依找出建在它們上面的檢視），所以租戶欄一律叫這個名字。
- `PlatformDb` 以平台角色的規則檢查（`SessionRole::Platform`），其他 pool 以服務角色的規則（`SessionRole::Service`）。

### 威脅模型與限制

- RLS 擋的是程式漏寫租戶條件。SQL 注入可以用服務角色改掉 `trustiot.tenant_id`，所以只用參數化查詢；`tools/sql-check` 隨 `cargo test --workspace` 擋下由字串組成的 SQL、`AssertSqlSafe`、SQL 裡的 `set_config` 與 `SET ROLE` 等（規則見[它的 README](../../tools/sql-check/README.md)）。
- 本機開發環境（`scripts/`）的 Postgres 只接受 TLS，憑證由 `up` 當場產生的開發 CA 簽發，給服務的 `TRUSTIOT_POSTGRES_SSLMODE` 是 `verify-full`、`TRUSTIOT_POSTGRES_CA_FILE` 指向那個 CA（Laysi/trustiot-v2#273，見 [scripts/README.md](../../scripts/README.md)「Postgres 的 TLS」）。開發環境給範例服務一個一般的資料庫角色，它以 `verify-full` 連上本機 Postgres，`/readyz` 的 `postgres` 檢查通過才就緒。
- AI 閘道（不依賴 contract）沒有 `tenant`；它若有租戶資料表，需要不經 contract 的租戶交易介面。

## contract feature

`services/ai-gateway` 不依賴 `crates/contract`（決策 28），但會使用本 kit，所以 kit 中需要 contract 的部分都放在 kit 的 `contract` feature 後面。這個 feature 預設開啟，一般服務直接得到 M1 錯誤格式；AI 閘道以 `default-features = false` 關掉它。

| | `contract` 開啟（預設） | `contract` 關閉 |
| --- | --- | --- |
| handler 逾時 | 504，body `{"code":"timeout","message_key":"error.timeout"}` | 504，沒有 body |
| handler panic | 500，body `{"code":"internal","message_key":"error.internal"}` | 500，沒有 body |
| 內部身分（`identity`） | 有；拒絕時 401，body `{"code":"unauthenticated","message_key":"error.unauthenticated"}` | 沒有這個模組 |
| 租戶交易（`tenant`） | 有 | 沒有這個模組；`postgres`（`verify-full` 與連線檢查）兩者都有 |

之後需要 contract 的功能也放在這個 feature 後面。路由不存在的 404、method 不符的 405 與 axum extractor 的拒絕訊息由 axum 產生，不是 kit 的回應，還不是 M1 格式。

`tests/dependency_isolation.rs` 對 kit 執行 `cargo tree`：關閉預設 feature 時相依樹裡沒有 contract，開啟時有，隨 `cargo test --workspace` 在 CI 執行。關閉 feature 的程式路徑要另外以 `cargo clippy -p service-kit --no-default-features --all-targets -- -D warnings` 與 `cargo test -p service-kit --no-default-features` 檢查，`--workspace` 的指令只建置預設 feature。

## 測試

| 指令 | 內容 | 需要 |
| --- | --- | --- |
| `cargo test -p service-kit --lib` | 單元測試：設定與平台設定、遮蔽、日誌格式、health、kit 的錯誤回應、lock key；內部身分的每一項檢查、公鑰設定與輪替（`--lib identity` 只跑這部分） | 無 |
| `cargo test -p service-kit --doc` | 文件範例，含證明背景工作拿不到使用者身分的 compile-fail 測試 | 無 |
| `cargo test -p service-kit --test identity` | 範例服務的 `/v1/whoami`：偽造簽章、過期、受眾不符、缺標頭都回 401 `unauthenticated`，輪替期間新舊 `kid` 都通過，無法使用的公鑰設定讓服務拒絕啟動；token 與公鑰不進日誌 | 無 |
| `cargo test -p service-kit --test lifecycle` | 範例服務的啟動、health（健康埠只有 `/healthz`、`/readyz`，其他路徑 404）、SIGTERM 與 SIGINT 收尾（兩個埠的 readiness 一致）、設定錯誤與埠被占用，用真的行程、訊號與 HTTP | 無 |
| `cargo test -p service-kit --test errors` | handler 逾時回 504、panic 回 500 並記成 JSON 日誌 | 無 |
| `cargo test -p service-kit --test logging` | JSON 日誌、trace id；body、標頭、query 與路徑不進日誌 | 無 |
| `cargo test -p service-kit --test otlp` | OTLP 匯出到假的接收端：預設關閉、帶 trace id、指標送出、不含 body；collector 連線被重設或回 400 時，`debug`、`trace` 與針對 SDK 的指令下，端點路徑、SDK 環境變數與 collector 回應的 canary 都不出現在 stdout 與 stderr；帶 query 的端點被拒絕且不印出 | 無 |
| `cargo test -p service-kit --test container` | 範例服務在容器內以 PID 1 執行（唯讀根目錄、任意 UID），`stop` 時收尾並以 0 結束 | podman 或 docker，會拉 `gcr.io/distroless/cc-debian13` |
| `cargo test -p service-kit --test job_lock` | 兩個副本互斥、session 中斷、guard 釋放、連線回到 pool、工作 panic 後排程繼續且日誌不含 panic 訊息；網路分割（經本機 TCP 代理，代理改成黑洞後丟棄封包但不關連線）時持有者在鎖逾時內停止工作，取鎖與釋放鎖在鎖逾時內放棄 | `TEST_DATABASE_URL`，或 podman 或 docker（會拉 `postgres:18.6-alpine`） |
| `cargo test -p service-kit --test tenant_db` | 租戶隔離：沒設定租戶查不到也寫不進任何列；租戶 A 讀不到、改不到、寫不進租戶 B 的列；pool 重用連線時租戶不會帶到下一個交易；服務角色不是擁有者、沒有 BYPASSRLS、關不掉 RLS；不安全的連線被拒；平台角色只看得到開放的表；outbox 範本（租戶事件、全平台事件、消費端讀取）；伺服器憑證與主機名稱驗證；範例服務在 `sslmode` 不是 `verify-full` 或沒有設定 `TRUSTIOT_POSTGRES_CA_FILE` 時拒絕啟動，`/v1/items` 只回 token 租戶的資料 | podman 或 docker（會拉 `postgres:18.6-alpine`，以測試時產生的 CA 啟用 TLS）；不使用 `TEST_DATABASE_URL`，因為測試要以 superuser 建角色，並需要知道伺服器的 CA |
| `cargo test -p service-kit --test postgres_trust` | 只信任設定的 CA：sqlx 編進來的 `webpki-roots` 是空清單，`Cargo.lock` 裡只有 sqlx 用它、sqlx 沒有載入系統憑證庫等其他信任來源；對真的 Postgres：設定的 CA（含多張 CA 的檔案）連得上，別的 CA 簽的、主機名稱不符的、沒有設定 CA 的都被拒 | 前兩項無；真 Postgres 那項要 podman 或 docker（同 `tenant_db`） |
| `cargo test -p sql-check` | 由字串組成的 SQL 與 kit 以外的租戶設定（`tools/sql-check`） | 無 |
| `cargo test -p service-kit --test dependency_isolation` | 關閉 feature 時不依賴 contract，開啟時依賴 | 無 |
| `cargo test -p service-kit --test chart_contract` | kit 的預設 API 埠、健康埠、探針的路徑與埠（`health`）與收尾時間符合 `deploy/chart/values.yaml` | 無 |

- `TEST_DATABASE_URL` 指向可以任意取 advisory lock 的 Postgres（測試不建表），且不能要求驗證主機名稱，因為分割測試改經 `127.0.0.1` 的代理連線；沒設時每個測試起一個用完即刪的 Postgres 容器。
- `tenant_db` 的測試共用一個啟用 TLS 的 Postgres 容器（`tests/support/postgres.rs`，記憶體上限 512 MB），最後一個用到它的測試結束時刪除；每個測試用自己的資料庫。
- 沒有容器執行環境時，需要它的測試在本機略過，並在 stderr 印出 `skipped:`；設了 `CI` 環境變數時改為失敗，CI 不會默默略過。
- 加 `-- --nocapture` 會印出測試報告用的數據，例如容器停止花的時間與 exit code。
