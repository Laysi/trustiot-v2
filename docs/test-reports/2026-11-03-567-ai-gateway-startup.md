# 以 chart 渲染的設定直接啟動 ai-gateway：就緒與兩種拒絕

對應 Laysi/trustiot-v2#567（自 Laysi/trustiot-v2#536 延後的第二個來源佐證；#536 的報告見 [2026-10-23-536-ai-gateway-deploy.md](2026-10-23-536-ai-gateway-deploy.md)）。

## 環境

- 機器：建置主機（arm64 Linux）。不跑 Kubernetes：只渲染 chart，閘道以本機行程啟動。
- 程式碼：`main` 的 commit `0679ccbc`（本報告是這個 commit 之上唯一的變更，沒有改程式）。
- 工具（`.prototools` 釘選）：Helm 4.3.0、Rust 1.99.0、Bun 1.4.2；`ai-gateway` 以 debug 建置（`cargo build --locked -p ai-gateway`）。
- Postgres：`bun scripts/dev.ts up` 起的本機 Postgres（TLS、`verify-full`，migration Job 已套用，`ai_gateway_app` 有密碼）。閘道的 readiness 包含 migration 與 Postgres 的檢查（`services/ai-gateway/README.md`「資料庫」），所以沒有 Postgres 就不會就緒。
- 兩種形態的 chart 輸入：
  - SaaS：`values-saas.yaml` + `tests/values/saas-instance.yaml` + `--set services.ai-gateway.enabled=true`。AI 設定來自實例檔：Anthropic（經範例 Cloudflare AI Gateway 的網址）與 OpenAI 兩個供應商，角色 `main`、`worker`、`router`。
  - on-prem：`values-onprem.yaml` + `tests/values/onprem-instance.yaml` + 一份只開啟 ai-gateway 並設定兩個供應商與角色 `main`、`router` 的覆寫檔（on-prem 的實例檔刻意沒有 AI 供應商）。
- 從渲染結果取出的設定：Deployment `trustiot-ai-gateway` 的 container 環境變數，加上它 `envFrom` 的兩個 ConfigMap（`trustiot-platform` 與 `trustiot-ai-gateway`）。
- 與叢集不同的地方（只有這些）：
  - 來自 Secret 的變數（資料庫密碼、兩把供應商金鑰、Sentry DSN）：金鑰與 DSN 用佔位值；資料庫密碼用本機 Postgres 的密碼。
  - Postgres 的位置、資料庫名稱、sslmode 與 CA 檔：改成本機 Postgres 的，不改的話連不到叢集位址。
  - 三個監聽位址（API、健康、管理）：改成 `127.0.0.1` 的空閒埠，避免與同一台機器上其他工作的 8080 至 8082 衝突。
  - OTLP 位址維持 chart 的值，本機連不到 collector，閘道只記 WARN `OTLP export exhausted retries`，與就緒無關。

## 指令

渲染（在 `deploy/chart`；`$HELM` 是 `proto bin helm --reporter text` 印出的路徑）：

```sh
"$HELM" template trustiot . -n trustiot -f values-saas.yaml -f tests/values/saas-instance.yaml \
  --set services.ai-gateway.enabled=true > saas.yaml
"$HELM" template trustiot . -n trustiot -f values-onprem.yaml -f tests/values/onprem-instance.yaml \
  -f onprem-ai.yaml > onprem.yaml
```

從每份渲染結果取出名為 `trustiot-ai-gateway` 的 Deployment 與同名 ConfigMap，把 container 的 `env` 與 `envFrom` 展開成一組環境變數（`secretKeyRef` 的值換成佔位值，並套用上面「與叢集不同的地方」），一次啟動只用這組環境變數：

```sh
bun scripts/dev.ts up                    # 本機 Postgres，migration 已套用
cargo build --locked -p ai-gateway
<展開後的環境變數> target/debug/ai-gateway
curl -s -o /dev/null -w '%{http_code}\n' "http://127.0.0.1:<健康埠>/readyz"
bun scripts/dev.ts purge
```

