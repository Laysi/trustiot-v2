# frontend/sandbox

儀表板頁面的沙箱 runtime（Laysi/trustiot-v2#150；檢查單 J4，決策 33）：AI 寫的頁面只在這裡執行。同一份建置結果有兩個使用者：

- **檢查服務**（`services/dashboard-check`）：發布前在無頭 Chromium 執行頁面，從 isolated world 比對每個標記與資料，回傳判定。
- **前端的儀表板畫面**（Laysi/trustiot-v2#462）：使用者檢視時在同一個沙箱執行頁面，顯示前再比對一次。

決定與理由在 [AI 助理與 AI 閘道](../../docs/architecture/ai-assistant.md)「儀表板」。Rust 端的型別、上限與 header 在 `crates/contract/src/api/dashboard/`（說明見 [crates/contract](../../crates/contract/README.md)「儀表板」）；本文件是 runtime（`window.tq`）、postMessage 協定、檢查規則與檢查流程的契約。

| 檔案 | 內容 |
| --- | --- |
| `src/protocol.ts` | 協定與判定的型別（zod），與 Rust 型別對應 |
| `src/formula.ts` | 計算語言的解析與求值 |
| `src/format.ts`、`src/locales.ts`、`src/time.ts` | `tq.format` 與時間的讀法 |
| `src/values.ts` | 參照解析成值（Rust `CheckRequest::resolves`、`value_of`、`declares` 的對應） |
| `src/compare.ts`、`src/geometry.ts`、`src/dom.ts` | 比對程式：檢查規則、幾何計算、事先保存的 DOM 函式 |
| `src/harden.ts` | 在頁面 script 執行前凍結這個 realm 的 ECMAScript 內建物件 |
| `src/runtime.ts` | 沙箱裡的 runtime：`window.tq`、載入頁面、畫完的判斷、檢視時的比對 |
| `src/host.ts` | host adapter：postMessage 的外層（SPA 與檢查的 host 頁共用） |
| `src/check-host.ts`、`src/checker.ts` | 檢查服務的 host 頁與 `compare.js` 的入口 |
| `src/entries/` | 四個 bundle 的入口；`static/` 是兩份 HTML 與樣式表 |
| `fixtures/` | 固定樣本：頁面、資料與預期判定（「固定樣本」） |
| `test/` | runtime、比對與固定樣本的測試 |

## 建置結果

`bun run --cwd frontend build` 在 Vite 之後執行 `tools/build-sandbox.ts`，寫到 `frontend/dist/sandbox/`。`web` 映像隨 SPA 帶著這些檔案；檢查服務的映像取同一個目錄（`frontend/dist/sandbox/` 整個）。每個 script 是一個獨立的 bundle（立即執行的函式，執行時不 import 任何東西，沒有 source map），不屬於 SPA 的 bundle。

| 網址路徑（`frontend/dist` 下同一路徑） | 內容 | 依 runtime 版本 |
| --- | --- | --- |
| `/sandbox/v1/sandbox.html` | 沙箱文件：依序載入 `sandbox.css`、`d3.js`、`runtime.js`，`<body>` 只有 `<div id="tq-root"></div>` | 是 |
| `/sandbox/v1/runtime.js` | runtime：`window.tq`、計算語言、postMessage 的沙箱端，以及檢視時用的比對程式（自帶一份，不載入 `compare.js`） | 是 |
| `/sandbox/v1/d3.js` | d3，`window.d3`，版本隨 runtime 凍結 | 是 |
| `/sandbox/v1/sandbox.css` | runtime 的樣式（版面重設、主題變數、檢視時的標示） | 是 |
| `/sandbox/v1/compare.js` | 比對程式：只定義 `globalThis.tqCompare`，自帶 d3-format、d3-time-format 與計算語言，不依賴頁面的 `window` | 是 |
| `/sandbox/check.html`、`/sandbox/check.js` | 檢查服務的 host 頁：定義 `globalThis.tqCheckHost`，建立沙箱 iframe、送資料、回報狀態給驅動程式 | 否 |

- 路徑與每個檔案的 header 由 `contract::api::dashboard::sandbox::SandboxFile` 定義（`path`、`content_type`、`headers`），檢查服務與 `services/web` 照它送出，不各自定義。`services/web` 給這些檔案的 header 要取代 SPA 的 header，不能疊加：瀏覽器會同時套用每個 CSP header，SPA 的 `frame-ancestors 'none'` 會讓沙箱框不進來。
- 沙箱 frame 自己能導向到哪裡，由外層頁面的 CSP `frame-src` 決定，不由沙箱文件的 header 決定：檢查服務的 host 頁是 `CHECK_HOST_CSP`；檢視時 SPA 的 CSP（Laysi/trustiot-v2#462）除了沙箱之外不能允許其他 frame 來源。
- host adapter（postMessage 的外層）是 TypeScript 模組，SPA 與 `check.js` 各自打包進去，不是獨立檔案。
- 每個 runtime 版本的檔案一旦發布就不再改變行為：已檢查過的儀表板版本永遠以它的 runtime 執行。改 `window.tq`、計算語言、格式化或檢查規則的行為要開新的版本目錄（`/sandbox/v2/`），並在 `RuntimeVersion::SHIPPED` 與 `protocol.ts` 的 `RUNTIMES` 加上它；只有安全修正可以改舊版本的檔案。目前只有 v1，`build-sandbox.ts` 從目前的原始碼建置它；開 v2 時要把 v1 的檔案凍結，不再從改過的原始碼重建。
- 沙箱文件的 CSP 不允許 inline script，所以 `sandbox.html` 只以 `<script src>` 載入檔案；`check:dist`（`tools/check-dist.ts`）確認 `dist/sandbox/` 正好是這七個檔案、兩份 HTML 沒有 inline script 或樣式，而且只引用同一目錄的檔案。
- d3 只收頁面用得到的模組（`d3-array`、`d3-axis`、`d3-format`、`d3-scale`、`d3-selection`、`d3-shape`、`d3-time`、`d3-time-format`），版本在 `frontend/package.json` 鎖定，它們自己的相依（`d3-color`、`d3-interpolate`、`d3-path`、`internmap`）以同一檔案的 `overrides` 鎖定，所以每月的 lock file 更新也不會移動它們；`renovate.json5` 不為這些套件開 PR。升級 d3 要開新的 runtime 版本。

