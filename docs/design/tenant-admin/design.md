# 租戶管理畫面設計稿：成員、型號與設備、閘道器

對應 Laysi/trustiot-v2#125（成員）、Laysi/trustiot-v2#126（型號與設備）、Laysi/trustiot-v2#127（閘道器）。2026-11-01 依當時的 main（`0627c59a`）、`frontend/src/styles/global.css`（品牌 token，Laysi/trustiot-v2#929）與各後端 issue（Laysi/trustiot-v2#105、Laysi/trustiot-v2#100、Laysi/trustiot-v2#116、Laysi/trustiot-v2#114、Laysi/trustiot-v2#112、Laysi/trustiot-v2#118）製作。設計稿與 HTML 頁面放在 repo 的 `docs/design/tenant-admin/`（不含截圖與產生頁面的腳本）。2026-11-01 使用者已對第 8 節的 8 個問題做出決定；實作與設計不同時，PR 要說明並同步更新這份文件，定案的規則再寫進 `frontend/README.md`。

## 檔案

- `index.html`：所有狀態的目錄，含每個狀態的路由、角色與重點，連結到各狀態的頁面。
- `<畫面>--<狀態>.html`：每個狀態一個獨立 HTML，CSS 內嵌、不連網。網址加 `?theme=dark` 看深色。
- 48 個狀態的截圖（390 與 1280 寬、淺色與深色）以及產生 HTML 的腳本不在 repo。

## 1. 定位與共用框架

### 這三頁在產品裡的位置

依 L3（2026-11-01 定案），首頁是對話框；設備、閘道器、型號、成員都是選單裡的次要頁面，也是 AI 回答裡連結指向的地方。所以這三頁的設計原則是：

1. 打開就看得到現況。列表是頁面主體，不放總覽卡片；總覽由 AI 回答提供。
2. 每個狀態都有下一步。空狀態、錯誤、權限不足、配額已滿都說明原因，並給一個能按的動作。
3. 網址可以重現畫面。篩選、展開的項目、要打開的對話框都寫在網址，AI 可以深層連結過來（見第 5 節）。

### 共用框架（請其他設計組對齊）

沿用 `frontend/src/app/Layout.tsx`，只增加導覽項目：

| 區塊 | 1280 寬 | 390 寬 |
| --- | --- | --- |
| 頁首 | logo＋名稱、主要導覽、組織選單、帳號選單；`max-w-6xl`（72rem）、高 56px、sticky | 選單按鈕、logo（不顯示名稱）、組織選單（最寬 11rem，過長截斷）、帳號選單 |
| 主要導覽 | 對話、儀表板、設備、閘道器、型號、成員（`NavLink`，目前頁 `bg-accent`） | 收進左側抽屜（`members--mobile-nav`），抽屜裡用「管理」小標分組 |
| 內容 | `main#main`，`max-w-6xl px-4 py-8`，區塊間距 24px | `px-4 pt-5`，區塊間距 20px |

導覽的順序是對話優先，接著依使用頻率排列，管理類項目放後面。之後加入的警報與通知中心建議放在頁首右側的鈴鐺，不佔導覽位置。成員只有管理員看得到（決定 1）。

頁首內的頁面標頭統一成：麵包屑（只有第二層以下的頁面才有）、選填的 eyebrow、`h1`（text-2xl）、一行說明，主要動作放右側；390 寬時主要動作換到下一行並填滿寬度。

## 2. 重用與新增的元件

重用（`frontend/src/components/ui/` 與既有頁面）：

- `Button` 的 default、outline、ghost、destructive、link 樣式；`focusRing`
- `Card`、`Alert`（destructive 與預設樣式）、`AlertDialog`（破壞性確認）、`DropdownMenu`（列操作）
- `FormField`、`Input`、`NativeSelect`（CSP 不允許 Radix Select）
- `StatusPage`（權限不足、邀請失效），`pages/device/EmptyState`（建議移到 `components/` 供各頁共用）
- `Spinner` 的文字（給螢幕閱讀器的「正在載入…」）

