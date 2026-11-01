# 入口限流的驗證（2026-11-01）

對應 issue：Laysi/trustiot-v2#113（API 閘道通用限流，SaaS 依 CF-Connecting-IP）。

同一天做了兩輪：第一輪是最初的實作；第二輪是安全審查之後的修正（IPv6 以 /64 計數、values 祕密掃描的條件、版本一致性檢查、`%XX` 編碼的案例），結果以第二輪為準。

## 環境

- 分支基於 main 的 `0627c59a`；變更是 `deploy/chart`（values、schema、`templates/_ingress.tpl`、`templates/ingress.yaml`、測試）、`scripts/nightly/platform.test.ts` 與文件。
- chart 測試、`scripts` 的測試與型別檢查：建置主機（x86_64 Linux 與 arm64 Linux VM），Bun 1.4.2、helm 4.3.0、kubeconform 0.8.0（`.prototools` 的版本）。
- Envoy 測試：第一輪在建置主機（x86_64 Linux，podman 5.7.0），第二輪在建置主機（arm64 Linux VM，podman）。映像 `docker.io/istio/proxyv2:1.30.5`，以多架構 index 的 digest `sha256:a30fb25b1e4cfc0fbc50a8a6795f5984a49022dd0ec09bcd8683b3db35af5ed3` 固定（Envoy 1.38.5-dev，與叢集的 Istio 1.30.5 相同）。filter 由 chart 以 `values-saas.yaml`、`tests/values/saas-instance.yaml` 加 `tests/values/rate-limit-envoy.yaml`（較小的限制，每小時補充，計數不受送出速度影響）渲染。Envoy 裡依 chart 的順序放 Lua filter（寫出要計數的用戶端）與 local rate limit filter，後面是兩條直接回應的路由（`/auth/login` 回 `login`，其餘回 `routed`），路徑正規化設定與 mesh 相同。
- 叢集：沒有執行。`deploy/chart/tests/e2e/ingress.e2e.ts` 的「rate limit」案例要在有 Istio ambient 的叢集上跑（nightly 每晚以兩種型態執行，PR 加 `smoke` label 時也會跑）。

## 指令

```sh
# chart 測試（含「ingress rate limit (Laysi/trustiot-v2#113)」區塊）
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

`original_ip_detection_extensions` 能不能用的實測：把 custom header 擴充（`header_name: cf-connecting-ip`）與 `use_remote_address: true` 放進同一個 HTTP connection manager，以同一個映像的 `envoy --mode validate` 驗證。

## 結果

第二輪：

| 檢查 | 結果 |
| --- | --- |
| `bun test deploy/chart` | 796 pass、0 fail |
| `bun test ./deploy/chart/tests/envoy-rate-limit.e2e.ts` | 11 pass、0 fail |
| `bun test deploy/flux` | 78 pass、0 fail |
| `bun test scripts/nightly scripts/ci` | 1049 pass、0 fail |
| `bun run --cwd scripts typecheck` | 沒有錯誤 |
| `check-workflows.ts`、`check-secret-handling.ts` | 沒有違規 |

`original_ip_detection_extensions` 與 `use_remote_address: true` 一起時，Envoy 拒絕設定：`Original IP detection extensions and use_remote_address may not be mixed`；`use_remote_address: false` 時通過。Istio 在入口 gateway 設 `use_remote_address: true`，所以改用 Lua filter（理由見 `deploy/chart/README.md`「入口的限流」）。

Envoy 測試的 11 個案例：

1. 超過規則的限制回 429、本文 `local_rate_limited`；每次換 `X-Forwarded-For`、`True-Client-IP`、`X-Real-IP` 都不會重設額度。
2. 每個 `CF-Connecting-IP` 各自計數，IPv6 也是。
3. IPv6 以 /64 計數：`2001:db8:1:2::a`、`2001:db8:1:2::b`、大寫、補零與結尾 IPv4 的寫法共用登入的額度，第 6 個請求（含同一個 /64 的另一個位址）回 429，每次換 `X-Forwarded-For` 也一樣；`2001:db8:1:3::a` 與 `2001:db8:1:1::a` 不受影響。
4. 沒有 `CF-Connecting-IP` 時，不論帶什麼標頭，都是同一個用戶端 `-`。
5. 入口會路由到同一路徑的寫法都算進規則：不同方法、查詢字串、`Host` 的大小寫、埠號與結尾的 `.`、`//auth//login`、`/auth/./login`、`/auth/x/../login`。
6. `%XX` 編碼：`/auth/log%69n` 被解碼成 `/auth/login`，路由回 `login`，也算進登入的規則（5 次之後 `/auth/login` 回 429）。
7. 規則旁邊的路徑與主機不算進規則。
8. `PathPrefix` 規則算進前綴本身與底下的路徑，以路徑段為界。
9. 另一個主機的規則分開計數（換發與登入互不影響）。
10. `perClient` 合計同一個用戶端在所有主機與路徑的請求。
11. 超過 `maxClients` 時最久沒出現的用戶端被忘掉，重新拿到完整的額度。