## 頁面

頁面是一段 HTML（不含 `<html>`、`<head>`、`<body>`），最多 40 KiB，由主 agent 的 `dashboard` 工具寫入。runtime 收到 `init` 後：

1. 以 `DOMParser` 解析頁面，取出所有 `<script>`（只接受 HTML 命名空間、沒有 `src`、沒有 `type` 或 `type` 是空的或 `text/javascript`（不分大小寫、去掉前後空白）的 classic script；其他的 script 元素一律移除、不執行）。
2. 其餘內容（瀏覽器放進 `<head>` 的 `<style>` 等也一樣）依序放進 `#tq-root`。
3. 依出現順序以 `new Function("tq", "d3", 原始碼)` 執行每個 script，`this` 是 `undefined`，一律是 strict mode。沙箱文件的 CSP 是 `script-src 'self' 'unsafe-eval'`，所以只有這份文件能這樣做；為什麼用 `new Function` 而不是 `'unsafe-inline'`、nonce 或 hash，見 `crates/contract/src/api/dashboard/sandbox.rs` 的模組說明。
4. 任何 script 丟出例外，就送 `error`（`phase: "page"`，訊息見「錯誤」）並停止執行後面的 script。

頁面要在 script 裡以 `tq.on("data", render)` 註冊繪圖函式，每次資料到齊時以 `render(tq)` 呼叫；同一份頁面會被呼叫多次（重新整理），所以 `render` 要先清掉上一次畫的東西。資料到了之後才註冊（例如在計時器裡），會立刻以目前的資料畫一次。

頁面寫在 `#tq-root` 以外的東西一樣算數：比對讀的是整份文件。

## 頁面 API（`window.tq`）

| API | 作用 |
| --- | --- |
| `tq.on("data", render)` | 註冊繪圖函式；只接受一個，再註冊會取代：資料已經到了時，新的函式立刻以目前的資料再畫一次（同一代的 `returned` 與 `rendered` 會再送一次），再註冊同一個函式則不會。`render` 可以回傳 promise。`"data"` 以外的事件丟 `TypeError` |
| `tq.dataset(name)` | 資料集：`{name, title, description, source, status, columns, rows, rowCount, meta}`。`status` 是 `ok` 或 `error`（`error` 時 `rows` 是空的，`messageKey` 說明原因）。`rows` 是物件陣列，鍵是欄名，另有 `$row`（列號）；`time` 欄是 `Date`（見「時間」），其他照原樣。`meta` 是 `{queried_at, row_count, range_from, range_to}`，時間是 `Date` 或 `null`。沒有宣告的名稱丟 `Error` |
| `tq.value(ref)` | 參照的值：數字、文字、布林、`Date` 或 `null`（對不到資料時）。不是參照的字串丟 `TypeError` |
| `tq.text(el, ref, format?)` | 以 `textContent` 寫入 `tq.format(tq.value(ref), format)`，並在元素加上 `data-tq`（參照）與 `data-tq-format`（有給格式時）。回傳 `el` |
| `tq.mark(el, ref, {chart, channel})` | 把圖形標記連到它的值：`data-tq`、`data-tq-chart`（圖名，`^[a-z][a-z0-9_]{0,47}$`）、`data-tq-channel`（`width`、`height`、`x`、`y`、`radius`、`angle`）。不寫文字。回傳 `el` |
| `tq.axis(g, axis, {dataset, column, format?})` | 在 SVG `<g>` 上畫 d3 軸（`axis` 是 `d3.axisBottom(scale)` 等），以 `tq.format` 格式化刻度，並標記每個刻度的文字（見「參照」的 `t` 與 band 軸）。整個 `<g>` 加上 `data-tq-axis`（`資料集|欄`）。資料集沒有這一欄時丟 `Error`。回傳 `g` |
| `tq.decor(el)` | 宣告不代表資料的圖形（格線、圖示、折線），加上 `data-tq-decor`。回傳 `el` |
| `tq.format(value, format?)` | 格式化（見「格式化」）；拒絕的格式丟 `FormatError` |
| `tq.locale`、`tq.tz`、`tq.theme` | `init` 給的語言、時區與主題，唯讀。不提供檢查或檢視模式：頁面在兩種模式下的 realm 相同，比對只看畫出來的 DOM |

- `tq.text`、`tq.mark`、`tq.decor`、`tq.axis` 的 `el` 也可以是 d3 selection；`tq.text` 與 `tq.mark` 這時的 `ref` 是 `(datum, index) => 參照` 的函式，對每個元素各做一次。
- `tq` 物件是凍結的；`window.tq` 不能指派，頁面重新定義它也不影響 runtime 交給 script 與 `render` 的那一個。
- `data-tq*` 屬性只由 runtime 寫入；頁面自己寫的 `data-tq*` 屬性，比對時與 runtime 寫的同等看待，所以寫錯也會被抓到。
- runtime 在任何頁面 script 執行前就保存它要用的函式（計時器、`MutationObserver`、DOM 原型上的 getter 與方法），並凍結這個 realm 的 ECMAScript 內建物件（`src/harden.ts`：`Object`、`Array`、`String`、`Number`、`RegExp`、`Math`、`Set`、`Map`、`Date`、`Intl` 等的原型與這些命名空間，連同它們的方法），頁面之後改原型或全域物件，影響不了 runtime 讀到什麼、什麼時候回報，也影響不了檢視時的比對（「比對程式」）。凍結保留了合法頁面覆寫繼承屬性的寫法（`obj.toString = …`、`err.name = …`、子類別的方法、d3 畫圖）：覆寫只在頁面自己的物件上生效，改不了原型本身。頁面不能再做的事：換掉內建物件原型上的方法給整個 realm 用、在 `Math`、`JSON`、`Intl` 或建構子上新增或替換屬性、整批替換內建函式。

## 參照

元素的 `data-tq` 寫明它顯示的值從哪裡來。每個參照只有一種寫法（Rust：`names::Ref`；TypeScript：`parseReference`、`formatReference`），共用 `crates/contract/tests/fixtures/dashboard/reference-vectors.json`：

