# deploy/images/dashboard-check

儀表板檢查服務（[services/dashboard-check](../../../services/dashboard-check/README.md)，Laysi/trustiot-v2#150）的映像：服務的執行檔、Debian 的 `chromium-headless-shell` 與它需要的函式庫，以及前端建置出的沙箱檔案。chart 的 `services.dashboard-check` 部署它（[chart README](../../chart/README.md)「儀表板檢查」）。

## 內容

| 路徑 | 內容 |
| --- | --- |
| `/usr/local/bin/dashboard-check` | 服務（aarch64 musl 靜態執行檔），`ENTRYPOINT` |
| `/usr/lib/chromium/chromium-headless-shell` | Debian 的 `chromium-headless-shell`，服務每次檢查啟動一個新的 |
| `/srv/www/sandbox/` | 前端建置結果（`frontend/dist/`）的 `sandbox/`：檢查用的 host 頁與每個 runtime 版本的沙箱檔案（[frontend/sandbox/README.md](../../../frontend/sandbox/README.md)「建置結果」）|
| `/var/lib/dpkg/status.d/` | 每個 Debian 套件的資料（distroless 的格式），release 的 SBOM 由 syft 從這裡列出套件 |

- 基底是 `gcr.io/distroless/cc-debian13:nonroot`（glibc 與 libstdc++，沒有 shell 與套件管理程式），與 repo 內其他 distroless/cc 的副本同一個 digest。
- 以 UID 65532 執行。chart 讓根目錄唯讀，Chromium 只寫 `/tmp`（記憶體 emptyDir），每次檢查一個目錄，檢查結束由服務刪除。
- 字型只有 Debian 帶進來的 DejaVu，沒有中日韓字型：檢查比對的是文字內容與圖形標記的尺寸，不比對字形，所以中文字顯示成方塊不影響判定。

## 建置

release.yml、nightly 與映像測試用同一份 build context（`scripts/release/rust-context.ts`）：交叉編譯的執行檔 `dashboard-check`、前端的建置結果 `www/` 與本目錄的 `Containerfile`（`image.toml` 的 `containerfile` 與 `[static]`，見 [deploy/images/README.md](../README.md)「自帶 Containerfile 的 Rust 服務」）。同一次發佈裡，本映像與 `web` 映像拿的是同一份前端建置。

arm64 那一層沒有 `RUN`，所以 x86 的機器不用模擬器也能建置：

1. `chromium` 階段在建置機器自己的平台上執行（`FROM --platform=$BUILDPLATFORM` 的 `debian:trixie-slim`，以 digest 釘選）。apt 用自己的設定：架構是 arm64，唯一的來源是 `snapshot.debian.org` 上 `DEBIAN_SNAPSHOT` 那個時間的 `trixie`、`trixie-updates` 與 `trixie-security`，一律走 https，以基底映像（以 digest 釘選）的 CA bundle 驗證伺服器憑證；再以該階段 `debian-archive-keyring` 的 Debian 金鑰驗證 Release 簽章，以簽過的索引檢查每個下載檔案的 SHA-256。snapshot 的 Release 檔保留原本的有效期限，所以不檢查期限（`Check-Valid-Until`），索引來自 snapshot 伺服器本身由 https 保證。apt 只下載（`--download-only`），不安裝。沒有另外釘選每個 `.deb` 的 SHA-256：那會讓 Renovate 更新 Chromium 的 pull request 還得手動補上每個套件的雜湊。
2. 套件資料庫是基底映像的套件，所以 apt 只下載基底沒有的；下列套件標成已安裝但其實不在映像裡，也不寫進映像的套件資料庫：`awk`、`debconf`、`openssl`、`openssl-provider-legacy`（基底套件的依賴，distroless 本來就沒有；`debconf` 只給不會執行的 maintainer script 用），以及 `x11-utils`、`xdg-utils`（`chromium-common` 給桌面瀏覽器的輔助 script 用，headless shell 不會執行）。
3. 每個下載的套件以 `dpkg-deb --extract` 解開到 `/rootfs`，不執行任何 maintainer script，控制欄位與檔案的 md5sums 寫到 `/rootfs/var/lib/dpkg/status.d/`。基底已有的套件只能升級（Debian 小版本更新時），版本比基底舊就停止；Chromium 的版本不是 `CHROMIUM_VERSION` 就停止；有 set-user-ID 或 set-group-ID 的檔案就停止。文件、man page 與語系檔刪掉（保留 `copyright`）。
4. 最後一層以基底為底，只有 `COPY`：`/rootfs`、執行檔與 `www/sandbox/`。

release 的 SBOM 必須列出 `chromium-headless-shell` 與 `chromium-common`（`image.toml` 的 `[sbom] packages`），沒列出就停止發佈。

## 釘選

| 釘選 | 在哪裡 | 怎麼更新 |
| --- | --- | --- |
| `gcr.io/distroless/cc-debian13:nonroot@sha256:…` | `Containerfile` 第一個 `FROM` | Renovate 與其他副本一起更新 |
| `docker.io/library/debian:trixie-slim@sha256:…` | `chromium` 階段的 `FROM` | Renovate |
| `DEBIAN_SNAPSHOT` | `ARG DEBIAN_SNAPSHOT=<YYYYMMDDThhmmssZ>` | 與 `CHROMIUM_VERSION` 一起，見下方「升級 Chromium」 |
| `CHROMIUM_VERSION` | `ARG CHROMIUM_VERSION=<Debian 版本>`，上一行是 `# renovate: datasource=deb …` | Renovate（`renovate.json5` 的 deb custom manager），從 `trixie-security` 的 arm64 套件 |

