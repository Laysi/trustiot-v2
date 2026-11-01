# 入口限流的驗證（2026-11-01）

對應 issue：Laysi/trustiot-v2#113（API 閘道通用限流，SaaS 依 CF-Connecting-IP）。

## 環境

- 分支基於 main 的 `0627c59a`；變更是 `deploy/chart`（values、schema、`templates/_ingress.tpl`、`templates/ingress.yaml`、測試）與文件。
- chart 測試與 `scripts` 的型別檢查：建置主機（x86_64 Linux），Bun 1.4.2、helm 4.3.0、kubeconform 0.8.0（`.prototools` 的版本）。
- Envoy 測試：建置主機（x86_64 Linux），podman 5.7.0；映像 `docker.io/istio/proxyv2:1.30.5`，以多架構 index 的 digest `sha256:a30fb25b1e4cfc0fbc50a8a6795f5984a49022dd0ec09bcd8683b3db35af5ed3` 固定（Envoy 1.38.5-dev，與叢集的 Istio 1.30.5 相同）。filter 由 chart 以 `values-saas.yaml`、`tests/values/saas-instance.yaml` 加 `tests/values/rate-limit-envoy.yaml`（較小的限制，每小時補充，計數不受送出速度影響）渲染，放進一個只有這個 filter 與 router 的 Envoy，路徑正規化設定與 mesh 相同。
- 叢集：沒有執行。`deploy/chart/tests/e2e/ingress.e2e.ts` 的「rate limit」案例要在有 Istio ambient 的叢集上跑（nightly 每晚以兩種型態執行，PR 加 `smoke` label 時也會跑）。

## 指令

```sh
# chart 測試（含新的「ingress rate limit (Laysi/trustiot-v2#113)」區塊）
bun test deploy/chart

# 渲染出的 filter 在 Istio 的 Envoy 裡（需要 podman 或 docker）
bun test ./deploy/chart/tests/envoy-rate-limit.e2e.ts

# 其他受影響的檢查
bun test deploy/flux
bun test scripts/nightly scripts/ci
bun install --cwd scripts --frozen-lockfile && bun run --cwd scripts typecheck
bun scripts/ci/check-workflows.ts
bun scripts/ci/check-secret-handling.ts
```

## 結果

| 檢查 | 結果 |
| --- | --- |
| `bun test deploy/chart` | 795 pass、0 fail |
| `bun test ./deploy/chart/tests/envoy-rate-limit.e2e.ts` | 9 pass、0 fail（約 120 秒） |
| `bun test deploy/flux` | 78 pass、0 fail |
| `bun test scripts/nightly scripts/ci` | 1048 pass、0 fail |
| `bun run --cwd scripts typecheck` | 沒有錯誤 |
| `check-workflows.ts`、`check-secret-handling.ts` | 沒有違規 |

Envoy 測試的 9 個案例：

1. 超過規則的限制回 429、本文 `local_rate_limited`；每次換 `X-Forwarded-For`、`True-Client-IP`、`X-Real-IP` 都不會重設額度。
2. 每個 `CF-Connecting-IP` 各自計數，IPv6 也是。
3. 沒有 `CF-Connecting-IP` 時，不論帶什麼標頭，都是同一個用戶端 `-`。
4. 入口會路由到同一路徑的寫法都算進規則：不同方法、查詢字串、`Host` 的大小寫、埠號與結尾的 `.`、`//auth//login`、`/auth/./login`、`/auth/x/../login`。
5. 規則旁邊的路徑與主機不算進規則。
6. `PathPrefix` 規則算進前綴本身與底下的路徑，以路徑段為界。
7. 另一個主機的規則分開計數（換發與登入互不影響）。
8. `perClient` 合計同一個用戶端在所有主機與路徑的請求。
9. 超過 `maxClients` 時最久沒出現的用戶端被忘掉，重新拿到完整的額度。

突變檢查（在同一個建置主機，改樣板後跑 Envoy 測試，確認測試抓得到，再還原）：

| 突變 | 結果 |
| --- | --- |
| 用戶端改讀 `x-forwarded-for` | 案例 1 失敗 |
| 沒有標頭時的動作 `invert_match` 改成 `false`（沒有標頭的請求不再歸到 `-`） | 案例 3 失敗 |
| `Exact` 的路徑比對不允許查詢字串 | 案例 4 失敗 |

## 結論

- 驗收條件 1「超過限流回 429（測試）」：通過。Envoy 測試以 chart 實際渲染的 filter 在 Istio 1.30.5 的 Envoy 確認回 429；chart 測試確認預設規則與比對。叢集上的確認（Istio 以 `targetRefs` 把 EnvoyFilter 掛到 Gateway，且排在 cookie 檢查之前）：待測，原因是這次沒有叢集，由 `ingress.e2e.ts` 的「rate limit」案例在 nightly 或 PR 的 smoke 執行。
- 驗收條件 2「用戶端自帶的 X-Forwarded-For 不能繞過 SaaS 的限流鍵（測試）」：通過。Envoy 測試確認換 `X-Forwarded-For` 等標頭不會重設額度；chart 測試確認 filter 只讀 `cf-connecting-ip`，`direct` 與地端拒絕使用它（有沒有 schema 驗證都一樣）。叢集上的同一項確認：待測，同上。
