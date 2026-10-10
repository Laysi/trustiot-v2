# 日誌管線：OpenTelemetry collector 寫入 ClickHouse 與轉送 SIEM（2026-10-11）

對應 issue：Laysi/trustiot-v2#73（決策 22）。原始資料在 [data/2026-10-11-73-otel-collector/](data/2026-10-11-73-otel-collector/)。

## 環境

- **受測 commit**：分支 `feat/73-otel-collector` 的 `f092ee6`（`4ce2fd1` 合併 main `404817d`），加上 `otel.e2e.ts` 的前置檢查修正（Helm 4 沒有 `helm list --all`，改用與 `cluster.ts` 相同的 `helm status` 檢查；修正隨本報告一起提交）。
- **本機**：x86_64 Fedora，podman 6.1.3；bun 1.4.2、helm 4.3.0、kubectl 1.36.5、Rust 工具鏈，皆由 `.prototools` 鎖定。
- **lab**：`trustiot-mac-m1`（Fedora Asahi，aarch64），k3s v1.36.5+k3s1，Istio 1.30.5 ambient。issue 的驗收條件寫「k3d 測試」，依 2026-10-10 使用者決定改在 lab 的 k3s 上執行（Kubernetes 只在 lab 執行）。
- **映像**：
  - collector：`docker.io/otel/opentelemetry-collector-contrib:0.161.0@sha256:fd328de2552466ad78385e1b1289c3f2402b1c45f265b252aab1955b42845ac1`（multi-arch index，lab 拉 arm64），ClickHouse 用 `values.yaml` 固定的 digest。
  - example-service：本機以 `aarch64-unknown-linux-musl`、`rust-lld` 交叉編譯 `crates/service-kit` 的 `example-service`（靜態執行檔），在 lab 以 `deploy/chart/tests/e2e/example-service/Containerfile` 建成 `localhost/trustiot-e2e/example-service:73`（image ID `82e9da24c66d`），匯入 k3s 的 containerd。
- **namespace**：`e2e-73`（Pod Security `restricted`）。執行身分是該 namespace 的 ServiceAccount `runner`，kubeconfig 只放在本機暫存目錄，API server 經 `ssh -L` 連線，測完刪除。
- **時間**：台北時間 2026-10-11 03:39–03:51（UTC 2026-10-10 19:39–19:51）。lab 鎖 UTC 19:39:57 取得、19:51 後釋放；測完刪除 namespace `e2e-73`、lab 上的建置目錄與這次匯入的映像。

## 指令

本機的 ClickHouse 測試（含新增的 `log_tables_for_the_collector`）與 chart 測試：

```sh
CARGO_BUILD_JOBS=2 cargo test -p ingest
bun test deploy/chart
```

本機 podman 的遮蔽驗證：ClickHouse 與 collector 以 `values.yaml` 的 digest 啟動（collector 加 `--read-only --cap-drop ALL --security-opt no-new-privileges`），collector 設定取自 `helm template`（地端型態、開啟轉送），套用 migration 0001–0005 兩次並以 0005 的兩句建立 collector 帳號，再以 OTLP/HTTP JSON 送出含個資與憑證的紀錄。過程與查詢結果見 [local-redaction.txt](data/2026-10-11-73-otel-collector/local-redaction.txt)。

lab 的叢集 e2e（example-service 映像的建置步驟見 `deploy/chart/tests/e2e/README.md`）：

```sh
KUBECONFIG=<暫存> \
E2E_NAMESPACE=e2e-73 \
E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:73 \
E2E_IMAGE_PULL_POLICY=Never \
E2E_REPORT_DIR=docs/test-reports/data/2026-10-11-73-otel-collector \
bun test --timeout 900000 --reporter=junit \
  --reporter-outfile=docs/test-reports/data/2026-10-11-73-otel-collector/otel-e2e.junit.xml \
  ./deploy/chart/tests/e2e/otel.e2e.ts
```

