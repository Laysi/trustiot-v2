# web（SPA 的靜態檔服務）

在 app 主機上提供 SPA（`frontend/`）的建置結果（Laysi/trustiot-v2#123，2026-10-21；決策 2、決策 27）。入口把 app 主機上 `/api`、`/auth`、`/internal` 以外的每個路徑都送到這個服務，不做 cookie 檢查、也不轉送 session cookie，所以它沒有 API、不知道使用者是誰、沒有狀態，也不連資料庫。

- 映像：`deploy/images/web`。`image.toml` 的 `[static]` 讓發佈流程建置 `frontend/`，把 `dist/` 放進映像的 `/srv/www`（[deploy/images/README.md](../../deploy/images/README.md)「帶靜態網站的 Rust 服務」）。
- 程式：`src/site.rs` 在啟動時載入檔案，`src/headers.rs` 是安全標頭與快取政策，`src/lib.rs` 回應請求。

## 回應

每個請求都以 path 完全比對啟動時載入的檔案（不解碼百分比編碼、不正規化 `..`），所以請求碰不到檔案表以外的任何東西。

| 請求 | 回應 |
| --- | --- |
| GET 或 HEAD，path 是網站的一個檔案（例如 `/static/B7f3kQ2x.js`、`/robots.txt`） | 200，該檔案 |
| GET 或 HEAD，path 在 `/static/` 底下（含 `/static` 本身）但不是檔案 | 404 `not found`，`text/plain`、`Cache-Control: no-store`：缺少的腳本或樣式表不會拿到偽裝成它的 `index.html` |
| GET 或 HEAD，其他 path（`/`、`/devices/42?tab=data` 等深連結） | 200，`index.html`：重新整理深連結時載入 SPA，由 SPA 的路由顯示頁面或它自己的「找不到」頁 |
| `If-None-Match` 列出檔案的 ETag（弱比較，`W/"…"` 也算）或 `*` | 304，帶 ETag、`Cache-Control` 與安全標頭 |
| 其他方法（POST、PUT、PATCH、DELETE、OPTIONS、TRACE、CONNECT） | 405 `method not allowed`，`Allow: GET, HEAD`、`Cache-Control: no-store` |

HEAD 的標頭與 GET 相同（包括 `Content-Length`），沒有 body。ETag 是檔案內容 SHA-256 的前 128 位元（十六進位，加引號）。

網站沒有 `favicon.ico` 時，瀏覽器自動要的 `/favicon.ico` 也會拿到 `index.html`（200、`text/html`），瀏覽器會忽略它；SPA 以 `<link rel="icon">` 指定圖示即可。service-kit 在 8080 埠的路由前面加上 `/healthz` 與 `/readyz`，所以 SPA 不能使用這兩個路徑；它們的回應沒有下方的安全標頭。

## 安全標頭

每個回應（200、304、404、405）都帶這些標頭，值由 `tests/serve.rs` 逐字固定，改值要同時改這張表：

| 標頭 | 值 | 理由 |
| --- | --- | --- |
| `Content-Security-Policy` | `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'` | 只執行、載入與連線到自己的檔案：沒有 inline 的腳本或樣式、沒有 `eval`、沒有 `data:`（`frontend/tools/check-dist.ts` 讓建置結果不含這些）；API 與事件串流在同一個主機（決策 2），`connect-src 'self'` 就夠 |
| `Content-Security-Policy-Report-Only` | `require-trusted-types-for 'script'; trusted-types 'none'` | Trusted Types 先只回報（瀏覽器主控台），第一批真正的頁面在瀏覽器檢查過後移進上一列 |
| `X-Content-Type-Options` | `nosniff` | 瀏覽器只照 `Content-Type` 解讀 |
| `Referrer-Policy` | `strict-origin-when-cross-origin` | 跨站時只送 origin。不用 `no-referrer`：在它之下瀏覽器對 GET、HEAD 以外的請求送 `Origin: null`，SPA 自己的 POST 也是，身分服務的跨站檢查會拒絕（[secrets.md](../../docs/architecture/secrets.md)「不放網址」） |
| `Permissions-Policy` | `camera=(), microphone=(), geolocation=(), payment=(), usb=()` | SPA 不用這些瀏覽器功能 |
| `Cross-Origin-Opener-Policy` | `same-origin` | 其他網站開的視窗拿不到這個視窗 |
| `Cross-Origin-Resource-Policy` | `same-origin` | 其他網站不能把這些檔案嵌入 |
| `X-Frame-Options` | `DENY` | 不能被框進其他頁面（舊瀏覽器；新的看 `frame-ancestors`） |
| `Strict-Transport-Security` | `max-age=31536000` | 只走 HTTPS 一年。不加 `includeSubDomains` 與 `preload`：地端時網域是客戶的 |

