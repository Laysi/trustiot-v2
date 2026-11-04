# 儀表板自訂頁面：沙箱外層、web 標頭與顯示時比對的強制（2026-11-03）

對應 issue：Laysi/trustiot-v2#997。設計與規則在 `frontend/sandbox/README.md`「顯示時比對的強制」、`services/web/README.md`「安全標頭」與「沙箱的檔案」。

## 環境

- 程式碼：分支 `feat/997-dashboard-sandbox-frame`。分支在推送前壓成一個 commit，下面說的版本是壓之前的中間版本，不在歷史裡。標頭在最後一次改 `services/web` 之後取得。第一輪的瀏覽器實測、單元測試與檢查在合併 2026-11-03 的 main 之後跑。第二輪安全審查的修正之後，瀏覽器實測、檢查服務的映像與單元測試重跑過；之後 render 開始不再算通知（第三輪），單元測試與瀏覽器實測再跑一次，檢查服務的映像測試交給 CI；其餘只改了註解與文件。
- 開發者工作站，Docker。Bun 與 Rust 是 `.prototools` 的版本。
- 瀏覽器：`frontend/e2e/image.ts` 釘選的 Playwright 映像（Playwright 1.64.1 附的 Chromium，linux/arm64），以 `--site-per-process` 執行（測試檔自己設定）。
- 伺服器：真的 `services/web`（`cargo build -p web` 的 debug 建置），在工作站的 127.0.0.1 提供 `bun run --cwd frontend build` 的 `frontend/dist`。
- 與 `scripts/browser-e2e/run.ts` 的不同：那個指令需要 podman，只在 x86_64 Linux 驗證過，所以這次沒有用它，而是直接啟動 `services/web` 與 Playwright 容器。容器裡以一個轉送程式把 `localhost:<埠>` 轉到工作站，瀏覽器看到的網址是 `http://localhost:<埠>`，與 `run.ts` 相同。直接用 Docker 的主機名稱時，Chromium 的 Local Network Access 會擋下沙箱 frame 的載入（`ERR_BLOCKED_BY_LOCAL_NETWORK_ACCESS_CHECKS`：頁面不是安全來源，目標是私有位址），所以要用 `localhost`。`custom-page.e2e.ts` 不登入，不需要身分服務。
- 外層頁面：`frontend/e2e/custom-page/harness.tsx`（真的 `CustomPageFrameView`、guard 與 `SandboxHost`），測試以 `services/web` 對 `/` 的真實回應標頭（含 `frame-src`）提供它；沙箱文件與 script 由 `services/web` 直接提供。
- 探測（「顯示時比對的量測」一節）：一支不在 repo 的 Playwright 程式，以工作站上 Playwright 附的 Chromium headless shell（`--site-per-process`）打開同一個外層頁面，沙箱檔案由 Playwright 的 route 從 `frontend/dist` 提供（標頭同 `services/web`）。它印出外層的顯示紀錄、port 上的每則回報與頁面自己的紀錄。
- 檢查服務的映像：以這個分支的 `frontend/dist` 建置 `dashboard-check` 映像（`scripts/nightly/service-image.ts` 的 `buildServiceImage`，與 nightly 相同），再以 `DASHBOARD_CHECK_IMAGE=<映像> bun test deploy/images/dashboard-check` 跑每個沙箱固定樣本。

## 指令

```sh
# 單元測試與檢查，repo 根目錄
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
cargo test -p web

# 瀏覽器實測
bun frontend/e2e/custom-page/build.ts
TRUSTIOT__ROOT_DIR="$PWD/frontend/dist" TRUSTIOT__HTTP__LISTEN_ADDR=127.0.0.1:<埠> \
  TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR=127.0.0.1:<健康埠> ./target/debug/web &
# 把 @playwright/test、playwright、playwright-core 與 frontend/e2e 放進 <work>（同 run.ts 的 stageBrowserWork），然後
docker run --rm --shm-size 1g --cap-drop ALL --security-opt no-new-privileges \
  -v <work>:/work:ro -v <out>:/out -w /work \
  -e E2E_BASE_URL=http://localhost:<埠> -e E2E_OUT=/out -e CI=1 <PLAYWRIGHT_IMAGE> \
  node node_modules/@playwright/test/cli.js test --config e2e/playwright.config.ts e2e/custom-page.e2e.ts

# 檢查服務的映像（先以 buildServiceImage 建置 <映像>）
DASHBOARD_CHECK_IMAGE=<映像> bun test deploy/images/dashboard-check

# 標頭
curl -s -D - -o /dev/null -H 'Host: app.example.test' http://127.0.0.1:<埠>/
curl -s -D - -o /dev/null -H 'Host: app.example.test' http://127.0.0.1:<埠>/sandbox/v1/sandbox.html
```

