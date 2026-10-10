# services

雲端微服務，一個子目錄一個服務（決策 4）。服務清單見[專案目錄](../docs/architecture/repository-layout.md)，切分與流量路徑見[部署與可搬移性](../docs/architecture/deployment.md)。

- 每個子目錄都是 workspace 成員（根目錄 `Cargo.toml` 的 `services/*`），必須有 `Cargo.toml`，並繼承 workspace 的 edition 與 lints（`cargo test -p repo-check` 會檢查）。
- 服務內部：`README.md`（繁中說明）、`migrations/`（只動自己的 schema）、`src/`、`tests/`（含跨租戶負向測試）。
- 服務之間只透過 `crates/contract` 的型別、事件與有型別的 client 溝通，不跨服務 JOIN；`services/ai-gateway` 不依賴 `crates/contract`（決策 28）。
