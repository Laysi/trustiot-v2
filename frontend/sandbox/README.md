# frontend/sandbox

儀表板頁面的沙箱 runtime（Laysi/trustiot-v2#150；檢查單 J4，決策 33）：AI 寫的頁面只在這裡執行。同一份建置結果有兩個使用者：

- **檢查服務**（`services/dashboard-check`）：發布前在無頭 Chromium 執行頁面，從 isolated world 比對每個標記與資料，回傳判定。
- **前端的儀表板畫面**（Laysi/trustiot-v2#462）：使用者檢視時在同一個沙箱執行頁面，顯示前再比對一次。

決定與理由在 [AI 助理與 AI 閘道](../../docs/architecture/ai-assistant.md)「儀表板」。Rust 端的型別、上限與 header 在 `crates/contract/src/api/dashboard/`（說明見 [crates/contract](../../crates/contract/README.md)「儀表板」）；本文件是 runtime（`window.tq`）、postMessage 協定、檢查規則與檢查流程的契約。

目前只有協定的型別（`src/protocol.ts`）與它的測試；runtime、計算語言的求值、比對程式、host adapter 與建置入口隨 Laysi/trustiot-v2#150 的其餘部分加入。

## 建置結果

| 網址路徑（`frontend/dist` 下同一路徑） | 內容 | 依 runtime 版本 |
| --- | --- | --- |
| `/sandbox/v1/sandbox.html` | 沙箱文件：載入下面三個檔案，`<body>` 只有 `<div id="tq-root"></div>` | 是 |
| `/sandbox/v1/runtime.js` | runtime：`window.tq`、計算語言、postMessage 的沙箱端 | 是 |
| `/sandbox/v1/d3.js` | d3，`window.d3`，版本隨 runtime 凍結 | 是 |
| `/sandbox/v1/sandbox.css` | runtime 的樣式（版面重設、主題變數） | 是 |
| `/sandbox/v1/compare.js` | 比對程式：定義 `globalThis.tqCompare`，自帶 d3-format 與 d3-time-format 及計算語言，不依賴頁面的 `window` | 是 |
| `/sandbox/check.html`、`/sandbox/check.js` | 檢查服務的 host 頁：建立沙箱 iframe、送資料、回報狀態給驅動程式 | 否 |

- 路徑與每個檔案的 header 由 `contract::api::dashboard::sandbox::SandboxFile` 定義（`path`、`content_type`、`headers`），檢查服務與 `services/web` 照它送出，不各自定義。
- host adapter（postMessage 的外層）是 TypeScript 模組，SPA 與 `check.js` 各自打包進去，不是獨立檔案。
- 每個 runtime 版本的檔案一旦發布就不再改變行為：已檢查過的儀表板版本永遠以它的 runtime 執行。改 `window.tq`、計算語言、格式化或檢查規則的行為要開新的版本目錄（`/sandbox/v2/`），並在 `RuntimeVersion::SHIPPED` 與 `protocol.ts` 的 `RUNTIMES` 加上它；只有安全修正可以改舊版本的檔案。
- 沙箱文件的 CSP 不允許 inline script，所以 `sandbox.html` 只以 `<script src>` 載入檔案。

## 頁面

頁面是一段 HTML（不含 `<html>`、`<head>`、`<body>`），最多 40 KiB，由主 agent 的 `dashboard` 工具寫入。runtime 收到 `init` 後：

1. 以 `DOMParser` 解析頁面，取出所有 `<script>`（只接受沒有 `type` 或 `type="text/javascript"`、沒有 `src` 的 classic script；其他的 script 元素一律移除）。
2. 其餘內容放進 `#tq-root`。
3. 依出現順序以 `new Function("tq", "d3", 原始碼)` 執行每個 script，`this` 是 `undefined`。為什麼用 `new Function` 而不是 `'unsafe-inline'`、nonce 或 hash，見 `crates/contract/src/api/dashboard/sandbox.rs` 的模組說明。
4. 任何 script 丟出例外，就送 `error`（`phase: "page"`）並停止執行後面的 script。