建議新增，放在 `components/ui/`：

| 元件 | 用途 | 說明 |
| --- | --- | --- |
| `DataList` | 表格與手機清單二選一 | md 以上用語意 `<table>`（含 `caption`），以下用 `<ul>` 卡片。兩種版本由同一份欄位定義產生，用 CSS `hidden md:table` 切換，被隱藏的那份不會進入無障礙樹 |
| `PresenceBadge`／`HealthBadge` | 在線與健康狀態 | 同時用形狀、文字、顏色表示：實心圓是正常或在線，菱形是部分失敗、套用中或過期，圈加斜線是離線，虛線圓是未知或等待連線。不靠顏色區分 |
| `UpgradeNotice` | 方案上限已滿 | 說明已滿的項目與數量、既有資料不受影響、升級後的上限；只有管理員看到「查看方案與用量」 |
| `UsageMeter` | 名額用量 | 文字寫出「6 / 10」，條狀圖只是輔助（`role="img"` 加 aria-label） |
| `ErrorSummary` | 表單頂端的錯誤摘要 | 送出失敗時取得焦點，每一項連到對應欄位 |
| `Dialog`（非警示用） | 邀請、綁定、匯入 | 與 `AlertDialog` 同一套樣式，加上右上角的關閉按鈕 |
| `Toast` | 邀請已寄出等成功通知 | `role="status"`，5 秒後消失；滑鼠移上去或取得焦點時暫停計時 |
| `SetupSteps` | 空狀態裡的設定步驟 | 與 L3 新租戶設定清單共用同一個元件與同一份完成判斷 |

新增的 token（`global.css`）：`--warning` 與 `--warning-soft`。淺色是 `oklch(0.53 0.12 65)` 與 `oklch(0.96 0.04 85)`，深色是 `oklch(0.8 0.13 80)` 與 `oklch(0.27 0.04 80)`。用在「部分讀取失敗」「設定套用中」「供電不足」等需要注意、但不是錯誤的狀態。對比見第 6 節。

## 3. 各畫面

### 3.1 成員（Laysi/trustiot-v2#125）：`/members`，只有管理員能進

頁面由上而下：

1. 頁面標頭，主要動作是「邀請成員」。
2. 名額用量：「成員數（Starter 方案）6 / 10」，並說明名額包含待接受的邀請（決定 4）。
3. 成員卡片：成員（名稱與 email，自己標「你」）、角色徽章、加入日期，每列一個「…」選單。
4. 待接受的邀請卡片：email、角色、邀請者、到期日。已過期的邀請用菱形加「已過期」標示。操作有「重新寄送」（過期的叫「重新邀請」）與「撤銷」。
5. 收合的「角色可以做什麼」說明。

| 狀態 | 檔案 | 重點 |
| --- | --- | --- |
| 一般 | `members--normal` | 長 email 會換行，不撐寬版面 |
| 載入中 | `members--loading` | 骨架列；邀請按鈕先停用，等名額讀到才能判斷能不能邀請 |
| 讀取失敗 | `members--error` | 錯誤列加「重試」 |
| 只有自己 | `members--empty` | 不顯示空的邀請表，改顯示下一步「邀請第一位成員」 |
| 沒有權限 | `members--viewer-denied` | `StatusPage`，列出管理員的名字，讓使用者知道該找誰 |
| 邀請表單的錯誤 | `members--invite-dialog` | 422 的 `field_errors` 依 JSON Pointer 對到欄位；第一個錯誤欄位取得焦點 |
| 邀請已寄出 | `members--invite-sent` | toast，新的邀請列以淡品牌色標出 |
| 名額已滿 | `members--quota-full` | `UpgradeNotice`；邀請按鈕停用，並以 `aria-describedby` 指向原因 |
| 列的操作選單 | `members--row-menu` | 「變更角色…」，分隔線後是紅色的「從組織移除…」 |
| 最後一位管理員 | `members--role-last-admin` | 驗收條件 2。後端錯誤代碼的翻譯顯示在對話框內，對話框不關閉。降級自己時另外警告 |
| 移除確認 | `members--remove-confirm` | `AlertDialog`，預設焦點在「取消」；破壞性按鈕寫出動作與對象（「移除黃小芳」） |
| 接受邀請 | `members--accept` | 先說明組織、邀請者、角色、以哪個帳號加入，再讓使用者接受 |
| 帳號不符 | `members--accept-mismatch` | 不顯示組織的細節，提供「登出並改用其他帳號」 |
| 邀請失效 | `members--accept-invalid` | 過期、已撤銷、已接受都顯示同一段說明，不洩漏是哪一種 |

