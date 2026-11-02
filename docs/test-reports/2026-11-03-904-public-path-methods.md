# 入口例外路徑：固定登入與健康檢查的路徑，方法清單與服務的路由對照

對應 issue：Laysi/trustiot-v2#904（來源：Laysi/trustiot-v2#859 安全審查的 Low-1、Low-2）。

## 環境

- 建置主機（arm64 Linux VM），在工作樹的複本上執行；分支 `fix/904-pin-ingress-public-paths`，基於 main `2b29ffdd`，改動尚未提交時的工作樹。
- 工具版本都照 `.prototools` 與 `rust-toolchain.toml`（Rust 1.99.0、Bun 1.4.2、helm 與 kubeconform 照 pin）。
- `bun test deploy/flux` 在本機執行。

## 指令

從 repo 根目錄執行：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --exclude agent
cargo test --locked -p repo-check
bun install --cwd scripts --frozen-lockfile
bun test scripts/ci/changes.test.ts
bun test deploy/chart
bun run --cwd scripts typecheck
bun test deploy/flux
```

變異測試：每次改一個檔案的一處（`sed -i`），跑 `cargo test -p repo-check --test repository ingress_public_path`，再還原。改了哪裡見下方表格，完整輸出在 [data/2026-11-03-904-public-path-methods/mutations.txt](data/2026-11-03-904-public-path-methods/mutations.txt)。

## 結果

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -D warnings` | 通過 |
| `cargo test --workspace --exclude agent` | 284 個 test result 全部 ok：2421 passed、0 failed、9 ignored |
| `cargo test -p repo-check` | 單元測試 26 passed（含 `src/public_paths.rs`），`tests/repository.rs` 11 passed（含 `ingress_public_path_methods_are_the_services_methods`） |
| `bun test scripts/ci/changes.test.ts` | 345 pass、0 fail |
| `bun test deploy/chart` | 838 pass、0 fail |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun test deploy/flux` | 79 pass、0 fail |

變異測試（`ingress_public_path_methods_are_the_services_methods`）：

| 變異 | 檔案 | 結果 |
| --- | --- | --- |
| 不改（基準） | | 通過 |
| 換發多一個 `PUT`：`post(renew).put(renew)` | `services/certs/src/renewal.rs` | 失敗：表放行 `[POST]`，服務提供 `[POST, PUT]` |
| `/healthz` 多一個 `POST` | `crates/service-kit/src/health.rs` | 失敗：表 `[GET, HEAD]`，服務 `[GET, HEAD, POST]` |
| 另一條 `.route(crate::login::LOGIN_PATH, get(..))` | `services/identity/src/login.rs` | 失敗：表 `[POST]`，服務 `[GET, HEAD, POST]` |
| 表上的根 CRL 少了 `HEAD` | `deploy/chart/templates/_ingress.tpl` | 失敗：表 `[GET]`，服務 `[GET, HEAD]` |
| 表上的登入多一個 `GET` | `deploy/chart/templates/_ingress.tpl` | 失敗：與路由、與 `api/openapi.json` 都不一致（兩條訊息） |
| OpenAPI 的 `/auth/login` 多一個 `get` | `api/openapi.json` | 失敗：表 `[POST]`，OpenAPI `[GET, HEAD, POST]` |
| MQTT 的 handshake 也接受 `CONNECT` | `services/mqtt-bridge/src/handshake.rs` | 失敗：讀不懂第二個方法比對，要求擴充檢查 |
| MQTT 轉接在 handshake 之前回應 `OPTIONS` | `services/mqtt-bridge/src/lib.rs` | 失敗：只有 handshake 能決定方法 |
| `LIVENESS_PATH` 改成 `/livez` | `crates/service-kit/src/health.rs` | 失敗：表固定的 `/healthz` 不等於服務的常數 |
| 全部還原 | | 通過 |

chart 測試涵蓋的路徑固定（`deploy/chart/tests/chart.test.ts`）：

- `every public path but mqtt is fixed to its service's path, with and without schema validation`：`login`、`healthCheck`、`certRenewal`、`rootCrl` 各用兩個別的值，有 schema 時被 `/ingress/publicPaths/<key>` 拒絕；`--skip-schema-validation` 時樣板以 `must be <path>, the only path the <service> service serves it on` 拒絕，服務開或關都一樣。
- `the MQTT path cannot be one of service-kit's health endpoints, with and without schema validation`：`mqtt` 設成 `/healthz` 或 `/readyz` 時，兩種情況都拒絕。
- 原本靠改 `login`、`healthCheck` 測的一般規則（同一路徑在兩個主機、限流規則跟著路徑、萬用字元、`..`、`/internal`、重複路徑）改用 `mqtt` 測。

## 結論

- 驗收條件 1（`login` 與 `healthCheck` 的路徑固定，有 schema 與樣板兩種情況的測試）：通過。
- 驗收條件 2（方法清單與服務的路由不一致時有測試會失敗）：通過。上面 9 個變異都讓 repo-check 失敗，還原後通過。
- 驗收條件 3（`bun test deploy/chart` 與 `cargo test --workspace` 都通過）：通過。`cargo test` 依 repo 慣例排除 `agent`，這次沒有改到它。
- 待測：真的 Envoy 上的方法比對，在 Laysi/trustiot-v2#905 做，不屬於本 issue。
