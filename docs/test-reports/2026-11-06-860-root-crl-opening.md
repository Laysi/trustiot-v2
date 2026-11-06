# 展示環境開放 gateway 主機名稱的 `/certs/root-crl`

Laysi/trustiot-v2#860

2026-11-06（UTC+8）讓展示環境 gateway 主機名稱的 `GET /certs/root-crl` 略過 Cloudflare Access，供閘道器取根 CRL（Laysi/trustiot-v2#831、Laysi/trustiot-v2#563）。開放的順序是安全關卡，順序沒有變：

1. 先讓 Cloudflare 的速率限制規則涵蓋這個路徑，讀回確認。
2. 實測規則確實會回 429。
3. 之後才加 Bypass application 與 tunnel 的錨定規則。

做法在 [展示環境 runbook](../runbooks/demo-environment.md)「架構」的「只有閘道器的固定路徑略過 Access」與「`/mqtt` 的速率限制」。

當天做了兩次：

- 第一次照原本的判定（第 41 次起要拿到 429）沒有通過，全部撤回。
- 使用者改了判定（理由在「結果」），第二次照新的判定通過並開放。

## 環境

- **送請求的機器**：
  - 工作站：所有的 burst、`between`、`after`，以及 `check --stage 2` 的第一組。
  - 展示機（lab 的 Mac mini M1，Fedora Asahi Remix、k3s、aarch64）：`check --stage 2` 的第二組。
- **Cloudflare**：zone 是 Free 方案。由協調者經 API 套用一份不進 repo 的變更清單，套用前經使用者同意。清單裡有主機名稱，所以不進 repo。
- **分支與 commit**：
  - `scripts/demo/tunnel.ts` 用分支 `feat/860-root-crl-bypass` 的 commit `df823c9d`。
  - 展示叢集的 values 用 main 的 commit `a87e08d6`：`deploy/flux/clusters/demo/trustiot/` 的 `values-saas.yaml` 加 `values-demo.yaml`，兩份都沒有 `certificateAuthority.rootCrl`。
- **入口**：入口的固定 ClusterIP 是 `10.43.0.80`。`route-origin` 的關卡通過：values、ConfigMap 與 Service 三處一致，而且在靜態區段內。
- **速率限制規則**：Free 方案唯一的一條。
  - 運算式：`(http.request.uri.path in {"/mqtt" "/certs/renew" "/certs/root-crl"})`。
  - 每個 IP 每 10 秒 40 次，封鎖 10 秒（`characteristics` 是 `["cf.colo.id", "ip.src"]`）。
- **Browser Integrity Check**：關閉它的 configuration rule 原本只涵蓋 gateway 主機名稱的 `/mqtt` 與 `/certs/renew`，這次加上 `/certs/root-crl`。
- **Bypass application**：與 `/certs/renew` 用同一個 reusable bypass policy。
- **tunnel 規則**：`^/certs/root-crl$` 排在 `^/certs/renew$` 之後，service 與它相同，不設 `originRequest.access`。寫入後讀回，與預期相同。

## 指令

主機名稱一律從環境變數讀，不寫進指令與輸出。

`checks.ts` 與變更清單放在 repo 外：

- 它只送 GET 並遮蔽輸出，內容不含主機名稱。
- 判定規則全文寫在下面「結果」，照著做就能重跑。

```sh
export DEMO_APP_HOST=… DEMO_GATEWAY_HOST=… DEMO_OTA_HOST=…   # 三個主機名稱，不進紀錄
bun <私有目錄>/checks.ts burst      # 1.3：規則有效（Bypass 之前）
bun <私有目錄>/checks.ts between    # 2.1：加了 Bypass application、還沒加 tunnel 規則
bun <私有目錄>/checks.ts after      # 3：加了 tunnel 規則之後
bun scripts/demo/tunnel.ts check --stage 2 --ssh-host "$DEMO_SSH_HOST"   # 從工作站與展示機各一組
```

兩次 burst 之間至少等 60 秒，讓計數與封鎖都過期。

## 結果

原始輸出已經遮蔽主機名稱、Access 的 team 網域，並去掉 `cf-ray`，放在 [data 目錄](data/2026-11-06-860-root-crl-opening/)。

### 第一次：原本的判定沒有通過，全部撤回

原本的判定是：在 10 秒內連續送 41 次，第 41 次起要拿到 Cloudflare 的 429。

