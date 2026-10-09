# 舊系統功能盤點

舊系統目前的每一項功能依模組列出如下，重建時都要有對應。角色中的「成員」包含 viewer、editor、admin；「系統」代表背景自動執行。現況分為可用、部分、有缺陷、只有規劃、沒有。

## 登入與帳號

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| Email 驗證碼登入：8 碼、填滿自動送出、重送、更換 email | 所有人 | 可用 |
| 登入後導回原頁，只接受同站網址 | 所有人 | 部分：只檢查開頭是 / 且不是 //，以 /\\ 開頭的網址可能被瀏覽器正規化成外站（未實測） |
| Session 自動續期：每 45 分鐘，以及切回分頁時（最多每 5 分鐘一次）；暫時性錯誤不登出 | 所有人 | 可用 |
| 登出並全域撤銷登入 | 所有人 | 可用：全域撤銷失敗時會被忽略 |
| 開發用帳密登入 | 開發者 | 可用（僅本地） |
| 「尚未加入任何租戶」畫面 | 未加入租戶的使用者 | 可用 |
| 切換租戶 | 屬於多個租戶的成員 | 部分：只顯示 ID 與角色 |
| 停權鎖定橫幅與停用輸入框；停權期間每 60 秒重查，恢復後自動解鎖 | 成員 | 可用 |
| SSO、依 email 網域自動加入 | — | 只有規劃（[#47](https://github.com/Laysi/trustiot/issues/47)） |
| 自助註冊與試用 | — | 只有規劃（[#8](https://github.com/Laysi/trustiot/issues/8)） |

## 租戶管理（平台後台）

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 建立租戶（名稱全域唯一，同時建立預設配額：閘道器 5、設備 50、型號 20、AI 每月 10 美元） | platform admin | 可用，但名稱重複時回 500，並殘留 AppConfig profile |
| 租戶列表，含停權中的租戶 | platform admin | 可用 |
| 軟刪除租戶 | platform admin | 有缺陷：成員仍可存取（[#117](https://github.com/Laysi/trustiot/issues/117)） |
| 停權與恢復租戶 | platform admin | 部分：只有 API，沒有畫面；停權後資料上傳與閘道器部署不會停（[#72](https://github.com/Laysi/trustiot/issues/72)） |
| 修改租戶名稱 | 租戶 admin | 部分：只有 API |
| 調整配額：閘道器、設備、型號數量與 AI 月額度 | platform admin | 部分：畫面只能調 AI 額度 |
| AI 月額度當月臨時調高 | platform admin | 可用 |
| 租戶 AI 用量卡：本月花費、上限、是否超額 | platform admin | 可用 |
| 模型定價：四種角色的模型 ID、名稱、輸入與輸出單價；main、tiny、decision 約 30 秒內生效，embedding 只有定價、目前沒在用 | platform admin | 可用 |
| 方案與計費頁 | — | 只有規劃（[#38](https://github.com/Laysi/trustiot/issues/38)、[#39](https://github.com/Laysi/trustiot/issues/39)、[#9](https://github.com/Laysi/trustiot/issues/9)） |
| 建立第一位 platform admin | 平台人員 | 沒有：要手動在 Cognito 與 DB 建立 |

## 成員與權限

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 成員列表 | 租戶 admin | 可用 |
| 用 email 加入成員，帳號不存在時自動建立 | 租戶 admin | 可用，但有治理問題（[#46](https://github.com/Laysi/trustiot/issues/46)） |
| 修改角色：viewer、editor、admin | 租戶 admin | 可用 |
| 移除成員，保留最後一位 admin | 租戶 admin | 可用（競態 [#107](https://github.com/Laysi/trustiot/issues/107)） |
| 成員救援：可跳過最後一位 admin 的限制 | platform admin | 可用 |
| 邀請與同意流程 | — | 只有規劃（[#46](https://github.com/Laysi/trustiot/issues/46)） |

## 總覽

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 租戶總覽：名稱、狀態、自己的角色、配額上限 | 成員 | 部分：只有上限，沒有實際用量 |
| 租戶設定 JSON 檢視 | 成員 | 可用（除錯用） |

## 閘道器

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 閘道器列表：名稱、佈建狀態、建立時間 | 成員 | 部分：沒有在線狀態（[#65](https://github.com/Laysi/trustiot/issues/65)、[#19](https://github.com/Laysi/trustiot/issues/19)） |
| 零接觸綁定：輸入硬體 ID 或 TPM 指紋，選填顯示名稱 | editor 以上 | 可用（失敗後卡住 [#84](https://github.com/Laysi/trustiot/issues/84)） |
| 綁定失敗節流：10 分鐘內失敗 10 次就鎖住；被擋下的嘗試也算失敗，持續重試會延長鎖定 | 系統 | 可用 |
| 手動建立閘道器並下載安裝包：10 分鐘有效、只能下載一次，附線上與離線安裝步驟 | editor 以上 | 部分：過期後沒有補救路徑；token 不是真正一次性且放在網址（[#105](https://github.com/Laysi/trustiot/issues/105)） |
| 解綁，閘道器回到待綁定 | platform admin | 部分：只有 API，沒有畫面 |
| 除役：憑證停用、不可逆 | platform admin | 可用 |
| 依硬體 ID 除役尚未綁定的機器（送修） | platform admin | 可用 |
| 刪除手動建立的閘道器 | platform admin | 有缺陷：只做軟刪除，憑證不撤銷、Thing 不清除 |

## 出廠登錄（平台後台）

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 單筆登錄：硬體 ID、TPM 指紋、thing 名稱 | platform admin | 可用 |
| CSV 批次登錄，結果分為新增、已存在、衝突、已除役 | platform admin | 可用 |
| 產線登錄 API | 產線 | 部分：唯一呼叫方是操作員工作機的註冊腳本，與 AI 用量共用同一把內部密鑰 |
| 設備池瀏覽：狀態、綁定租戶、佈建時間（最新 500 台） | platform admin | 可用 |
| 佈建事件紀錄（最近 100 筆） | platform admin | 可用 |
| 閘道器首次連線時的放行檢查 | 系統 | 部分：信任自報 ID（[#118](https://github.com/Laysi/trustiot/issues/118)） |
| 零接觸出廠映像：內嵌共用 claim 憑證與自動佈建設定 | 平台人員 | 部分：只有手動 runbook，裝置端沒有腳本（[#64](https://github.com/Laysi/trustiot/issues/64)） |

## 設備型號

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 型號建立、編輯、刪除；名稱建立後不可改 | editor 以上（viewer 唯讀） | 可用 |
| 資料點編輯器：表單、JSON、YAML 三種模式 | editor 以上 | 可用 |
| 資料點驗證：功能碼 1–4、位址不重疊、字串需長度、名稱不重複 | 系統 | 部分：只有 AI 路徑會驗證（[#103](https://github.com/Laysi/trustiot/issues/103)） |
| knowledge 文字：給 AI 判讀故障碼的依據 | editor 以上 | 可用 |
| 仍有設備使用的型號不可刪除 | 系統 | 可用 |
| 型號與設備名稱規則：小寫英數加單一連字號，1–40 字 | 系統 | 部分：只有管理畫面的 API 會檢查，AI 上架路徑不檢查 |

## 設備

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 設備建立、編輯、刪除；名稱、閘道器、型號建立後不可改 | editor 以上（viewer 唯讀） | 可用 |
| 連線設定：modbus-tcp、modbus-rtu-tcp、mock；client 或 server 模式；ip、port、站號（預設 1） | editor 以上 | 可用，但未驗證輸入 |
| 取樣間隔 | editor 以上 | 可用，但未驗證輸入 |
| 現場備註 assetNotes，給 AI 的實體背景 | editor 以上 | 可用 |
| 超過配額的提示 | 系統 | 可用 |
| 設備狀態、即時值、時序圖表畫面 | — | 沒有 |
| 設備警報與門檻通知 | — | 沒有（[#20](https://github.com/Laysi/trustiot/issues/20)），由新功能「AI 警報規則」補上 |

## 邊緣運作（閘道器背景執行）

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 閘道器、設備、型號變更後，自動編譯設定並下發到閘道器 | 系統 | 有缺陷：可能讀到舊版（[#139](https://github.com/Laysi/trustiot/issues/139)） |
| 閘道器回報設定套用結果（成功或錯誤訊息） | 系統 | 部分：畫面上看不到 |
| Modbus 讀取：功能碼 1–4、合併讀取、各型別解碼 | 閘道器 | 可用 |
| 同一台 RS485 轉換器下的多台設備 | 閘道器 | 有缺陷（[#183](https://github.com/Laysi/trustiot/issues/183)） |
| 被動模式（設備連進閘道器） | 閘道器 | 部分：未實測 |
| 斷線重連、逾時、退避重試 | 閘道器 | 可用 |
| 批次上傳（預設每 15 分鐘） | 閘道器 | 有缺陷：過大會崩潰（[#178](https://github.com/Laysi/trustiot/issues/178)） |
| 離線緩衝：只存記憶體，每台設備最多 1 萬筆 | 閘道器 | 部分（[#177](https://github.com/Laysi/trustiot/issues/177)、[#179](https://github.com/Laysi/trustiot/issues/179)） |
| 設備在線狀態與最新值回報 | 閘道器 | 有缺陷：設備斷線後仍顯示在線，沒有 LWT 或心跳（[#160](https://github.com/Laysi/trustiot/issues/160)、[#19](https://github.com/Laysi/trustiot/issues/19)）；任何設定變更都會讓全租戶設備的最新值歸零、顯示離線，最長約 15 分鐘 |
| 模擬設備（mock） | 閘道器 | 可用 |
| 開機後自動取得設定：未就緒時退避重試，拿不到租戶 ID 就每 30 秒重抓 | 閘道器 | 可用（[#156](https://github.com/Laysi/trustiot/issues/156)） |

## 時序資料（雲端背景執行）

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 接收閘道器上傳並寫入 ClickHouse | 系統 | 部分：會有重複資料（[#104](https://github.com/Laysi/trustiot/issues/104)） |
| 依型號自動建表，欄位只增不刪，欄位說明給 AI 讀 | 系統 | 部分：可被注入（[#103](https://github.com/Laysi/trustiot/issues/103)） |
| 處理失敗時重試；連續失敗約 10 小時（200 次）轉入死信佇列，死信保留 14 天、要手動撈回 | 系統 | 可用，但沒有告警 |
| 資料保留期限 | — | 沒有（[#41](https://github.com/Laysi/trustiot/issues/41)） |

## AI 助理

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 對話串：建立、列表（最近 50 筆）、刪除，依使用者隔離 | 成員 | 可用，但產生中刪除對話會讓整個聊天停擺（[#88](https://github.com/Laysi/trustiot/issues/88)）；改名與封存只有 API，沒有畫面 |
| 自動產生對話標題 | 系統 | 可用 |
| 重新產生回答、編輯已送出的訊息 | 成員 | 可用（PR [#172](https://github.com/Laysi/trustiot/issues/172) 已修，[#171](https://github.com/Laysi/trustiot/issues/171)） |
| 複製回答、匯出 Markdown、切換重新產生的不同版本 | 成員 | 可用 |
| 即時顯示進度與推理過程 | 成員 | 可用；回答本身不逐字串流，完成後一次顯示 |
| 停止產生 | 成員 | 部分：只停前端顯示，伺服器端仍繼續執行並計費 |
| 問題分類與必要時反問釐清 | 系統 | 可用 |
| 閒聊快速路徑：小模型直接回答，不進分析流程 | 系統 | 可用 |
| 時序資料分析：查詢、計算、碳排、月增率與年增率 | 成員 | 部分：長分析會中斷（[#99](https://github.com/Laysi/trustiot/issues/99)）、回應慢（[#146](https://github.com/Laysi/trustiot/issues/146)）；prompt 寫死示範資料表，每次分析都先白跑一次失敗的查詢 |
| 查詢護欄：只准單一 SELECT、禁外部 table function、預設 100 筆、最多 500 筆 | 系統 | 部分：用正規表示式，可被繞過（[#102](https://github.com/Laysi/trustiot/issues/102)） |
| 最終覆審：decision 模型依查到的資料改寫草稿、不得新增數字；完成後清掉中間的工具與 SQL 訊息 | 系統 | 可用 |
| AI 繪製長條圖與折線圖 | 成員 | 可用 |
| ISO 50001 報告：下載 HTML，由瀏覽器列印成 PDF | 成員 | 部分：驗證狀態與簽章欄是假值 |
| 對話式設備上架：列出、驗證、建型號、建設備、改備註、讀設備資訊 | editor 以上（viewer 只能讀） | 可用 |
| AI 額度徽章與超額阻擋 | 成員 | 可用，但檢查失敗時會放行（[#53](https://github.com/Laysi/trustiot/issues/53)）；超額時也保存被擋下的訊息 |
| AI 用量記帳：每次模型呼叫記錄 token 成本 | 系統 | 部分：非同步送出，失敗只寫 log（[#108](https://github.com/Laysi/trustiot/issues/108)） |
| 租戶停權時拒絕對話，並保存被擋下的訊息 | 系統 | 部分：檢查失敗時放行；membership 快取暖機後可繞過（[#72](https://github.com/Laysi/trustiot/issues/72)） |
| 依瀏覽器時區解讀與查詢時間 | 系統 | 可用 |
| 服務重啟時把中斷的對話標為失敗，讓使用者重送 | 系統 | 可用 |
| 不透露底層 AI 供應商，出錯時不講技術細節 | 系統 | 可用（只靠 prompt） |
| 共用文件庫（新增與讀取文件） | 成員 | 有缺陷：跨租戶，viewer 也能寫入（[#101](https://github.com/Laysi/trustiot/issues/101)） |
| 上傳附件給 AI | — | 只有規劃：按鈕在但未串接（[#126](https://github.com/Laysi/trustiot/issues/126)） |
| 對話網址與深連結 | — | 只有規劃（[#138](https://github.com/Laysi/trustiot/issues/138)） |
| MCP 對外介面、對話內回報 bug | — | 只有規劃（[#136](https://github.com/Laysi/trustiot/issues/136)、[#135](https://github.com/Laysi/trustiot/issues/135)） |

## 知識與文件庫（RAG）

已上線的只有型號知識與設備備註；多模態設備文件庫的作法已在 [#28](https://github.com/Laysi/trustiot/issues/28)、[#124](https://github.com/Laysi/trustiot/issues/124)、[#44](https://github.com/Laysi/trustiot/issues/44)、[#34](https://github.com/Laysi/trustiot/issues/34) 定案（2026-07-17），尚未實作。

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 型號知識 knowledge：使用者維護的代碼表與語意，AI 視為權威（第 1 層） | editor 以上 | 可用 |
| 設備現場備註 assetNotes：AI 判讀用的實體背景 | editor 以上 | 可用 |
| 回答層級規則：型號知識 → 遙測佐證 → 未定義就明說，不得臆測；文件庫這一層尚未加入 | 系統 | 部分：只靠 prompt（[#4](https://github.com/Laysi/trustiot/issues/4)、[#6](https://github.com/Laysi/trustiot/issues/6)） |
| 設備文件庫：上傳、列表、刪除、下載原廠手冊與故障碼表 PDF，掛在型號下 | editor 以上 | 只有規劃（[#28](https://github.com/Laysi/trustiot/issues/28)） |
| 背景處理：PDF 逐頁轉圖與重疊裁切，產生多模態向量；可重試、可查狀態 | 系統 | 只有規劃（[#34](https://github.com/Laysi/trustiot/issues/34)、[#124](https://github.com/Laysi/trustiot/issues/124)） |
| 視覺檢索：依租戶與型號搜尋頁面，重排序後由看圖模型閱讀，引用附文件名與頁碼（第 1.5 層） | 成員 | 只有規劃（[#124](https://github.com/Laysi/trustiot/issues/124)） |
| 檢索結論經使用者核可後寫回型號知識 | editor 以上 | 只有規劃（[#28](https://github.com/Laysi/trustiot/issues/28)） |
| 文件庫的租戶隔離：向量、原始檔、處理工作全路徑 fail-closed | 系統 | 只有規劃（[#44](https://github.com/Laysi/trustiot/issues/44)） |
| 網路搜尋草稿層：附來源、標未驗證，核可後寫回知識（[#6](https://github.com/Laysi/trustiot/issues/6)） | — | 只有規劃 |
| AI 自產記憶與知識圖 | — | 只有規劃（[#152](https://github.com/Laysi/trustiot/issues/152)–[#155](https://github.com/Laysi/trustiot/issues/155)） |
| 事件紀錄：agent 工具呼叫與結論、人的操作、異常與處置、使用者回饋（append-only） | 系統 | 只有規劃（[#153](https://github.com/Laysi/trustiot/issues/153)） |
| 機台實體與關係：機器、產線與 Modbus 端點掛鉤，取代自由文字備註 | editor 以上 | 只有規劃（[#154](https://github.com/Laysi/trustiot/issues/154)） |

## 共通功能

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 深色與淺色主題，預設跟隨系統 | 所有人 | 可用；切換鈕與登出鈕只在對話頁 |
| 多語系 | — | 只有規劃：文字寫死，以繁中為主，對話介面混有英文（[#66](https://github.com/Laysi/trustiot/issues/66)–[#70](https://github.com/Laysi/trustiot/issues/70)） |
| 錯誤頁與錯誤追蹤 | — | 沒有（[#163](https://github.com/Laysi/trustiot/issues/163)、[#166](https://github.com/Laysi/trustiot/issues/166)–[#169](https://github.com/Laysi/trustiot/issues/169)）；只有讀取使用者資訊失敗時的重試畫面 |
| 操作紀錄 | — | 部分：只有閘道器佈建事件 |
| 服務日誌 | 開發者 | 部分：只有容器 stdout 與 CloudWatch，沒有集中查詢與告警 |
| 閘道器日誌 | 平台人員 | 部分：元件日誌寫在 Pi 的 SD 卡（INFO 級、上限 50 MB），要 SSH 進去看；系統 journal 只在記憶體；不上傳雲端 |

## 維運與開發工具

| 功能 | 角色 | 現況 |
| --- | --- | --- |
| 本地一鍵開發環境，可選配示範資料 | 開發者 | 可用 |
| stage 部署（rc tag 觸發） | 開發者 | 可用 |
| 閘道器程式發佈 | 開發者 | 可用（手動觸發）；推送到現場要手動建 deployment，沒有分批與回滾 |
| 閘道器映像製作：燒錄、強化、TPM 金鑰、註冊 | 平台人員 | 部分（[#140](https://github.com/Laysi/trustiot/issues/140)、[#119](https://github.com/Laysi/trustiot/issues/119)、[#121](https://github.com/Laysi/trustiot/issues/121)） |
| Modbus 設備模擬器：5 種設備，含 MS300 變頻器模型 | 開發與展示 | 可用 |
| 模擬行為庫：工作排程、季節、故障事件、設備連動 | 開發者 | 可用 |
| 測試資料產生器（iot-data-generator） | 開發者 | 有缺陷：建置已壞、script API 未驗證就執行程式碼（[#106](https://github.com/Laysi/trustiot/issues/106)、[#115](https://github.com/Laysi/trustiot/issues/115)） |
| 閘道器元件部署 CLI（aiot-dev） | 開發者 | 部分 |
| PR 自動檢查：各元件測試與 lint，加上資料格式漂移檢查 | 開發者 | 可用 |
| rc tag 自動產生 AI 發佈說明 | 開發者 | 可用 |
| AWS 維運腳本：清 AppConfig 舊版本、刪 Greengrass 部署、取 Cognito token | 開發者 | 可用，但 ID 寫死 |
| 資料備份與還原 | — | 沒有（[#15](https://github.com/Laysi/trustiot/issues/15)） |