| 形式 | 值 |
| --- | --- |
| `c\|資料集\|欄\|列` | 一格；列是結果裡從 0 起算的索引，沒有前導零，小於 2000 |
| `f\|計算名` | 計算的值（「計算語言」） |
| `k\|常數名` | 常數的值（數字或文字） |
| `m\|資料集\|欄位` | 一次執行的事實：`queried_at`（查詢時間）、`row_count`（列數）、`range_from`、`range_to`（語意資料集讀的範圍；SQL 資料集沒有，對不到） |
| `t\|資料集\|欄\|值` | 連續軸的刻度：數字照 `String(number)` 的寫法（`1500`、`-2.5`、`1.5e-7`、`1e+21`），時間寫成 `YYYY-MM-DDThh:mm:ss.sss`（牆上時間，不帶時區） |

- 資料集名、計算名、常數名共用一組名稱：`^[a-z][a-z0-9_]{0,47}$`；欄名 `^[A-Za-z_][A-Za-z0-9_]{0,63}$`（SQL 的計算欄要以 `AS` 命名）。
- band 軸（`scaleBand`、`scalePoint`）的刻度是類別，標成 `c|資料集|欄|列`，列是第一個值等於該類別的列；沒有任何列等於的類別不標記（它的文字若有數字，就是標記以外的數字）。
- `t` 參照只能標在 `tq.axis` 軸裡的刻度文字上：其他地方的 `t` 參照可以寫任何值，一律判 `unmapped`。
- 帶數字的固定文字（產線名稱「Line 2」、標準編號）要宣告成文字常數，以 `k|名稱` 顯示：頁面上的數字一律要能追到資料或宣告過的常數。

## 格式化

`tq.format(value, format)` 與比對程式用同一份程式碼，所以顯示與檢查的字串相同。

- `null` 一律是「—」（U+2014）。
- 數字：d3-format，`format` 是 d3-format 的格式字串（例如 `,.1f`、`.0%`、`,.3~s`），依 `init.locale` 以 `zh-TW`（貨幣 `NT$`）或 `en`（貨幣 `$`）的 `d3.formatLocale` 定義格式化；沒有給時用 `,.2~f`。
- 時間：d3-time-format 的 `utcFormat`（例如 `%Y-%m-%d`、`%m/%d %H:%M`），依 locale（`zh-TW` 與 `en-US` 的定義隨 d3-time-format 4.1.0）；沒有給時，時分秒都是 0 用 `%Y-%m-%d`，否則 `%Y-%m-%d %H:%M`。無效的 `Date` 是「—」。
- 文字照原樣，`format` 不作用。
- 布林：`zh-TW` 是「是」與「否」，`en` 是 `Yes` 與 `No`。
- 會自己加上數字的格式一律拒絕（`FormatError`；比對時這樣的 `data-tq-format` 讓標記判 `mismatch`）：以數字填補（只有 `0` 旗標，也就是填 `0` 且對齊 `=`，可以）、寬度超過 32、格式超過 64 字元、不是 d3-format 的格式（含 d3 會當成 `g` 的未知型別字母）；時間格式只接受 `%a %A %b %B %c %d %e %f %g %G %H %I %j %L %m %M %p %q %Q %s %S %u %U %V %w %W %x %X %y %Y %%`（可加 `-`、`_`、`0`），不接受 `%Z`（固定印出 `+0000`）與格式裡自己的數字。

### 時間

結果裡的時間有三種寫法：`YYYY-MM-DD`、`YYYY-MM-DD hh:mm:ss[.f…]`（SQL，查詢指定的時區）與帶時差的 RFC 3339（語意查詢，請求的時區）。runtime 一律保留寫出來的日期與時刻、忽略時差，轉成 UTC 欄位等於該牆上時間的 `Date`（毫秒以下捨去），所以頁面要用 `d3.scaleUtc` 與 `utcFormat`，看到的就是查詢當地的時間。`m|資料集|queried_at`、`range_from`、`range_to` 是時間點，先換成 `init.tz` 的牆上時間（瀏覽器不認得的時區以 UTC 代替）再照同樣方式處理。

## 計算語言

計算（`spec` 的 `computations`）以一個只產生純量的公式算出一個數字。Rust 在 `dashboard` 工具寫入時解析並驗證（`crates/contract/src/api/dashboard/formula.rs`），runtime 與比對程式以 TypeScript（`src/formula.ts`）解析並求值；兩邊跑同一份 `crates/contract/tests/fixtures/dashboard/formula-vectors.json`（每個案例：公式、標準樹或錯誤類別與位置、值）。

```text
formula   = expr
expr      = term { ("+" | "-") term }            左結合
term      = unary { ("*" | "/") unary }          左結合
unary     = "-" unary | primary
primary   = number | "(" expr ")" | aggregate "(" column ")" | column "[" index "]"
aggregate = "sum" | "avg" | "min" | "max" | "count" | "first" | "last"
column    = dataset "." name
index     = digits                               小於資料集的 max_rows（最多 2000）
number    = digits [ "." digits ] [ ("e" | "E") [ "+" | "-" ] digits ]
```

- token 之間只能有空白、tab、CR、LF；聚合函式只有小寫；`1.` 與 `1e999` 是無效的數字；欄一定要聚合或加索引。
- 上限：512 位元組、64 個節點（數字、欄與運算子）、32 層括號與負號。
- 值都是 IEEE 754 雙精度或 `null`：
  - `資料集.欄[i]`：第 i 列的格；沒有這一列或格是 `null` 時是 `null`。
  - `sum`：依列的順序從 0 加總非 `null` 的格；都沒有時是 `null`。`avg` 是這個和除以個數。
  - `min`、`max`：依列的順序第一個最小、最大的非 `null` 格；都沒有時是 `null`。
  - `count`：非 `null` 格的個數，任何型別都可以；空欄是 0。
  - `first`、`last`：第一列、最後一列的格；沒有列或格是 `null` 時是 `null`。
  - 運算元有 `null` 就是 `null`；結果不是有限數（除以 0、溢位、`0/0`）時是 `null`。
