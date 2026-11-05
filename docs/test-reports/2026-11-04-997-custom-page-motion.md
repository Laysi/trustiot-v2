# 儀表板自訂頁面：沙箱不播放動作（2026-11-04）

對應 issue：Laysi/trustiot-v2#997。規則在 `frontend/sandbox/README.md`「顯示時比對的強制」的「沒有動作」與下一項（逃過這條規則的動畫）。

起因：`:hover` 開始的 transition 或動畫，在比對時算一次退步，`MAX_REGRESSIONS`（3）調低之後，有 `:hover` 效果的正當頁面被指標經過第 4 次就關閉。這次的變更讓沙箱不播放 CSS 轉場與動畫，`:hover` 的樣式直接套用；比對的規則不變，逃過這條規則的動畫照舊是發現。

## 環境

- 程式碼：分支 `feat/997-custom-page-wiring`（`3bd51753`），加上這次尚未提交的變更。下面每一項都以最後的程式跑；瀏覽器實測用的 `frontend/dist` 與最後的 `bun run --cwd frontend build` 逐檔相同。接線的重跑在收窄排除之前，見「指令」之後的說明。
- 開發者工作站；瀏覽器實測、探測與檢查服務的映像都在容器裡執行。Bun 與 Rust 是 `.prototools` 的版本。
- 瀏覽器實測與探測：`frontend/e2e/image.ts` 釘選的 Playwright 映像，Chromium 156.0.8078.4（linux/arm64），`--site-per-process`。伺服器與容器的接法同 [2026-11-03-997-custom-page-sandbox](2026-11-03-997-custom-page-sandbox.md)「環境」：真的 `services/web`（debug 建置）在工作站的 127.0.0.1 提供 `frontend/dist`，容器裡的轉送程式把 `localhost:<埠>` 轉過去。
- 檢查服務的映像：`bun test deploy/images/dashboard-check` 自己建置（chromium-headless-shell 156.0.8078.47，linux/arm64）。
- 探測：[probes/](data/2026-11-04-997-custom-page-motion/probes/) 的五支程式，每支自己設定頁面，不需要伺服器；各自的輸出在同名的 `.json`。

## 指令

```sh
# 單元測試與檢查，repo 根目錄
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
cargo test -p web
cargo test -p repo-check

# 檢查服務的映像（自己建置映像）
bun test deploy/images/dashboard-check

# 瀏覽器實測：同 2026-11-03-997-custom-page-sandbox 的「指令」，檔案是 e2e/custom-page.e2e.ts
docker run --rm --shm-size 1g --cap-drop ALL --security-opt no-new-privileges \
  -v <work>:/work:ro -v <out>:/out -w /work \
  -e E2E_BASE_URL=http://localhost:<埠> -e E2E_OUT=/out -e CI=1 <PLAYWRIGHT_IMAGE> \
  node node_modules/@playwright/test/cli.js test --config e2e/playwright.config.ts e2e/custom-page.e2e.ts

# 沒有排除時的對照：把 frontend/sandbox/src/dom.ts 的 `&& !ended(animation)` 暫時換成 `&& true`，
# 重新建置 frontend/dist，只跑「no motion」的測試（之後改回，git diff 確認 dom.ts 與改之前相同）
... test --config e2e/playwright.config.ts e2e/custom-page.e2e.ts -g "<no motion 的測試名稱>"

# 舊的排除規則的對照：把 dom.ts 的 ended() 裡 delay、activeDuration、endDelay 三個值的判斷暫時換回
# `endTime` ≤ 0，重新建置 frontend/dist，跑「no motion」的 5 個測試（之後改回、重新建置）
... test --config e2e/playwright.config.ts e2e/custom-page.e2e.ts -g "no motion"

# 探測（<modules> 是含 playwright-core 的 node_modules）
MODULES=<modules> IMAGE=<PLAYWRIGHT_IMAGE> \
  sh docs/test-reports/data/2026-11-04-997-custom-page-motion/probes/run.sh probe4.mjs
```

（`<PLAYWRIGHT_IMAGE>` 是 `frontend/e2e/image.ts` 的值。）