突變檢查（在建置主機上的副本改樣板後跑 Envoy 測試，確認測試抓得到；工作目錄不變）：

| 突變 | 結果 |
| --- | --- |
| 用戶端改讀 `x-forwarded-for`（第一輪） | 案例 1 失敗 |
| 沒有標頭時的動作 `invert_match` 改成 `false`（第一輪） | 案例 4 失敗 |
| `Exact` 的路徑比對不允許查詢字串（第一輪） | 案例 5 失敗 |
| Lua 不把 IPv6 算成 /64，原樣使用位址（第二輪） | 案例 3 失敗（0 pass、1 fail） |

第一輪（修正前）：`bun test deploy/chart` 795 pass、Envoy 測試 9 pass、`bun test deploy/flux` 78 pass、`bun test scripts/nightly scripts/ci` 1048 pass，都是 0 fail。

## 安全審查的處理

| 項目 | 處理 |
| --- | --- |
| M1：IPv6 在自己的 /64 換位址就拿到新額度 | 已修：Lua filter 把 IPv6 算成 /64，IPv4 不變；Envoy 測試案例 3 與突變檢查 |
| M2：同一帳號從很多位址送請求 | 移到 Laysi/trustiot-v2#950（身分服務每個 session 的並行上限），README 指向它 |
| L1：`CF-Connecting-IP` 沒送到源站時所有請求共用 `-` | runbook：`docs/runbooks/demo-environment.md`「入口的限流與 CF-Connecting-IP」、`deploy/flux/README.md`「入口的限流」（Managed Transform 要關閉、部署後的確認方式、緊急關閉）；監看在 Laysi/trustiot-v2#955 |
| L2：同一個 NAT 後面的使用者共用額度 | 移到 Laysi/trustiot-v2#954，README 指向它 |
| L3：EnvoyFilter 沒生效時不會被察覺、測試映像與叢集版本脫鉤 | `scripts/nightly/platform.test.ts` 檢查三處 Istio 版本一致；README 寫明以 `pilot_total_xds_rejects` 與 `listener_manager.lds.update_rejected` 監看（Laysi/trustiot-v2#955）；叢集上的證據待 smoke |
| L4：values 祕密掃描的放寬只靠 schema | 只有符合 `^(/[a-z0-9][a-z0-9._-]*)+$` 的值逐段檢查，已知形狀仍對整段比對；加了 URL 帶帳密與大寫 base64 的反例測試 |
| `%XX` 編碼的行為 | Envoy 測試案例 6 |

## 結論

- 驗收條件 1「超過限流回 429（測試）」：通過。Envoy 測試以 chart 實際渲染的 filter 在 Istio 1.30.5 的 Envoy 確認回 429；chart 測試確認預設規則與比對。叢集上的確認（Istio 以 `targetRefs` 把 EnvoyFilter 掛到 Gateway，且排在 cookie 檢查之前）：待測，原因是這次沒有叢集，由 `ingress.e2e.ts` 的「rate limit」案例在 nightly 或 PR 的 smoke 執行。
- 驗收條件 2「用戶端自帶的 X-Forwarded-For 不能繞過 SaaS 的限流鍵（測試）」：通過。Envoy 測試確認換 `X-Forwarded-For` 等標頭不會重設額度，IPv6 在同一個 /64 換位址也不會；chart 測試確認限流只看 Lua 從 `CF-Connecting-IP` 寫出的內部標頭（用戶端自帶的同名標頭會被移除），`direct` 與地端拒絕使用它（有沒有 schema 驗證都一樣）。叢集上的同一項確認：待測，同上。