互動：

- 變更角色用對話框，不在表格裡直接下拉。原因是改角色會立即生效，在下拉選單誤選的代價太高。
- 成功之後焦點的去向：移除成員後移到成員卡片的標題，邀請後回到「邀請成員」按鈕。

### 3.2 型號與設備（Laysi/trustiot-v2#126）

#### 設備列表：`/devices`

- 工具列：搜尋、閘道器、型號、狀態，條件都寫在網址。API 一次回傳所有設備、不分頁（Laysi/trustiot-v2#126 的留言），所以篩選在前端做。設備超過約 500 台時另開 issue 加分頁，見決定 6。
- 欄位：設備（名稱連到 `/devices/:id`，下方是型號）、狀態（H1 的健康狀態；閘道器離線時顯示「未知（閘道器離線）」，不誤報離線）、閘道器、連線、取樣間隔、「…」選單（編輯設定、刪除）。
- 空狀態 `devices--devices-empty-setup` 是三個步驟：綁定閘道器、建立型號、新增設備。每一步是否完成依實際資料判斷，只有目前這一步有按鈕，並與 L3 的設定清單共用元件。
- 檢視者看到同一個列表，但沒有新增按鈕與列操作（`devices--devices-viewer`）；用連結打開 `/devices/new` 時顯示權限說明（`devices--device-new-denied`）。

#### 新增與編輯設備：`/devices/new`、`/devices/:id/settings`

表單用整頁，不用對話框：欄位多，手機上的對話框捲動不好用，整頁也可以深層連結。表單分三張卡片：

1. 基本資料：名稱、閘道器、型號。這三項建立後不能改：新增時在提示裡說明，編輯時改成唯讀文字。
2. 連線：傳輸方式用說明較完整的單選卡片，因為 RTU over TCP 的共用連線規則需要解釋；另有主機、連接埠、站號。
3. 取樣：取樣間隔用下拉選單，選項 1 秒、10 秒、30 秒、1 分鐘、5 分鐘、15 分鐘、1 小時。低於方案下限的選項仍然列出但停用，並標出需要的方案（升級牆）；提示寫「你的方案（Starter）最短 1 分鐘」。

錯誤處理：

- `devices--device-new-errors`（驗收條件）：表單上方的錯誤摘要取得焦點，每個欄位下方顯示翻譯後的訊息，欄位標上 `aria-invalid`，提示文字保留不被取代（沿用 `FormField`）。
- `devices--device-edit-conflict`：收到 409 `device.revision_mismatch` 時保留使用者的輸入，提示重新載入最新設定，並把使用者的修改列在旁邊比對。
- `devices--device-edit-ended`：收到 409 `device.binding_ended` 時停用所有欄位，只留「刪除設備」。
- 更新是整筆取代（PUT）：送出時一定帶上完整的表單內容，不能只送改過的欄位，否則沒送的選填欄位會回到預設值（Laysi/trustiot-v2#126 的留言）。

刪除設備（`devices--device-delete`）：確認對話框說明閘道器會停止讀取這台設備，已收到的資料會保留到保存期限。

