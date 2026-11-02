# 圖表系列色改用設計系統色盤的驗證（2026-11-02）

對應 issue：Laysi/trustiot-v2#1003（前端的圖表系列色改用設計系統的色盤：第一色是藍，品牌色相移到第三）。元件的用法與規則在 `frontend/README.md` 的「儀表板元件」。Laysi/trustiot-v2#895 的驗證方法與舊色盤的結果在 [2026-11-01-895-dashboard-components.md](2026-11-01-895-dashboard-components.md)，那份報告記錄 2026-11-01 測的舊色盤，不改。

## 環境

- 程式碼：基於 main 的 `484e1669`，本次變更是 `frontend/src/styles/global.css` 的 `--chart-1`～`--chart-6`（淺色與深色各六個值）、`docs/design/chat-dashboards/assets/base.css` 的同一組副本，以及相關文件。
- 系列色的來源是 `docs/design/system/tokens.css` 的 `--chart-series-1`～`--chart-series-6`（淺色與深色），對應 `chart-series-N` ↔ `--chart-N`；issue 內列的 2026-11-02 數值與 `tokens.css` 相同。
- 前端檢查、色盤檢查與瀏覽器實測都在建置主機（arm64 Linux）：Bun 1.4.2（`.prototools` 的版本）。
- 瀏覽器：Debian trixie 的 `chromium-headless-shell` 156.0.8078.47（與 Laysi/trustiot-v2#895 的報告同一個映像），字型 Noto，以 SwiftShader 繪圖；視窗桌面 1280 px、手機 390 px（2 倍像素）。
- 預覽頁（不進 repo）：以 SPA 的 `vite.config.ts` 為本建置，一頁放五張卡片（時間序列、長條、KPI、狀態、表格），元件、樣式與資料都是 repo 的 `frontend/src/dashboard/`、`frontend/src/styles/global.css` 與 `fixtures.ts`；為了六個顏色都畫出來，時間序列與長條的資料換成每張六個系列（Line A～F、Site 1～6，其餘規格不變）。以 Bun 的 HTTP 伺服器在 127.0.0.1 提供，每個回應帶 `services/web/src/headers.rs` 的 CSP（強制）與 Trusted Types（只回報）。
- 色盤檢查工具：資料視覺化的色盤驗證程式（OKLab 色差，Machado 2009 的色盲模擬，嚴重度 1.0），與 Laysi/trustiot-v2#895 用的相同。

## 指令

```sh
# 前端與 repo 的檢查，在建置主機上，repo 根目錄
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
bun install --cwd scripts --frozen-lockfile
bun test scripts
cargo test --locked -p repo-check

# 色盤：相鄰兩色是閘門，--pairs all 只報告；底色是 --card 與 --background 由 oklch() 換成的十六進位
bun <色盤驗證程式> "#1463dc,#df6815,#2b8d6e,#b88513,#6737b7,#d64992" --mode light --surface "#ffffff"
bun <色盤驗證程式> "#3e89f7,#b85207,#40a986,#966c13,#9b7be9,#c13480" --mode dark --surface "#14171e"
bun <色盤驗證程式> "#3e89f7,#b85207,#40a986,#966c13,#9b7be9,#c13480" --mode dark --surface "#0d0f15"
# 再各加 --pairs all 報告任意兩色

# 前三色兩兩（設計系統：散佈圖這類任兩點都可能相鄰的圖最多用前 3 色）
bun <色盤驗證程式> "#1463dc,#df6815,#2b8d6e" --mode light --surface "#ffffff" --pairs all
bun <色盤驗證程式> "#3e89f7,#b85207,#40a986" --mode dark --surface "#14171e" --pairs all
# 設計系統 README 原本的數字：相鄰兩色與前三色兩兩的指令，色 3 換成 H173 的 #218d74（淺）、#37a98d（深）

# 舊色盤的副本搜尋（排除 2026-11-01 的報告與它的資料目錄）
grep -rniE "0b8466|2a78d6|d4572a|6250d6|c2427a|8a6d00|21a27f|4f93ea|d9693a|9085e9|d9628f|b38f12" \
  --exclude-dir=.git --exclude-dir=node_modules --exclude-dir=target \
  --exclude-dir=2026-11-01-895-dashboard-components --exclude=2026-11-01-895-dashboard-components.md .
```

瀏覽器實測的程式是一次性的，不進 repo（把它自動化屬於進階驗證，見 Laysi/trustiot-v2#895 報告的做法）。做法：建置預覽頁；啟動無頭 Chromium；經 DevTools Protocol 以 `Emulation.setEmulatedMedia` 切換 `prefers-color-scheme`、以 `Page.addScriptToEvaluateOnNewDocument` 收集 `securitypolicyviolation`，並記錄主控台、`Log` 與例外；頁面內以 `getComputedStyle` 讀 `--chart-1`～`--chart-6`，以 canvas 把任何 CSS 顏色（含 `oklch()`）換成 RGB，對比依 WCAG 2.2 的公式從頁面實際算出的顏色計算；卡片底色取圖例色塊往上第一個不透明的祖先元素；ECharts 的 canvas 以 `getImageData` 數出每個系列色的像素（±2）。

