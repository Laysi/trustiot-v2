# 畫面設計稿

本目錄放 2026-11-01 完成的五組畫面設計稿：每組有一份 `design.md`（資訊階層、元件與互動、狀態、無障礙，以及 2026-11-01 使用者對所有待決問題做出的決定）和可以直接在瀏覽器打開的 HTML 頁面。實作這些畫面的 issue 以這裡為準。

## 五組設計稿

| 目錄 | 內容 | 實作的 issue | 主要檔案 |
| --- | --- | --- | --- |
| [`tenant-admin/`](tenant-admin/design.md) | 租戶後台：成員（邀請、角色、名額）、型號與設備、閘道器（綁定、健康）；48 個狀態 | Laysi/trustiot-v2#125、Laysi/trustiot-v2#126、Laysi/trustiot-v2#127、Laysi/trustiot-v2#114、Laysi/trustiot-v2#105 | [`design.md`](tenant-admin/design.md)、[`index.html`](tenant-admin/index.html)（所有狀態的目錄） |
| [`platform-admin/`](platform-admin/design.md) | 平台後台：租戶列表與詳情、停權與恢復、閘道器與設備池、佈建事件、操作紀錄、模型角色、system prompt、AI 用量 | Laysi/trustiot-v2#131、Laysi/trustiot-v2#132、Laysi/trustiot-v2#153、Laysi/trustiot-v2#154、Laysi/trustiot-v2#140、Laysi/trustiot-v2#130 | [`design.md`](platform-admin/design.md)、[`index.html`](platform-admin/index.html) |
| [`home-ai/`](home-ai/design.md) | 對話首頁：新租戶設定清單、通用首頁（AI 不可用時）、情境 AI 按鈕與建議問題、@ 提及、問句範本、LLM 續寫；含可操作的輸入框原型 | Laysi/trustiot-v2#129、Laysi/trustiot-v2#155、Laysi/trustiot-v2#156、Laysi/trustiot-v2#158 | [`design.md`](home-ai/design.md)、[`index.html`](home-ai/index.html)、[`00-composer-prototype.html`](home-ai/00-composer-prototype.html) |
| [`ms4-ms5/`](ms4-ms5/design.md) | MS4–MS5 的選單頁：警報與通知中心、報表、機台與生產紀錄、文件庫 | Laysi/trustiot-v2#168、Laysi/trustiot-v2#169、Laysi/trustiot-v2#172、Laysi/trustiot-v2#173、Laysi/trustiot-v2#177、Laysi/trustiot-v2#193、Laysi/trustiot-v2#198 | [`design.md`](ms4-ms5/design.md)、[`alerts.html`](ms4-ms5/alerts.html)、[`reports.html`](ms4-ms5/reports.html)、[`machines.html`](ms4-ms5/machines.html)、[`documents.html`](ms4-ms5/documents.html) |
| [`device-page/`](device-page/design.md) | 設備頁：資料狀態、目前讀數、歷史圖表、「問 AI」入口；19 種狀態 | Laysi/trustiot-v2#781 | [`design.md`](device-page/design.md)、[`index.html`](device-page/index.html)、[`device-page.html`](device-page/device-page.html) |

## 怎麼看

- GitHub 上點 HTML 只會顯示原始碼。把 repo clone 下來，用瀏覽器直接打開 HTML 檔即可（檔案是自足的靜態頁，不連網路）。
- 切換深色：`tenant-admin/`、`platform-admin/`、`home-ai/`、`ms4-ms5/` 的頁面在網址加 `?theme=dark`（`home-ai/` 也接受 `?theme=light`，不加時跟隨系統）。
- 切換狀態：`ms4-ms5/` 與 `device-page/` 的頁面右下角有切換控制；`ms4-ms5/` 的網址參數是 `?state=<狀態>`，`device-page/` 是 `#state=<狀態>`（要深色再加 `&theme=dark`）。
- 頁面中 `/settings` 這類以斜線開頭的連結是設計稿裡假的應用程式路由，不會開到任何檔案。
- 截圖與產生這些頁面的腳本不放在 repo；要看畫面就開 HTML。

## 使用規則

- 這是 2026-11-01 的設計稿。實作與設計稿不同時，PR 要寫明哪裡不同與原因，並在同一個 PR 更新對應的 `design.md`；HTML 頁面只在畫面的行為或版面改變時才跟著改。
- 每組 `design.md` 的「決定（2026-11-01）」是使用者對該組所有待決問題的答案，共 51 題：全部採用建議，只有對話首頁的第 14 題改採 B，新增 `--warning`（琥珀色）token，把「異常」與「錯誤」分開。設備頁另有三項決定（資料停滯的判斷、新增 `--warning`、圖表等共用元件）。
- `--warning` token 本身不在這些設計稿的範圍：實作時兩個主題（淺色與深色）都要補值，並重跑 `frontend/README.md` 規定的對比報告。各組設計稿提的數值略有出入，實作時統一成一組。
- 模擬資料全部是虛構的（公司、人名、email、位址與編號都不是真實資料）。新增或修改頁面時維持這一點，也不要放入內部主機名稱或真實位址（規則見 [密鑰與權限](../architecture/secrets.md) 的「公開 repo 的資訊規則」）。
