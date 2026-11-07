# 展示環境的 prompt caching：真模型多輪對話的快取命中

Laysi/trustiot-v2#554

assistant 的 prompt caching（[AI 助理](../architecture/ai-assistant.md)「Prompt caching」）在 Laysi/trustiot-v2#145 只以錄下的回應測過。本報告以展示環境裡一段真模型的多輪對話，確認第二次模型呼叫起都讀到快取（`cache_read_input_tokens` 大於 0）。

這次沒有另外呼叫模型：assistant 每次模型呼叫都在日誌 `model call completed` 記下用量（「Prompt caching」第 4 點），日誌經 OpenTelemetry collector 存進展示環境的 ClickHouse，所以以唯讀查詢讀出 2026-11-06 那段對話的每次呼叫。

## 環境

- **環境**：展示環境（Laysi/trustiot-v2#457），展示機是 lab 的 Mac mini M1（Fedora Asahi Remix、k3s、aarch64），以 Flux 部署。
- **版本**：HelmRelease `trustiot` 的 chart 是 `0.0.0-rc.15`（main `74b26f3e`），assistant 映像 `0.0.0-rc.15`。查詢時最新一筆部署是 2026-11-06T05:48:37Z，`deployed`（打開回答工具的 values 變更，Laysi/trustiot-v2#1121）。
- **模型**：assistant 呼叫 AI 閘道的 `main` 角色，五次呼叫都由 `anthropic/claude-opus-5-5` 回答（沒有換到備援模型）。
- **對話**：2026-11-06 05:50 UTC 展示環境的一段對話，一個租戶、兩輪提問。第 1 輪呼叫模型 3 次、第 2 輪 2 次（每輪的工具呼叫各算一步）。兩輪間隔約 2 秒，在預設的 5 分鐘 TTL 內。
- **日誌**：`trustiot_logs.otel_logs`，`ServiceName = 'assistant'`、`Body = 'model call completed'`；用量在 `LogAttributes` 的 `usage.input`、`usage.cache_write`、`usage.cache_read`、`usage.output`，分別是 Anthropic 回報的 `input_tokens`、`cache_creation_input_tokens`、`cache_read_input_tokens` 與 `output_tokens`（`services/assistant/src/agent.rs`）。日誌不含問題與回答的內容。

## 指令

唯讀查詢，在工作站執行。展示機的 ssh 主機從環境變數讀，不寫進紀錄；ClickHouse 的帳號與密碼在 pod 裡讀，不經過工作站。

```sh
: "${DEMO_SSH_HOST:?set DEMO_SSH_HOST to the ssh host of the demo machine}"
ssh "$DEMO_SSH_HOST" 'bash -s' <<'EOF'
set -e
Q="SELECT toString(Timestamp) AS t, LogAttributes['conversation_id'] AS conv, LogAttributes['run_id'] AS run, LogAttributes['step'] AS step, LogAttributes['usage.input'] AS inp, LogAttributes['usage.cache_write'] AS cw, LogAttributes['usage.cache_read'] AS cr, LogAttributes['usage.output'] AS outp, LogAttributes['model'] AS model FROM trustiot_logs.otel_logs WHERE ServiceName = 'assistant' AND Body = 'model call completed' ORDER BY Timestamp FORMAT TSV"
sudo k3s kubectl -n trustiot exec -i trustiot-clickhouse-0 -c clickhouse -- sh -c 'clickhouse-client --user "$CLICKHOUSE_USER" --password "$(cat "$CLICKHOUSE_PASSWORD_FILE")" --query "$0"' "$Q"
EOF
```

2026-11-07 查詢時，表裡只有這段對話的 5 筆，assistant pod 自己的日誌（`kubectl logs`）也是同樣的 5 筆。

## 結果

原始資料：[model-calls.tsv](data/2026-11-07-554-prompt-caching-demo/model-calls.tsv)。對話、run 與呼叫的 UUID 換成對話 A、第幾輪與第幾步，其餘欄位照查詢的輸出。

| 呼叫 | 輪／步 | `input_tokens` | 快取寫入 | 快取讀取 | 輸出 | 讀自快取的比例 |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 1／1 | 4 | 13,615 | 0 | 251 | 0% |
| 2 | 1／2 | 2 | 716 | 13,615 | 270 | 95.0% |
| 3 | 1／3 | 2 | 1,005 | 14,331 | 764 | 93.4% |
| 4 | 2／1 | 4 | 617 | 15,336 | 175 | 96.1% |
| 5 | 2／2 | 2 | 810 | 15,953 | 282 | 95.2% |

「讀自快取的比例」是快取讀取除以這次呼叫的輸入總量（`input_tokens` 加快取寫入加快取讀取）。

- 第 1 次呼叫寫入 13,615 token：工具定義、system prompt、租戶脈絡與第一個問題。快取讀取是 0，因為之前沒有相同前綴的呼叫（表裡沒有更早的紀錄）。
- 從第 2 次起，每次呼叫讀到的正好是前一次讀到的加上前一次寫入的：13,615；13,615 + 716 = 14,331；14,331 + 1,005 = 15,336；15,336 + 617 = 15,953。也就是前一次呼叫送出的整段前綴都從快取讀，新寫入的只有那一步新增的工具結果與回應。
- 第 4 次呼叫是第 2 輪的第一次呼叫，讀到 15,336 token，等於第 1 輪結束時的整段前綴：對話的自動快取跨輪也命中，不只在同一輪的工具迴圈裡。
- 不含快取的輸入（`input_tokens`）每次只有 2–4 token。

## 結論

- 驗收條件「多輪對話第二次呼叫起 `cache_read_input_tokens` 大於 0」：**通過**。第 2 到第 5 次呼叫的快取讀取都大於 0，跨輪的第一次呼叫也是。
- 範圍寫「至少 3 輪的對話」：這段對話是 **2 輪**（5 次模型呼叫）。第 3 輪走的是同一個機制（自動快取隨對話往後延伸），已由第 2 輪證明，所以沒有為了第 3 輪另外花費呼叫。展示驗收 Laysi/trustiot-v2#459 的預演會有至少 3 輪的對話，預演後用上面同一個查詢補記，唯讀、不另外花費。
- 不在本 issue 範圍、沒有測的：跨租戶共用 system prompt 的快取（展示環境目前只有一個租戶）、超過 5 分鐘 TTL 之後的回覆，以及重複量測與成本分析（範圍的「不包含」）。
