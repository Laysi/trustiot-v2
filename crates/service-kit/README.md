# service-kit

雲端服務的共用骨架，實作[部署文件](../../docs/architecture/deployment.md)「讓 k8s 只是打包工作的服務設計規則」：設定只來自環境變數或設定檔、health 與 readiness、收到 SIGTERM 正常收尾、JSON 日誌與 OTLP 匯出、排程工作以 Postgres advisory lock 互斥。每個服務的 `main` 都透過 `service_kit::run` 啟動。

| 模組 | 內容 |
| --- | --- |
| `config` | 環境變數與 TOML 設定檔、啟動時驗證（`Validate`）；錯誤訊息不含設定值 |
| `health` | `/healthz`、`/readyz` 與 readiness 檢查 |
| `shutdown` | SIGTERM、SIGINT 收尾，`Shutdown` 狀態 |
| `http` | 請求 span 與存取日誌、handler 逾時、panic 轉成 500 |
| `telemetry` | JSON 日誌、trace id、OTLP 匯出、敏感欄位遮蔽 |
| `job_lock` | 排程工作同一時間只在一個副本執行 |

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

`run` 依序：

1. 讀取並驗證設定；有錯就記一行 ERROR、exit code 1，不啟動。
2. 啟動日誌與 trace，安裝 SIGTERM、SIGINT handler。
3. 呼叫 `start` 取得服務的 router；回傳錯誤時 exit code 1。啟動期間收到 SIGTERM 就直接以 exit code 0 結束。
4. 監聽 `http.listen_addr`，加上 health 路由與 kit 的 layer，readiness 開始回 200。
5. 收到 SIGTERM 後依[收尾](#收尾sigterm)結束，回傳 exit code。

`Context` 提供 `config()`（已驗證的服務設定）、`health()`（註冊 readiness 檢查）、`shutdown()`（收尾狀態）與 `spawn(task)`（收尾時會等待的背景工作）。

## 設定

- 每個設定鍵有點分隔的路徑，例如 `http.listen_addr`。用環境變數 `TRUSTIOT__HTTP__LISTEN_ADDR` 設定（前綴之後每段轉大寫，以 `__` 連接），或寫在 `TRUSTIOT_CONFIG_FILE` 指定的 TOML 檔；兩者都有時以環境變數為準。值為空字串的變數視同未設定。
- 頂層的 `http`、`shutdown`、`telemetry` 屬於 kit（`config::KIT_SECTIONS`），其他頂層鍵交給服務自己的設定型別。設定型別加上 `#[serde(deny_unknown_fields)]`，拼錯的鍵才會在啟動時被擋下；沒有自己設定的服務用 `config::NoConfig`。
- 環境變數的值都是字串，kit 依欄位型別轉成數字或布林；清單用逗號分隔，例如 `TRUSTIOT__CORS__ORIGINS=https://a.example,https://b.example`。元素本身含逗號的清單寫在設定檔。
- 時間長度欄位加 `#[serde(deserialize_with = "service_kit::config::duration")]`，值寫成 `5s`、`500ms`、`1m 30s`。
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
| `http.listen_addr` | `TRUSTIOT__HTTP__LISTEN_ADDR` | `0.0.0.0:8080` | 監聽位址；health 端點也在這個埠 |
| `http.request_timeout` | `TRUSTIOT__HTTP__REQUEST_TIMEOUT` | `30s` | handler 產生回應標頭的時間上限，逾時回 504；串流的 body 不受限。用 504 而不是 503，是因為入口會重試 503，非冪等的請求可能被執行兩次 |
| `shutdown.drain_delay` | `TRUSTIOT__SHUTDOWN__DRAIN_DELAY` | `5s` | 收到 SIGTERM 後照常服務多久，等入口、ztunnel 與 kube-proxy 不再送來新請求 |
| `shutdown.drain_timeout` | `TRUSTIOT__SHUTDOWN__DRAIN_TIMEOUT` | `20s` | 停止接受連線後，進行中的請求與背景工作結束的時間上限；必須大於 0 |
| `telemetry.log_filter` | `TRUSTIOT__TELEMETRY__LOG_FILTER` | `info` | 預設等級加上以逗號分隔的 `target=level`，例如 `info,sqlx=warn` |
| `telemetry.otlp_endpoint` | `TRUSTIOT__TELEMETRY__OTLP_ENDPOINT` | 未設定（不匯出） | OTLP/HTTP 接收端的 base URL，例如 `http://otel-collector:4318`；只接受 `http://`（mesh 已加密這一段），不得含帳密或 query |

## Health 與 readiness

- `GET /healthz`：行程能處理 HTTP 就回 200 `{"status":"ok"}`。不檢查依賴，依賴故障時才不會讓每個 pod 都被重啟。
- `GET /readyz`：啟動完成、尚未收到 SIGTERM、且每個檢查都通過時回 200 `{"status":"ready"}`；否則回 503，`status` 是 `starting`、`draining` 或 `unavailable`。
- `Health::add_check(name, check)` 註冊依賴檢查，例如資料庫 ping。每個檢查最多 800 ms，結果快取 1 秒，同時進來的 probe 共用同一輪檢查；檢查開始失敗與恢復時各記一行日誌，錯誤文字不能含祕密。
- 兩個端點都在主埠，不記存取日誌、不受 handler 逾時限制，回應帶 `Cache-Control: no-store`。
- Istio ambient 下 kubelet 的 probe 來自 169.254.7.127、不經 ztunnel，chart 的 NetworkPolicy 要放行這個位址（[可行性測試](../../docs/feasibility/2026-10-10-edge-poc.md)）。

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

- 設定 `telemetry.otlp_endpoint` 後，日誌送到 `<url>/v1/logs`、trace 送到 `<url>/v1/traces`（OTLP/HTTP protobuf，批次送出），resource 帶 `service.name` 與 `service.version`。預設關閉；只設 `OTEL_EXPORTER_OTLP_ENDPOINT` 不會開啟匯出，端點以 kit 的設定為準。
- 其他 OpenTelemetry SDK 的標準環境變數仍然有效，例如取樣的 `OTEL_TRACES_SAMPLER` 與批次的 `OTEL_BSP_*`；在 chart 上 `OTEL_*` 只由 chart 設定。
- SDK 自己的診斷日誌（target 以 `opentelemetry` 開頭）只寫 stdout，不匯出，避免匯出失敗時自我循環。

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

- `run_periodically` 每個週期試一次，直到 `Shutdown::stopping`；失敗記 WARN。`run_exclusive(job)` 只試一次，拿不到鎖回 `Ok(None)`；`try_acquire()` 取得 `JobLockGuard`，自行 `release()`。
- 鎖跟著資料庫 session：副本當機或斷線時 session 結束，鎖跟著釋放。工作執行期間每 5 秒 ping 一次 session（`with_heartbeat` 可調）；session 斷掉時工作在下一個 await 被取消並回 `JobLockError::Lost`，因為別的副本可能已經接手。guard 被 drop（panic、取消）時關閉連線，鎖也一併釋放。

規則：

1. 工作必須冪等：鎖只防止同時執行，不防止重複執行。
2. 連線必須是 session 層級：直連 Postgres，或經 session mode 的 pooler；不能經 transaction mode 的 pooling，否則鎖會落在別的 session。
3. key 由 `lock_key(service, job)` 以 64 bit FNV-1a 從固定命名空間、服務名稱與工作名稱算出。這個算法永遠不能改，否則滾動更新期間新舊 pod 算出不同 key，會同時執行同一個工作。改服務或工作的名稱也等於換一把鎖。
4. 工作執行期間佔用 pool 的一條連線。

## contract feature

`services/ai-gateway` 不依賴 `crates/contract`（決策 28），但會使用本 kit，所以 kit 中需要 contract 的部分都放在 kit 的 `contract` feature 後面。這個 feature 預設開啟，一般服務直接得到 M1 錯誤格式；AI 閘道以 `default-features = false` 關掉它。

| | `contract` 開啟（預設） | `contract` 關閉 |
| --- | --- | --- |
| handler 逾時 | 504，body `{"code":"timeout","message_key":"error.timeout"}` | 504，沒有 body |
| handler panic | 500，body `{"code":"internal","message_key":"error.internal"}` | 500，沒有 body |

之後需要 contract 的功能（例如 Laysi/trustiot-v2#52 驗證內部身分）也放在這個 feature 後面。路由不存在的 404、method 不符的 405 與 axum extractor 的拒絕訊息由 axum 產生，不是 kit 的回應，還不是 M1 格式。

`tests/dependency_isolation.rs` 對 kit 執行 `cargo tree`：關閉預設 feature 時相依樹裡沒有 contract，開啟時有，隨 `cargo test --workspace` 在 CI 執行。關閉 feature 的程式路徑要另外以 `cargo clippy -p service-kit --no-default-features --all-targets -- -D warnings` 與 `cargo test -p service-kit --no-default-features` 檢查，`--workspace` 的指令只建置預設 feature。

## 測試

| 指令 | 內容 | 需要 |
| --- | --- | --- |
| `cargo test -p service-kit --lib` | 單元測試：設定與平台設定、遮蔽、日誌格式、health、kit 的錯誤回應、lock key | 無 |
| `cargo test -p service-kit --test lifecycle` | 範例服務的啟動、health、SIGTERM 與 SIGINT 收尾、設定錯誤，用真的行程、訊號與 HTTP | 無 |
| `cargo test -p service-kit --test errors` | handler 逾時回 504、panic 回 500 並記成 JSON 日誌 | 無 |
| `cargo test -p service-kit --test logging` | JSON 日誌、trace id；body、標頭、query 與路徑不進日誌 | 無 |
| `cargo test -p service-kit --test otlp` | OTLP 匯出到假的接收端：預設關閉、帶 trace id、不含 body | 無 |
| `cargo test -p service-kit --test container` | 範例服務在容器內以 PID 1 執行（唯讀根目錄、任意 UID），`stop` 時收尾並以 0 結束 | podman 或 docker，會拉 `gcr.io/distroless/cc-debian13` |
| `cargo test -p service-kit --test job_lock` | 兩個副本互斥、session 中斷、guard 釋放、連線回到 pool | `TEST_DATABASE_URL`，或 podman 或 docker（會拉 `postgres:18.6-alpine`） |
| `cargo test -p service-kit --test dependency_isolation` | 關閉 feature 時不依賴 contract，開啟時依賴 | 無 |
| `cargo test -p service-kit --test chart_contract` | kit 的預設埠、health 路徑與收尾時間符合 `deploy/chart/values.yaml` | 無 |

- `TEST_DATABASE_URL` 指向可以任意取 advisory lock 的 Postgres（測試不建表）；沒設時每個測試起一個用完即刪的 Postgres 容器。
- 沒有容器執行環境時，需要它的測試在本機略過，並在 stderr 印出 `skipped:`；設了 `CI` 環境變數時改為失敗，CI 不會默默略過。
- 加 `-- --nocapture` 會印出測試報告用的數據，例如容器停止花的時間與 exit code。