## 快取

| 檔案 | `Cache-Control` |
| --- | --- |
| `/static/` 底下的檔案（檔名是內容的 hash） | `public, max-age=31536000, immutable` |
| `index.html`、深連結與根目錄的其他檔案（檔名每次建置都一樣） | `no-cache`：每次都問，ETag 相同時回 304 |
| 404、405 | `no-store` |

## 啟動時載入的網站

`root_dir` 底下的每個檔案在啟動時讀進記憶體，之後不再讀磁碟（新版網站隨新映像上線）。任一項不符就不啟動，記一行 ERROR `startup failed`（訊息只寫相對路徑），以 exit code 1 結束：

- 根目錄要有 `index.html`。
- 只能有一般檔案與目錄，不能有符號連結或其他特殊檔案。
- 每個檔名與目錄名只用 ASCII 英數字與 `_`、`-`、`.`，`.` 不能開頭（沒有隱藏檔，也沒有需要百分比編碼的名字，所以請求的 path 就是檔名）。
- 副檔名只能是下表的其中一個（與 `frontend/tools/check-dist.ts` 及發佈流程的 `scripts/release/static-build.ts` 相同，`bun test scripts/release` 比對）：

  | 副檔名 | `Content-Type` |
  | --- | --- |
  | `css` | `text/css; charset=utf-8` |
  | `html` | `text/html; charset=utf-8` |
  | `ico` | `image/x-icon` |
  | `js` | `text/javascript; charset=utf-8` |
  | `png` | `image/png` |
  | `svg` | `image/svg+xml` |
  | `txt` | `text/plain; charset=utf-8` |
  | `webmanifest` | `application/manifest+json` |
  | `woff2` | `font/woff2` |

- 全部檔案加起來不超過 32 MiB（`MAX_SITE_BYTES`）：整個網站都在記憶體裡，長得過大的建置在啟動時就失敗，而不是在執行中撞到 pod 的記憶體上限。

載入成功時記一行 INFO `loaded the site`，帶 `files`（檔案數）與 `bytes`。檔案都在記憶體後就就緒，readiness 不需要其他條件。請求日誌只有 service-kit 記錄的路由，不記原始 path。

## 設定

設定由 `service_kit::config` 載入（kit README 的「設定」）。

| 變數 | 說明 |
| --- | --- |
| `TRUSTIOT__ROOT_DIR` | 網站所在的目錄，必須是絕對路徑；預設 `/srv/www`，就是映像放網站的位置 |

不讀任何平台設定（`TRUSTIOT_*`），也不需要 Secret。健康埠（預設 8081）只有 `/healthz` 與 `/readyz`，網站在 8080。設定不能用時以 exit code 1 結束。

在本機以 fixture 網站啟動：

```sh
TRUSTIOT__ROOT_DIR="$PWD/services/web/tests/fixtures/site" cargo run -p web
```

## 測試

`cargo test -p web`：

- `tests/serve.rs`：以 router 直接送請求，固定每個安全標頭的值，涵蓋深連結、檔案與它的 `Content-Type` 與快取政策、`/static/` 底下缺檔的 404（包括 `..` 與百分比編碼的 path、大小寫不同的檔名）、HEAD、ETag 與 304、405，以及載入時拒絕的每一種網站。
- `tests/process.rs`：啟動真的執行檔（經 service-kit），以 HTTP 取深連結、HEAD 一個檔案、`/healthz` 與 POST；沒有 `index.html` 的目錄讓它以 exit code 1 結束。
- `tests/fixtures/site/`：測試用的小網站（`index.html`、兩個 `/static/` 檔、`favicon.svg`、`robots.txt`）。
