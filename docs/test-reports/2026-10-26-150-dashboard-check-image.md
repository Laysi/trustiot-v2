# 儀表板檢查服務：映像測試與 nightly 隔離證明的本機部分

對應 Laysi/trustiot-v2#150。

這份報告記錄 2026-10-26 在開發工作站上的檢查：儀表板檢查服務（`services/dashboard-check`）的映像以真的 headless Chromium 檢查沙箱的每一個固定樣本頁面（驗收條件 2「用真的檢查服務容器」的檢查服務部分），以及 nightly 隔離證明（驗收條件「檢查服務的隔離」）在叢集以外能驗的部分。隔離本身要在 nightly 的 k3d 叢集上證明，工作站不跑 Kubernetes，所以那一項在下方「結論」寫成待測，證據是帶 `smoke` label 的 PR 觸發的 nightly run。

## 環境

- 程式碼：分支 `feat/150-s3-checker`，commit `600aca8`（包含沙箱 runtime 的分支 `feat/150-s2-runtime` 的 `a9c1814`）。
- 開發工作站；Bun 1.4.2 與其餘工具依 `.prototools` 釘選，Rust 1.99.0（`rust-toolchain.toml`）。
- 容器引擎是 Docker，映像平台 `linux/arm64`。
- 映像：`deploy/images/dashboard-check`，本機建置的 `localhost/trustiot-e2e/dashboard-check:image-test`，`docker image inspect` 的大小 830,539,700 bytes（未壓縮），使用者 `65532:65532`。本機建置的 manifest list digest（含 Docker 的 attestation，每次建置都不同）是 `sha256:6ef0a921b483af748c6beb98012047727a972f5bbee0fd213764a19dfa77a056`。
  - 基底 `gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2`。
  - 下載套件的階段 `docker.io/library/debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad`。
  - `DEBIAN_SNAPSHOT=20261022T170820Z`，`CHROMIUM_VERSION=156.0.8078.47-1~deb13u1`（Debian `trixie-security` 的 `chromium-headless-shell`）。
- 服務以 chart 的設定執行（映像測試自己設定）：根目錄唯讀、`/tmp` 是 128Mi 的 tmpfs、拿掉所有 capability、`no-new-privileges`、Docker 預設的 seccomp profile、記憶體上限 512Mi、UID 65532。

## 指令

```sh
# 映像測試：以 nightly 的 buildServiceImage 建置（與 release.yml 同一份 build context，含前端建置），
# 再對每個固定樣本頁面跑一次檢查。--reporter=junit 只為了取得每個樣本的時間。
proto install
bun install --cwd frontend --frozen-lockfile
bun test deploy/images/dashboard-check --reporter=junit --reporter-outfile=<目錄>/image-test.xml

# nightly 的單元測試（隔離檢查的計畫、探測 script 與輸出的解析、assistant 的儀表板 API 檢查、
# 映像快取的輸入）
bun install --cwd scripts --frozen-lockfile
bun test scripts/nightly
bun run --cwd scripts typecheck

# 探測 script 本身（需要 GNU 的 getent、timeout、base64；工作站沒有時略過，CI 的 runner 有）：
# 在 Debian 的 Bun 容器裡以唯讀掛載的 repo 跑一次
docker run --rm --network none -v "$PWD":/repo:ro -w /repo -e HOME=/tmp <Debian 的 Bun 映像> \
  bun test scripts/nightly/dashboard-check.test.ts

# chart、CI 規則與 workflow
bun test deploy/chart
bun test scripts/ci
bun scripts/ci/check-workflows.ts
bun scripts/ci/lint-workflows.ts
bun scripts/ci/check-secret-handling.ts
```

## 結果

### 映像測試

`bun test deploy/images/dashboard-check`：44 個測試通過，0 個失敗，29.0 秒（映像已在快取裡，Chromium 那一層沒有重新下載）。

- `Containerfile` 的靜態規則：基底以 digest 釘選；`linux/arm64` 的階段沒有 `RUN`；每個 `RUN` 是遇錯即停的 sh；套件只來自釘選的 Debian snapshot，以 Debian 的金鑰驗證；Renovate 追蹤 `trixie-security` 的 Chromium；最後是數字的使用者。
- 映像：`linux/arm64`，以 UID 65532 執行；套件資料庫列出釘選版本的 `chromium-headless-shell`（release 的 SBOM 從這裡找到它）。
- 33 個固定樣本頁面（`frontend/sandbox/fixtures`）全部得到樣本預期的判定，以樣本自己的 `matchesExpectation` 判斷（含 `may_also`）。每個樣本的預期與時間：[data/2026-10-26-150-dashboard-check-image/cases.csv](data/2026-10-26-150-dashboard-check-image/cases.csv)。其中 12 個只有真實瀏覽器能判斷（`browser_only` 為 `true`）：非同步的錯誤與未處理的 rejection、五種導向（其中表單送出與彈出視窗被擋下，頁面照常通過）、`::before`／`::after` 的內容、清單的 marker、CSS 動畫、封閉的 shadow root、請求外部樣式表。
- 判定裡沒有網址；所有檢查結束後容器裡沒有留下瀏覽器行程。
- 不加 `--no-sandbox` 時，Chromium 在 Docker 預設的 seccomp profile 下拒絕啟動（服務以 `--no-sandbox` 啟動 Chromium 的原因，見 `services/dashboard-check/README.md`「威脅模型」）。