頁面要在 script 裡以 `tq.on("data", render)` 註冊繪圖函式，每次資料到齊時以 `render(tq)` 呼叫；同一份頁面會被呼叫多次（重新整理），所以 `render` 要先清掉上一次畫的東西。

## 頁面 API（`window.tq`）

| API | 作用 |
| --- | --- |
| `tq.on("data", render)` | 註冊繪圖函式；只接受一個，再註冊會取代。`render` 可以回傳 promise |
| `tq.dataset(name)` | 資料集：`{name, title, description, source, status, columns, rows, rowCount, meta}`。`status` 是 `ok` 或 `error`（`error` 時 `rows` 是空的，`messageKey` 說明原因）。`rows` 是物件陣列，鍵是欄名，另有 `$row`（列號）；`time` 欄是 `Date`（見「時間」），其他照原樣 |
| `tq.value(ref)` | 參照的值：數字、文字、布林、`Date` 或 `null` |
| `tq.text(el, ref, format?)` | 以 `textContent` 寫入 `tq.format(tq.value(ref), format)`，並在元素加上 `data-tq`（參照）與 `data-tq-format`（有給格式時）。回傳 `el` |
| `tq.mark(el, ref, {chart, channel})` | 把圖形標記連到它的值：`data-tq`、`data-tq-chart`（圖名，`^[a-z][a-z0-9_]{0,47}$`）、`data-tq-channel`（`width`、`height`、`x`、`y`、`radius`、`angle`）。不寫文字。回傳 `el` |
| `tq.axis(g, axis, {dataset, column, format?})` | 在 SVG `<g>` 上畫 d3 軸（`axis` 是 `d3.axisBottom(scale)` 等），以 `tq.format` 格式化刻度，並標記每個刻度的文字（見「參照」的 `t` 與 band 軸）。整個 `<g>` 加上 `data-tq-axis`。回傳 `g` |
| `tq.decor(el)` | 宣告不代表資料的圖形（格線、圖示），加上 `data-tq-decor`。回傳 `el` |
| `tq.format(value, format?)` | 格式化（見「格式化」） |
| `tq.locale`、`tq.tz`、`tq.theme`、`tq.mode` | `init` 給的語言、時區、主題與模式（`view` 或 `check`），唯讀 |

`tq.text` 的 `el` 與 `tq.mark` 的 `el` 也可以是 d3 selection，這時 `ref` 是 `(datum, index) => 參照` 的函式，對每個元素各做一次。

`data-tq*` 屬性只由 runtime 寫入；頁面自己寫的 `data-tq*` 屬性，比對時與 runtime 寫的同等看待，所以寫錯也會被抓到。

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
- band 軸（`scaleBand`、`scalePoint`）的刻度是類別，標成 `c|資料集|欄|列`，列是第一個值等於該類別的列。
- 帶數字的固定文字（產線名稱「Line 2」、標準編號）要宣告成文字常數，以 `k|名稱` 顯示：頁面上的數字一律要能追到資料或宣告過的常數。

## 格式化

`tq.format(value, format)` 與比對程式用同一份程式碼，所以顯示與檢查的字串相同。

- `null` 一律是「—」（U+2014）。
- 數字：d3-format，`format` 是 d3-format 的格式字串（例如 `,.1f`、`.0%`、`,.3~s`），依 `init.locale` 以 `d3.formatLocale` 的 `zh-TW` 或 `en-US` 定義格式化；沒有給時用 `,.2~f`。
- 時間：d3-time-format 的 `utcFormat`（例如 `%Y-%m-%d`、`%m/%d %H:%M`），依 locale；沒有給時，時分秒都是 0 用 `%Y-%m-%d`，否則 `%Y-%m-%d %H:%M`。
- 文字照原樣，`format` 不作用。
- 布林：`zh-TW` 是「是」與「否」，`en` 是 `Yes` 與 `No`。

### 時間