#### 型號：`/models`、`/models/new`、`/models/:id`

- 列表的欄位：名稱、資料點數、使用中的設備數（連到 `/devices?model=…`）、更新日期。頁首的動作是「匯入 JSON」與「新增型號」。
- 編輯頁（`devices--model-edit`）：
  - 桌面是可直接編輯的表格，欄位有鍵、功能碼、位址（0 起算，下方換算成說明書上的位址，例如 40001）、資料型別、位元組順序、長度（只有字串）、倍數＋偏移、說明。
  - bool 與字串不顯示倍數與偏移，bool 也不顯示位元組順序；後端同樣會拒絕這些組合。
  - 資料點可以上下移動或刪除（每個按鈕都有 aria-label，例如「上移第 2 個資料點」）。
  - 手機改成可展開的資料點卡片。
  - 儲存前提示：使用這個型號的設備會收到新的設定版本。
- `devices--model-edit-errors`：`/points/2/address` 這類路徑對到第 3 列的位址欄。有錯的列整列淡紅底並加左側紅線，訊息寫出「第 3 個資料點（energy_kwh）」；手機上有錯的卡片自動展開。
- 檢視者看到同一頁的唯讀版本，可以匯出（`devices--model-viewer`）。
- 匯入（`devices--model-import`）：
  1. 選檔後先在瀏覽器以 zod 檢查格式，並顯示預覽（名稱、資料點數、格式版本）。
  2. 名稱衝突時可以直接改名後再匯入。
  3. 伺服器回傳的錯誤逐條列出。
- 匯出：直接下載 `<名稱>.json`。
- 刪除仍在使用的型號（`devices--model-delete-in-use`）：收到 409 `device_model.in_use` 時，對話框改成說明，並連到使用這個型號的設備。

### 3.3 閘道器（Laysi/trustiot-v2#127）：`/gateways`

- 狀態摘要列：在線、離線、等待第一次連線的數量，各自是篩選連結；右邊寫「每 30 秒自動更新」並附「立即更新」。
- 表格欄位：
  - 閘道器：硬體 ID，本身是展開按鈕。
  - 狀態：presence 加離線原因（shutdown 是「已關機」、connection_lost 是「連線中斷」、heartbeat_timeout 是「心跳逾時」），下方寫狀態從什麼時候開始。
  - 最後回報：相對時間，`<time datetime>` 與 title 帶絕對時間。
  - 已套用設定：applied 與 desired 相同時寫「第 14 版，已是最新」；不同時用菱形標「套用中：第 3 → 4 版」。
  - 設備與健康：設備健康的計數加運作旗標，例如供電不足、時鐘未校正、離線緩衝積壓。
  - 程式版本。
- 展開列（`gateways--detail-open`）：H1 的運作指標，包括 CPU、記憶體、資料分割區、SoC 溫度、開機時間、重啟次數、離線緩衝、時鐘、憑證到期。離線時另有「為什麼離線？」情境按鈕，MS3 才會出現。不另做詳情頁。
- 自動更新：
  - 每 30 秒重新查詢，視窗不在前景時暫停。
  - 更新時不顯示骨架，也不放進 live region，避免螢幕閱讀器重複朗讀。
  - 更新失敗而畫面上已有資料時（`gateways--stale`），保留舊資料並提示「以下是 14:03 的狀態」，免得把舊的「在線」當成現在的狀態。
