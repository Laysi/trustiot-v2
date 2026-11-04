# SuggestedQuestions

依該組織的設備產生的建議問題：首頁空狀態的 4–6 張卡片，或回答下方 1–3 個追問。

新增（對話為主軸，2026-11-01）。讓第一次打開 TrustIoT 的人知道可以問什麼，而且問題本身就是他們自己的設備。

## 使用者提供

- `questions`：`{ id, text, reason? }`。`text` 寫成使用者會問的話，用設備的真實名稱（「冷卻水塔 2 號這週為什麼耗電變高？」「哪些閘道器現在離線？」）；`reason` 一句話說為什麼建議它（「用電比上週高 18%」「2 台超過 10 分鐘沒有回報」）。
- `variant`：`cards`（首頁）或 `followups`（回答下方，小的 outline 按鈕）。
- `onPick`：把問題填進 ChatComposer，焦點移到輸入框；不自己送出，讓使用者能改。

## 規則

- 卡片是 `card` 底、`border` 框、`radius-lg` 的按鈕，hover 與選取時換 `brand-soft` 底與 `brand-strong` 框。不要做成填色的小藥丸：那是 StatusBadge 的形狀，而 `brand-soft` 和 `status-normal-soft` 幾乎同色。
- 每張卡左邊一個 16px 的中性圖示（Zap 用電、Thermometer 溫度、Router 閘道器、CalendarDays 報表），顏色 `muted-foreground`；不用狀態圖示，免得被讀成警告。
- 卡片的 `aria-pressed` 標出目前填進輸入框的那一個。
- 一律從資料產生，不放範例文案；沒有設備時改成「新增第一台閘道器」的引導，不顯示建議。