結果裡的時間有三種寫法：`YYYY-MM-DD`、`YYYY-MM-DD hh:mm:ss[.f…]`（SQL，查詢指定的時區）與帶時差的 RFC 3339（語意查詢，請求的時區）。runtime 一律保留寫出來的日期與時刻、忽略時差，轉成 UTC 欄位等於該牆上時間的 `Date`，所以頁面要用 `d3.scaleUtc` 與 `utcFormat`，看到的就是查詢當地的時間。`m|資料集|queried_at`、`range_from`、`range_to` 是時間點，先換成 `init.tz` 的牆上時間再照同樣方式處理。

## 計算語言

計算（`spec` 的 `computations`）以一個只產生純量的公式算出一個數字。Rust 在 `dashboard` 工具寫入時解析並驗證（`crates/contract/src/api/dashboard/formula.rs`），runtime 與比對程式以 TypeScript 解析並求值；兩邊跑同一份 `crates/contract/tests/fixtures/dashboard/formula-vectors.json`（每個案例：公式、標準樹或錯誤類別與位置、值）。

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
- 錯誤類別（封閉）：`empty`、`too_long`、`unexpected_character`、`unexpected_token`、`unexpected_end`、`unknown_function`、`invalid_name`、`bare_column`、`invalid_number`、`index_out_of_range`、`too_complex`、`unknown_dataset`、`unknown_column`、`column_not_numeric`；位置是從 1 起算的字元位置。

## 畫完

runtime 收到 `data` 後呼叫 `render(tq)`。這一代算畫完的條件：

1. `render` 回傳，回傳 promise 時它 resolve；
2. 接著兩次 `requestAnimationFrame`；
3. `#tq-root` 之下連續 200 ms 沒有 DOM 變動（`MutationObserver`）。

全部在收到 `data` 後 5 秒內完成時，送出 `rendered {generation}`；`render` 丟出例外或 promise reject 時送 `error {phase: "render"}`。沒有 `tq.done()`：畫完由 runtime 判斷，不由頁面宣告，頁面也就不能提早宣告。檢查服務不採信 `rendered`，會在 isolated world 自己確認同樣的靜止條件（「檢查服務的流程」）。上限與時間在 `contract::api::dashboard::check`（`RENDER_TIME_LIMIT`、`QUIET_PERIOD`）與 `protocol.ts`。

## postMessage 協定

每則訊息都是 `{tq: 1, type, ...}`，型別與欄位在 `src/protocol.ts`。

| 方向 | `type` | 欄位 | 時機 |
| --- | --- | --- | --- |
| host → 沙箱 | `init` | `runtime`、`page`、`spec`（公開部分）、`locale`、`tz`、`theme`、`mode` | 收到 `ready` 後一次 |
| host → 沙箱 | `data` | `generation`（從 1 起遞增）、`datasets`：每個資料集 `{status: "ok", result}` 或 `{status: "error", dataset, message_key}` | 全部資料集都有結果或錯誤時；重新整理時以新的 `generation` 再送 |
| 沙箱 → host | `ready` | `runtime` | runtime 載入後 |
| 沙箱 → host | `rendered` | `generation` | 「畫完」 |
| 沙箱 → host | `error` | `phase`（`page` 或 `render`）、`message`（最多 200 字元） | script 或 `render` 失敗 |
| 沙箱 → host | `resize` | `height`（整數，0 到 20000） | 內容高度改變（`ResizeObserver`） |
| 沙箱 → host | `source` | `reference` | 檢視模式下使用者點了有標記的值 |

- 沙箱端只接受 `event.source === window.parent` 的訊息，`init` 只接受一次，之後只接受 `data`。
- host 端只接受 `event.source` 是該 iframe 的 `contentWindow`、`event.origin` 是 `"null"` 的訊息，並以 `parseSandboxMessage` 嚴格解析：頁面和 runtime 在同一個 realm，這些訊息頁面也送得出來，所以一律視為不可信。`error` 的 `message` 只當作頁面自己的文字顯示；`source` 的參照要對得到目前的資料才用；`resize` 限制在範圍內。host 從不執行沙箱送來的任何內容。
- host 以 `postMessage(message, "*")` 送出：沙箱是 opaque origin，沒有可以指定的 origin。送出的內容只有頁面本來就看得到的東西（頁面、spec 的公開部分、這位使用者能看的資料）。
- host 給沙箱的 `spec` 是 `publicSpec(spec)`：資料集沒有 SQL 與語意請求。