| 量測 | 數值 |
| --- | --- |
| 每次檢查的時間（33 次） | 128–5166 ms，中位數 411 ms |
| 兩個 5 秒的樣本 | `not-rendered`（資料送出後 5 秒內沒有 `returned`）與 `timeout-render`（畫面一直沒有靜止） |
| 容器記憶體用量的最高讀數 | 109.1 MiB（上限 512Mi） |

同一天在 commit `b7f5da7`（映像的內容與 `600aca8` 相同）跑過一次：44 個通過，每次檢查 125–5157 ms（中位數 406 ms），記憶體最高 94.4 MiB。

### nightly 的隔離檢查（叢集以外的部分）

- `bun test scripts/nightly`：386 個通過，1 個略過（探測 script 本身，工作站沒有 GNU 的 `getent` 與 `timeout`），8 個失敗；失敗的都在 `certs.test.ts`，以工作站的 `openssl` 驗證憑證，與這次的變更無關，main 上同樣失敗。`scripts/nightly/dashboard-check.test.ts` 35 個中 34 個通過、1 個略過；`api.test.ts` 40 個通過（含兩種部署型態以 nightly 的 values 渲染 chart：SaaS 有儀表板檢查與 assistant 的 `TRUSTIOT__DASHBOARD_CHECK__URL`，地端兩者都沒有）。
- 探測 script 在 Debian 的 Bun 容器裡（`--network none`）：35 個全部通過，包括 script 本身的測試：本機一個會回應的連線讀回 `answered`（含帶 `"$(id)"`、反引號與中文的請求本文）、收到請求就關閉的連線是 `silent 0`、沒有人聽的埠是 `silent 3`、`.invalid` 名稱查不到，以及沒收到開始的那一行時什麼都不探測、以 5 結束。
- 開發時另以 nightly 的 Postgres 基底映像（探測容器用的映像，UID 26、唯讀、拿掉所有 capability）手動跑過探測 script：有網路時名稱查得到、外部位址回 HTTP 301，連不上的位址在上限時間結束（`silent 124`），被拒絕的連線是 `silent 3`；`--network none` 時名稱查不到、連線是 `silent 3`。也在一個 Docker 網路上，從探測容器把 assistant 的那一個檢查（固定樣本的共用請求與 `correct` 頁面，7,807 bytes）送給儀表板檢查的映像：HTTP 200，判定 `passed`，62 個標記；送 `{}` 是 400 `invalid_request`。這些是手動的一次性檢查，自動化的對應是上面的單元測試與 nightly 本身。
- `bun test deploy/chart`：687 個通過。`bun test scripts/ci`：526 個通過。`check-workflows`、`lint-workflows`（3 個 workflow）、`check-secret-handling`（7 個容器建置檔）都沒有違規；`bun run --cwd scripts typecheck` 通過。

## 結論

- 驗收條件 2 的檢查服務部分（用真的檢查服務容器）：**通過**。標記的值與資料不一致（`wrong-row`、`format-injection`）、對不到任何一列（`unmapped-mark`、`tick-outside-axis`）、圖形的尺寸或位置不對（`wrong-bar-height`、`wrong-point-position`、`wrong-pie-angle`、`axis-domain`）、寫死的數字、同步與非同步的錯誤、導向與竄改原型的頁面，真的 Chromium 都判為不通過，種類正確；正確的頁面通過。「不發布、使用者仍看到上一版」由 assistant 那一側的測試涵蓋，不在這份報告。
- 驗收條件「檢查服務的隔離」：**待測**。原因：要在 nightly 的 k3d 叢集上執行（工作站不跑 Kubernetes），而且要等 assistant 的設定接受 `TRUSTIOT__DASHBOARD_CHECK__URL` 與它的儀表板 API 一起合併後才能部署。證據是合併後帶 `smoke` label 的 PR 的 nightly run：報告的「the dashboard check runs isolated」一項與 API 檢查的儀表板清單。這份報告只證明檢查本身的邏輯與探測 script 的行為。