## 結果

原始資料：[palette.txt](data/2026-11-02-1003-chart-series-palette/palette.txt)（色盤檢查的輸出）、[browser.json](data/2026-11-02-1003-chart-series-palette/browser.json)（瀏覽器的 19 項檢查與每個顏色的對比）。

### 系列色

`global.css` 的 `--chart-1`～`--chart-6`（與 `tokens.css` 的 `--chart-series-1`～`--chart-series-6` 逐一相同）：

| 序號 | 色相 | 淺色 | 深色 |
| --- | --- | --- | --- |
| 1 | 藍 | `#1463dc` | `#3e89f7` |
| 2 | 橘 | `#df6815` | `#b85207` |
| 3 | 青綠（品牌色相） | `#2b8d6e` | `#40a986` |
| 4 | 赭 | `#b88513` | `#966c13` |
| 5 | 紫 | `#6737b7` | `#9b7be9` |
| 6 | 洋紅 | `#d64992` | `#c13480` |

### 色盤檢查（順序相鄰的兩色，閘門）

| 檢查 | 門檻 | 淺色（卡片與頁面 `#ffffff`） | 深色（卡片 `#14171e`、頁面 `#0d0f15`） |
| --- | --- | --- | --- |
| 亮度範圍、彩度下限 | 通過 | 通過（L 0.43–0.77） | 通過（L 0.48–0.67） |
| 紅色盲、綠色盲模擬下最接近的一對 | ≥ 8（下限 6） | 9.2（色 3 與 2，紅色盲） | 13.4（色 4 與 3，綠色盲） |
| 一般視覺下最接近的一對 | ≥ 15 | 17.7（色 4 與 3） | 18.5（色 4 與 3） |
| 對底色的對比 | ≥ 3:1 | 通過 | 通過（兩種底色都通過） |

深色的兩種底色算出的色差相同（色差與底色無關），只有對比不同。

### 色盤檢查（任意兩色，只報告）

| 檢查 | 淺色 | 深色 |
| --- | --- | --- |
| 紅色盲、綠色盲模擬下最接近的一對 | 0.8（色 4 與 2，綠色盲） | 1.0（色 4 與 2，綠色盲） |
| 一般視覺下最接近的一對 | 9.2（色 4 與 2） | 8.2（色 4 與 2） |

不相鄰的赭（色 4）與橘（色 2）最接近，色盲模擬下幾乎無法分辨；折線與長條以相鄰配對為準，隱藏中間的系列後兩者可能相鄰，所以圖例、tooltip 與來源一律寫出系列名稱，不只靠顏色（與 Laysi/trustiot-v2#895 相同的處理）。任意兩色的檢查不通過，與 Laysi/trustiot-v2#895 的舊色盤相同（舊色盤最差是藍與紫），所以規則沒有變：只對相鄰兩色保證。

### 色盤檢查（前三色兩兩，2026-11-02 審查時補量）

設計系統規定散佈圖這類任兩點都可能相鄰的圖最多用前 3 色，所以前三色要兩兩分得開：

| 檢查 | 門檻 | 淺色 | 深色 |
| --- | --- | --- | --- |
| 紅色盲、綠色盲模擬下最接近的一對 | ≥ 8（下限 6） | 9.2（色 3 與 2，紅色盲） | 13.7（色 3 與 2，綠色盲） |
| 一般視覺下最接近的一對 | ≥ 15 | 23.2（色 3 與 1） | 21.4（色 3 與 1） |

### 與設計系統 README 原本的數字

設計系統 README 的「分類色（序列）」原本寫相鄰兩色 10.1／14.0（色盲模擬）與 18.4／19.0（一般視覺）、前三色兩兩 10.1／14.5 與 22.4／20.5。這些是 `chart-series-3` 還在 H173（`#218d74`／`#37a98d`）時量的，README 自己有註明；青綠移到現在的 H168 後沒有重量。以同一個驗證程式和 H173 的青綠重算，這六個數字全部重現（`palette.txt` 的最後四段）；用現在的色票就是上面的數字。維護者 2026-11-02 審查時決定改成現在的數字，README 的那一句與附註在同一個 PR 更新。

### 瀏覽器（淺色與深色，桌面 1280 px；手機 390 px）

19 項全部通過：

| 檢查 | 淺色 | 深色 |
| --- | --- | --- |
| 頁面解析出的 `--chart-1`～`--chart-6` 等於設計系統的值 | 通過 | 通過 |
| 圖例的色塊依序用六個顏色，色塊畫出的顏色等於解析出的 `--chart-N` | 通過（12 個色塊） | 通過（12 個色塊） |
| 時間序列與長條的 canvas 畫出六個系列色（像素數） | 通過（每色在時間序列 650–688 個、長條 3,647–12,393 個像素） | 同左 |
| 沒有 CSP 或 Trusted Types 違規（桌面與手機） | 通過 | 通過 |
| 沒有主控台錯誤、警告或例外（桌面與手機） | 通過 | 通過 |

