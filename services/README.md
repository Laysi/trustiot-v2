# services

雲端微服務，一個子目錄一個服務（決策 4）。服務清單見[專案目錄](../docs/architecture/repository-layout.md)，切分與流量路徑見[部署與可搬移性](../docs/architecture/deployment.md)。

- 每個子目錄都是 workspace 成員（根目錄 `Cargo.toml` 的 `services/*`），必須有 `Cargo.toml`，並繼承 workspace 的 edition 與 lints（`cargo test -p repo-check` 會檢查）。
- 服務內部：`README.md`（繁中說明）、`migrations/`（只動自己的 schema）、`src/`、`tests/`（含跨租戶負向測試：以 [crates/testkit](../crates/testkit/README.md) 宣告每張表與每個端點，漏宣告的會讓測試失敗）。
- 有 HTTP API 的服務在 library 匯出 `pub fn openapi() -> utoipa::openapi::OpenApi`，並登記在 `tools/openapi`，規格才會進 `api/openapi.json`；沒有 HTTP API 的服務也要在那裡登記為 `SERVICES_WITHOUT_API`，否則 `cargo test --workspace` 失敗（做法見 [tools/openapi](../tools/openapi/README.md)）。
- 服務之間只透過 `crates/contract` 的型別、事件與有型別的 client 溝通，不跨服務 JOIN；`services/ai-gateway` 不依賴 `crates/contract`（決策 28）。
- 需要另一個服務的資料時，消費它的 outbox 事件，在自己的 schema 維護投影（例如 ingest、certs、gateway-ops 的閘道器綁定，gateway-ops 的型號與設備），在 `tools/migrate` 的 registry 登記為消費端，並把服務加進 chart 的 `trustiot.migrate.platformLogins`（平台角色讀 producer 的事件）。