- `count` 以外的聚合與索引只接受 `number` 欄。
- 錯誤類別（封閉）：`empty`、`too_long`、`unexpected_character`、`unexpected_token`、`unexpected_end`、`unknown_function`、`invalid_name`、`bare_column`、`invalid_number`、`index_out_of_range`、`too_complex`、`unknown_dataset`、`unknown_column`、`column_not_numeric`；位置是從 1 起算的字元位置（Unicode code point）。

## 畫完

runtime 收到 `data` 後呼叫 `render(tq)`：

1. `render` 回傳（回傳 promise 時它 fulfilled），runtime 送 `returned {generation}`；丟出例外或 promise reject 時送 `error {phase: "render"}`，不送 `returned`。
2. 接著兩次 `requestAnimationFrame`；
3. 整份文件連續 200 ms 沒有 DOM 變動（`MutationObserver`），runtime 送 `rendered {generation}` 與 `resize`。

- 檢查模式（`mode: "check"`）：從開始呼叫 `render` 起 5 秒內沒有靜止，就不送 `rendered`。檢查服務也不採信 `rendered`，會在 isolated world 自己確認靜止（「檢查服務的流程」）。
- 檢視模式（`mode: "view"`）：沒有時限（背景分頁的 `requestAnimationFrame` 會暫停）；送 `rendered` 前先做檢視時的比對（「比對程式」）。
- 沒有 `tq.done()`：畫完由 runtime 判斷，不由頁面宣告，頁面也就不能提早宣告。上限與時間在 `contract::api::dashboard::check`（`RENDER_TIME_LIMIT`、`QUIET_PERIOD`）與 `protocol.ts`。
- 新的 `generation` 開始後，舊的那一代不再送任何訊息。

### 錯誤

- runtime 自己接住的例外（頁面 script 載入時與 `render` 同步丟出或 reject 的）以 `error` 回報，`message` 是 `名稱: 訊息`，控制字元與不可見字元換成空白、去掉前後空白、最多 200 字元，沒有可見文字時省略。
- 非同步的例外（計時器、事件處理、沒有處理的 rejection）runtime 接不住。沙箱是 opaque origin，`runtime.js` 也不以 CORS 載入，瀏覽器的 `error` 事件只看得到「Script error.」，所以：檢視模式送不帶 `message` 的 `error {phase: "render"}`；檢查模式不送，檢查服務從 CDP 的 `Runtime.exceptionThrown` 取得完整訊息（自己接住的例外不會出現在那裡，要以 host 頁回報的 `error` 為準）。

## postMessage 協定

每則訊息都是 `{tq: 1, type, ...}`，型別與欄位在 `src/protocol.ts`，一律以 strict 解析（多的欄位就不是協定的訊息）。

| 方向 | `type` | 欄位 | 時機 |
| --- | --- | --- | --- |
| host → 沙箱 | `init` | `runtime`、`page`、`spec`（公開部分）、`locale`、`tz`、`theme`、`mode` | 收到 `ready` 後一次 |
| host → 沙箱 | `data` | `generation`（從 1 起遞增）、`datasets`：每個資料集 `{status: "ok", result}` 或 `{status: "error", dataset, message_key}` | 全部資料集都有結果或錯誤時；重新整理時以新的 `generation` 再送 |
| 沙箱 → host | `ready` | `runtime` | runtime 載入後 |
| 沙箱 → host | `returned` | `generation` | `render` 回傳（「畫完」第 1 步） |
| 沙箱 → host | `rendered` | `generation` | 「畫完」 |
| 沙箱 → host | `error` | `phase`（`page` 或 `render`）、`message`（最多 200 字元，可省略） | script 或 `render` 失敗（「錯誤」） |
| 沙箱 → host | `resize` | `height`（整數，0 到 20000） | 畫完後與內容高度改變時（`ResizeObserver`） |
| 沙箱 → host | `source` | `reference` | 檢視模式下使用者點了對得到資料的標記 |

- runtime 只在 opaque origin（`origin === "null"`）而且有上層視窗的 frame 裡啟動，直接打開 `runtime.js` 不做任何事。它只接受 `isTrusted`、而且 `source` 是 `window.parent` 的訊息（兩者都以頁面執行前保存的 getter 讀，頁面自己 dispatch 的 `MessageEvent` 不算）；`init` 只接受一次，之後只接受 `generation` 比目前大、而且每個宣告的資料集正好一筆的 `data`。
- host 端只接受 `event.source` 是該 iframe 的 `contentWindow`、`event.origin` 是 `"null"` 的訊息，並以 `parseSandboxMessage` 嚴格解析：頁面和 runtime 在同一個 realm，這些訊息頁面也送得出來，所以一律視為不可信。`error` 的 `message` 只當作頁面自己的文字顯示；`source` 的參照要對得到目前的資料才用；`resize` 限制在範圍內。host 從不執行沙箱送來的任何內容。
- 第二次 `ready` 或 iframe 第二次 `load` 代表 frame 重新載入或導向了：host adapter 呼叫 `onNavigate`，之後不再送出或轉交任何訊息。
- host 以 `postMessage(message, "*")` 送出：沙箱是 opaque origin，沒有可以指定的 origin。送出的內容只有頁面本來就看得到的東西（頁面、spec 的公開部分、這位使用者能看的資料）。
- host 給沙箱的 `spec` 是 `publicSpec(spec)`：資料集沒有 SQL 與語意請求；`SandboxHost.init` 以 `InitMessageSchema` 解析，多的欄位會被去掉。

### host adapter（`src/host.ts`）

`new SandboxHost({container, runtime, base, title, onMessage, onNavigate})` 在 `container` 裡建立 `sandbox="allow-scripts"`、`referrerpolicy="no-referrer"` 的 iframe（`sandbox` 屬性在 `src` 之前設定），載入 `<base>/v<runtime>/sandbox.html`；收到 `ready` 後送出 `init(...)` 給的內容，之後每次 `data(datasets)` 都是新的一代（回傳 `generation`）。`onMessage` 收到每則通過檢查的訊息；`generation` 比已送出的還大的 `returned`、`rendered` 不轉交。`destroy()` 移除 frame 並停止監聽。SPA（Laysi/trustiot-v2#462）以 `base: "/sandbox"` 使用它；檢查服務的 host 頁以 `base: "."`。