（`<PLAYWRIGHT_IMAGE>` 是 `frontend/e2e/image.ts` 的值；容器裡另有把 `localhost:<埠>` 轉到工作站的轉送程式，見「環境」。）

## 結果

- 瀏覽器：`custom-page.e2e.ts` 的 27 個測試全部通過，連續兩次；第二輪安全審查的修正之後是 30 個，同樣連續兩次全部通過；第三輪（render 開始不再算通知）之後再跑一次，30 個全部通過，那次的 `frontend/dist` 與推送的 commit 建出的逐位元相同（[playwright.txt](data/2026-11-03-997-custom-page-sandbox/playwright.txt) 是這一次的輸出；五次量到的曝光在 [exposure.txt](data/2026-11-03-997-custom-page-sandbox/exposure.txt)）。
- 檢查服務的映像：76 個測試全部通過，含每個固定樣本得到預期的判定（`view-transition` 現在預期 `script_error`）；62 次檢查，每次 149 到 5175 ms（中位數 442 ms），容器記憶體最高 204.5 MiB（上限 512 Mi）。第二輪修正後重新建置：77 個測試全部通過（新的 `element-internals` 樣本預期 `script_error`）；63 次檢查，每次 148 到 5167 ms（中位數 436 ms），容器記憶體最高 113.2 MiB。
- 標頭：[headers.txt](data/2026-11-03-997-custom-page-sandbox/headers.txt)。SPA 的 CSP 多了 `frame-src app.example.test/sandbox/v1/sandbox.html`；`/sandbox/v1/sandbox.html` 帶 `sandbox allow-scripts`、`frame-ancestors 'self'`、`Cross-Origin-Resource-Policy: cross-origin`，沒有 `X-Frame-Options`；`runtime.js` 只有 `nosniff`、`cross-origin` 與 HSTS；`/sandbox/check.html` 是 404。
- `cargo test -p web`：28 個測試通過。`bun run --cwd frontend test`：880 個裡 878 個通過（第二輪修正後 881 個裡 879 個，第三輪後 882 個裡 880 個），失敗的 2 個是工作站的 ICU 與 CI 不同（`dashboardFormat` 的毫秒、表格的時間格），main 上同樣失敗，與這次的變更無關。`typecheck`、`lint`、`build`、`check:dist`、`api:check`、`bun run --cwd scripts typecheck` 與 `bun test scripts/browser-e2e scripts/ci/changes.test.ts`（371 個）通過。

## 顯示時比對的量測

安全審查找到兩條 runtime 沒被通知的路（`<style>` 的 `disabled`、`document.adoptedStyleSheets`），以及一個看準固定心跳的頁面讓錯的值在 40 秒裡顯示了 29.7 秒。改成隨機時間的比對、等畫出來才顯示、回報中斷就藏起之後量到的：

**曝光**（`custom-page.e2e.ts`，錯的值在外層顯示時被畫出的 ms；頁面自己的紀錄，不計 runtime 在 frame 裡的隱藏）：

| 頁面 | 最後兩次的總曝光 | 該改動的回報之後 | 改動到回報 |
| --- | --- | --- | --- |
| `<style>` 的 `disabled` 打開假的值 | 0、1 | 0、0 | 14、28 ms |
| 以亂數間隔閃現 60 ms（20 次） | 0、0，第 4 次被抓到時移除 | 0、0 | 3 到 18 ms |
| 載入時宣告、3 秒後開始的 CSS 動畫 | 0、0 | 0、0 | 一開始就藏著 |
| `:hover` 開始、延遲 1 秒的 transition | 0、0 | 0、0 | 指標進來時就藏起 |
| SVG `baseVal` 改長條寬度 | 0、1 | 0、0 | 10、18 ms |

