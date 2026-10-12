# Postgres 映像建置的下載重試與 AGE 備援來源

對應 issue：Laysi/trustiot-v2#369（PR Laysi/trustiot-v2#307 的映像、PR Laysi/trustiot-v2#358 的 CI 失敗：`curl: (7) Failed to connect to archive.apache.org`）。

## 環境

- 建置機：x86_64 Linux 主機，Docker 29.8.2，`docker build --platform linux/arm64`（與 release.yml 相同，arm64 階段沒有 `RUN`，不用模擬器）。
- 基底與版本不變：`ghcr.io/cloudnative-pg/postgresql:18.6-minimal-trixie`（同一個 digest）、Apache AGE 1.8.0、`PG_PACKAGE_VERSION` 18.6-1.pgdg13+2。
- 測試對象：本 issue 的分支 `fix/369-postgres-image-download-retry`；「改動前」是 main 的 `da6b4c8` 的 `Containerfile`。
- 日期：2026-10-13（Asia/Taipei）。

## 指令

原始資料與腳本在 [data 目錄](data/2026-10-13-369-postgres-image-download-retry/)。每個建置都由 `build.sh <標籤> [--build-arg ...]` 執行（`KEEPCACHE=1` 時保留快取，其餘為 `--no-cache`）：

```sh
bash build.sh before                      # main 的 Containerfile
bash build.sh after                       # 本分支，兩個來源都正常
KEEPCACHE=1 bash build.sh sim-refused --build-arg AGE_MIRROR_URL=https://127.0.0.1:9/age
KEEPCACHE=1 bash build.sh sim-404     --build-arg AGE_MIRROR_URL=https://archive.apache.org/dist/age/PG17
KEEPCACHE=1 bash build.sh sim-wronghash --build-arg AGE_SHA512=<128 個 0>
bash compare-images.sh                    # before 與 after 的檔案系統與設定比對
bun test deploy/images/postgres -t Containerfile
bun run --cwd scripts typecheck
```

## 結果

| 情境 | 結果 | 記錄 |
| --- | --- | --- |
| after，鏡像正常 | 從 `dlcdn.apache.org` 下載，0.7 秒內 SHA-512 `age.tar.gz: OK`；整個建置成功 | [after.log](data/2026-10-13-369-postgres-image-download-retry/after.log) |
| 鏡像連不上（`127.0.0.1:9`，連線被拒） | `curl: (7) Failed to connect` 重試 6 次（間隔 5 秒，共 25 秒），然後改用 `archive.apache.org`，SHA-512 `OK`，建置成功。舊的 `--retry 5` 在這種錯誤不會重試 | after.log 的 `sim-refused` |
| 鏡像沒有該版本（404） | 同樣重試 6 次後改用存檔，建置成功。`--retry-all-errors` 也會重試永久性的 404，多花約 30 秒，只在鏡像移出版本時發生 | after.log 的 `sim-404` |
| 雜湊不符（`AGE_SHA512` 設成全 0） | 兩個來源都下載了檔案但 `age.tar.gz: FAILED`，訊息 `Apache AGE 1.8.0 could not be downloaded and verified from any source`，建置失敗（exit 1）；兩條路徑都以 SHA-512 驗證 | cmp.log 的 `sim-wronghash` |
| PGDG deb | 與 AGE 共用 `fetch`（重試參數相同），SHA-256 驗證不變（`server-dev.deb: OK`） | 各建置記錄 |

存檔站下載很慢（鏡像 0.7 秒，存檔約 230 至 250 秒），所以鏡像優先確實縮短正常建置時間，這也是 PR Laysi/trustiot-v2#358 的 CI 在 `archive.apache.org` 上遇到 76 秒連線逾時的背景。

### 映像是否相同

`docker image inspect` 的 layer digest 在改動前後不同：前三層（基底）相同，後三層（複製 AGE 檔案的三個 `COPY`）不同。原因是每次建置都重新編譯 `age.so`，tar 內檔案與目錄的 mtime 不同（建置時間），內容沒有變。`compare-images.sh` 以 `docker export` 比對兩個映像（[cmp.log](data/2026-10-13-369-postgres-image-download-retry/cmp.log)）：

- 全部檔案內容的 SHA-256 彙總相同（`82cbe72f...`）。
- 檔案模式、型別、連結目標的彙總相同。
- 映像設定（User、Entrypoint、Cmd、Env、架構）相同。
- 檔案列表去掉 mtime 欄位後沒有差異；有差異的 22 行只有 mtime（`02:21` 對 `02:22`）。

ARG `AGE_MIRROR_URL`、`AGE_ARCHIVE_URL` 只在 `build` 階段，最終映像沒有 `RUN`，所以內容不受影響。同一台機器上相同 Containerfile 重建，layer digest 本來就不會相同（mtime）；要位元組相同需要另外固定 mtime（`SOURCE_DATE_EPOCH`），不在本 issue 範圍。

### 靜態測試

`bun test deploy/images/postgres -t Containerfile`：5 項通過，含新增的「every download retries connection errors and is verified against a pinned hash」（只有一個 curl 呼叫且帶 `--retry-all-errors`、`--retry-delay`、`--connect-timeout`；AGE 的鏡像與存檔順序；兩個來源都經 SHA-512 檢查）。`bun run --cwd scripts typecheck` 通過。

### Debian snapshot 的 apt

不需要改：`apt-get update` 與 `install` 都已帶 `-o Acquire::Retries=5`，apt 的重試包含連線失敗（與 curl 的 `--retry` 不同）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 兩個 curl 下載在連線失敗時重試 | 通過：`sim-refused` 記錄顯示 6 次嘗試；PGDG 與 AGE 共用同一個 `fetch`，由靜態測試確認 |
| 鏡像失敗時改用 `archive.apache.org`，兩條路徑都以 SHA-512 驗證 | 通過：`sim-refused`、`sim-404` 成功改用存檔；`sim-wronghash` 證明兩個來源都驗證 SHA-512 |
| 改動前後在同一台機器上重建，digest 相同或寫明原因 | 內容相同，layer digest 因 mtime 不同，已說明並以檔案內容比對佐證 |
| `bun test deploy/images/postgres` 在 CI 的 `postgres-image` job 通過 | 待測：需要 arm64 runner，PR 開啟後由 CI 證明；本機只跑了不需要啟動映像的 Containerfile 測試 |
