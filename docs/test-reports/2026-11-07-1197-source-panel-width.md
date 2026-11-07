# 從 KPI 與狀態元件打開的資料來源面板的寬度

對應 issue：`Laysi/trustiot-v2#1197`。依據：`Laysi/trustiot-v2#1195` 的兩欄走查（問題 1）與 `docs/test-reports/2026-11-07-1179-projector-walkthrough.md` 的儀表板頁 KPI 那一步。

## 環境

- 一台 arm64 Linux 虛擬機（建置主機），rootless podman。
- 瀏覽器：`frontend/e2e/image.ts` 釘住的 Playwright 映像（Chromium），zh-TW、Asia/Taipei、縮放 100 %；淺色，另加一個 1280×720 的深色視窗。
- 頁面：UI 預覽（`vite --mode ui-preview`）。草稿面板與預覽草稿的第 2 版（KPI「Energy, week of Oct 26」）只在 `Laysi/trustiot-v2#1195` 的預覽改動裡（分支 `feat/1195-two-column-walkthrough`），本分支不含它們，所以兩次量測都在合併後的樹上跑：
  - 修正前：`feat/1195-two-column-walkthrough` 的 `e976c301`。它以 `feat/1175-source-panel` 的 `5947317b`（PR `Laysi/trustiot-v2#1178`，本分支的起點）為基礎，所以就是「`5947317b` 加上預覽改動」。
  - 修正後：本分支 `fix/1197-source-panel-width` 的 `f732df2b`（程式的 commit）與 `e976c301` 的合併，沒有衝突；只在本機合併後量測，沒有提交這個合併。
- 兩次都用同一份腳本（本報告資料目錄的 `browser-check.mjs`、`run.sh`，改自 `Laysi/trustiot-v2#1195` 的走查腳本）。

## 指令

從（上述合併後的）工作樹根目錄：

```
bun install --cwd frontend --frozen-lockfile
bash docs/test-reports/data/2026-11-07-1197-source-panel-width/run.sh
```

`VIEWPORTS=1280x720,1279x720` 只跑部分視窗，`SHOTS=0` 不截圖。資料在 `docs/test-reports/data/2026-11-07-1197-source-panel-width/`：`browser-check-before.jsonl`（修正前）、`browser-check-after.jsonl`（修正後）。截圖只給審查者看，不進 repo。

腳本在每個視窗打開預覽對話「Line 2 energy this week」，依序按這些數字打開資料來源面板，量完後按面板的關閉鈕：

- (a) 草稿面板裡 KPI「Energy, week of Oct 26」的數字；
- (b) 最後一則回答的卡片裡同一個 KPI；
- (c1) 從最後一則回答按「儲存到儀表板列表」再按「開啟儀表板」，儀表板頁上同一個 KPI（資料集是用量查詢）；
- (c2) 「Plant energy overview」的計算 KPI「Energy this week」；
- (c3) 「Daily meter readings」的狀態元件「Meter 1」（資料集是 SQL）。

每一步先把元件頂端捲到頂端列下方（簡報者先指著數字再按），然後量：面板的寬高、元件格線與格線項目的寬度、打開它的數字移動了多少、面板裡「一行三個字以內、至少三行」的文字（`narrowCount`）、被自己的框截掉的文字、頁面有沒有橫向捲動、焦點在哪、console 錯誤。關閉後量焦點在哪、是否在頂端列與輸入框之間，以及項目是否回到原來的寬度。

## 結果

寬×高是面板的 CSS px。「一字一行」是 `narrowCount`；「截斷」只列看得到的文字（腳本也列出寬 1 px 的 `sr-only` 元素，例如查詢區塊的 `legend`，那是給螢幕閱讀器的隱藏文字，不算）。

| 視窗 | 位置 | 修正前 | 一字一行 | 截斷 | 修正後 | 一字一行 | 截斷 | 修正後格線欄數 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1280×720 | (a) 草稿面板 | 204×2885 | 7 | `week`、`kwh` | 466×614 | 0 | 無 | 2 |
| 1280×720 | (b) 回答 | 358×725 | 0 | 無 | 358×754 | 0 | 無 | 1 |
| 1280×720 | (c1) 儀表板頁，存下的草稿 | 226×2257 | 5 | 無 | 1078×577 | 0 | 無 | 4 |
| 1280×720 | (c2) Plant energy overview | 226×752 | 2 | 無 | 1078×337 | 0 | 無 | 4 |
| 1280×720 | (c3) Daily meter readings | 226×1017 | 1 | `meter`、`value`、`installed`、`seen` | 1078×401 | 0 | 無 | 4 |
| 1536×768 | (a) | 204×2885 | 7 | `week`、`kwh` | 466×614 | 0 | 無 | 2 |
| 1536×768 | (b) | 358×725 | 0 | 無 | 358×754 | 0 | 無 | 1 |
| 1536×768 | (c1) | 226×2257 | 5 | 無 | 1078×577 | 0 | 無 | 4 |
| 1536×768 | (c2) | 226×752 | 2 | 無 | 1078×337 | 0 | 無 | 4 |
| 1536×768 | (c3) | 226×1017 | 1 | 四個欄位名稱 | 1078×401 | 0 | 無 | 4 |
| 1920×1080 | (a) | 204×2405 | 7 | `week`、`kwh` | 466×614 | 0 | 無 | 2 |
| 1920×1080 | (b) | 358×725 | 0 | 無 | 358×754 | 0 | 無 | 1 |
| 1920×1080 | (c1) | 226×2257 | 5 | 無 | 1078×577 | 0 | 無 | 4 |
| 1920×1080 | (c2) | 226×752 | 2 | 無 | 1078×337 | 0 | 無 | 4 |
| 1920×1080 | (c3) | 226×1017 | 1 | 四個欄位名稱 | 1078×401 | 0 | 無 | 4 |
| 1279×720（單欄） | (a) 草稿面板（在對話下方） | 476×585 | 0 | 無 | 1010×545 | 0 | 無 | 2 |
| 1279×720 | (b) 回答 | 274×2125 | 4 | 無 | 606×565 | 0 | 無 | 2 |
| 1279×720 | (c1) | 510×597 | 0 | 無 | 1078×577 | 0 | 無 | 2 |
| 1279×720 | (c2) | 510×337 | 0 | 無 | 1078×337 | 0 | 無 | 2 |
| 1279×720 | (c3) | 510×440 | 0 | 無 | 1078×401 | 0 | 無 | 2 |
| 1280×720 深色 | (a)～(c3) | (a) 是 204×2405，其餘與淺色相同 | | | 每一步都與淺色相同 | | | |

