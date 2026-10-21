# 代表使用者的呼叫、冒用與重播、背景工作的身分（2026-10-21）

Laysi/trustiot-v2#106（父 issue Laysi/trustiot-v2#21，清單 A4）：服務代表使用者呼叫其他服務時向身分服務換 token，換 token 只沿 chart 從 `mesh.calls` 推得的呼叫清單；冒用與重播的負向測試；背景工作不靠使用者身分完成工作。設計見決策 27 的「代表使用者的呼叫與背景工作」。原始資料在 [data/2026-10-21-106-on-behalf-identity/](data/2026-10-21-106-on-behalf-identity/)。

## 環境

| 項目 | 內容 |
| --- | --- |
| 機器 | 建置主機（x86_64 Linux），經 check queue 執行：worktree 的已追蹤與未追蹤檔案複製過去，`CI=1`（需要容器的測試失敗而不略過），容器測試用 podman |
| 工具 | Rust 1.99.0（`rust-toolchain.toml`）、Bun 1.4.2、helm 4.3.0、kubeconform 0.8.0（`.prototools`） |
| 程式碼 | 分支 `feat/106-on-behalf-identity`。2026-10-21 的檢查以 main `cc185b9` 為基礎，跑在提交前的工作目錄上；2026-10-22 的修正在工作目錄上檢查後，分支快轉到 main `6a0938e`（多的兩個 commit 只改 `edge/agent`、`scripts/demo`、`deploy/flux` 與文件）再提交 |
| 叢集 | 沒有。mesh e2e（`deploy/chart/tests/e2e/mesh.e2e.ts`）只在 nightly 的 smoke 執行，見「結論」 |

## 指令

從 repo 根目錄：

```sh
# 型別檢查、chart 測試（含呼叫矩陣與兩個 e2e helper 的單元測試）、assets 的測試
proto install helm kubeconform
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test deploy/chart
cargo test --locked -p assets

# 整個 workspace
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
cargo run --locked -p openapi -- --check
cargo test --locked --workspace
cargo test --locked -p service-kit --no-default-features
bun test scripts/ci/changes.test.ts

# 本機
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/check-workflows.ts
```

單獨跑驗收條件的測試：

```sh
cargo test --locked -p identity --lib exchange::tests
cargo test --locked -p identity --test ext_authz a_routed_token_is_exchanged_along_the_call_list_only
cargo test --locked -p service-kit exchange_verifier
cargo test --locked -p service-kit --doc on_behalf
cargo test --locked -p assets --test background_identity
bun test deploy/chart -t "calls on behalf of users"
```

## 結果

| 檢查 | 結果 | 原始資料 |
| --- | --- | --- |
| `cargo clippy --locked -p contract -p service-kit -p identity --all-targets -- -D warnings` | 沒有警告 | [01](data/2026-10-21-106-on-behalf-identity/01-rust-contract-service-kit-identity.txt) |
| `cargo test --locked -p contract -p service-kit -p identity` | 36 組測試（含 doctest）共 379 通過、0 失敗；需要 Postgres 的測試在容器裡實際執行 | [01](data/2026-10-21-106-on-behalf-identity/01-rust-contract-service-kit-identity.txt) |
| `cargo clippy` 與 `cargo test`，`-p service-kit --no-default-features` | 沒有警告；20 組共 136 通過、0 失敗 | [01](data/2026-10-21-106-on-behalf-identity/01-rust-contract-service-kit-identity.txt) |
| `bun run --cwd scripts typecheck` | 沒有錯誤（含 `deploy/chart/tests/e2e/` 的新檔案與改過的 `mesh.e2e.ts`） | [02](data/2026-10-21-106-on-behalf-identity/02-typecheck-chart-assets.txt) |
| `bun test deploy/chart` | 9 個檔案 476 通過、0 失敗 | [02](data/2026-10-21-106-on-behalf-identity/02-typecheck-chart-assets.txt) |
| `cargo test --locked -p assets` | 全部通過，含 `the_consumer_does_its_work_while_no_request_can_carry_a_user`（5.22 秒） | [02](data/2026-10-21-106-on-behalf-identity/02-typecheck-chart-assets.txt) |
| `cargo fmt --all --check`、`bun scripts/ci/check-secret-handling.ts`、`bun scripts/ci/check-workflows.ts`（本機） | 通過；沒有違規 | — |
| 整個 workspace 的 `cargo clippy`、`cargo test`、`cargo run -p openapi -- --check`、`bun test scripts/ci/changes.test.ts` | **沒有執行**：2026-10-21 晚上排進 check queue 時建置主機約 10 分鐘沒有回應（check queue 回報連不上建置主機），不在工作站上跑。要在提交前或 PR 的 CI 補上 | — |

上表的檢查依序在 15:58 與 16:15（工作站時間）開始，跑的是當時的工作目錄。2026-10-22 依安全審查改了 Rust 程式、chart 與測試（見下一節），上表的結果不涵蓋這些修改。

## 2026-10-22 安全審查後的修正

安全審查沒有 Critical 或 High，兩個 Medium 已修正；其餘記在 Laysi/trustiot-v2#509。