## 檢查規則

比對程式與檢查服務依下列規則判定，每條規則對應判定的一個問題類別（`contract::api::dashboard::check::Problem`；每個類別有固定的英文提示，`ProblemKind::hint`）。比對讀整份文件（不只 `#tq-root`），`<style>` 與 `<script>` 的文字不算。

| 規則 | 不符時的類別 |
| --- | --- |
| 沙箱回報 `ready`，頁面註冊了 `render`，`render` 在 5 秒內回傳 | `not_rendered` |
| 回傳後在時限內畫完（靜止 200 ms）；整個檢查在 15 秒內；沒有 SVG 動畫元素（`animate`、`animateMotion`、`animateTransform`、`set`、`discard`），也沒有還會改變畫面的 CSS 動畫、轉場或 Web Animation（`document.getAnimations()` 裡 `playState` 是 `running` 或 `paused`，或 `pending` 的） | `timeout`（`phase`：`render` 或 `check`） |
| 沒有任何 script 例外或未處理的 rejection（含非同步的） | `script_error`（訊息見「錯誤」，最多 200 字元，移除控制與不可見字元） |
| 沙箱 frame 沒有導向、沒有開新視窗、沒有送出表單 | `navigated` |
| 沙箱除了自己 runtime 版本的檔案之外，沒有請求任何檔案（CSP 的 `'self'` 允許同一主機的 script 與樣式表；檢視時這種請求會從使用者的瀏覽器送到平台的主機） | `requested` |
| 每個 `data-tq` 都是合法參照，而且對得到資料：資料集、欄、列、計算、常數都存在；`t` 參照只在 `tq.axis` 軸裡的文字標記上；圖形標記有合法的 `data-tq-chart` 與 `data-tq-channel` | `unmapped`（參照的名稱都是宣告過的才回傳參照，否則只回元素的標籤） |
| 文字標記（有 `data-tq`、沒有 `data-tq-chart` 與 `data-tq-channel` 的元素）的 `textContent` 等於 `tq.format(值, data-tq-format)`；格式被拒絕時一律不相等。而且畫出來的文字要等於 `textContent`：標記是單一文字節點（沒有子元素，`tq.text` 寫的就是這樣），整段照順序、正向顯示——沒有被藏起來（`display: none`、`visibility: hidden`，或畫得太淡：文字顏色的 alpha 乘上標記本身與每一層祖先的 `opacity` 低於 `MIN_TEXT_ALPHA`（0.1）；顏色取 `color`、`-webkit-text-fill-color` 與 SVG 文字的 `fill`（乘上 `fill-opacity`）裡最淡的，`filter` 裡的 `opacity()` 也乘進去；瀏覽器序列化顏色的每種寫法都讀得出 alpha，包括 `color()`、`lab()`、`lch()`、`oklab()`、`oklch()`；讀不懂的顏色、漸層這類不是單一顏色的填色都當成透明）、沒有被變形成別的樣子（標記與祖先的 `transform`、`rotate`、`scale`、`zoom` 合起來，SVG 元素以 `getScreenCTM` 計算，含 HTML 祖先與 viewBox：不能鏡像、不能轉超過 `MAX_TEXT_TURN_DEG`（45°），也不能在一個方向比另一個方向拉長超過 `MAX_TEXT_STRETCH`（4）倍；平移與等比縮放可以，但畫出來的字級（字級乘上縮放）至少 `MIN_TEXT_SIZE_PX`（2 px）；有 `perspective`、`transform-style: preserve-3d`，或變形讀不懂時一律不符）、SVG 文字不逐字擺放（標記與它所屬的 `text`、`tspan` 的 `x`、`y`、`dx`、`dy` 都只有一個值，沒有 `rotate` 與 `textLength`，不在 `textPath` 上）、字距不讓數字疊在一起（`letter-spacing` 不小於 `MIN_LETTER_SPACING_EM`，−0.2 em）、沒有 bidi 重排（`unicode-bidi` 的 override 或 `plaintext`）、`::first-letter` 與 `::first-line` 沒有被改樣式（字級、顯示、淡到看不見、變形、字距）、方框有面積（不是 `font-size: 0` 的零大小）、沒有被有裁切的 `overflow` 方框裁掉（內容超出方框，刪節號也算）。方框大小、裁切與 SVG 的 `getScreenCTM` 需要版面；有沒有版面由呼叫端說明（檢查服務與 runtime 都在瀏覽器裡，一律有版面；單元測試的 DOM 沒有），從不由頁面自己能改樣式的 `<html>` 方框推斷。`display: contents` 這種沒有方框但文字照樣顯示的標記不算被藏 | `mismatch`（回傳參照與資料裡的值） |
| 有版面時，文件根元素（`<html>`）的方框有面積：沙箱自己的樣式讓它一定有（frame 的寬度、`#tq-root` 的 padding），沒有面積代表頁面藏起了標記實際畫出的樣子（例如把根元素縮成零而內容照樣溢出顯示） | 每個標記都不符：文字標記 `mismatch`，圖形標記 `mark_geometry` |
| 所有 SVG 圖形（`rect`、`circle`、`ellipse`、`line`、`path`、`polygon`、`polyline`、`use`）都在圖形標記、`tq.axis` 或 `tq.decor` 之下 | `unmapped`（沒有參照，回傳標籤） |
| 標記以外沒有數字：不在文字標記裡的文字節點（圖形標記裡的文字也算）、每個元素上會顯示或朗讀的屬性（`title`、`alt`、`aria-label`、`aria-description`、`aria-valuetext`、`aria-valuenow`、`aria-valuemin`、`aria-valuemax`、`aria-roledescription`、`aria-placeholder`、`placeholder`、`value`、`label`）、`::before`、`::after`、`::marker` 的 `content`（`open-quote`、`close-quote` 看 `quotes`），以及有編號的清單項目（`display: list-item` 而 `list-style-type` 不是 `none`、`disc`、`circle`、`square`、`disclosure-*`；文字的 `list-style-type` 看文字），都沒有十進位數字（`\p{Nd}`，含全形） | `literal_number`（回傳位置與第一段數字，只含數字、`.` 與 `,`，最多 32 字元；清單項目回傳它在兄弟元素裡的序號） |
| 同一張圖（`data-tq-chart`）同一個 `channel` 的標記：`width`、`height`、`radius`（寬的一半）依 `getBoundingClientRect` 與值的絕對值成正比（比例是各標記「大小 / 值」的中位數，誤差 1% 或 1 px 內；值為 0 或 `null` 時不超過 1 px；最大的標記不到 1 px 時，所有有值的標記都不符）；`x`、`y` 是值的一次函數（以方框中心為位置，Theil–Sen 直線，誤差為位置範圍的 1% 或 1 px；三個以上的標記才判定；時間以毫秒計）；`angle` 只用於 `d3.arc` 畫的 `path`（沒有 `padAngle`、`cornerRadius`），以弧的角度與值成正比（誤差 1% 或外緣 1 px 的弧長），而且畫面上是等比的：路徑在螢幕上的變形在任一方向的拉長不超過另一方向的 `MAX_SLICE_STRETCH`（1.01）倍（鏡像與旋轉不改變角度，可以）；值不是數字（或時間）的標記一律不符 | `mark_geometry`（回傳參照與 `channel`） |
| 連續軸的每個刻度落在該欄可能的軸範圍內：數字是所有值的範圍（全部相同時是 `[值, 值+1]`，沒有值時是 `[0, 1]`）再擴大到包含 0，時間是所有值的範圍（只有一個時前後各加一天），再取 d3 `nice` 在刻度數 1 到 20 之下的聯集；同一軸三個以上的刻度，位置（刻度 `<g>` 的原點，依 `getScreenCTM`）是刻度值的一次函數 | `mark_geometry`（`channel: "axis"`） |
| 沒有 `canvas`、`img`、`picture`、SVG `image`、`video`、`audio`、`iframe`、`frame`、`object`、`embed`、`input`、`textarea`、`select`、shadow root（開放的由比對程式找，封閉的由檢查服務以 CDP 找），`content` 只有加引號的文字與 `normal`、`none`、`*-quote`（沒有 `counter()`、`attr()`、`url()`） | `unsupported_content` |
| 最多 20,000 個元素、10,000 個標記（有 `data-tq` 的元素），在其他規則之前先數 | `too_large`（超過時只回這一個問題） |

