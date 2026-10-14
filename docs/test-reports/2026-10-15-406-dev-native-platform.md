# 本機開發環境：以原生平台建置與啟動映像（回歸驗證）

對應 issue：Laysi/trustiot-v2#406。

## 環境

- 機器：開發工作站（x86_64 Linux，8 核），rootless podman 6.1.3，Bun 1.4.2。
- 修正前：`origin/main` 的 `8e29d03`（`scripts/lib` 為 main 的內容，只加了新的 e2e 測試）。
- 修正後：分支 `fix/406-dev-native-platform`（main `8e29d03` 加本次變更）。
- 這台機器的 podman 儲存是共用的，裡面還有其他工作的映像與容器。測試期間持有開發 e2e 的鎖，期間沒有其他 e2e 在這台機器上跑；測試與重現都只動 `up` 用的四個基底映像，並在結束後還原（見下方「儲存的狀態」）。
- 端對端測試設定：`CARGO_BUILD_JOBS=2`，cargo 輸出目錄在工作樹內；測試用自己的實例名稱、狀態目錄與隨機埠，沒有動平常使用的環境。

## 指令

單元測試與檢查（在建置主機上以 `rcheck.sh 406` 的單一工作執行）：

```sh
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/check-workflows.ts
```

手動重現（修正前的行為），基底為 `IMAGES.serviceBase`：

```sh
podman pull --platform linux/arm64 "<IMAGES.serviceBase>"
podman build --quiet --tag localhost/trustiot-exp-406:a .                       # Containerfile: FROM <IMAGES.serviceBase>
podman build --quiet --platform linux/amd64 --tag localhost/trustiot-exp-406:b .
```

端對端回歸測試（兩個情境：儲存裡是別的架構的服務基底，或別的架構的 Mosquitto；修正前在 main 的 `scripts/lib` 上跑，修正後在分支上跑）：

```sh
TRUSTIOT_DEV_E2E=1 CARGO_BUILD_JOBS=2 bun test scripts/tests/e2e.test.ts -t "up uses images"   # 修正前、修正後各一次
TRUSTIOT_DEV_E2E=1 CARGO_BUILD_JOBS=2 bun test scripts/tests/e2e.test.ts                        # 修正後，整個檔案
```

## 結果

### 單元測試與檢查

`rcheck.sh 406`（建置主機，最終版本的檔案）：typecheck 通過；`bun test scripts` 826 pass、4 skip（3 個 e2e 測試需要 `TRUSTIOT_DEV_E2E=1`，另有 1 個需要 aarch64 主機）、0 fail；`check-secret-handling` 與 `check-workflows` 無違規；`scripts/ci/changes.test.ts` 171 pass（沒有測試開始讀 `scripts/` 以外的檔案，不需要改 `scripts/ci/changes.ts`）。

### 手動重現

| 步驟 | 建出的映像架構 |
| --- | --- |
| 儲存裡有 arm64 的 `IMAGES.serviceBase`，`podman build` 不帶 `--platform` | arm64（這就是 issue 的 bug） |
| 同上，`podman build --platform linux/amd64` | amd64 |
| 儲存裡有 arm64 的 Mosquitto，`podman run` 不帶 `--platform` | 容器起不來：`exec container process /bin/sh: Exec format error` |
| 同上，`podman run --platform linux/amd64` | 成功，並改拉 amd64 複本（`uname -m` 為 `x86_64`） |

所以 `--platform` 能讓 podman 在儲存裡只有別的架構的複本時改拉原生架構，不需要另外檢查再拉取。

### 端對端：修正前（main）

原始輸出：[data/2026-10-15-406-dev-native-platform/e2e-before-main.log](data/2026-10-15-406-dev-native-platform/e2e-before-main.log)

- 2 個情境都失敗（0 pass、2 fail）。
- 服務基底情境：`up` 建出服務映像，`FAIL example-service: GET /healthz ... Unable to connect`、`GET /readyz` 同，`up` 以 1 結束；也就是 issue 描述的行為（服務映像是 arm64，容器起不來）。
- Mosquitto 情境：`up` 在啟動 Mosquitto 前的 `podman run`（寫入設定檔的輔助容器）就失敗：`image platform (linux/arm64/v8) does not match the expected platform (linux/amd64)`、`exec container process /bin/sh: Exec format error`。issue 要求檢查的其他以 digest 啟動的容器（Postgres、ClickHouse、Mosquitto）因此確實受影響，不只服務映像。