接線的實測（[2026-11-04-997-custom-page-wiring](2026-11-04-997-custom-page-wiring.md)）在加上「沒有動作」之後重跑過，指令見該報告。之後只收窄了 `dom.ts` 對已結束動畫的排除（安全審查的建議），接線實測的頁面沒有動畫，所以沒有再重跑。

## 結果

### Chromium 的行為（探測）

| 問題 | 結果 | 資料 |
| --- | --- | --- |
| 第一個 layer 的 `!important` 是否勝過頁面的 `!important` | 勝過頁面不在 layer 裡的、在頁面自己 layer 裡的，以及 `::before`、`::after`、`::marker` 與捲動時間軸的：計算值都是 `0s`、`0s`、`running`、`auto`、`none`。`style` 屬性的 `!important` 不受影響（`2s`、`1s`、`opacity`）。 | [probe1.json](data/2026-11-04-997-custom-page-motion/probes/probe1.json) 的 `cascade` |
| 有名稱的 layer | 頁面再寫一次同名的 layer（`@layer tq-motion { #join { animation: k 5s !important } }`），就以 specificity 勝出（`5s`）；沒有名稱的 layer 頁面無法加入（`0s`）。所以 `sandbox.css` 的 layer 沒有名稱。 | [probe3.json](data/2026-11-04-997-custom-page-motion/probes/probe3.json) 的 `join`、`anon` |
| 哪些偽元素有動畫，一條規則一個偽元素是否擋住 | 沒有 layer 時 `::marker`、`::before`、`::after`、`::scroll-marker-group`、`::scroll-marker`、`::column`、`::picker-icon`、`::backdrop` 有進行中的動畫；加上 layer 之後，動畫與之後改值引起的轉場都沒有。 | [probe2.json](data/2026-11-04-997-custom-page-motion/probes/probe2.json) |
| `::first-letter` 的規則 | 只設動作屬性的 `::first-letter` 規則也會讓區塊的第一個字自成一個方框：標記的文字從 1 個矩形變成 2 個。Chromium 不為 `::first-letter`、`::first-line`、`::selection` 播放動畫，所以這三個不列進 layer。 | [probe5.json](data/2026-11-04-997-custom-page-motion/probes/probe5.json) |
| 0 秒的動畫在比對時的狀態 | `:hover` 或 script 加上 class 開始的 0 秒 CSS 動畫，在那個 task 結尾（runtime 的 `requestCompare`）是 `finished` 但 `pending: true`，下一個 animation frame 才不是 `pending`；`endTime` 0、`currentTime` 0。效果被設成 0 的 `animate()` 到 task 結尾時已不是 `pending`。沒有一個是 `running`。 | [probe4.json](data/2026-11-04-997-custom-page-motion/probes/probe4.json) |
| 播放速率 | `updatePlaybackRate(-1)` 與 `reverse()` 之後，`playbackRate` 在下一個 frame 前仍讀到 1；目前時間被移到 −2000 的 0 秒效果是 `running`；捲動時間軸的 `endTime` 與目前時間讀不成數字。 | [probe3.json](data/2026-11-04-997-custom-page-motion/probes/probe3.json) 的 `timing` |

第五列的 `pending` 是 `dom.ts` 的 `liveAnimations` 加上排除的原因：排除的只有沒有暫停、效果的 `getComputedTiming()` 自己的 `delay`、`activeDuration`、`endDelay` 都是數字 0（正好是「沒有動作」給的時間）、往前播放（速率大於 0）而且目前時間 ≥ 0 的動畫；往回播放的一律算（README「沒有動作」的第四項寫明為什麼畫不出中間的狀態）。第六列是 `seal.ts` 讓速率立刻生效的原因：否則還沒套用的負速率會讀成舊的正速率而被排除。讀不成數字的值不排除。

排除原本是 `endTime` ≤ 0，安全審查指出它也放過了逃過 layer 的負延遲動畫（`style` 屬性的 `animation: x 10s -10s both !important`，同樣在 0 結束），於是改成上面三個值都是 0。實測（下一節的 (e)）：script 設定這樣的動畫時，那個 task 結尾的比對讀到它 `pending`、時間 0、速率 1，舊的規則會排除它，新的規則算它。同一個動畫結束後再 `reverse()`：呼叫後立刻讀到 `pending: true`、速率 −1、時間 0，但那個 task 結尾的比對讀到的已是 `finished`、不再 `pending`，所以比對根本不判斷它（`findingsAfterReverse` 0）；這只是 Chromium 156 的行為，排除不依賴它：若它在比對時仍 `pending`，往回播放就算一個發現。

