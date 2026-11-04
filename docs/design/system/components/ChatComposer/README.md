# ChatComposer

對話的輸入框：多行文字欄加送出鈕，產生回答時送出鈕換成「停止」。它是 TrustIoT 首頁的主角。

新增（對話為主軸，2026-11-01）。首頁空狀態時放在畫面中央、上面是問候、下面是 SuggestedQuestions；對話開始後固定在內容欄底部。

## 使用者提供

- `label`：看得到或只給螢幕閱讀器（`sr-only`）的標籤，例如「問 TrustIoT」；不可以只靠 placeholder。
- `value`、`onChange`、`onSend`；`generating` 與 `onStop` 控制「停止」。
- `placeholder`：用該組織的設備舉例，例如「問問你的設備，例如：冷卻水塔 2 號這週用電多少？」
- `suggestions`：首頁空狀態才給；點一個只會把問題填進輸入框，不會自己送出。

## 規則

- 外框 `card` 底、`input` 1px 框線、`radius-xl`、12px 內距、`shadow-sm`；文字欄 16px（`body`），手機不會自動放大。
- 聚焦時整個外框畫 2px `ring` outline（offset 2px），文字欄本身不畫。
- 送出鈕是預設 Button 的 icon 尺寸（`primary`，也就是品牌色），`aria-label="送出"`；欄位空白時停用。產生中換成 outline 的「停止」（Square 圖示加文字）。
- Enter 送出、Shift + Enter 換行；輸入法組字中（`isComposing`）的 Enter 不送出。提示寫在欄位下方：「Enter 送出，Shift + Enter 換行」。
- 一頁只有一個 ChatComposer。
