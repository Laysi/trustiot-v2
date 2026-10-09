# 觀測：日誌、看板、錯誤追蹤

服務日誌集中存進 ClickHouse，在 Grafana 查詢與告警。錯誤追蹤照 [#163](https://github.com/Laysi/trustiot/issues/163)、[#166](https://github.com/Laysi/trustiot/issues/166)–[#169](https://github.com/Laysi/trustiot/issues/169) 用 Sentry：SaaS 送 sentry.io，地端預設關閉。

日誌：

1. 所有 Rust 服務與閘道器用 tracing 輸出結構化日誌，經 OpenTelemetry collector 存進 ClickHouse 的獨立資料庫，並設保留期限。
2. 閘道器：warn 以上隨 MQTT 持續上傳，有流量上限；完整日誌由後台下指令上傳指定時段；不寫 SD 卡。
3. 地端可以把 OTLP 另外轉送到客戶自己的 SIEM。
4. 日誌不得包含聊天內容、request body、憑證與個資（[#110](https://github.com/Laysi/trustiot/issues/110)）。

看板與告警：

1. Grafana 部署在同一個 k3s，查詢日誌與指標並設定告警；主機掛了它也跟著掛，所以 SaaS 另設不跑在同一台主機的外部監控，從外部定時檢查網站、API 與 MQTT 轉接，主機、入口或 Cloudflare Tunnel 掛了會通知人，SaaS 上線前（MS4）完成。
2. 建議至少要有的告警：資料接收失敗與積壓、閘道器離線、設定套用失敗、AI 呼叫錯誤率與排隊、主機資源、憑證到期。

錯誤追蹤（[#163](https://github.com/Laysi/trustiot/issues/163)、[#166](https://github.com/Laysi/trustiot/issues/166)–[#169](https://github.com/Laysi/trustiot/issues/169)）：

1. 前端 SPA 與 Rust 服務都接 Sentry SDK，DSN 由設定指定；地端預設關閉，錯誤仍進日誌並由 Grafana 告警，客戶有自己的 Sentry 就指過去。
2. PII 防線：不送預設 PII，beforeSend 白名單；聊天內容、request body、憑證一律不外送。
3. 事件帶 environment、release（git SHA）、租戶與對話、執行標記。
4. \[#166](https://github.com/Laysi/trustiot/issues/166) 尚未決定的子項沿用：sentry.io 資料區域（US 或 EU）、方案與用量上限、專案切分、保留期。
5. 閘道器不接 Sentry SDK（頻寬與離線限制），錯誤經日誌上傳。