同一個 `DEBIAN_SNAPSHOT` 讓同一個 commit 每次建出相同的套件。

## 升級 Chromium

Debian 的 `trixie-security` 每出一個新的 Chromium 都是安全更新，Renovate 一看到就開 pull request（不等每週的排程），只改 `CHROMIUM_VERSION`。同一個 pull request 要：

1. 把 `DEBIAN_SNAPSHOT` 改成新版本 arm64 套件進入 snapshot 的時間，也就是下列 JSON 裡 `arm64.deb` 那一筆的 `first_seen`（`<版本>` 要做 URL 編碼，`~` 照寫即可）：

   ```
   https://snapshot.debian.org/mr/package/chromium/<版本>/binfiles/chromium-headless-shell/<版本>?fileinfo=1
   ```

   沒改時建置在版本檢查停止（snapshot 裡還是舊版本）。改成更晚的時間也可以，但其他套件會一起換成那個時間的版本。
2. 確認映像測試通過：ci.yml 的 `dashboard-check-image` job 會在 PR 上執行，也可以在本機跑（`bun test deploy/images/dashboard-check`，見下方「測試」）。
3. 合併前加上 `smoke` label，讓 nightly 從原始碼建出新映像並在叢集裡檢查（[CI 與發佈](../../../docs/architecture/ci-release.md)「nightly.yml」）。

## 怎麼知道映像落後

已部署的映像不會自己更新，Chromium 的安全修正要等下一次發佈才進叢集。維運人員可以從這些地方發現落後：

- Renovate 開著的 Chromium pull request，或 Dependency Dashboard issue 上待處理的 `chromium-headless-shell` 更新：表示 main 已經落後 Debian。
- 部署中的版本：chart 以 digest 部署映像，該版發佈頁附的 SBOM（`dashboard-check` 的 SPDX）列出 `chromium-headless-shell` 的版本，可與 [Debian 安全追蹤](https://security-tracker.debian.org/tracker/source-package/chromium) 上 trixie 的修正版本比對。
- 目前沒有自動警示：自動比對部署中的版本與 Debian 的最新版本並通知維運人員，還沒有做。

## 測試

```sh
bun test deploy/images/dashboard-check
DASHBOARD_CHECK_IMAGE=<本機映像> bun test deploy/images/dashboard-check
```

- ci.yml 的 `dashboard-check-image` job 在 arm64 runner 上執行這個測試，在映像、服務、它依賴的 crate、沙箱或前端的建置工具有變更時執行（規則在 `scripts/ci/changes.ts`，見 [CI 與發佈](../../../docs/architecture/ci-release.md)「依變更路徑決定 job」）。
- 沒給 `DASHBOARD_CHECK_IMAGE` 時，以 nightly 的方式建置映像（`scripts/nightly/service-image.ts` 的 `buildServiceImage`，與 release.yml 同一份 build context），需要 rustup 與 `proto install` 裝好的 zig、cargo-zigbuild、cargo-auditable，並會重建 `frontend/dist`。`DASHBOARD_CHECK_SITE=<目錄>` 改用該目錄的前端建置結果，不重建。
- 容器引擎用 docker，沒有 docker 時用 podman（記憶體上限需要 rootful 的引擎或 cgroup 委派）；引擎要能執行 linux/arm64 容器（arm64 主機，或以 binfmt_misc 註冊的 arm64 模擬器），否則映像相關的測試在本機略過、設了 `CI` 時失敗。`Containerfile` 的檢查在哪裡都會執行。
- 服務以 chart 的設定執行：根目錄唯讀、`/tmp` 是 chart `tmpSizeLimit` 大小的記憶體檔案系統、拿掉所有 capability、`no-new-privileges`、引擎預設的 seccomp profile、chart 的記憶體上限、UID 65532。
- 頁面是沙箱的固定樣本（[frontend/sandbox/fixtures](../../../frontend/sandbox/fixtures)，[frontend/sandbox/README.md](../../../frontend/sandbox/README.md)「固定樣本」），全部都跑，以樣本自己的 `matchesExpectation` 判斷；其中 `unit` 為 `false` 的只有這個測試涵蓋（真實的版面、`::before`／`::after`／`::marker`、CSS 動畫、封閉的 shadow root、非同步的錯誤、導向、請求、isolated world）。樣本的模組要用前端的套件，建置時會安裝；給了 `DASHBOARD_CHECK_IMAGE` 或 `DASHBOARD_CHECK_SITE` 時先執行 `bun install --cwd frontend --frozen-lockfile`。
- 檢查的項目：arm64 與 UID；套件資料庫列出釘選的 Chromium；每個樣本頁面得到預期的判定，判定裡沒有網址；檢查結束後沒有留下瀏覽器行程；不加 `--no-sandbox` 時 Chromium 拒絕啟動（服務以 `--no-sandbox` 啟動 Chromium 的原因，見服務 README 的「威脅模型」）。結束時印出每次檢查的時間與容器記憶體用量的最高讀數。