- 規則改成三個路徑並讀回之後，送了 4 輪 45 次的 burst，一個 429 都沒有拿到：
  - 有的在 Access 後面送；
  - 有的照變更清單的備案在 Access 之後送（加了 Bypass application，connector 回 403）。
- 診斷用的一輪 150 次：
  - 在 Access 之後送；
  - 第一個 429 在第 63 次；
  - 第 109 次起才全部是 429。
- 判定沒有通過，照事先約定的失敗規則，當天全部撤回：Bypass application、tunnel 規則與速率限制運算式都撤回。這一輪的原始輸出沒有保存。

原因：Cloudflare 的計數會晚幾秒才更新，這段時間裡超過門檻的請求仍會到達源站（[Rate limiting rules](https://developers.cloudflare.com/waf/rate-limiting-rules/) 的 "Important remarks"）。所以單一連線在 10 秒內送 41 次，第 41 次不一定被擋。

[Laysi/trustiot-v2#795 的報告](2026-10-30-795-demo-data-path.md)的結果也是同一回事：

- 60 個並行的 `POST /certs/renew` 只有 9 個拿到 429；
- `GET /mqtt` 兩輪各 60 個，拿到 429 的是 106 個。

### 新的判定（使用者 2026-11-06 決定）

在同一條連線上送 300 次 `GET /certs/root-crl`（不帶 User-Agent，和閘道器一樣），全部滿足才通過：

| 條件 | 為什麼 |
| --- | --- |
| 第 1 到 40 次都是沒被限流時的回應（這裡是 Access 的 302），沒有 429 | 路徑還沒開放，而且計數是新的 |
| 每個回應都是那個回應或 Cloudflare 的 429，`server: cloudflare` | 路徑在 Access 後面時沒有請求能到 certs，所以 429 只可能是 Cloudflare 的 |
| 第一個 429 在第 200 次以前 | 規則確實計數這個路徑，留了計數延遲的餘裕 |
| 第 251 到 300 次全部是 429 | 封鎖撐得住 |
| 300 次在 10 秒內送完 | 都落在同一個計數期間 |
| burst 之後 30 秒內回到沒被限流時的回應 | 封鎖會結束 |

失敗規則和順序都沒有變：

- 失敗時等 60 秒再跑一次。
- 再失敗就撤回規則的變更並停下來問，不當場放寬判定。
- 只有完全沒有 429 時，才改走 Access 之後的備案。

同一條連線讓請求落在同一個資料中心，Cloudflare 每個資料中心各自計數（[Request rate calculation](https://developers.cloudflare.com/waf/rate-limiting-rules/request-rate/)）。

### 第二次：通過並開放

| 步驟 | 時間（UTC） | 結果 |
| --- | --- | --- |
| 1.1、1.2 規則改成三個路徑 | 17:10 以前 | 讀回的運算式、`description`、計數、門檻與動作都正確，其他欄位沒變 |
| 1.3 burst 第 1 輪（[burst-r1.txt](data/2026-11-06-860-root-crl-opening/burst-r1.txt)） | 17:10:00 | **未通過**：<br>• 第 1 到 46 次是 302；第一個 429 在第 47 次<br>• 300 次中 223 次是 429，5.59 秒送完<br>• 第 251 到 300 次中有 11 次是 302：251、257、259、270、277–279、286、296、298–299 |
| 1.3 burst 第 2 輪，隔 60 秒以上（[burst-r2.txt](data/2026-11-06-860-root-crl-opening/burst-r2.txt)） | 17:11:21 | **通過**：<br>• 第 1 到 51 次是 302；第一個 429 在第 52 次<br>• 第 129 到 300 次全部是 429，300 次中 220 次是 429，5.13 秒送完<br>• 送完 2.1 秒、4.2 秒仍是 429；6.3 秒回到 302 |
| 1.4 configuration rule | | Browser Integrity Check 關閉的路徑加上 `/certs/root-crl` |
| 2.1 Bypass application 加 `between`（[between-r.txt](data/2026-11-06-860-root-crl-opening/between-r.txt)） | 17:13:24 | **通過**：<br>• `/certs/root-crl` 是 connector 的 403：Access 放行了，tunnel 還沒有規則<br>• `/certs%2Froot-crl` 與 `/certs%2froot-crl` 仍是 Access 的 302<br>• `/certs%2Frenew` 也是 302，這一項只記錄 |
| 2.2 tunnel 規則 | | `^/certs/root-crl$` 加在 `^/certs/renew$` 之後，讀回與預期相同 |
| 3 `after`（[after-r.txt](data/2026-11-06-860-root-crl-opening/after-r.txt)） | 17:13:54 | **通過**，見下表 |
| 3 `check --stage 2`（[tunnel-check-stage2.txt](data/2026-11-06-860-root-crl-opening/tunnel-check-stage2.txt)） | 17:14:20 | **通過**，見下方說明 |

第 2 輪 burst 的關鍵行：

```text
first 429: request 52; 220 of 300 got 429
PASS: requests 1-40 Access's redirect and no 429, the first 429 at request 52 (by 200), 251-300 all Cloudflare's 429, within 10 s, then Access's redirect again
```

`after` 各路徑的結果（不帶 cookie）：

| 請求 | 結果 |
| --- | --- |
| `GET /certs/root-crl` | 404，`text/plain; charset=utf-8`，`cache-control: no-store`，25 bytes：certs 的 `no root CRL is published\n`。values 沒有根 CRL，所以這就是預期的回應 |
| `/certs/root-crl/x`、`/certs/root-crl/`、`/certs/ROOT-CRL` | connector 的 403（`body-bytes 0`） |
| `/certs/root-crlx`、`/certs%2Froot-crl`、`/certs%2froot-crl`、`/certs`、`/`、`/auth/login`、`/healthz` | Access 的 302 |
| `/certs%2Frenew` | Access 的 302（只記錄，不在關卡內） |

`check --stage 2` 的結果：

- 從工作站與展示機各一組 13 項，全部 `PASS`：
  - `GET /certs/root-crl` 是 `unexpected`（入口的 404，第 2 段算到達）；
  - `/certs/root-crl/x` 與 `/mqtt/x` 是 `refused`；
  - `/certs%2Froot-crl` 是 `access`；
  - 其他路徑與 app、ota 主機名稱是 `access`；
  - `/mqtt` 的升級是 101，`POST /certs/renew` 是 400。
- `cloudflared.service` 沒有重啟。

### 觀察

- **速率限制在 Access 之前執行**。第 2 輪的 burst 在路徑還在 Access 後面時就拿到 Cloudflare 的 429，所以 `http_ratelimit` phase 先於 Access 的判斷。Cloudflare 的文件沒有寫這個順序，這是 2026-11-06 的實測。第一次沒有拿到 429，原因是計數延遲，不是順序。這次不需要走 Access 之後的備案。
- **第 1 輪沒通過的原因**：第 251 到 300 次中有 11 次是 302。代表計數延遲之外，同一條連線的請求在封鎖開始後仍有一部分沒被擋下。這次沒有查清楚是哪一種原因；照失敗規則跑的第 2 輪通過。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 加 Bypass 之前，速率限制規則涵蓋 `/certs/root-crl`（讀回的 ruleset），並實測拿到 Cloudflare 的 429 | **通過**：<br>• 規則讀回正確<br>• 判定改成上面的新判定（使用者 2026-11-06 決定，理由是 Cloudflare 的計數延遲）<br>• 第 1 輪未通過，照失敗規則跑的第 2 輪通過 |
| 開放後，gateway 主機名稱的 `GET /certs/root-crl` 不帶 cookie 拿到與展示環境 values 相同的根 CRL；同主機名稱的其他路徑與 `/certs/root-crl/x` 仍被擋下 | **通過，以 404 判定**：<br>• 展示環境的 values 沒有 `certificateAuthority.rootCrl`，certs 回 404 `no root CRL is published`，與 values 相符。使用者 2026-11-06 決定照這樣開放，並記錄成「404，與 values 相符」<br>• 為展示環境簽一份根 CRL 另開 Laysi/trustiot-v2#1111；之後再比對內容<br>• `/certs/root-crl/x` 是 connector 的 403，其他路徑是 Access 的 302（不是 403） |
| `BYPASS_PATHS` 含 `/certs/root-crl`，`bun test scripts/demo` 通過，runbook 已更新 | **通過**（這個分支） |
| 實測紀錄放在 `docs/test-reports/`，不含展示環境的主機名稱或其他私有資訊 | **通過**（本報告與 data 目錄） |

待測：

- 開放後重複的 burst。
- 從其他資料中心送的 burst（每個資料中心各自計數）。

兩項都屬於延後的進階驗證，與「把 `checks.ts` 移進 `scripts/demo/`」一起延後到 Laysi/trustiot-v2#1112（P2）。