- 綁定（只有管理員）：
  - 欄位是硬體 ID 與認領碼（Laysi/trustiot-v2#114 的持有證明，形式依決定 5：出廠時印在標籤上的認領碼）。
  - 綁定失敗（`gateways--bind-failed`）：「不在設備池」與「已綁在別的租戶」顯示同一段說明，與後端的相同回應一致。輸入保留，可以修正後再按一次（驗收條件 2）。
  - 網路或 503 錯誤（`gateways--bind-network`）：明說「綁定沒有完成」，主按鈕改成「重試綁定」；後端的綁定可以重試，重送不會重複綁定。
  - 被限流（`gateways--bind-rate-limited`）：依 `retry_after` 倒數，按鈕停用，並說明這是為了防止猜測硬體 ID。
  - 數量已滿（`gateways--quota-full`）：使用 `UpgradeNotice`。
  - 成功（`gateways--bound`）：新的閘道器置頂並標色，狀態是「等待第一次連線」，說明接電與網路後約 2 分鐘上線，下一步是「新增設備」。
- 非管理員：看得到列表與健康狀態，沒有綁定按鈕；空狀態改成請管理員綁定，並列出管理員的名字（`gateways--viewer`、`gateways--viewer-empty`）。

## 4. 共通的互動規則

- 寫入的保護（Laysi/trustiot-v2#678）：送出前比對畫面所屬的租戶與工作階段目前的租戶，不同時擋下送出並提示重新載入。這三個畫面都是寫入畫面，所以依賴 Laysi/trustiot-v2#678。
- 寫入中：主按鈕停用並改成「儲存中…」，對話框在寫入期間不能用 Esc 關閉。
- 錯誤文字一律以 `message_key` 翻譯，`details` 填入佔位符（例如 `{current}` 版本）；已知的鍵都已有中英翻譯（Laysi/trustiot-v2#100、Laysi/trustiot-v2#99）。新的鍵，例如 `member.last_admin`、`gateway.claim_rejected`、各種配額已滿，由後端 issue 依 CLAUDE.md 的規則在同一個變更裡補上翻譯。
- 破壞性操作一律用 `AlertDialog`：預設焦點在「取消」，破壞性按鈕寫出動作與對象。這些操作不要求輸入名稱確認，因為刪除的都是可以重建的設定，不是資料。

## 5. 路由與深層連結（給 AI 回答使用）

| 網址 | 畫面 |
| --- | --- |
| `/devices?gateway=tg-0001&model=power-meter&status=offline&q=…` | 篩選後的設備列表（`devices--devices-filtered`），條件以可移除的 chip 顯示 |
| `/devices/new?gateway=tg-0001` | 新增設備，預先選好閘道器 |
| `/devices/:id/settings` | 編輯設備 |
| `/models`、`/models/new`、`/models/:id`、`/models?import` | 型號列表、新增、編輯、匯入 |
| `/gateways?status=offline`、`/gateways#tg-0002` | 閘道器篩選；`#硬體 ID` 會展開那台並捲動到它 |
| `/gateways?bind` | 打開綁定對話框（只有管理員；其他角色忽略這個參數） |
| `/members`、`/members?invite` | 成員；打開邀請對話框 |
| `/invitations/accept#<token>` | 接受邀請（token 放在 fragment，見決定 3） |

被連結指到的列會加上淡品牌底色與左側品牌色線（`data-target`），並取得焦點（`tabIndex=-1`），3 秒後淡出。

## 6. 無障礙（WCAG 2.2 AA）

對比由 `src/contrast.ts` 計算（oklch 轉 sRGB，再算相對亮度），全部通過：

| 主題 | 組合 | 對比 | 需要 |
| --- | --- | --- | --- |
| 淺／深 | 在線文字 success / card | 5.65 / 8.51 | 4.5 |
| 淺／深 | 離線文字 destructive / card | 5.40 / 5.70 | 4.5 |
| 淺／深 | 新增：警告文字 warning / card | 5.45 / 9.48 | 4.5 |
| 淺／深 | 新增：旗標文字 foreground / warning-soft | 15.77 / 13.46 | 4.5 |
| 淺／深 | 新增：警告圖示 warning / warning-soft | 4.85 / 7.99 | 3 |
| 淺／深 | 次要文字 muted-fg / muted（展開列） | 5.47 / 6.07 | 4.5 |
| 淺／深 | 目標列次要文字 muted-fg / brand-soft | 5.01 / 5.96 | 4.5 |
| 淺／深 | 錯誤列文字 destructive / 4% 紅底 | 5.23 / 4.80 | 4.5 |
| 淺／深 | 品牌色線 brand / card（圖形） | 3.52 / 5.09 | 3 |
| 淺／深 | 名額條 brand / muted（圖形） | 3.21 / 4.28 | 3 |
| 淺／深 | 輸入框框線、焦點框 | 3.11 / 3.47、6.94 / 13.38 | 3 |

