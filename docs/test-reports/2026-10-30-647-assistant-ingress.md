# assistant：聊天 API 經入口的路由與 nightly 的檢查（2026-10-30）

Laysi/trustiot-v2#647：nightly 的 smoke 在兩份 values 上加 `/api/assistant` 的 cookie 路由，並經入口呼叫 assistant 的對話串 API（Laysi/trustiot-v2#645）；文件補上展示叢集打開 assistant 的步驟。不提問：nightly 的 AI 閘道沒有真的金鑰。

## 環境

- **程式碼**：分支 `feat/647-assistant-ingress`，基於 main `80a732a`。只改 `scripts/nightly/`、三份文件與本報告，沒有改任何服務、chart 的樣板與預設值、flux 的叢集設定。
- **機器**：本機（x86_64 Linux 工作站）跑單元測試與靜態檢查；叢集上的檢查只在 CI（nightly workflow，k3d，兩份 values）跑，工作站不跑 Kubernetes。
- **工具**：`.prototools` 釘選的 Bun、helm、kubeconform、kubectl 與 flux。
- **設定**：單元測試以假的 `deps` 取代叢集（`scripts/nightly/api.test.ts` 的 `assistantFake`：對話屬於開始它的 cookie、刪除與還原、受眾不同的 token），沒有網路與容器。

## 指令

在 repo 根目錄：

```sh
bun install --cwd scripts --frozen-lockfile
bun test scripts/nightly/api.test.ts
bun test scripts
bun run --cwd scripts typecheck
bun test deploy/chart
bun test deploy/flux
bun scripts/ci/check-workflows.ts
bun test scripts/ci/changes.test.ts
gitleaks git --redact --log-opts="origin/main..HEAD" .
```

nightly 的證據：PR 帶 `smoke` label 後 CI 以兩份 values 部署並跑 `checkAssistantApi`（`scripts/nightly/api.ts`）。

## 結果

| 指令 | 結果 |
| --- | --- |
| `bun test scripts/nightly/api.test.ts` | 63 項通過，其中 `checkAssistantApi` 21 項 |
| `bun test scripts` | 1732 項通過、9 項略過（需要容器或叢集的測試）、0 項失敗 |
| `bun run --cwd scripts typecheck` | 無錯誤 |
| `bun test deploy/chart` | 711 項通過 |
| `bun test deploy/flux` | 78 項通過（以 HEAD 的 chart 渲染展示叢集的 values） |
| `bun scripts/ci/check-workflows.ts` | 無違規 |
| `bun test scripts/ci/changes.test.ts` | 283 項通過，不需要新的路徑規則（測試只讀 `services/assistant/src/api/conversations.rs`，與讀其他服務的原始碼相同；`scripts` 的測試在每次都執行的 workflow-policy job） |

`checkAssistantApi` 的單元測試涵蓋：

- 成功的順序與每個請求的形狀：管理者先等路由、不帶 cookie、成員 A 開始與列出、成員 B 讀與列、A 刪除、讀、已刪除清單、還原、讀，最後直接送到 assistant 的兩個 token；寫入帶 JSON 的 `Content-Type` 與 `{}`，經入口只帶 cookie、不帶身分標頭，直接送只帶身分標頭。
- 等待：路由前的連線中斷、404、503 與 Istio 的 403 都只是還沒好；沒有租戶的管理者拿到 2xx 立刻失敗；超過上限時訊息寫出等什麼與最後的回應。
- 十四種偏離（不帶 cookie 沒被擋、開始失敗或沒有 ID、列表缺少或不是 JSON、B 讀得到或被別的碼拒絕或列得到、刪除不是 204 或有本文、刪除後讀得到、已刪除清單缺少、還原被拒或回別的對話）：每種都失敗，訊息寫出步驟與狀態碼，不含 cookie 或 token。
- 直接送到 assistant：對照組（受眾是 assistant 的 token 被拒）失敗並說明下一項證明不了什麼；受眾是資產服務的 token 被接受或以別的狀態碼被拒，檢查都失敗。
- 路徑常數與 `services/assistant/src/api/conversations.rs` 的四個常數相同；`API_ROUTES` 有 `/api/assistant`，兩份 values 渲染出 `trustiot-assistant` 的 app 主機路由。

## nightly 的證據

PR Laysi/trustiot-v2#790 帶 `smoke` label，head `1dd10e27` 的 nightly run [41100242876](https://github.com/Laysi/trustiot-v2/actions/runs/41100242876)（2026-10-30）通過。兩份 values（`values-api-onprem.yaml`、`values-api-saas.yaml`）各自部署後，`checkAssistantApi` 的報告行相同：

- 入口在 0–1 s 內路由 `/api/assistant/v1/conversations`
- 租戶成員 `POST /api/assistant/v1/conversations`：201，列表看得到它
- 同租戶的另一個成員：404 `not_found`，列表看不到
- 刪除 204、讀取 404 `not_found`、已刪除清單看得到、還原 200、讀取 200
- 沒有 cookie：401 `unauthenticated`
- 沒有租戶的平台管理員：403 `permission_denied`
- 經 port-forward 直接打 assistant：成員給 assistant 的 token 200，給 assets 的 token 401 `unauthenticated`

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 帶 `smoke` label 的 PR 的 nightly 在兩份 values 上經入口完成六項檢查 | 通過：run 41100242876，兩份 values 六項都通過（上一節） |
| `bun test scripts`、`bun run --cwd scripts typecheck`、`bun test deploy/chart`、`bun test deploy/flux` 通過 | 通過（上表） |
| `deploy/flux/README.md` 的「打開 assistant」與 `deploy/chart/README.md` 的路由範例更新；`bun test deploy/flux` 以 HEAD 的 chart 渲染展示叢集的 values 仍通過 | 通過：兩份文件已更新，`bun test deploy/flux` 78 項通過（沒有改 `values-demo.yaml`，打開展示環境的服務不在本 issue） |
