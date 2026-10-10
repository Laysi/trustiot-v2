# api

`openapi.json`：各服務由 Rust 程式碼產生的 OpenAPI 規格合併而成，進版控，不手改。重新產生：`cargo run -p openapi`（規則與新增服務的做法見 [tools/openapi](../tools/openapi/README.md)）。CI 重新產生後有差異就失敗；前端的 API client 由它產生，規格不會與程式脫節。