1 ms 的總曝光都在回報抵達外層之前，也就是 README 寫的每次改動的跨行程時間差。開發中的其他次也只量到 0 或 1 ms。

**比對成本**（探測，中位數／p90／最大）：10 個標記 5.5／7／8.9 ms；200 個標記加 400 個方框 35／46／48 ms；1000 個標記加 4000 個方框 100／118／118 ms；3000 個標記加 12000 個方框 224／264／282 ms。小頁面量到的平均隨機間隔約 90 到 120 ms。依此取平均間隔為成本的 20 倍（約 5% CPU），夾在 100 ms 到 1 秒。

**沒被通知的路**（探測）：把 SVG DOM 的包裝暫時拿掉，等於一條沒被通知的路；反覆「50 ms 錯、50 ms 對」的頁面跑 12 次，12 次都以 `failed`（`unnotified`）關閉，關閉前錯的值在外層顯示時被畫出的累計時間是 336、84、86、9、99、46、102、0、64、86、176、125 ms（平均約 101 ms，與平均間隔相符）。包裝放回後同一頁面總共 2 ms 曝光，以退步上限關閉。

**原本沒被通知的路**（探測，修正後）：`disabled`、`rule.style =`（PutForwards）、`CSS.registerProperty`、自訂元素的 `define`、`ElementInternals.states`、popover、`setCustomValidity`、`ElementInternals.setValidity`、`designMode`、`location.hash`（`:target`）、自訂 highlight、模擬切換深色模式：都在 2 到 7 ms 內藏起，沒有關閉。`new CSSStyleSheet()` 與 `startViewTransition` 丟出例外。SVG DOM 的 `baseVal` 與 `currentScale` 在 Chromium 不產生變動紀錄（探測：0 筆），包裝後 2 ms 內藏起。

**卡住執行緒**（探測）：乾淨的頁面卡住 4 秒，外層在卡住開始後約 0.83 秒（最後一則回報後 1 秒）藏起，恢復後立刻再顯示；改了被通知的值再卡住，5 ms 內藏起。e2e：卡住不放的頁面在最後一則回報至少 1 秒後藏起，最後一個心跳至少 10 秒後移除。

**藏起的 frame 的繪製**（探測）：外層以透明藏起後約半秒，frame 的 `requestAnimationFrame` 從每秒約 60 次降到大約每秒 1 次。所以乾淨的結果改成等兩個 animation frame 再送：沒有這一步時，e2e 在閃避的頁面重新顯示的那一刻量到 1 到 2 ms 的重疊，加上之後是 0；以 Web Animations API 移動元素 3.5 秒的頁面，動畫結束後約 2 秒再顯示。

### 第二輪安全審查（同日）

審查在修正後的分支找到：runtime 把頁面自己 `dispatchEvent` 的事件也當成通知，而且通知一直有效到下一個 animation frame，所以頁面每次改值後送出假的捲動或焦點事件，沒被通知的改動就被當成有原因（探測：錯的值在 10 秒裡被顯示約 6.35 秒，最後以退步上限關閉，從沒以 `unnotified` 關閉）；SVG `<style>` 的 `disabled` 沒有包住；`ElementInternals` 的 ARIA 能讓螢幕閱讀器念出錯的數字。修正：通知只算 `isTrusted` 的事件，而且只到那個 task 結束（事件在它的 task 結尾比對）；取樣改成對齊繪製；包住 SVG `<style>` 的 `disabled` 與每個元素介面自己的 `disabled` setter、元素的 `scrollTop`／`scrollLeft`；`attachInternals` 丟出例外。

**沒被通知的路的關閉時間**（探測，暫時拿掉 SVG `<style>` 的包裝當作沒被通知的路；攻擊開始到以 `unnotified` 關閉之間外層顯示 frame 的時間；每一次的數字在 [probes.txt](data/2026-11-03-997-custom-page-sandbox/probes.txt)）：

