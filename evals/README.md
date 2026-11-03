# evals

AI 評測題庫：分析題（MS3 與舊系統比對）、RAG 檢索（見[知識與文件庫](../docs/architecture/rag.md)）、輸入補全延遲、跨租戶 Cypher 攻擊（MS5）。題庫用來選模型與參數，也當回歸測試。

題庫的指標測試 2026-11-03 起暫停（維護者的決定，原因是費用；[docs/architecture/ai-assistant.md](../docs/architecture/ai-assistant.md)「驗證」）。題目照常寫進這裡，但不對付費模型跑；評測工具在 Laysi/trustiot-v2#223。

## 儀表板題（`dashboard/questions.json`）

「叫出儀表板」的題目（Laysi/trustiot-v2#935）：使用者要一個元件畫得出的儀表板時，主 agent 要寫元件模式的規格，不寫自訂頁面（決策 33 的 2026-11-01 修訂，[docs/architecture/ai-assistant.md](../docs/architecture/ai-assistant.md)「元件模式」）。這是題庫裡第一份有格式的題目；分析題的格式由 Laysi/trustiot-v2#159 訂，這份只取能讓評測工具（Laysi/trustiot-v2#223）自動評分的最小欄位。

### 格式（format 1）

JSON，不認得的欄位一律拒絕。最上層：

| 欄位 | 說明 |
| --- | --- |
| `format` | 格式的版本，目前是 `1` |
| `questions` | 題目，見下表 |

每題：

| 欄位 | 說明 |
| --- | --- |
| `id` | `dashboard-` 加小寫英文、數字與 `-`，不重複；報告以它對照 |
| `question` | 使用者的問題，原文照送（zh-TW） |
| `expect.kinds` | 正確的答案一定要有的元件種類（`time_series`、`bar`、`kpi`、`table`、`status`），至少一個；可以另外有別的元件 |
| `expect.custom_page` | 能不能有自訂頁面；這份題目都是 `false` |
| `reference` | 一份正確的答案：`dashboard` 工具的一次 `write` 輸入，建立只有元件的新儀表板。它只證明這題元件畫得出來，評分不拿模型的規格和它比對（SQL 與元件的寫法可以不同） |

`cargo test -p assistant the_question_banks_references_are_component_writes` 檢查這個檔案：格式、`id`、每題的 `reference` 經工具的輸入解析與合併後通過 contract 的 `validate_version`、沒有頁面也沒有 `custom_page` 項目、畫出 `expect.kinds` 的每一種；五種元件每種至少有一題問到。它不執行資料集，也不呼叫模型。

### 評分（評測工具照這個做）

每題開一個新對話問 `question`，資料是 lab 的資料集（[datasets/lab](../datasets/lab/README.md)；題目用到的資料點是合成 fixture 的 `power_kw` 與 `energy_kwh`）。一題通過的條件：

1. 這次提問最後一次存下的 `dashboard` 寫入（工具結果 `saved: true`）存在，也就是模型的規格通過了驗證、資料集執行成功、元件與結果相符；
2. 那一版沒有頁面，元件清單沒有 `custom_page` 項目；
3. 元件清單有 `expect.kinds` 的每一種；
4. 回答沒有要移除的 SQL、HTML 或程式碼（`answer_guard` 的 `Answer::removed` 是空的）。

結果存成測試報告（[docs/test-reports](../docs/test-reports/README.md)），每題記下通過與否、寫入次數、被拒的問題種類、元件種類與 token 用量。