Codex 的 pre-commit 審查指出往回播放、時間正好 0 的情況（在 0 時處於哪個階段取決於方向），所以排除再收窄成只放過往前播放的動畫；「沒有動作」與 `seal.ts` 不會產生負的速率，只有頁面 script 會。

### 瀏覽器實測

- `custom-page.e2e.ts` 的 35 個測試全部通過（[playwright.txt](data/2026-11-04-997-custom-page-motion/playwright.txt)，最後的程式跑一次；收窄成只放過往前播放之前的程式也連續兩次全部通過）。新的 5 個在「no motion」：
  - (a) 有 `:hover` 背景 transition、`:hover` 的 `forwards` 動畫與 `pointerenter` 時 `animate()` 的卡片，指標進出十次，frame 一次都沒藏起，`:hover` 的樣式在指標進來後立刻是最後的值。
  - (b) 頁面自己的 `!important`（不在 layer、`@layer first`、`@layer second` 加捲動時間軸、`::after`）的計算值都是沒有動作的值，`:hover` 的 transform 立刻套用。
  - (c) 寫在 `style` 屬性 `!important` 的 transition：每次指標經過 frame 都藏起，第 4 次（`MAX_REGRESSIONS` + 1）之後 frame 被移除。
  - (d) 頁面 script 把自己的 `<style>`（沒有名稱的 layer 裡的 `!important` transition）放到 `sandbox.css` 前面：它的 layer 先宣告而勝出（計算值 `transform`），和 (c) 一樣每次指標經過都藏起，藏起 4 次後 frame 被移除。
  - (e) script 以 `style` 屬性的 `!important` 設定 `slide 10s -10s both`（`delay` −10000、`activeDuration` 10000、`endTime` 0）：比對算它，frame 藏起；之後的 `reverse()` 只記錄（見上一段）。
- 原本的兩個 CSS 動作案例改成 `style` 屬性的 `!important`（樣式表裡的動作不再播放），仍量到同樣的保證；Web Animations 的案例改成：效果被設成 0 的 `animate()` 立刻停在最後的 keyframe、沒有發現；把目前時間移到 −3000 的動畫讓 frame 藏起，直到它結束才再顯示。
- 曝光（[exposure.json](data/2026-11-04-997-custom-page-motion/exposure.json)；錯的值在外層顯示時被畫出的 ms）：

| 案例 | `total` | `beyondReport` | 從畫出到藏起（ms） |
| --- | --- | --- | --- |
| 載入時在 `style` 屬性宣告、3 秒後開始的 CSS 動畫 | 0 | 0 | 50（frame 在那之前就因動畫進行中而藏著） |
| `style` 屬性裡、`:hover` 開始、延遲 1 秒的 transition | 0 | 0 | 17（同上，frame 從指標進來就藏著） |
| `style` 屬性的 transition，每次指標經過 | — | — | 藏起 4 次，之後 frame 被移除 |
| 放到 `sandbox.css` 前面的樣式表的 transition，每次指標經過 | — | — | 藏起 4 次，之後 frame 被移除 |
| 負延遲的 `style` 屬性動畫 | — | — | 設定後藏起 1 次 |
| 其他（`disabled`、SVG、每個 frame 打開錯的值、時有時無） | 0 到 4 | 0 | 與 2026-11-03 的量測同一量級 |

- 沒有排除時的對照（[playwright-without-exemption.txt](data/2026-11-04-997-custom-page-motion/playwright-without-exemption.txt)）：(a) 不通過，指標經過幾次後 frame 被移除（`Target page, context or browser has been closed`），也就是 `:hover` 的 0 秒動畫每次都算一次退步；同一次的另一個測試通過。
- 舊的排除規則（`endTime` ≤ 0）的對照（[playwright-end-time-rule.txt](data/2026-11-04-997-custom-page-motion/playwright-end-time-rule.txt)）：(e) 不通過（60 秒內沒有發現），其他 4 個通過；改回之後 (e) 通過。

### 單元測試與檢查

