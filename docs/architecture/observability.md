# 觀測：日誌、看板、錯誤追蹤

服務日誌集中存進 ClickHouse，在 Grafana 查詢與告警。錯誤追蹤照 [#163](https://github.com/Laysi/trustiot/issues/163)、[#166](https://github.com/Laysi/trustiot/issues/166)–[#169](https://github.com/Laysi/trustiot/issues/169) 用 Sentry：SaaS 送 sentry.io，地端預設關閉。

日誌：

1. 所有 Rust 服務與閘道器用 tracing 輸出結構化日誌，經 OpenTelemetry collector 存進 ClickHouse 的獨立資料庫，並設保留期限。
2. 閘道器：warn 以上隨 MQTT 持續上傳，有流量上限；完整日誌由後台下指令上傳指定時段；不寫 SD 卡。
3. 地端可以把 OTLP 另外轉送到客戶自己的 SIEM。
4. 日誌不得包含聊天內容、request body、憑證與個資（[#110](https://github.com/Laysi/trustiot/issues/110)）。
5. 管線（2026-10-11，Laysi/trustiot-v2#73）：Helm chart 部署 OpenTelemetry collector（contrib 版，以 digest 固定），每個服務以 `TRUSTIOT__TELEMETRY__OTLP_ENDPOINT` 把 OTLP 送給它；collector 遮蔽後把日誌與 trace 寫進 ClickHouse 的 `trustiot_logs.otel_logs`、`otel_traces`，保存 30 天，以伺服器寫入時間計算（`services/ingest/migrations/clickhouse/0006_logs.sql`）。collector 的 ClickHouse 帳號只能寫入這兩張表，由 migration Job 以 SQL 建立（決策 14）。地端開啟轉送時，同一份遮蔽後的日誌（可加 trace）以 OTLP/HTTP 或 gRPC 送到客戶的 SIEM。設定與欄位見 [chart 的 README](../../deploy/chart/README.md)「OpenTelemetry collector」。
6. 第 4 點在 collector 再擋一次：名稱像憑證、個資、request body 或聊天內容的屬性一律遮蔽（名單與 service-kit 相同），所有屬性值、日誌 body、span 名稱與 status message 裡的私鑰、URL 帳密、Bearer 與 JWT 等 token、API key、email、電話與身分證字號也遮蔽；遮蔽在寫進 ClickHouse 與轉送 SIEM 之前，適用於所有來源。這和 service-kit 的遮蔽一樣只是最後防線。
7. Rust 服務的日誌由 `crates/service-kit` 輸出：stdout 每行一個 JSON 物件，請求內的日誌帶 trace id；OTLP 匯出（日誌、trace、指標）預設關閉，設定 collector 位址後才開啟，collector 位址的路徑不論日誌等級都不進日誌（OpenTelemetry SDK 的診斷日誌只輸出 INFO 以上）；名稱像憑證、個資或內容的欄位，值一律遮蔽。遮蔽只是最後防線，第 4 點仍要靠程式本身不記錄這些值；格式與規則見[它的 README](../../crates/service-kit/README.md)。

看板與告警：

1. Grafana 部署在同一個 k3s，查詢日誌與指標並設定告警；主機掛了它也跟著掛，所以 SaaS 另設不跑在同一台主機的外部監控，從外部定時檢查網站、API 與 MQTT 轉接，主機、入口或 Cloudflare Tunnel 掛了會通知人，SaaS 上線前（MS4）完成。
2. 建議至少要有的告警：資料接收失敗與積壓、閘道器離線、設定套用失敗、AI 呼叫錯誤率與排隊、主機資源、ClickHouse 磁碟水位（規則與門檻見[資料保存期限與磁碟水位](data-retention.md#磁碟水位告警)）、憑證到期（certs 的 `certs.crl.expiring` 是 1 或沒有資料；根 CRL 的 `certs.root_crl.expiring` 或 `certs.root_crl.invalid` 是 1，見 [services/certs/README.md](../../services/certs/README.md#crl-與告警) 的「CRL 與告警」，Laysi/trustiot-v2#830）、服務間事件積壓（`crates/outbox` 的 `outbox.consumer.lag.age` 持續上升（lag 數量到 10 000 就不再增加，所以看 age）；叢集內長寫入交易與未收尾的 prepared transaction 會讓所有訂閱停住，要另有資料庫層的監控，見 [outbox README](../../crates/outbox/README.md#長寫入交易與-prepared-transaction)；以及某個訂閱的 lag 序列沒有資料：消費端停止量測時不回報舊值，見 [outbox README 的 Lag 指標](../../crates/outbox/README.md#lag-指標)）。
3. 服務的指標以 OpenTelemetry 儀表建立，經 service-kit 的 OTLP 匯出送到 collector；第一個是 2026-10-10 加入的 outbox lag（Laysi/trustiot-v2#54）。collector 目前收下指標後丟棄，存放位置隨 Grafana 決定（Laysi/trustiot-v2#183）。

錯誤追蹤（[#163](https://github.com/Laysi/trustiot/issues/163)、[#166](https://github.com/Laysi/trustiot/issues/166)–[#169](https://github.com/Laysi/trustiot/issues/169)）：

1. 前端 SPA 與 Rust 服務都接 Sentry SDK，DSN 由設定指定；地端預設關閉，錯誤仍進日誌並由 Grafana 告警，客戶有自己的 Sentry 就指過去。
2. PII 防線：不送預設 PII，beforeSend 白名單；聊天內容、request body、憑證一律不外送。
3. 事件帶 environment、release（git SHA）、租戶與對話、執行標記。
4. \[#166](https://github.com/Laysi/trustiot/issues/166) 尚未決定的子項沿用：sentry.io 資料區域（US 或 EU）、方案與用量上限、專案切分、保留期。
5. 閘道器不接 Sentry SDK（頻寬與離線限制），錯誤經日誌上傳。