| 探測頁 | 取樣對齊繪製（採用；「現行」是提交的版本，「第一版」是外層藏著時也對齊繪製的版本，沒被通知的改動走同一段程式） | 任意時間比對一次（修正前的取樣） | 每次取樣只比一次、隨機選繪製前或後（試過，不採用） |
| --- | --- | --- | --- |
| 每個 frame 打開、繪製後關掉，自送假事件掩護 | 第一版 26 到 262 ms，平均 123 ms（8 次） | 同下一列（假事件已經不算） | — |
| 同上，不掩護 | 現行 30 到 173 ms，平均 102 ms（5 次）；第一版 4 到 307 ms，平均 82 ms（8 次） | 123 到 6979 ms，平均 2.1 秒（8 次） | 47 到 640 ms，平均 327 ms（5 次） |
| 50 ms 錯、50 ms 對 | 現行 22 到 426 ms，平均 185 ms（5 次）；第一版 23 到 342 ms，平均 98 ms（5 次） | 23 到 562 ms，平均 251 ms（5 次） | 16 到 1041 ms，平均 289 ms（5 次） |
| 不掩護的大頁面（15000 個方框） | 第一版 0.9 到 2.0 秒（4 次） | 一次 18.5 秒，三次 27 秒內沒關閉 | 0.7 到 2.7 秒（4 次） |

採用對齊繪製：每一種都比修正前好或差不多，大頁面與每個 frame 閃一下的頁面差最多；代價是每次取樣兩次比對，所以平均間隔加倍（小頁面約 220 ms，CPU 一樣約 5%；夾到上限 1 秒的大頁面 CPU 加倍）。只要頁面有一條沒被通知的路，就能把錯的值安排在取樣看不到的時間，沒有上限；在改動前後卡住執行緒不到 1 秒也屬於這一類。真正的防線是 `seal.ts` 的清單完整，目前沒有已知的這種路。細節不公開（`docs/architecture/secrets.md`「公開 repo 的資訊規則」第 4 條），處理是 Laysi/trustiot-v2#40 的上線前提條件。

**反覆登記 render 的掩護**（審查的探測頁，修正後）：頁面每 8 ms 交替以兩個不畫東西的函式呼叫 `tq.on("data", …)`，原本每次 render 開始都設定「有通知」並一直保持到下一次比對，沒被通知的改動因此只算退步（只看到 `compared/2`）。render 開始不再算通知（它改的 DOM 本來就經 `MutationObserver` 通知，新的資料在送達時通知）之後，同一頁在攻擊開始後約 0.2 秒以 `failed`（`unnotified`）關閉。

**焦點掩護**（探測）：頁面以自己元素的 `focus()`／`blur()` 掩護時（真的事件），每次改動在那個 task 結尾就被比對到：外層在 7 到 12 ms 內藏起，之後一直藏著，沒有關閉也沒有再顯示。e2e 以被通知的路做同樣的兩種掩護，量到 3 到 8 ms（第一次改動的回報抵達前），之後一直藏著。

**對齊繪製的副作用與修正**：第一版在外層藏著時也在繪製後的 task 比對。以亂數間隔閃現 60 ms 的 e2e 頁面因此 4 次裡 2 次沒有在 60 秒內被關閉：探測頁 4 次裡 2 次在某次藏起之後 17 秒都沒再顯示，追蹤看到 frame 一藏起，它的 animation frame 就降到約每秒一次，等兩個 animation frame 的「等畫出來才送」被下一次閃現取消。只在 animation frame 比對（不在繪製後的 task）時 4 次都正常，以 `setTimeout` 代替 `MessageChannel` 時偶爾延遲 1 秒，只送空的 task 時正常；原因沒有找到。改成只在外層可能顯示 frame 時（最近送出的是乾淨的結果）對齊繪製後，同一個探測頁 10 次裡 1 次延遲 2 秒、10 次都在第 4 次被抓到時關閉，e2e 的該測試連跑 4 次與整套兩次都通過。沒被通知的改動在被關閉前不會送出發現，所以這個條件不改變上表的數字。