## 檢查規則

比對程式依下列規則判定，每條規則對應判定的一個問題類別（`contract::api::dashboard::check::Problem`；每個類別有固定的英文提示，`ProblemKind::hint`）。「根」是 `#tq-root`。

| 規則 | 不符時的類別 |
| --- | --- |
| 沙箱回報 `ready`，頁面註冊了 `render`，`render` 在 5 秒內回傳 | `not_rendered` |
| 回傳後在時限內畫完（靜止 200 ms）；整個檢查在 15 秒內 | `timeout`（`phase`：`render` 或 `check`） |
| 沒有任何 script 例外或未處理的 rejection（含非同步的） | `script_error`（訊息由檢查服務以 CDP 取得，最多 200 字元，移除控制與不可見字元） |
| 沙箱 frame 沒有導向、沒有開新視窗、沒有送出表單 | `navigated` |
| 每個 `data-tq` 都是合法參照，而且對得到資料：資料集、欄、列、計算、常數都存在 | `unmapped`（參照的名稱都是宣告過的才回傳參照，否則只回元素的標籤） |
| `tq.text` 標記的元素，`textContent` 等於 `tq.format(值, data-tq-format)` | `mismatch`（回傳參照與資料裡的值） |
| 根之下所有 SVG 圖形（`rect`、`circle`、`ellipse`、`line`、`path`、`polygon`、`polyline`）都在 `tq.mark`、`tq.axis` 或 `tq.decor` 之下 | `unmapped`（沒有參照，回傳標籤） |
| 標記以外沒有數字：根之下不在 `data-tq` 元素裡的文字節點、會顯示或朗讀的屬性（`title`、`alt`、`aria-label`、`aria-description`、`aria-valuetext`、`aria-valuenow`、`aria-valuemin`、`aria-valuemax`、`aria-roledescription`、`aria-placeholder`、`placeholder`、`value`、`label`）、`::before`、`::after`、`::marker` 的 `content`，以及有編號的清單項目，都沒有十進位數字（`\p{Nd}`，含全形）。`<style>` 與 `<script>` 的內容不算 | `literal_number`（回傳位置與那段數字，只含數字、`.` 與 `,`，最多 32 字元） |
| 同一張圖（`data-tq-chart`）同一個 `channel` 的標記：`width`、`height`、`radius` 與值成正比（從 0 起，比例一致，誤差 1% 或 1 px 內，值為 0 或 `null` 時不超過 1 px）；`x`、`y` 是值的一次函數（三個以上的標記才判定）；`angle` 依弧的角度與值成正比 | `mark_geometry`（回傳參照與 `channel`） |
| 連續軸的每個刻度落在該欄所有值（數字含 0）的範圍經 d3 `nice` 之後的區間內；刻度位置是刻度值的一次函數 | `mark_geometry`（`channel: "axis"`） |
| 根之下沒有 `canvas`、`img`、`picture`、SVG `image`、`video`、`audio`、`iframe`、`frame`、`object`、`embed`、`input`、`textarea`、`select`、shadow root，`content` 只有加引號的文字（沒有 `counter()`、`attr()`、`url()`） | `unsupported_content` |
| 根之下最多 20,000 個元素、10,000 個標記 | `too_large` |

- 判定通過時回傳報告：每個不同的參照與它在資料裡的值，依文件順序。assistant 以 `CheckVerdict::validate` 確認報告與問題裡的每個參照、值與標籤都來自它送出的資料，才交給模型。
- 問題最多 20 個，依發現順序，超過就截斷。
- 這個檢查是 J5「數字只來自查詢」的品質關卡，不是安全邊界；安全邊界是沙箱與它的 header。

### 檢查不到的部分

- 折線與面積的路徑（`path`）只能標成 `tq.decor`，形狀不檢查；要檢查的值另外以點（`tq.mark` 的 `x`、`y`）標記。
- 以 HTML 元素的寬高畫的長條沒有標記時看不出來；圖形一律用 SVG 並以 `tq.mark` 標記。
- 用文字寫數字（「一百二十」），以及依時間或環境改變內容的頁面，檢查擋不住；所以檢視時前端再比對一次（Laysi/trustiot-v2#462）。
- 沙箱還沒處理的外送缺口在 Laysi/trustiot-v2#699。