- 清單的編號只有符號（`none`、`disc`、`circle`、`square`、`disclosure-*`）不算數字；字母與羅馬數字等其他編號也算：計數器樣式超出範圍時瀏覽器改用十進位（例如 `lower-roman` 超過 3999、`lower-alpha` 在 0 以下），頁面能以 `start`、`value`、`reversed` 與 `counter-reset`、`counter-set`、`counter-increment` 在任何地方改變 `list-item` 計數器，`@counter-style` 也能定義任何符號，而 DOM 沒有讀出編號實際文字的 API。
- 判定通過時回傳報告：每個不同的參照與它在資料裡的值，依文件順序。assistant 以 `CheckVerdict::validate` 確認報告與問題裡的每個參照、值與標籤都來自它送出的資料，才交給模型。
- 問題最多 20 個，依發現順序（先文件順序的各規則，再幾何與動畫），完全相同的問題只算一次，超過就截斷。
- 這個檢查是 J5「數字只來自查詢」的品質關卡，不是安全邊界；安全邊界是沙箱與它的 header。

### 檢查不到的部分

- 折線與面積的路徑（`path`）只能標成 `tq.decor`，形狀不檢查；要檢查的值另外以點（`tq.mark` 的 `x`、`y`）標記。`tq.decor` 本身也不檢查：畫成數字形狀的裝飾、被宣告成裝飾的長條都看不出來。
- 以 HTML 元素的寬高畫的長條沒有標記時看不出來；圖形一律用 SVG 並以 `tq.mark` 標記。
- 用文字寫數字（「一百二十」）、`\p{Nd}` 以外的數字字元（上標、圈號數字、羅馬數字），以及依時間或環境改變內容的頁面，檢查擋不住；所以檢視時前端再比對一次（Laysi/trustiot-v2#462）。
- 只影響畫面不影響 DOM 的手法：文字標記的 bidi 重排、裁切、變形、逐字擺放、字距、`::first-letter`/`::first-line` 樣式、隱藏與透明度（含祖先的 `opacity`）已經會判 `mismatch`（見上），但下列仍檢查不到：被別的元素蓋住（整個或部分數字被上層的元素、偽元素或圖形遮住）；祖先元素造成的裁切或 bidi 重排；`clip-path`、`mask`；`filter` 裡 `opacity()` 以外的效果與 `mix-blend-mode`；與背景同色或對比不足；字型本身的字形；只在 `:hover` 出現的樣式；清單以外的屬性（例如其他 ARIA 屬性）。
- 只在檢視時才發出的同主機請求（`requested` 那一列）檢查看不到。這是已接受的殘留（擁有者 2026-10-27）：沙箱是 opaque origin，沙箱文件的 CSP 用 `script-src 'self'` 與 `style-src 'self'` 讓 runtime 自己的檔案載得進來，連帶讓顯示中的頁面能對自己這個主機發出 GET（當成 script 或 style 載入）。頁面讀不到回應（只當 script 或 style 載入，不是資料）；因為來源是 opaque，瀏覽器把它當成別的 site，不送平台的 `SameSite=Lax` session cookie（2026-10-26 在 Chromium 實測，見 `crates/contract/src/api/dashboard/sandbox.rs` 的模組說明）；CSP 把這些請求限制在同一個主機（含轉址）。發布前的檢查會拒絕發出這種請求的頁面（`requested`）。要完全關掉得把 runtime 以雜湊式 CSP 內嵌，目前不做。
- 沙箱還沒處理的外送缺口在 Laysi/trustiot-v2#699。

## 比對程式

同一份實作（`src/compare.ts`）有兩個入口：

