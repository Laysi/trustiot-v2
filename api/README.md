# api

`openapi.json`：各服務由 Rust 程式碼產生的 OpenAPI 規格合併而成，進版控，不手改。CI 重新產生後有差異就失敗；前端的 API client 由它產生，規格不會與程式脫節。