錯誤列的紅底原本是 6%，深色主題只有 4.45:1，所以改成 4% 並加上左側紅線，紅底只是輔助。完整的表在 `src/contrast.out.md`。

其他規則：

- 鍵盤：
  - 全部可以用 Tab 操作；選單用方向鍵移動、Esc 關閉，焦點回到觸發的按鈕（Radix 提供）。
  - 對話框把焦點鎖在框內，打開時焦點在第一個欄位，破壞性確認則在「取消」。
  - 手機抽屜也鎖住焦點。
  - 表格的展開按鈕用 `aria-expanded` 與 `aria-controls`。
- 焦點框沿用 `focusRing`（outline，強制色彩模式下也看得到）。
- 不只用顏色表示狀態：每個狀態都有形狀與文字；條狀圖旁邊都有數字。
- 標記：
  - 表格有 `caption` 與 `scope="col"`；只有圖示的按鈕都有 aria-label，並寫出對象（「管理 黃小芳 的成員資格」）。
  - 表單錯誤：欄位設 `aria-invalid`，並以 `aria-describedby` 同時指向提示與錯誤。錯誤摘要用 `role="alert"`，送出失敗後取得焦點。
  - 載入時只有一段 `role="status"` 的「正在載入…」，骨架本身不會被朗讀。
- 觸控：手機上的點擊目標至少 36px，抽屜裡的項目 44px；390 寬時沒有橫向捲動（`shoot.ts` 對 48 個狀態都檢查過）。
- 動畫：`prefers-reduced-motion` 時骨架不閃動；這條規則沿用 `global.css`。

## 7. API 與待補的後端資料

| 畫面需要 | 來源 | 狀態 |
| --- | --- | --- |
| 成員列表、邀請、改角色、移除、名額 | Laysi/trustiot-v2#105、Laysi/trustiot-v2#110 | 尚無 API；需要租戶端的 `GET /members`，並在回應附上名額（used / limit） |
| 錯誤代碼：最後一位管理員、名額已滿、已是成員 | Laysi/trustiot-v2#105 | 鍵名待後端決定，畫面先用 `member.last_admin` |
| 設備、型號的 CRUD 與匯入匯出 | Laysi/trustiot-v2#99、Laysi/trustiot-v2#100 | 已有 API |
| 設備的健康狀態 | Laysi/trustiot-v2#118 | 尚無；列表需要每台設備目前的狀態，最好附在設備列表或一支批次 API 裡，避免每台各打一次 |
| 方案的最短取樣間隔 | Laysi/trustiot-v2#116、Laysi/trustiot-v2#111 | 需要能讀到「目前方案的權益」（最短間隔、各項上限）的 API，畫面才能在送出前停用選項 |
| 閘道器列表與 presence | Laysi/trustiot-v2#112（`GET /api/gateway-ops/v1/gateways`） | 已有；沒有設備計數與健康旗標，這兩項等 Laysi/trustiot-v2#118 |
| 自助綁定與持有證明 | Laysi/trustiot-v2#114 | 尚無；欄位形式依決定 5 是認領碼 |

## 8. 決定（2026-11-01）

2026-11-01 使用者對下列 8 題全部**採用建議（A）**。