其他：

- 修正後每一步，打開面板的項目都佔滿整個格線（`spansGrid`，`grid-column: 1 / -1`），關閉後回到原來的寬度（`itemBackInItsCell`）。
- 打開時被點的數字只往上移 0–54 px（`triggerMoved`）：卡片變寬後標題不再折成三行；沒有左右移動。這幾步的 KPI 都是該列的第一格；排在同一列後面的元件會移到面板下方（單元測試涵蓋，見下）。
- 焦點：打開後都在面板標題（`h4`「資料來源」），兩次相同。關閉後，修正前焦點落在 `body`；修正後回到打開它的數字（`isTrigger` 為真），而且在頂端列與輸入框之間（`inBand`）。
- 修正前後都沒有橫向捲動，沒有 console 錯誤，草稿列表的錯誤提示沒有出現。
- (b) 在 1280×720 高了 29 px（725 → 754）：點到的列原本以視窗寬度（`sm`）排兩欄，現在以面板寬度（`@lg`，32rem）判斷，358 px 的面板排一欄，`week` 與 `kwh` 各一行。
- 修正前 (a) 的高度在不同視窗是 2405 或 2885（寬度都是 204 px），原因沒有追查；修正後每個視窗都是 614，不影響結論。
- 腳本第一次在另一台 x86_64 的建置主機跑修正後的樹時，(c1) 在 60 秒內等不到儀表板頁的 KPI；之後在 arm64 的建置主機重跑全部視窗都通過。Playwright 映像釘的是 arm64，記錄在這裡，結果以 arm64 為準。

單元測試（happy-dom 不算版面，所以斷言規則本身）：

- `src/pages/conversations/ConversationSourcePanel.test.tsx`「a KPI whose panel is open takes the whole row of the answer's grid and of the draft panel's」：回答（`@lg:grid-cols-2`）與草稿面板（`sm:grid-cols-2`）裡，第二個 KPI 打開面板時佔整列，另一個 KPI 不變，關閉後恢復。
- `src/pages/dashboards/DashboardPage.test.tsx`「a KPI or a status whose source panel is open takes the whole row; the others keep their cells」：儀表板頁（`xl:grid-cols-4`）的 KPI 與狀態元件。
- `src/dashboard/SourcePanel.test.tsx`「the panel's layout」：面板帶 `data-source-panel` 與 `@container`，事實與點到的列以面板寬度排；很長的欄位名稱換行、不截斷。
- `src/dashboard/DashboardComponent.test.tsx`「the source panel's focus」：關閉後焦點回到打開它的值、卡片的「資料來源」按鈕或最後打開的值；點擊沒有把焦點給按鈕時（Safari），焦點不會被送到頁面的其他地方。

## 結論

- [x] 1280×720 時，從草稿面板的 KPI 與儀表板頁的 KPI 打開的面板至少 358 px 寬：草稿面板 466 px，儀表板頁 1078 px（c1、c2 與狀態元件 c3）；`narrowCount` 都是 0。1536×768 與 1920×1080 相同。
- [x] 1279×720（單欄）時，從回答卡片的 KPI 打開的面板 606 px 寬，`narrowCount` 0。
- [x] 欄位名稱讀得到全名：修正後沒有截斷的欄位名稱（修正前 `week`、`kwh`，以及 c3 的四個）。
- [x] 測試與 `frontend` 的 `api:check`、`typecheck`、`lint`、`test`（8 個分片，1979 通過，0 失敗）、`build`、`check:dist` 在建置主機上通過；CI 待 PR 開出後跑。
- 待測：Firefox、WebKit 與螢幕閱讀器的實測，與手機寬度（`sm` 以下本來就是一欄）沒有在這次走查；沿用 `Laysi/trustiot-v2#960` 的瀏覽器檢查。面板打開時捲進視窗是 `Laysi/trustiot-v2#1184`，這裡沒有改。
