# 專案目錄

新系統開新的 repo `Laysi/trustiot-v2`，舊 repo `Laysi/trustiot` 封存。雲端服務、閘道器程式與共用 crate 都在同一個 Cargo workspace（決策 4）；前端與腳本用 Bun，版本與 Rust、CI 工具（cargo-deny、gitleaks）一起鎖在 `.prototools`。

```
trustiot-v2/
├── Cargo.toml                 # Cargo workspace：成員、共用 lints、edition、release profile
├── .prototools                # rust、bun、cargo-deny、gitleaks 版本
├── .proto/plugins/            # proto 沒有內建外掛的工具（cargo-deny、gitleaks）：官方下載網址與校驗
├── rust-toolchain.toml        # rustup 依此選 Rust 版本，與 .prototools 相同
├── deny.toml                  # cargo-deny 規則：授權白名單、漏洞公告、相依來源
├── CLAUDE.md                  # agent 規範只放這裡，不另放 AGENTS.md
├── .githooks/pre-commit       # Codex 審查關卡
├── .github/workflows/         # ci.yml、release.yml、nightly.yml（M5，見 docs/architecture/ci-release.md）
├── .github/actions/           # workflow 共用的本地 action，例如 setup-proto（安裝釘選版本的 proto 與工具）
├── api/openapi.json           # 由 Rust 程式碼產生，進版控
├── crates/
│   ├── contract/              # 服務間契約：內部 API 型別、事件格式、錯誤代碼（M1）
│   ├── protocol/              # 閘道器↔雲端的 MQTT 訊息格式（M4）
│   ├── service-kit/           # 服務共用骨架：設定、health、SIGTERM、tracing、驗內部身分、SET LOCAL 租戶
│   ├── outbox/                # 事件框架：同交易寫入、冪等消費、重播
│   ├── openai-client/         # 薄的 OpenAI 相容 client（決策 15）
│   └── testkit/               # 測試用 Modbus 端點與故障注入（M7）、跨租戶負向測試
├── services/
│   ├── identity/              # 身分與租戶（含 ext_authz 端點）
│   ├── assets/                # 資產
│   ├── certs/                 # 憑證與佈建（含 HTTPS 換發端點）
│   ├── ingest/                # 資料接收；ClickHouse DDL
│   ├── telemetry-query/       # 時序查詢
│   ├── gateway-ops/           # 閘道器營運
│   ├── mqtt-bridge/           # WebSocket ↔ TCP 轉接
│   ├── assistant/             # AI 助理
│   ├── ai-gateway/            # AI 閘道（不依賴 contract）
│   ├── alerts/                # 警報與通知
│   ├── field-context/         # 現場脈絡（AGE）
│   └── knowledge/             # 知識庫
├── edge/
│   ├── agent/                 # 閘道器 agent
│   ├── updater/               # 自建 OTA 更新程式
│   └── image/                 # 映像與強化（LUKS、防火牆、systemd）
├── factory/workstation/       # 產線工作站：TPM 取 CSR、呼叫登錄 API
├── frontend/                  # Vite + React SPA
├── tools/
│   ├── migrate/               # migration Job
│   └── repo-check/            # repo 規則檢查，隨 cargo test 執行
├── deploy/
│   ├── chart/                 # Helm chart；values-saas.yaml、values-onprem.yaml
│   ├── flux/                  # SaaS 的 GitOps 設定（Flux）
│   └── host/                  # 主機安裝：k3s、cloudflared（SaaS）
├── scripts/                   # Bun + TS：本地一鍵啟動（M6）、種子資料、CI 檢查（ci/）
├── evals/                     # 題庫：分析、RAG、補全延遲、跨租戶 Cypher 攻擊
├── datasets/lab/              # lab 資料集索引與標註
└── docs/                      # 跨服務架構文件；test-reports/ 放測試報告與原始資料

每個服務內部（以 assets 為例）：
services/assets/
├── README.md                  # 繁中說明
├── migrations/                # 只動自己的 schema
├── src/                       # main.rs、api、domain、db、events
└── tests/                     # 含跨租戶負向測試
```

- 每個服務有自己的 `migrations/`，只動自己的 schema；`tools/migrate` 是部署時的 migration Job，依序跑各服務的 migration 與 ClickHouse DDL，並冪等重建每租戶的資料庫角色（M8）。
- `crates/protocol` 是閘道器與雲端共用的訊息格式（M4），要能編到 aarch64，不引入只能在雲端編譯的相依套件。
- `services/ai-gateway` 不依賴 `crates/contract`，保留單獨抽出開源的選項（決策 28）。
- API 規格從 Rust 程式碼產生（例如 utoipa），各服務的規格合併成 `api/openapi.json` 進版控，CI 檢查是否與程式一致；前端的 API client 由它產生，規格不會與程式脫節。
- 部署以 Helm chart 打包：同一份 chart 搭配 `values-saas.yaml` 與 `values-onprem.yaml`，客戶自有的 k8s 或 OpenShift 也用這份 chart；cloudflared 是 SaaS 主機元件，放在 `deploy/host/`，不在 chart 內。
- 大型資料集放物件儲存，repo 只放索引與標註。
- Workspace 成員：`crates/`、`services/`、`edge/`、`factory/`、`tools/` 底下的每個子目錄都是成員（根目錄 `Cargo.toml` 以 glob 列出，新增 crate 不必改成員清單），必須有 `Cargo.toml`，並以 `edition.workspace = true` 與 `[lints] workspace = true` 繼承 workspace 設定；非 Rust 的子目錄（目前只有 `edge/image`）列在 `exclude`。`frontend/` 與 `scripts/` 用 Bun，不是 workspace 成員。第三方相依套件一律先宣告在根目錄的 `[workspace.dependencies]`。
- Rust 版本同時寫在 `.prototools` 與 `rust-toolchain.toml`：proto 透過 rustup 安裝 Rust、不為 cargo 建 shim，cargo 實際用哪個版本由 rustup 讀 `rust-toolchain.toml` 決定。兩者要一起改，`tools/repo-check` 會檢查一致；它也檢查每個成員繼承 workspace 設定、repo 內沒有 `AGENTS.md`。
- 測試報告與原始資料放 `docs/test-reports/`，命名與內容規則見 [測試報告](../test-reports/README.md)。