- **M1 清單被串接**：身分服務以 token 的受眾認定呼叫端，所以 `{assistant:[assets], assets:[identity]}` 這種清單會讓 assistant 拿換來的 assets token 再換出 identity 的。現在換出的 token 帶 RFC 8693 的 `act` claim（`contract::Actor`，記下呼叫端），token 交換拒絕帶 `act` 的 token（403，日誌 `already_exchanged`），只換一跳。改動的測試：`exchange/tests.rs` 的 `chains_and_cycles_never_extend_the_request_token` 換成 `an_exchanged_token_is_never_exchanged_again`（清單 `{"assets":["certs"],"certs":["assets","alerts"]}`，換來的 certs token 換 assets、alerts、certs 都是 403），預期的 claims 加上 `act`。
- **M2 代表使用者呼叫的服務連得到整個身分服務**：包括把任何 session cookie 換成任何受眾 token 的 ext_authz 檢查。現在 token 交換在自己的埠（`exchange.listen_addr`，預設 8082，chart 的埠名 `token-exchange`，由 `crates/service-kit` 新增的 `Context::serve_port` 提供，和 API 埠同樣的 layer 與收尾），chart 給它自己的 Service、AuthorizationPolicy 與 NetworkPolicy（`<release>-identity-exchange`，`templates/token-exchange.yaml`）：
  - 它的 AuthorizationPolicy 只讓代表使用者呼叫的服務連 `token-exchange` 埠；沒有這種服務時沒有規則。
  - 身分服務自己的 AuthorizationPolicy 只讓入口 gateway 與 `mesh.calls` 列身分服務的服務連 API 埠；身分服務的 Service 不再開 `token-exchange`。
  - 兩個 NetworkPolicy 各放行自己的呼叫端的 pod（HBONE 的 15008）；NetworkPolicy 看不到通道裡的埠，所以分埠只靠 AuthorizationPolicy。
  - 改動與新增的測試（`deploy/chart/tests/chart.test.ts`）：每條政策的規則與 selector、兩個 NetworkPolicy 的來源、兩個 Service 的埠與 selector、沒有代表使用者呼叫時 token 交換的政策什麼都不放行、身分服務關閉時沒有這些物件、README 呼叫矩陣多一列 `identity-exchange`；渲染失敗的三種情況：`services.identity-exchange` 這個名稱、有代表使用者呼叫卻沒有 `token-exchange` 埠、只剩 `token-exchange` 埠。`tests/ext_authz.rs` 與 `scripts/tests/e2e.test.ts` 改連 token 交換的埠。mesh e2e 改連 `<release>-identity-exchange:8082`，身分服務替身也在 8082 回應，並多一個只有 AuthorizationPolicy 擋得住的探測：assets 連身分服務的 API 埠，第 1、4、5 階段要被拒，第 2 階段（只剩 NetworkPolicy）要通過。
- 依 2026-10-21 的常設決定，Medium 與 Low 的修正不加新的測試、不再送審查。

2026-10-22 在工作站上跑的檢查（建置主機沒有回應，只跑不需要編譯 Rust 的部分；原始資料 [03](data/2026-10-21-106-on-behalf-identity/03-chart-after-review-2026-10-22.txt)）：

| 檢查 | 結果 |
| --- | --- |
| `bun test deploy/chart/tests/chart.test.ts` | 410 通過、0 失敗 |
| `bun test deploy/chart/tests/e2e deploy/chart/tests/ingress-authz.test.ts`（`deploy/chart` 的其他 8 個檔案） | 74 通過、0 失敗 |
| `bun run --cwd scripts typecheck`（含改過的 `mesh.e2e.ts`） | 沒有錯誤 |
| `bun test scripts/nightly` | 270 通過、0 失敗 |
| `cargo fmt --all --check` | 通過 |

Rust 的修改在提交後於建置主機補跑，結果見下一節「2026-10-22 修正後的完整檢查」。

每條驗收條件對應的測試（2026-10-22 修正後的名稱與內容；修正後的結果見下一節）：

- **冒用**
  - `services/identity/src/exchange/tests.rs`：
    - `only_listed_pairs_are_exchanged`：清單是 `{"assets":["certs"],"certs":["alerts"]}`，清單外的六個組合都回 403 `permission_denied`，包括反向、沒有呼叫的服務、身分服務自己、換給自己；清單內的 certs → alerts 換得到。
    - `an_exchanged_token_is_never_exchanged_again`：換來的 token 即使沿著清單上的呼叫也不能再換（M1）。
  - `services/identity/tests/ext_authz.rs`：
    - `a_routed_token_is_exchanged_along_the_call_list_only`：真的身分服務執行檔，在 token 交換自己的埠，ext_authz 簽給 assets 的 token 換得到 certs 的，換不到 alerts 的；換來的 certs token 也換不到 assets 的；沒有 token 或偽造的 token 都回 401。
  - `crates/service-kit`：
    - `the_exchange_verifier_accepts_any_audience_and_nothing_else` 與 `any_audience_still_checks_everything_else`：交換用的驗證端只放寬受眾，其他檢查不變。
  - `deploy/chart/tests/chart.test.ts` 的 `calls on behalf of users`：
    - 只有 `mesh.calls` 中兩個啟用的服務之間的呼叫進得了 `TRUSTIOT_ON_BEHALF_CALLS`，`ai-gateway`、`postgres` 與 `clickhouse` 都不算。
    - 這些呼叫端只進得了 token 交換自己的 AuthorizationPolicy 與 NetworkPolicy（`<release>-identity-exchange`），而且只到 `token-exchange` 埠；入口 gateway 只連身分服務的 API 埠（M2）；其他服務都沒有這個設定。
    - README 的呼叫矩陣與兩種部署形態的渲染結果相同。