- **檢查（`compare.js`）**：定義 `globalThis.tqCompare(input)`，同步回傳判定。輸入是 `CompareInput`（`protocol.ts`）：`runtime`、`spec`（公開部分）、`datasets`（結果）、`locale`、`tz`，以 zod 解析（解析失敗丟出例外）。輸入一律由呼叫端提供，從不讀頁面或 runtime 的狀態；比對程式自己解析參照、計算公式、格式化。輸出是判定（`CheckVerdict`）：通過時 `{verdict: "passed", report: {marks}}`，否則 `{verdict: "failed", problems}`。CDP 才看得到的問題（`not_rendered`、`script_error`、`navigated`、`requested`、封閉的 shadow root、`timeout` 的時限）由檢查服務補上。檢查服務以 CDP 在沙箱 frame 建立 isolated world 後注入並呼叫它：isolated world 的全域物件與原型是自己的，頁面改寫 DOM 原型或全域物件騙不到它。
- **檢視（runtime 內建）**：runtime 在頁面的 script 執行**之前**保存原始的 DOM 函式並凍結這個 realm 的內建物件（`harden`，見「頁面 API」），每次畫完後在頁面的 realm 比對：對不到資料或不相符的文字標記換成「—」並加上 `data-tq-flag`，不相符的圖形標記加上 `data-tq-flag`，標記以外的數字換成「—」，`::before`、`::after`、`::marker` 有問題的元素加上 `data-tq-flag-pseudo`（關掉產生的內容），不支援的內容加上 `data-tq-flag-hide`（隱藏），超過上限時整份文件隱藏；樣式在 `sandbox.css`。每次比對先清掉上一次的標示（之後畫對了就不再標示）。runtime 一律當成有版面比對，所以 frame 沒有版面時（例如 host 以 `display: none` 藏起 frame）每個標記都判不符；host 要暫時藏起 frame 時用 `visibility: hidden`。
  - **保證**：runtime 載入之後，頁面的 script 不論做什麼，都改不了這次比對的結果，唯一的例外是改 DOM，而改了的 DOM 比對下一次就看得到。比對與它呼叫的每一段（文字、格式化、時間、幾何、內建的 d3-format）都只讀凍結後的內建物件，所以頁面換掉 `Intl`、`Math`、`Number`、`String`、`RegExp`、`Array`、`Set`、`Map` 或它們原型上的方法都沒有用（`harden` 擋下），頁面自己的 `tq.text` 與比對因此一定算出同一個、正確的字串。
  - **持續改動有上限**：畫完後的 DOM 變動（tooltip、計時器）靜止後再比對一次；頁面若一直改個不停，從第一次改動起最多 `MAX_RECHECK_DELAY_MS`（1000 ms）內一定再比對一次，不會被無限延後。一直把 runtime 改正的值又改回錯的，連續 `MAX_CORRECTION_ROUNDS`（10）次之後整份文件隱藏，不再比對（fail closed）；一直以正確的值更新的頁面則沒有東西要改正，會一直正常顯示。
  - 比對本身丟出例外時（理論上頁面讓它用到的東西壞掉），整份文件從此隱藏，也不再比對。這個結果只用來顯示，從不當作判定：判定一律以檢查服務在 isolated world 的比對為準（「威脅模型」第 5 點）。

## 檢查服務的流程

`services/dashboard-check` 收到 `CheckRequest`（`contract::api::dashboard::check`）後要依下列順序處理，細節由該服務決定：

1. 驗證請求（`CheckRequest::validate`），runtime 版本要是建置裡有的。
2. 啟動新的 `chromium-headless-shell` 行程（`--remote-debugging-pipe`、`--no-sandbox`、全新的 profile），一次只做一個檢查。
3. 以 CDP `Fetch` 攔截所有請求，從映像裡的前端建置結果供給 `SandboxFile` 的檔案與 `/sandbox/check.html`、`/sandbox/check.js`，header 用 `SANDBOX_DOCUMENT_HEADERS`、`SANDBOX_ASSET_HEADERS`、`CHECK_HOST_HEADERS`；來源用不會解析的固定 origin（例如 `https://check.invalid`）。其他請求一律拒絕；沙箱 frame 的文件請求只要不是預期的那一個，就判 `navigated`，沙箱 frame 的其他請求只要不是該 runtime 版本的檔案，就判 `requested`（判定裡不放網址）。
4. 打開 host 頁，以 `Runtime.addBinding` 加上 `tqCheckReport`，再以 `Runtime.callFunctionOn` 或 `Runtime.evaluate` 呼叫 `tqCheckHost.start(request)`（`CheckRequest` 整個，含 `page`；不合格的請求丟出例外）。host 頁在可信的那一層，頁面碰不到；它建立 1200×800 的沙箱 frame（依 `resize` 長高，最多 20000），收到 `ready` 後送 `init`（`theme: "light"`、`mode: "check"`）與第 1 代 `data`，並以 `tqCheckReport(JSON 字串)` 回報，每一種最多一次：`{"type":"ready"}`、`{"type":"returned"}`（第 1 代的 `render` 回傳）、`{"type":"rendered"}`（第 1 代畫完）、`{"type":"error","message":…}`（第一個錯誤；`message` 已依「錯誤」處理，可省略）。host 頁不回報導向；導向與請求由 CDP 看。
5. 收集沙箱 frame 的 `Runtime.exceptionThrown`（`script_error`）、`Page.frameNavigated` 與視窗開啟（`navigated`），以 `DOM.getDocument`（`pierce: true`）找 shadow root。
6. 等 `rendered` 或時限：送出資料後 5 秒內沒有 `returned` 是 `not_rendered`；有 `returned` 而沒有畫完（沒有 `rendered`，或自己確認不到靜止）是 `timeout`（`phase: "render"`）。這些回報都經過沙箱，頁面偽造得出來，所以只用來分類失敗，畫完一律以 isolated world 自己的確認為準。之後在沙箱 frame 以 `Page.createIsolatedWorld` 建立 isolated world，先確認 200 ms 靜止，再注入 `compare.js` 並以請求的 `CompareInput` 部分呼叫 `tqCompare`。
7. 合併問題（最多 20 個），以 Rust 型別解析並 `validate`，回傳判定；關閉瀏覽器並刪掉 profile。
8. 日誌只記類別與耗時，不記頁面、資料或訊息。

## 固定樣本