系列色對真正的卡片與頁面底色的對比（WCAG 2.2，圖形的門檻是 3:1；以頁面算出的顏色計算，淺色的卡片與頁面都是 `#ffffff`、深色的卡片是 `#14171e`、頁面是 `#0d0f15`）：

| 序號 | 淺色對卡片與頁面 | 深色對卡片 | 深色對頁面 |
| --- | --- | --- | --- |
| 1 | 5.46 | 5.23 | 5.59 |
| 2 | 3.43 | 3.63 | 3.88 |
| 3 | 4.09 | 6.18 | 6.60 |
| 4 | 3.28 | 3.80 | 4.06 |
| 5 | 7.48 | 5.47 | 5.84 |
| 6 | 4.02 | 3.48 | 3.71 |

與設計系統 README 的對比表（5.5、3.4、4.1、3.3、7.5、4.0；5.2、3.6、6.2、3.8、5.5、3.5）四捨五入後相同。

### 舊色盤的副本

上面的 `grep`（十二個舊色值，不分大小寫，含與不含 `#`）排除 2026-11-01 的報告與它的資料目錄之後，只找到這份報告裡的搜尋指令本身，沒有任何舊色的副本；`rgb(…)` 與 `oklch(…)` 寫法的舊色值也搜過，沒有。2026-11-01 的報告與資料目錄記錄當時測的舊色盤，不改。

### 前端與 repo 的檢查

| 指令 | 結果 |
| --- | --- |
| `bun run --cwd frontend api:check` | 通過 |
| `bun run --cwd frontend typecheck` | 通過 |
| `bun run --cwd frontend lint` | 通過（180 個檔案，沒有需要修的） |
| `bun run --cwd frontend test` | 42 個檔案、772 個測試全部通過 |
| `bun run --cwd frontend build` | 通過（超過 500 kB 的警告是 SPA 原本的主 chunk） |
| `bun run --cwd frontend check:dist` | 通過 |
| `bun test scripts` | 2,019 個通過、9 個略過（需要容器引擎的端到端測試，沒有跑）、0 個失敗 |
| `cargo test --locked -p repo-check` | 10 個通過 |

### 截圖

截圖檔不進 repo（報告的原始資料只放文字），以下逐張描述。預覽頁每張圖六個系列：

| 截圖 | 內容 |
| --- | --- |
| 淺色、桌面 | 五張卡片，兩欄。時間序列「Power by line」六條線由下而上依序是藍、橘、青綠、赭、紫、洋紅，門檻「Limit 1,245.5 kW」的虛線在最上；圖例六個色塊同樣的顏色，藍色是第一個。長條「Energy by day」每個類別六根，由左而右藍、橘、青綠、赭、紫、洋紅。KPI 的迷你走勢是藍色；狀態元件的區間標示是主色（品牌的深薄荷），不是系列色。表格不用系列色 |
| 深色、桌面 | 同樣的內容，卡片是深色；六個顏色是深色那一組：藍較亮，橘與赭偏暗，青綠、紫、洋紅各自比淺色的稍亮或稍暗；色塊、線與長條都看得出來，橘與赭是最接近的一對 |
| 淺色、手機寬度 | 五張卡片單欄，圖例換行成兩列；時間序列與長條的六個顏色與桌面相同；表格右側被卡片切掉、可左右捲動 |
| 深色、手機寬度 | 深色的同一頁，沒有橫向捲動 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 兩個主題的 `--chart-1`～`--chart-6` 都等於設計系統的系列色 | 通過：瀏覽器解析出的值逐一等於 `tokens.css`，canvas 畫出這些顏色 |
| 兩個主題重跑色盤檢查並通過：相鄰兩色在紅色盲、綠色盲下 ≥ 8、一般視覺 ≥ 15、對卡片與頁面底色 ≥ 3:1 | 通過：淺色 9.2／17.7、深色 13.4／18.5；對比最低 3.28（淺色）與 3.48（深色） |
| repo 裡沒有舊系列色的副本 | 通過：搜尋只找到這份報告裡的指令本身（排除 2026-11-01 的報告） |
| `frontend/README.md` 的系列色一條與設計系統的「未同步」一節已更新 | 通過 |
| `bun run --cwd frontend` 的 `test`、`typecheck`、`lint`、`build`、`check:dist` 都通過 | 通過：見「前端與 repo 的檢查」 |
| 儀表板元件在淺色與深色主題的截圖附在 PR | 四張截圖（淺色與深色 × 桌面與手機寬度）在審查時給維護者看；圖檔不進 repo，上面的「截圖」逐張描述 |

待測：Firefox 與 WebKit 的顏色顯示（延續 Laysi/trustiot-v2#895 的待測項目）；設計系統其他關於系列色的規則（赭色不和警告色同時出現）前端還沒做到，不在這次範圍。
