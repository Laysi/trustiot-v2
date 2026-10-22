# openapi

產生 `api/openapi.json`：各服務從 Rust 程式碼匯出的 OpenAPI 規格，加上 `crates/contract` 的 M1 錯誤 schema，合併成一份進版控的規格。前端的 API client 由它產生（Laysi/trustiot-v2#123），規格不會與程式脫節。

| 指令 | 作用 |
| --- | --- |
| `cargo run -p openapi` | 重新產生並寫入 `api/openapi.json`（改了 API 型別後執行，連同結果一起提交） |
| `cargo run -p openapi -- --check` | 只比對，不寫檔；與程式不一致時以 1 結束並指出第一個差異的行 |
| `cargo test -p openapi` | 單元測試，以及 `tests/openapi.rs` 對本 repo 的比對 |

CI（`rust` job 的「OpenAPI spec is current」）重新產生後有任何差異就失敗並印出差異；`cargo test --workspace` 也跑 `committed_spec_is_what_the_code_generates`，所以忘了重新產生的變更在本機測試就會失敗。

## 輸出

- 檔案是兩格縮排的 JSON，所有物件的鍵依字母排序、結尾一個換行；同樣的程式一定產生同樣的文字，所以差異檢查才有意義。
- `info.version` 是 `lib.rs` 的 `API_VERSION`，代表 API 契約的版本，不是軟體版本：發版不會改動規格。契約有用戶端必須注意的改變時才手動調高。
- 內容：`components.schemas` 一定含 `ErrorBody`、`ErrorCode`、`FieldError`、`MessageKey`（M1，來源是 `contract::openapi::SharedSchemas`）。服務的規格可以直接以 `#/components/schemas/ErrorBody` 描述錯誤回應，不必自己宣告。

## 合併規則

合併不會靜默覆蓋任何東西，下列情況一律是錯誤，訊息指出是哪兩個來源：

- 兩個來源對同一路徑定義同一個方法（入口 gateway 依路徑前綴轉送，路徑必須屬於單一服務）。
- 同名 schema（或其他 `components` 項目）定義不同；定義完全相同則合併。
- 同名 tag 定義不同。
- `operationId` 重複（產生 client 需要它唯一）。
- 合併後有找不到目標的 `#/components/...` 參照。
- 來源的 OpenAPI 版本不同，或來源設了 `servers`、`security` 等整份規格層級的欄位：這些會改變其他服務的意義，不能由單一服務決定。

## 新增服務

1. 服務的 library 匯出 `pub fn openapi() -> utoipa::openapi::OpenApi`，通常是 `#[derive(utoipa::OpenApi)]` 型別的 `openapi()`，列出它的 handler（`#[utoipa::path]`）與 schema。請求、回應與錯誤型別都要有 `ToSchema`；錯誤回應用 `contract::ErrorBody`。
2. 在 `tools/openapi/Cargo.toml` 加上該服務的相依，在 `src/lib.rs` 的 `SERVICES` 加一行 `Source { name: "<目錄名>", openapi: <crate>::openapi }`。`name` 就是 `services/` 底下的目錄名。
3. 沒有 HTTP API 的服務（例如純背景工作）改列在 `SERVICES_WITHOUT_API`，這是一項要審查的決定。
4. `cargo run -p openapi` 並提交 `api/openapi.json`。

`tests/openapi.rs` 的 `every_service_directory_is_registered` 檢查 `services/` 的每個子目錄恰好列在 `SERVICES` 或 `SERVICES_WITHOUT_API` 其中之一，所以新服務不會被漏掉。`services/ai-gateway` 列在 `SERVICES_WITHOUT_API`：它的內部 API 收發供應商的原生協定（Anthropic Messages、OpenAI Decisions），格式由供應商的 API 文件定義，閘道自己的部分（路徑、標頭、錯誤）寫在它的 README；它也不依賴 `crates/contract`（決策 28），而這裡每份規格共用 contract 的錯誤型別。本工具不是服務，可以依賴 contract。

`crates/service-kit` 的 `example-service` 只用於示範與測試，不是產品服務，不進 `api/openapi.json`。