- **重播**
  - `tokens_are_refused_by_other_audiences_and_once_expired`：
    - 原 token 與換來的 token 只在自己的受眾通過，`ServiceName::ALL` 的其他每個服務都以 `wrong_audience` 拒絕。
    - 過期的 token 被自己的受眾以 `expired` 拒絕，也換不到新的（401）。
  - `a_token_without_a_second_left_is_not_exchanged`：驗證端的 5 秒誤差容許內、但已過期的 token 不換。
  - `an_exchanged_token_is_never_exchanged_again`：換來的 token 的到期等於原 token，而且不能再換，所以一串呼叫無法延長到期（2026-10-21 跑的是它的前身 `chains_and_cycles_never_extend_the_request_token`：一路換下去，每一張的到期都等於最初那張）。
  - `the_new_token_expires_with_the_old_one_and_at_most_a_minute_from_now`：換來的 token 最多 60 秒。
- **背景工作**
  - `services/assets/tests/background_identity.rs`：
    - assets 以空的公鑰集啟動：readiness 回 503；新租戶的 viewer 帶著真的 token 呼叫 `/api/assets/v1/tenant` 回 503 `unavailable`，工作完成後也一樣。
    - 事件消費者仍套用 `identity.tenant_created`：日誌 `created: true`，`assets.tenants` 有那個租戶的一列。
  - `crates/service-kit/src/identity/on_behalf.rs` 的 `compile_fail` doctest（E0308）：`ServiceIdentity` 不能交給 `OnBehalf::call`。

## 2026-10-22 修正後的完整檢查

建置主機恢復後，在提交後的程式碼（`527d60c`，以 main `6a0938e` 為基礎）上經 check queue 執行（`CI=1`，需要容器的測試不會略過），涵蓋「指令」一節的全部檢查，包括 2026-10-21 沒有執行的整個 workspace 的檢查。原始資料 [04](data/2026-10-21-106-on-behalf-identity/04-workspace-after-review-2026-10-22.txt)。

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 沒有警告 |
| `cargo test --locked --workspace` | 115 組共 899 通過、0 失敗；需要容器的測試實際執行 |
| `cargo run --locked -p openapi -- --check` | `api/openapi.json is up to date` |
| `cargo deny --locked check` | advisories、bans、licenses、sources 都是 ok |
| `cargo clippy` 與 `cargo test`，`-p service-kit --no-default-features` | 沒有警告；20 組共 136 通過、0 失敗 |
| `bun run --cwd scripts typecheck` | 沒有錯誤 |
| `bun test deploy/chart` | 9 個檔案 484 通過、0 失敗 |
| `bun test scripts`（含 `scripts/ci/changes.test.ts`） | 48 個檔案 1481 通過、4 略過（需要 podman 或 arm64 的測試）、0 失敗 |

`cargo test --workspace` 包含上一節列出的每一個驗收條件測試，都通過。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 服務冒用測試：未被允許的服務帶使用者 token 呼叫，被拒 | **應用層與 chart 在 2026-10-22 修正後通過；叢集待測**。清單外的服務換不到 token（403），換來的 token 不能再換，把自己的 token 轉給別人會被以受眾不符拒絕；chart 只讓清單內的呼叫端連到被呼叫端與身分服務的 `token-exchange` 埠。叢集上的 L4 拒絕由 `deploy/chart/tests/e2e/mesh.e2e.ts` 驗證：每個探測都帶著被呼叫端會接受的使用者 token，清單外的呼叫要被拒，清單內的呼叫要帶著 token 到達。這個測試只在 nightly 的 smoke 執行，**待 PR 加上 `smoke` label 的那次執行**，結果補進本報告。叢集上的服務都跑身分服務替身、不驗證 token，所以那裡看到的拒絕都是 L4 的；token 交換的應用層檢查由上面的 Rust 測試驗證 |
| token 重播測試：過期或受眾為別的服務的 token 被拒 | **2026-10-22 修正後通過**（`act` 宣告、token 交換改到自己的埠、`an_exchanged_token_is_never_exchanged_again` 取代前身測試之後重跑） |
| 背景工作沒有使用者身分也能完成自己的工作 | **2026-10-22 修正後通過**（修正動到 `service-kit` 的 `serve_port` 之後重跑） |

待補的檢查：只剩叢集上的 mesh e2e，在 PR 加上 `smoke` label 之後的 nightly 部署執行，結果補進本報告與 Laysi/trustiot-v2#106。