## 比對程式（`compare.js`）

`compare.js` 定義 `globalThis.tqCompare(input)`：

- 輸入是 `CompareInput`（`protocol.ts`）：`runtime`、`spec`（公開部分）、`datasets`（結果）、`locale`、`tz`。輸入一律由呼叫端提供，從不讀頁面或 runtime 的狀態；比對程式自己解析參照、計算公式、格式化。
- 輸出是判定（`CheckVerdict`）：通過時 `{verdict: "passed", report: {marks}}`，否則 `{verdict: "failed", problems}`。CDP 才看得到的問題（`script_error`、`navigated`、shadow root、`timeout`）由檢查服務補上。
- 檢查時，檢查服務以 CDP 在沙箱 frame 建立 isolated world 後注入並呼叫它：頁面改寫 DOM 原型或全域物件騙不到它。
- 檢視時（Laysi/trustiot-v2#462），runtime 在頁面的 script 執行**之前**載入它，先保存原始的 DOM 函式（`Node.prototype.textContent` 的 getter、`Element.prototype.getAttribute`、`querySelectorAll`、`getBoundingClientRect` 等）與 `postMessage`，畫完後在頁面的 realm 比對，把對不到資料或不相符的值換成「—」並標示。這個結果只用來顯示，從不當作判定：頁面在同一個 realm 裡還是能干擾它。

## 檢查服務的流程

`services/dashboard-check` 收到 `CheckRequest`（`contract::api::dashboard::check`）後，細節由該服務決定，順序如下：

1. 驗證請求（`CheckRequest::validate`），runtime 版本要是建置裡有的。
2. 啟動新的 `chromium-headless-shell` 行程（`--remote-debugging-pipe`、`--no-sandbox`、全新的 profile），一次只做一個檢查。
3. 以 CDP `Fetch` 攔截所有請求，從映像裡的前端建置結果供給 `SandboxFile` 的檔案與 `/sandbox/check.html`、`/sandbox/check.js`，header 用 `SANDBOX_DOCUMENT_HEADERS`、`SANDBOX_ASSET_HEADERS`、`CHECK_HOST_HEADERS`；來源用不會解析的固定 origin（例如 `https://check.invalid`）。其他請求一律拒絕；沙箱 frame 的文件請求只要不是預期的那一個，就判 `navigated`。
4. 打開 host 頁，以 `Runtime.evaluate` 交給它 `init` 與 `data` 的內容（host 頁在可信的那一層，頁面碰不到）；host 頁透過 `Runtime.addBinding` 回報 `ready`、`rendered`、`error` 與時間。
5. 收集沙箱 frame 的 `Runtime.exceptionThrown`（`script_error`）、`Page.frameNavigated` 與視窗開啟（`navigated`），以 `DOM.getDocument`（`pierce: true`）找 shadow root。
6. 等 `rendered` 或時限；之後在沙箱 frame 以 `Page.createIsolatedWorld` 建立 isolated world，先確認 200 ms 靜止，再注入 `compare.js` 並以請求的內容呼叫 `tqCompare`。
7. 合併問題（最多 20 個），以 Rust 型別解析並 `validate`，回傳判定；關閉瀏覽器並刪掉 profile。
8. 日誌只記類別與耗時，不記頁面、資料或訊息。

## 測試

- `bun run --cwd frontend test`：`src/protocol.test.ts` 讀 `crates/contract/tests/fixtures/dashboard/` 的固定樣本（由 Rust 型別寫出），確認每個都能解析且不遺失欄位、`publicSpec` 與 Rust 的 `Spec::public` 相同、參照的測試向量答案一致、判定與沙箱訊息拒絕封閉集合以外的內容。
- `bun run --cwd frontend typecheck` 涵蓋 `sandbox/`。
- 判定的固定樣本是格式的範例，不是執行 `check-request.json` 的預期結果。