### 端對端：修正後（分支）

原始輸出：[data/2026-10-15-406-dev-native-platform/e2e-after-fix.log](data/2026-10-15-406-dev-native-platform/e2e-after-fix.log)

- 整個 `scripts/tests/e2e.test.ts`：3 pass、0 fail（兩個新情境，加上原本的完整流程：up、seed、換密碼重啟、down、purge、身分服務的 ext_authz）。
- 測試開始時若儲存裡已有別的架構的這個映像，測試直接失敗並說明處理方式，不改動儲存（`up` 會替換掉那份複本，之後無法原樣放回）；本次執行時儲存裡都是原生複本，沒有觸發。
- 兩個情境在 `up` 之前都先確認儲存裡的名稱解析成別的架構，`up` 之後服務映像與 Mosquitto 都是原生架構（`linux/amd64`），`up` 印出 `every check passed` 與 `ok   example-service: GET /readyz`。

### 儲存的狀態（映像 ID 與平台）

測試前後 `up` 會用到的四個映像完全相同，[前](data/2026-10-15-406-dev-native-platform/podman-store-before.txt)、[後](data/2026-10-15-406-dev-native-platform/podman-store-after.txt)：

| 映像 | ID（前 12 碼） | 平台 |
| --- | --- | --- |
| `apache/age`（IMAGES.postgres） | `18c595f8c163` | linux/amd64 |
| `clickhouse/clickhouse-server`（IMAGES.clickhouse） | `6cda6a9dc9a2` | linux/amd64 |
| `library/eclipse-mosquitto`（IMAGES.mosquitto） | `8c4aa03990b1` | linux/amd64 |
| `distroless/cc-debian13`（IMAGES.serviceBase） | `a7d72ba3ecf0` | linux/amd64 |

整個映像清單（`podman images --digests` 的名稱、tag、digest 與 ID，每個映像一列）前後排序後逐行相同；本次拉的 arm64 複本與重現用的兩個映像都已依映像 ID 移除，沒有動其他映像；測試用的容器、網路與磁碟區也沒有留下。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 依重現步驟操作，`up` 建出原生架構的服務映像，服務啟動 | 通過。修正後的「服務基底」情境：`up` 成功、服務映像是 `linux/amd64`、`/readyz` 通過 |
| 單元測試：建置參數含原生平台，平台由主機推得，x86_64 與 aarch64 各一例 | 通過。`scripts/tests/stack.test.ts` 的「native platform」：`nativePlatform("x64")`、`("arm64")`、預設值取自 `process.arch`、不支援的架構被拒；`serviceBuildOptions` 在兩種架構下都帶對應的 `--platform`，且通過 `containerBuildArgs` 的檢查；`runArgs` 對 digest 映像帶 `--platform`、對本機建的映像不帶 |
| 回歸驗證：修正前失敗，修正後通過，結果存在 `docs/test-reports/` | 通過。見上方兩份輸出：修正前 2 fail，修正後 3 pass |
| 範圍：其他以 digest 啟動的容器同樣檢查 | 完成。`runArgs` 與 Mosquitto 的輔助 `podman run` 都帶 `--platform`；修正前的 Mosquitto 情境證明它們受影響。Postgres 與 ClickHouse 走與 Mosquitto 相同的 `runArgs`（映像很大，沒有各做一個 e2e 情境） |
| 範圍：架構不符時錯誤訊息寫明原因與處理方式 | 完成。`up` 建完服務映像後檢查它的架構，不符就以 `platformMismatchMessage` 停止（說明儲存裡可能有別的架構的基底，並說明 `podman rmi` 後重跑）；有 `--platform` 時這條路徑正常不會觸發，單元測試只驗證訊息內容 |

後續建議（不在本 issue 範圍）：沒有。
