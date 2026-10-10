# 專案目錄

新系統開新的 repo `Laysi/trustiot-v2`，舊 repo `Laysi/trustiot` 封存。雲端服務、閘道器程式與共用 crate 都在同一個 Cargo workspace（決策 4）；前端與腳本用 Bun，版本與 Rust、CI 工具（cargo-deny、gitleaks）、chart 與叢集工具（helm、kubeconform、kubectl）、發佈工具（zig、cargo-zigbuild、cargo-auditable、syft、cosign）一起鎖在 `.prototools`。

```
trustiot-v2/
├── Cargo.toml                 # Cargo workspace：成員、共用 lints、edition、release profile
├── .prototools                # rust、bun、cargo-deny、gitleaks、helm、kubeconform、kubectl 與發佈工具的版本
├── .proto/plugins/            # proto 沒有內建外掛的工具（cargo-deny、gitleaks、helm、kubeconform、kubectl、發佈工具）：官方下載網址與校驗
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
│   ├── ingest/                # 資料接收；ClickHouse DDL（migrations/clickhouse/）
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
│   ├── openapi/               # 合併各服務的 OpenAPI 規格，產生 api/openapi.json
│   ├── release-selftest/      # release.yml 的流程自測映像，不是服務；第一個服務映像加入後刪除
│   └── repo-check/            # repo 規則檢查，隨 cargo test 執行
├── deploy/
│   ├── chart/                 # Helm chart；values-saas.yaml、values-onprem.yaml
│   ├── istio/                 # Istio ambient 的安裝 values 與 ext_authz provider（叢集共用的平台元件）
│   ├── flux/                  # SaaS 的 GitOps 設定（Flux）
│   ├── host/                  # 主機安裝：k3s、cloudflared（SaaS）
│   └── images/                # release.yml 發佈的 arm64 映像：每個子目錄一個（image.toml 或 Containerfile）
├── scripts/                   # Bun + TS：本地一鍵啟動（M6，podman）、種子資料、CI 檢查（ci/）、發佈流程（release/）；見 scripts/README.md
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

- 每個服務有自己的 `migrations/`，只動自己的 schema；`tools/migrate` 是部署時的 migration Job，依序跑各服務的 migration 與 ClickHouse DDL（`services/ingest/migrations/clickhouse/`，檔案格式見 [services/ingest/README.md](../../services/ingest/README.md)），並冪等重建每租戶的資料庫角色（M8）。執行期程式碼不改 schema：`tools/repo-check` 擋下 `services/*/src` 裡建表、改表、刪表的 SQL。
- `crates/protocol` 是閘道器與雲端共用的訊息格式（M4），要能編到 aarch64，不引入只能在雲端編譯的相依套件。2026-10-10 定案：MQTT 5、topic 樹 `trustiot/v1/gw/{閘道器憑證 CN}/{up|down}/{channel}`、payload 以 CBOR 編碼、每種訊息有大小上限與拆包規則，以 `v`（主版本）與 `rev`（修訂版）管理相容性並以凍結的 v1 固定樣本測試；完整規格與選 CBOR 的理由在 [`crates/protocol/README.md`](../../crates/protocol/README.md)。
- `services/ai-gateway` 不依賴 `crates/contract`，保留單獨抽出開源的選項（決策 28）。
- API 規格從 Rust 程式碼產生（utoipa），各服務的規格合併成 `api/openapi.json` 進版控，CI 檢查是否與程式一致；前端的 API client 由它產生，規格不會與程式脫節。每個有 HTTP API 的服務在 library 匯出 `pub fn openapi() -> utoipa::openapi::OpenApi`，登記在 `tools/openapi`；`cargo run -p openapi` 把各服務的規格與 `crates/contract` 的 M1 錯誤 schema 合併（鍵排序固定、同一路徑與方法、同名但不同定義的 schema、重複的 operationId、找不到目標的 `$ref` 都是錯誤，不會靜默覆蓋），寫入 `api/openapi.json`。CI 重新產生後有差異就失敗，`cargo test --workspace` 也做同一項比對。細節見 [tools/openapi](../../tools/openapi/README.md)。
- 部署以 Helm chart 打包：同一份 chart 搭配 `values-saas.yaml` 與 `values-onprem.yaml`，客戶自有的 k8s 或 OpenShift 也用這份 chart；cloudflared 是 SaaS 主機元件，放在 `deploy/host/`，不在 chart 內；Istio ambient 是整個叢集共用的平台元件，安裝 values 與入口用的 ext_authz provider 放在 `deploy/istio/`，裝 chart 之前先裝。values 分三層：`values.yaml` 是預設值；`values-saas.yaml` 與 `values-onprem.yaml` 只寫部署型態的差異；主機名稱、外部端點與 Secret 名稱寫在安裝實例的覆寫檔（SaaS 的在 `deploy/flux/`，地端由客戶安裝時提供）。values 只以名稱引用 Secret，不放密鑰；`values.schema.json` 擋下缺漏與不安全的設定。欄位與 chart 對服務的約定見 [deploy/chart/README.md](../../deploy/chart/README.md)。
- 本地開發環境（M6）用 rootless podman 起容器，不跑 Kubernetes：`bun scripts/dev.ts up` 起 Postgres（含 AGE）、ClickHouse、Mosquitto 與 `lib/services.ts` 列出的服務，帳密每次產生、只存在使用者的 XDG 狀態目錄，所有埠只綁 127.0.0.1。服務的容器介面照 chart 的約定（8080、`/healthz`、`/readyz`、`TRUSTIOT_*` 環境變數），日後改用 k3d 只需換掉啟動容器的那一層。決定的理由、與 k3d 方案的取捨及與正式環境的差異見 [scripts/README.md](../../scripts/README.md)。
- 容器映像在 `deploy/images/<名稱>/` 宣告，目錄名就是映像名與 chart 的服務名：Rust 服務放 `image.toml` 指定 Cargo 套件，其他映像放自己的 `Containerfile`。release.yml 依目錄發佈，新增映像不必改 workflow，見 [CI 與發佈](ci-release.md) 的「release.yml」。
- 大型資料集放物件儲存，repo 只放索引與標註。
- Workspace 成員：`crates/`、`services/`、`edge/`、`factory/`、`tools/` 底下的每個子目錄都是成員（根目錄 `Cargo.toml` 以 glob 列出，新增 crate 不必改成員清單），必須有 `Cargo.toml`，並以 `edition.workspace = true` 與 `[lints] workspace = true` 繼承 workspace 設定；非 Rust 的子目錄（目前只有 `edge/image`）列在 `exclude`。`frontend/` 與 `scripts/` 用 Bun，不是 workspace 成員。第三方相依套件一律先宣告在根目錄的 `[workspace.dependencies]`。
- Rust 版本同時寫在 `.prototools` 與 `rust-toolchain.toml`：proto 透過 rustup 安裝 Rust、不為 cargo 建 shim，cargo 實際用哪個版本由 rustup 讀 `rust-toolchain.toml` 決定；CI 則直接以 rustup 依 `rust-toolchain.toml` 安裝（原因見 [CI 與發佈](ci-release.md) 的「工具安裝」）。兩者要一起改，`tools/repo-check` 會檢查一致；它也檢查每個成員繼承 workspace 設定、repo 內沒有 `AGENTS.md`。
- 測試報告與原始資料放 `docs/test-reports/`，命名與內容規則見 [測試報告](../test-reports/README.md)。