**setter 掃描**（Chromium headless shell）：對每個 `*Element` 介面原型上的每個 setter 寫一次值，看 `MutationObserver` 有沒有紀錄。不留紀錄的：`Element.scrollTop`、`scrollLeft`；`HTMLStyleElement.disabled`、`SVGStyleElement.disabled`；`SVGSVGElement.currentScale`、`zoomAndPan`；`<a>`／`<area>` 的 `hash`、`host`、`hostname`、`password`、`pathname`、`port`、`protocol`、`search`、`username`；`nonce`；`HTMLScriptElement.async`；`HTMLDialogElement.returnValue`；`<input>` 的 `checked`、`indeterminate`、`value` 與選取範圍，`<select>` 的 `selectedIndex`、`value`，`<option>` 的 `selected`，`<textarea>` 的 `value` 與選取範圍；媒體元素的 `currentTime`、`muted`、`playbackRate` 等。有 `disabled` setter 的元素介面：`SVGStyleElement`、`HTMLStyleElement`、`HTMLLinkElement`、`HTMLButtonElement`、`HTMLFieldSetElement`、`HTMLInputElement`、`HTMLOptGroupElement`、`HTMLOptionElement`、`HTMLSelectElement`、`HTMLTextAreaElement`，只有兩個 `<style>` 不留紀錄。SVG 點的清單（`polyline.points.getItem(0).x = …`）也不留紀錄，它是 `SVGPoint`，已在 SVG 數值介面的包裝裡。

實測中找到並修正的問題：

1. **藏起的 frame 永遠比對不乾淨**：外層原本以 `visibility: hidden` 藏起 frame。Chromium 對這樣藏起的跨來源 frame 停止繪製，`requestAnimationFrame` 不觸發、動畫不前進，所以「畫完」等不到兩個 animation frame、動畫結束不了，頁面一被藏起就再也顯示不出來（從一開始就對不到資料的值不會換成「—」；動畫結束後也不再顯示）。改成透明、不接收指標、`inert` 與 `aria-hidden`。
2. 測試本身的錯誤：來源面板是 region 不是 dialog；`sendBeacon` 在 CSP 擋下前就回傳 true（改以 CSP 違規與沒有完成的請求判斷）；找 port 的頁面把自己的 `MessageChannel` 也記成找到的 port；被 CSP 擋下的請求在 Playwright 也有 `request` 事件（改看是否完成）；淡出數值的動畫會讓數值被換成「—」（改成移動不含數值的元素）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 頁面的對外請求全部被擋下，讀不到 cookie、儲存空間與外層 DOM | 通過：fetch、XMLHttpRequest、WebSocket、beacon、圖片、字型、CSS 背景圖、表單、Worker、子 frame、`window.open`、導向外層與導向其他主機都被擋下或關閉 frame；`localStorage`、`sessionStorage`、IndexedDB、外層文件都讀不到，cookie 看不到平台的 cookie；子 frame 是另一個 origin |
| 資料值含 HTML 或 script 時以文字顯示 | 通過（瀏覽器與單元測試） |
| 顯示時對不到資料的標記顯示「—」並標示 | 通過（瀏覽器；修正 1 之後） |
| 外層只依 port 上的比對結果顯示 frame，含刻意對抗的頁面 | 通過：偽造、重送、大量 window 訊息、找 port、比對後經 DOM、CSSOM、封閉 shadow root、動畫改值、經 HTML 與 SVG `<style>` 的 `disabled`、SVG `baseVal`、建構的樣式表、CSS 動畫與 `:hover` transition 改值、以亂數間隔閃現、每個 frame 改值並以自送或自己引起的事件掩護、一直改回、卡住執行緒、重新載入與導向；量到的曝光見「顯示時比對的量測」 |
| 點標記的數字，來源面板的 SQL 與查詢時間與 API 一致 | 通過（瀏覽器與單元測試；資料是固定樣本） |
| 自訂頁面的卡片有「自訂圖表」標示與說明 | 通過（單元測試） |
| `services/web` 的標頭 | 通過（`cargo test -p web` 與上面的實際回應） |
| 前端與 `cargo test -p web` 的檢查 | 通過，除了上面兩個與變更無關的工作站失敗；CI 在 PR 上跑全部 |

待測：

- Chrome 以外的瀏覽器（Laysi/trustiot-v2#946），包括它們如何繪製藏起的 frame。
- 有一條沒被通知的路時，取樣沒有上限（README「還擋不住的」；處理是 Laysi/trustiot-v2#40 的上線前提條件）。
- 比對成本與探測的數字是開發者工作站上的值；較慢的裝置上比對更貴，平均隨機間隔會跟著變長（最多 1 秒）。
- 經 Istio 入口的實際網域（`frame-src` 取自 `Host`，這次以 `localhost:<埠>` 與 `app.example.test` 驗證）。
- `scripts/browser-e2e/run.ts` 的整套流程（需要 podman，見「環境」）。