## 結果

叢集 e2e：5 個測試全部通過，39 個斷言，72.98 秒（[otel-e2e.junit.xml](data/2026-10-11-73-otel-collector/otel-e2e.junit.xml)）。第一次執行在前置檢查就停止（`helm list --all` 在 Helm 4 不存在，沒有安裝任何東西），修正後第二次執行通過。查到的紀錄、TTL 與 SIEM 摘錄在 [otel-e2e.json](data/2026-10-11-73-otel-collector/otel-e2e.json)。

| 測試 | 結果 | 摘要 |
| --- | --- | --- |
| 請求經 example-service 後 ClickHouse 有帶 trace id 的結構化紀錄 | 通過 | trace id `12316c0a1a9d9db527c18c98cc231c7a` 的 `otel_logs` 紀錄：ServiceName `example-service`、SeverityText `INFO`、method `POST`、route `/v1/echo`、status `200`；`otel_traces` 有 span `POST /v1/echo` |
| 兩張日誌表有保留期限 | 通過 | 從伺服器讀回 `MergeTree PARTITION BY toDate(ReceivedAt) ... TTL ReceivedAt + toIntervalDay(30) SETTINGS ... ttl_only_drop_parts = 1` |
| SIEM 收到同一筆紀錄 | 通過 | 替身 SIEM 輸出含同一個 trace id 的 log 與 span；不帶 Bearer 標頭直接送給它回 `401` |
| 含 email、token、request body 的紀錄已遮蔽 | 通過 | ClickHouse：Body `login failed for **** with Authorization: **** token **** **** phone ****`，`user.email`、`token`、`http.request.body` 都是 `****`；span 名稱 `GET /users/****`、status message `failed for ****`、event `bad ****`；SIEM 收到的是同樣遮蔽後的版本，所有原始值都找不到 |
| collector 符合 restricted Pod Security | 通過 | `runAsNonRoot: true`、`seccompProfile: RuntimeDefault`，在 `restricted` 的 namespace 裡建立成功 |

本機：

- `bun test deploy/chart`：203 個測試全部通過。
- `cargo test -p ingest`：`clickhouse_schema_and_tenant_isolation` 通過（38.62 秒），其中 `log_tables_for_the_collector` 驗證 TTL、分區、collector 帳號只能 INSERT 與 `DESC TABLE`，不能 SELECT、ALTER、DELETE。
- podman 遮蔽驗證：只取代符合的子字串，其餘內容保留（例如 `forwarded by **** key ****`、`****db:5432/x`）；`message.content`、`prompt`、`requestBody` 整個遮蔽；SIEM 的輸出 grep 每個原始值都是 0 筆。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 一個請求經範例服務後，ClickHouse 日誌資料庫出現帶 trace id 的結構化紀錄（k3d 測試，改在 lab k3s） | 通過 | `otel.e2e.ts` 第 1 個測試；`otel-e2e.json` 的 `request` |
| 日誌表有保留期限（測試讀取 TTL） | 通過 | `otel.e2e.ts` 第 2 個測試與 `cargo test -p ingest`；`otel-e2e.json` 的 `tables` |
| 設定轉送目的地後，測試用的 OTLP 接收端收到同一筆日誌 | 通過 | `otel.e2e.ts` 第 3 個測試；`otel-e2e.json` 的 `siem` |
| 送出含 email、token 與 request body 的測試日誌後，ClickHouse 日誌資料庫中的紀錄已遮蔽 | 通過 | `otel.e2e.ts` 第 4 個測試；`otel-e2e.json` 的 `redaction`；`local-redaction.txt` |

待測：collector 帳號在正式部署由 migration Job 建立（Laysi/trustiot-v2#60），本次 e2e 以測試內的 `clickhouse-client` 代替；metrics 目前收下後丟棄，儲存方式待 Laysi/trustiot-v2#183 決定。
