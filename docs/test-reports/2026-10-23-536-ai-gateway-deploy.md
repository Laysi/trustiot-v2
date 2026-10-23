# ai-gateway 部署：映像、供應商金鑰、模型角色與呼叫端授權

對應 Laysi/trustiot-v2#536（PR Laysi/trustiot-v2#562）。

## 環境

- 程式碼：分支 `feat/536-ai-gateway-deploy`，commit `a5eae2a9`（本報告之後的 commit 只加這份報告與 `data/`）。
- chart 測試與 nightly 部署：GitHub Actions，在 PR 的 CI 執行（[PR 的 checks](https://github.com/Laysi/trustiot-v2/pull/562/checks)）。
  - `ci.yml` 的 `helm` job：[run 39905073998](https://github.com/Laysi/trustiot-v2/actions/runs/39905073998/job/120255123899)。
  - `nightly.yml` 的 `smoke` job（PR 帶 `smoke` label）：[run 39905074266](https://github.com/Laysi/trustiot-v2/actions/runs/39905074266/job/120255060790)，k3d 叢集，ai-gateway 映像以 `cargo auditable zigbuild` 建成 `aarch64-unknown-linux-musl` 的 arm64 映像。
- scripts 測試：建置主機（x86_64 Linux），Bun 1.4.2，`.prototools` 釘選的工具。
- 工作站：`bun test deploy/flux` 與 `scripts/ci/*` 的檢查。
- nightly 的 ai-gateway 設定（`scripts/nightly/api.ts` 的 `AI_GATEWAY_SMOKE`）：Anthropic 供應商、Secret `trustiot-anthropic`（nightly 自動建立的假值，不是真的金鑰）、角色 `smoke` 用 `anthropic/claude-smoke`。

## 指令

```sh
# chart（CI 的 helm job 執行同一指令）
bun test deploy/chart

# Bun scripts：安裝、型別檢查、測試
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts

# GitOps 與 CI 規則（工作站）
bun test deploy/flux
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/check-workflows.ts
bun scripts/ci/lint-workflows.ts

# nightly：PR 加上 smoke label 後由 .github/workflows/nightly.yml 執行
```

## 結果

| 檢查 | 結果 |
| --- | --- |
| `bun test deploy/chart`（CI helm job） | 609 pass、0 fail；兩種形態都通過 Kubernetes 1.30.0 與 1.36.0 的 `kubeconform -strict`（[ci-chart-tests.txt](data/2026-10-23-536-ai-gateway-deploy/ci-chart-tests.txt)，只取與本 issue 相關的測試列與總數） |
| `bun install` + `bun run --cwd scripts typecheck` | 型別檢查沒有錯誤 |
| `bun test scripts` | 1599 個測試，1590 pass、0 fail，其餘 9 個沒有執行（skip 或 todo，本次沒有逐一列出） |
| `bun test deploy/flux` | 75 pass、0 fail（CI helm job 也是 75 pass） |
| `check-secret-handling`、`check-workflows`、`lint-workflows` | 沒有違規 |
| nightly smoke（CI） | 建出 ai-gateway 映像（5776808 bytes），`nightly-onprem` 與 `nightly-saas` 兩個 namespace 的 `deployment/trustiot-ai-gateway` 都 `successfully rolled out`，smoke job 成功（[nightly-smoke.txt](data/2026-10-23-536-ai-gateway-deploy/nightly-smoke.txt)） |

chart 測試涵蓋的內容（見 `deploy/chart/tests/chart.test.ts` 的「AI gateway settings」與「only the assistant, the knowledge base and alerts may call ai-gateway」）：

- 金鑰只以 `secretKeyRef` 進入 ai-gateway（`TRUSTIOT_AI_ANTHROPIC_API_KEY`、`TRUSTIOT_AI_OPENAI_API_KEY`），其他服務沒有 `TRUSTIOT_AI_*`，ConfigMap 與 platform ConfigMap 都沒有金鑰。
- 渲染出的 ConfigMap 內容與值逐項相符，設定改變時 checksum 讓 pod 滾動。
- 閘道啟動時會拒絕的設定（未知供應商或設定、模型的供應商沒有金鑰 Secret、上限與時間超出範圍、時間預算超出 `requestTimeout`，算到毫秒）在有沒有 schema 驗證時都讓渲染失敗。
- AuthorizationPolicy 與 NetworkPolicy 只允許列在 `mesh.calls` 的 alerts、assistant、knowledge；其他服務列 ai-gateway 時渲染失敗；任何 host 的 ingress 路由都不能通往 ai-gateway。

就緒不需要有效的金鑰：nightly 的金鑰是假值，pod 仍然就緒（啟動時只檢查金鑰的格式，見 `services/ai-gateway/README.md`「金鑰與 TLS」）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 映像 `deploy/images/ai-gateway`（arm64）建得出來；`bun test scripts` 通過；nightly 帶 smoke label 時建出並部署 | 通過 |
| 金鑰只到 ai-gateway，ConfigMap 與 env 字面值沒有金鑰，只以 `secretKeyRef` 注入各供應商的金鑰（`bun test deploy/chart`） | 通過 |
| ai-gateway 的 AuthorizationPolicy 只允許 assistant、knowledge、alerts，其他服務列它就渲染失敗，與呼叫矩陣一致 | 通過 |
| nightly smoke 兩份 values 都打開 ai-gateway，pod 就緒 | 通過 |
| `deploy/flux/README.md` 說明展示叢集的 values，`bun test deploy/flux` 通過 | 通過 |

另外準備了一項檢查：在本機以 chart 渲染的環境變數直接啟動 `ai-gateway` 二進位，涵蓋 SaaS、on-prem、缺 OpenAI 金鑰應拒絕、缺 `requestTimeout` 應拒絕四種情況。這項檢查待測：2026-10-23 建置主機的檔案系統兩度變成唯讀，工作沒有跑完。可以啟動的兩種情況，已由 nightly 在叢集裡以同一份設定啟動並就緒佐證；應拒絕的兩種情況，chart 在渲染時就擋下，測試已涵蓋。這項檢查是第二個來源，不是驗收條件。