每種形態跑三次：

1. 完整的渲染設定，期望 `/readyz` 回 200，SIGTERM 後正常結束。
2. 拿掉 `TRUSTIOT_AI_OPENAI_API_KEY`（角色 `router` 用 `openai/gpt-6-luna`，而供應商的金鑰變數沒有值），期望拒絕啟動。
3. 拿掉 `AI_GATEWAY__HTTP__REQUEST_TIMEOUT`（`requestTimeout` 在 ConfigMap 裡的對應設定；沒有它時 `http.request_timeout` 是 service-kit 預設的 30 秒，小於角色的回應上限），期望拒絕啟動。

## 結果

原始輸出：[data/2026-11-03-567-ai-gateway-startup/run.log](data/2026-11-03-567-ai-gateway-startup/run.log)（含兩種形態展開後的環境變數名稱，秘密值以 `<placeholder>` 取代，以及每次啟動的日誌）。腳本檢查了每次的輸出都不含資料庫密碼與佔位值。

| 形態 | 情況 | 結果 |
| --- | --- | --- |
| SaaS | 完整的渲染設定 | `GET /readyz` 200 `{"status":"ready"}`、`GET /healthz` 200；日誌 `model role ready` 三次（`main`、`router`、`worker`）與 `listening`；SIGTERM 後 exit 0 |
| SaaS | 沒有 OpenAI 金鑰 | exit 1；`startup failed`：`environment variable TRUSTIOT_AI_OPENAI_API_KEY (providers.openai.key_env) is not set, is empty or is not UTF-8` |
| SaaS | 沒有 `AI_GATEWAY__HTTP__REQUEST_TIMEOUT` | exit 1；`startup failed`：`roles.main: queue_wait plus the longer of timeouts.response and timeouts.first_token must fit in http.request_timeout less 2000 ms; raise AI_GATEWAY__HTTP__REQUEST_TIMEOUT or lower the role's limits` |
| on-prem | 完整的渲染設定 | `GET /readyz` 200 `{"status":"ready"}`、`GET /healthz` 200；`model role ready` 兩次（`main`、`router`）與 `listening`；SIGTERM 後 exit 0 |
| on-prem | 沒有 OpenAI 金鑰 | exit 1；訊息同 SaaS |
| on-prem | 沒有 `AI_GATEWAY__HTTP__REQUEST_TIMEOUT` | exit 1；訊息同 SaaS |

可見的行為：

- 兩種拒絕都在 `startup failed` 時結束，訊息只寫設定與變數名稱，沒有值；閘道在拒絕時不開監聽。
- 沒有 `requestTimeout` 對應設定時的訊息點名 `AI_GATEWAY__HTTP__REQUEST_TIMEOUT`，而且指出是哪個角色放不進時間預算。on-prem 的 `main` 角色沒有設 `queueWait`，仍然被拒：角色預設的 `response` 是 300 秒，30 秒的預設預算放不下。
- 沒有發現與 chart 或 `services/ai-gateway/README.md` 不符的行為。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 兩種形態以渲染出的環境變數啟動成功，就緒端點回 200 | 通過（SaaS 與 on-prem 都是 `/readyz` 200）。Postgres、監聽位址與秘密值的替換見「環境」 |
| 兩種應拒絕的情況，閘道以設定錯誤結束，訊息指出哪個設定 | 通過（兩種形態都 exit 1，訊息指出 `TRUSTIOT_AI_OPENAI_API_KEY` 與 `AI_GATEWAY__HTTP__REQUEST_TIMEOUT`） |
| 證據寫進 `docs/test-reports/` | 通過（本報告與 `data/`） |

限制：金鑰是佔位值，所以這份證據不涵蓋供應商接受金鑰（見 chart README「AI 閘道」的金鑰確認步驟）；管理埠沒有以 `0.0.0.0` 對外聽（兩種形態都沒有開啟身分服務，chart 不設 `AI_GATEWAY__ADMIN__LISTEN_ADDR`，這裡只把預設的 loopback 位址換成空閒埠）。