1. 非管理員看不看得到「成員」頁？
   - **採用 A**：導覽對非管理員隱藏「成員」；用連結進來時顯示權限說明，並列出管理員名字。理由：檢視者的主要工作是看資料，名單裡的 email 對他沒用處，也少暴露一些個資。
   - 未採用 B：所有角色都能看唯讀的名單（多一個需要權限檢查的 API 面）。
2. 編輯者能不能建立、修改設備與型號？
   - **採用 A**：可以，依角色說明「編輯者能新增與修改設備、型號」。閘道器綁定與成員管理只給管理員（Laysi/trustiot-v2#114 寫明由租戶管理員綁定）。
   - 未採用 B：只有管理員可以（小團隊會把所有人都設成管理員，反而更不安全）。
3. 邀請憑證怎麼傳（Laysi/trustiot-v2#105：不能放在查詢參數）？
   - **採用 A**：放在 URL fragment（`#token`，不會送到伺服器、不會進入存取紀錄或 Referer）。頁面以 POST 送出後立即用 `history.replaceState` 清掉。
   - 未採用 B：信裡不帶 token，使用者登入後依 email 列出「待接受的邀請」（使用者可能登入別的帳號而找不到邀請）。`members--accept-mismatch` 仍說明登入了別的帳號時怎麼辦。
4. 待接受的邀請算不算成員名額？
   - **採用 A**：算。否則管理員可以一次寄出超過上限的邀請，接受時才被拒絕，對被邀請的人很不友善。
   - 未採用 B：不算，只在接受時檢查（失敗發生在被邀請的人那一端）。
5. 閘道器的持有證明用什麼形式（Laysi/trustiot-v2#114）？
   - **採用 A**：出廠時印在標籤上的認領碼，例如 12 碼、去掉容易混淆的字元。介面就是兩個文字欄位，`gateways--bind` 照這個設計。
   - 未採用 B：掃描標籤上的 QR code（要相機權限與額外的解碼套件，之後可加在 A 上）；C：由閘道器本身簽章（需要人在現場）。
6. 設備數量多時的列表做法？
   - **採用 A**：MS2 照 API 一次全部載入，在前端篩選；設備超過約 500 台時另開 issue，加入伺服器端分頁與篩選。
   - 未採用 B：現在就做虛擬捲動（設備數還小，是過早的複雜度）。
7. 閘道器可不可以取名字？API 目前只有硬體 ID。
   - **採用 A**：租戶可以給閘道器一個顯示名稱，例如「一廠二樓配電室」，硬體 ID 仍然顯示在旁邊。需要另開後端 issue；這份設計稿先只顯示硬體 ID。
   - 未採用 B：不取名字，用設備清單辨識（現場有很多台閘道器時很難認）。
8. 升級牆的「查看方案與用量」要連到哪裡？
   - **採用 A**：連到設定頁的「方案與用量」區塊（L1 的租戶用量）；MS6 有金流之前，那裡只顯示用量與「聯絡我們升級」。
   - 未採用 B：連到官網的方案頁（另一個網域，會讓使用者離開產品）。

## 9. 驗證方式

- 所有狀態都由腳本產生（不在 repo），再用 headless Chrome（DevTools protocol）在 390（DPR 2，mobile）與 1280 寬下，以淺色與深色截圖，共 192 張。截圖時開啟 `prefers-reduced-motion`。
- 390 寬的 48 個狀態都檢查過 `scrollWidth`，沒有橫向捲動。
- 人工檢查過的截圖：`members--normal`（1280 淺色、390 深色）、`members--role-last-admin`（390 淺色）、`devices--model-edit-errors`（1280 淺色）、`devices--device-new-errors`（390 淺色）、`devices--devices-empty-setup`（1280 淺色）、`gateways--detail-open`（1280 深色）、`gateways--bind-failed`（1280 淺色）、`gateways--normal`（390 淺色）。檢查時修正了兩個問題：桌面寬度下選單按鈕仍顯示，以及深色主題錯誤列的對比不足。