`fixtures/` 是帶預期判定的頁面，給前端的單元測試、檢查服務的映像測試（在瀏覽器裡跑全部）與 assistant 的驗收測試共用：

- `request.json`：共用的 `CheckRequest`，少了 `page`；三個資料集（`daily_energy`、`hourly_power` 是 SQL，`meter_usage` 是語意用量）、三個計算、兩個常數，`locale` 是 `zh-TW`、`tz` 是 `Asia/Taipei`。
- `pages/<名稱>.html`：每個案例一頁。
- `cases.json`：`{request, cases: [{name, page, unit, about, expect}]}`。一個案例的請求是 `request.json` 加上 `page` 檔案的內容。`unit` 為 `true` 的案例由前端的單元測試在 happy-dom 裡跑（`test/fixtures.test.ts`）；`false` 的只有瀏覽器看得出來，`about` 說明原因。
- `expect` 是 `{verdict: "passed", marks}` 或 `{verdict: "failed", problems, may_also?}`。比對方式在 `fixtures/expect.ts` 的 `matchesExpectation`：通過的案例要有完全相同的 `marks`（順序也相同）；失敗的案例要有每個預期的問題（列出的欄位與值都相同，沒列出的欄位，例如 `script_error` 的 `message`，不比），而且沒有預期與 `may_also` 以外類別的問題。
- 案例涵蓋 design B 第 3.4 節（正確的頁面、寫死的數字、錯列、錯的長條高度、對不到資料的標記、同步與非同步的 script 錯誤、導向、改寫原型），以及每條規則各一頁。送出表單與開新視窗的頁面預期**通過**：沙箱沒有 `allow-forms`、`allow-popups`，什麼都不會發生；瀏覽器裡判成 `navigated` 代表 frame 少了 `sandbox` 屬性。

只有瀏覽器測試涵蓋的部分：真實的版面（`getBoundingClientRect`、`getScreenCTM`；單元測試從 SVG 屬性推算位置）、`::before`、`::after`、`::marker` 的 `content`、清單的預設樣式、CSS 動畫、封閉的 shadow root、非同步的 script 錯誤、導向、請求、isolated world 本身、頁面替換 realm 的內建物件（`builtin-tampering`；`harden` 在真實的 realm 凍結它們，共用的測試 realm 不能被替換）、文字標記需要版面才看得出的遮蔽（`text-mark-first-letter`、`text-mark-first-letter-color` 的 `::first-letter`、`text-mark-clipped` 與 `text-mark-ellipsis` 的裁切、`text-mark-font-size-zero` 的零大小方框、`text-mark-root-zero` 把根元素的方框縮成零、`text-mark-faint` 的各種顏色寫法與祖先透明度、`text-mark-turned` 與 `wrong-pie-stretched` 需要 `getScreenCTM` 的變形；結構、`display`、`visibility`、透明色、`unicode-bidi` 的樣本與 `slanted-labels` 在單元測試就能判，變形與顏色的各條規則另有 `test/compare.test.ts` 的單元測試），以及 SVG 裡的 `<script>`（happy-dom 解析不出來）。

## 測試

- `bun run --cwd frontend test` 跑：
  - `src/protocol.test.ts`：讀 `crates/contract/tests/fixtures/dashboard/` 的固定樣本（由 Rust 型別寫出），確認每個都能解析且不遺失欄位、`publicSpec` 與 Rust 的 `Spec::public` 相同、參照的測試向量答案一致、判定與沙箱訊息拒絕封閉集合以外的內容。
  - `src/formula.test.ts`：共用的計算語言測試向量，以及位置、長度與求值的細節。
  - `test/runtime.test.ts`：頁面的載入、畫完的訊息、頁面 API、檢視模式的比對與標示，包括一直改回錯值的頁面會在上限內被隱藏、一直以正確值更新的頁面會繼續顯示。
  - `test/harden.test.ts`：在 `node:vm` 的獨立 realm 裡凍結內建物件（像瀏覽器的 frame 一樣有自己的內建物件），確認在呼叫時才查內建的比對不受頁面替換影響、覆寫繼承屬性的合法寫法仍可用、全域物件仍可擴充、重複呼叫安全，以及凍結後 d3 仍能畫比例尺、nice、扇形與格式化。
  - `test/format.test.ts`：數字、時間、布林與 `null` 的格式化，以及被拒絕的格式。
  - `test/time.test.ts`：結果裡的時間、時間點換成檢視者時區、刻度參照的時間文字。
  - `test/values.test.ts`：參照的值與頁面看到的值；Rust 寫出的 `verdict-passed.json` 每個標記的值都等於從 `check-request.json` 解析出的值。
  - `test/geometry.test.ts`：比例、直線、軸範圍與 `d3.arc` 扇形角度的計算。
  - `test/compare.test.ts`：比對的每條規則（計算樣式與動畫用替身）、判定的去重與上限、文字與 CSS `content` 的讀法。
  - `test/source.test.ts`：沙箱原始碼不用 `innerHTML`、`eval`、網路 API 與導向，只在 runtime 一處把字串變成程式（頁面 script），只 import 鎖定版本的 d3 模組。
  - `test/build.test.ts`：建置正好寫出 `SandboxFile` 的檔案與 host 頁，每個 script 都是獨立的 classic script，沒有 source map 與網路 API。
  - `test/dom.test.ts`：事先保存的 DOM 函式不受頁面之後的改寫影響、動畫的計數。
  - `test/host.test.ts`：host adapter 只接受自己 frame、opaque origin、格式正確的訊息，`ready` 後才送 `init` 與 `data`，第二次 `ready` 或 `load` 視為導向。
  - `test/check-host.test.ts`：`tqCheckHost.start` 拒絕不合格的請求、只執行一次，回報的種類、順序與訊息處理，frame 的大小。
  - `test/fixtures.test.ts`：固定樣本的檔案與預期判定都合格，`unit` 案例在 happy-dom 裡得到預期的判定。
- `bun run --cwd frontend typecheck` 涵蓋 `sandbox/`。
- happy-dom 的視窗各有自己的 JavaScript realm，但 Node、Element 等基底類別的原型在視窗之間共用：`test/harness.ts` 在每個 frame 關閉時還原頁面改過的原型。