| 指令 | 結果 |
| --- | --- |
| `api:check`、`typecheck`、`lint`、`build`、`check:dist` | 通過 |
| `bun run --cwd frontend test` | 1048 個裡 1046 個通過（[frontend-test.txt](data/2026-11-04-997-custom-page-motion/frontend-test.txt)）；失敗的 2 個是工作站的 ICU 與 CI 不同（`dashboardFormat` 的毫秒、表格的時間格），main 上同樣失敗，與這次的變更無關。第一次收窄排除之後的第一次執行另有 1 個 `runtime.test.ts` 的測試（「events the page dispatches itself explain nothing」，happy-dom 裡沒有動畫）在 30 秒的上限逾時，那次整體跑了 91 秒（平常 60 秒）；單獨重跑 3 次與整套重跑都通過；最後的程式跑一次，只有那 2 個失敗 |
| `cargo test -p web` | 28 個通過 |
| `cargo test -p repo-check` | 通過 |
| `bun test deploy/images/dashboard-check` | 第一次 77 個通過、1 個失敗：`slanted-labels` 的 KPI 判成 `mismatch`，原因是 layer 當時含 `::first-letter` 規則（見上表第四列）；拿掉 `::first-letter`、`::first-line`、`::selection` 之後 78 個全部通過，含新的 `css-motion-stopped` 樣本通過與 `css-animation` 樣本 `timeout`；64 次檢查，每次 144 到 5179 ms（中位數 440 ms），容器記憶體最高 104.3 MiB（上限 512 Mi）。改了排除之後重跑：78 個全部通過，64 次檢查，每次 146 到 5176 ms（中位數 439 ms），記憶體最高 102.5 MiB；最後的程式（只放過往前播放）：78 個全部通過，64 次檢查，每次 143 到 5182 ms（中位數 439 ms），記憶體最高 103.6 MiB（[dashboard-check.txt](data/2026-11-04-997-custom-page-motion/dashboard-check.txt)） |

`dom.test.ts` 與 `seal.test.ts` 的新測試以暫時拿掉被測的程式確認會失敗：拿掉排除時 `dom.test.ts` 2 個失敗，把排除換回 `endTime` ≤ 0 時 2 個失敗（仍計數的情況、`Object.prototype` 上的值），再放過往回播放時 2 個失敗（仍計數的情況、時間 0 往回播放的情況），拿掉 `seal.ts` 的時長設定或速率設定時 `seal.test.ts` 4 個失敗；改回之後全部通過。

## 結論

- `:hover` 的效果直接套用，正當頁面不再因 `:hover` 累積退步：通過（(a)，十次指標經過沒有一次藏起；沒有排除時同一個測試不通過）。
- 頁面的 `!important` 打不開動作：通過（(b) 與探測第一、二列），限於頁面的樣式表；`style` 屬性的 `!important`、把自己的樣式表移到 `sandbox.css` 前面的 script 會逃過，由比對接住。
- 逃過的動作照舊藏起並計數：通過（(c) 與 (d)，各藏起 4 次後移除；(e) 負延遲的動畫藏起；載入時的動畫與延遲的 transition 兩個案例 `beyondReport` 皆 0）。
- Web Animations API：`animate()`、`new KeyframeEffect()`、`updateTiming()` 之後效果沒有時長與延遲，速率立刻生效：通過（`seal.test.ts`、瀏覽器實測）。把目前時間移到開始之前的動畫仍是發現：通過。
- 排除只限 `delay`、`activeDuration`、`endDelay` 都是 0、往前播放而且時間 ≥ 0：通過（`dom.test.ts` 的 26 種仍計數的情況，含往回播放的與負延遲在 0 結束的，以及 Codex 指出的時間 0 往回播放、`pending` 的情況；不從 `Object.prototype` 讀；瀏覽器實測 (e)，舊的規則下不通過）。
- 和檢查服務一致：通過（映像測試 78 個）。
- 不在這次範圍：`animation-trigger`（Biome 不認得這個屬性，沒有列進 layer；Chromium 支援 `none`，見 probe2 的 `triggerNone`），捲動時間軸以外的時間軸只靠比對接住；`::column`、`::picker()` 等偽元素的規則只以探測與固定樣本確認，沒有各自的瀏覽器實測。
