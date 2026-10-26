# ai-gateway-api

AI 閘道管理 API 的線上格式（Laysi/trustiot-v2#606）：模型角色的列表、更換的請求本文、操作者標頭與錯誤代碼。閘道（`services/ai-gateway/src/admin.rs`）與唯一的呼叫端身分服務（Laysi/trustiot-v2#608）共用這一份定義，兩邊不會各寫一份而走樣。路由、狀態碼與驗證方式寫在 [services/ai-gateway/README.md](../../services/ai-gateway/README.md)「管理 API」與「驗證方式」。

規則：

- 只有 serde 型別與常數，沒有 HTTP client 或 server。
- 不依賴 `crates/contract`，也不依賴任何會帶進它的 crate（決策 28：AI 閘道與產品無關，用到的 crate 也一樣）。`tests/no_contract.rs` 以 `cargo tree -p ai-gateway-api` 檢查。
- 回應寬鬆讀取：不認得的欄位略過，不認得的錯誤代碼讀成 `ErrorCode::Unknown`，所以閘道之後可以加欄位與代碼，不必同時更新身分服務。請求本文 `SetModels` 嚴格讀取（`deny_unknown_fields`）。
- `role_path` 只接受角色名稱（1 到 32 個小寫英文字母、數字或 `_`，字母開頭），呼叫端不會把其他文字拼進路徑。

測試：`cargo test -p ai-gateway-api`（格式的固定 JSON、錯誤代碼與狀態碼、操作者與角色名稱的規則、沒有 contract）。
