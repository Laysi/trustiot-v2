# 服務映像的 digest 不再取決於 podman 以 root 或 rootless 執行

對應 issue：Laysi/trustiot-v2#384。前一份證據是 [rc.5 的 arm64 重建報告](2026-10-13-372-rc5-arm-rebuild.md)（Laysi/trustiot-v2#372），規則見 [CI 與發佈](../architecture/ci-release.md) 的「交叉編譯與可重現建置」。原始資料與腳本在 [data/2026-10-15-384-rootless-independent-digest/](data/2026-10-15-384-rootless-independent-digest/)。

## 環境

- **build host**：x86_64、WSL2，Ubuntu 26.04.1。podman 5.7.0（`5.7.0+ds2-3build1`）、buildah 1.42.1、containers-storage 1.61.0、skopeo 1.21.0，rootless 以一般使用者執行，root 經 `sudo`。所有執行都經 check queue（rcheck，標籤 384），並以記憶體上限執行。
- **runner 的 podman**：x64 的 `ubuntu:24.04`（`sha256:f610ab94…`，與 Laysi/trustiot-v2#310、Laysi/trustiot-v2#372 相同）容器內以 apt 安裝，版本與 ubuntu-24.04-arm runner 相同：podman `4.9.3+ds1-1ubuntu0.2`、buildah `1.33.7+ds1-1ubuntu0.24.04.3`、skopeo `1.13.3+ds1-2ubuntu0.24.04.3`、crun `1.14.1-1`、containers-image `5.29.2-2`。容器由 build host 的 root podman 以 `--privileged` 執行，容器內 root 直接執行 podman，rootless 以使用者 `runner`（uid 1001，自己的群組與 subordinate ID）執行。
- **映像**：`identity`、`assets`、`migrate`，以 `deploy/images/rust-binary.Containerfile` 打包，base 是 `distroless/static-debian13:nonroot@sha256:e2e927ec…`。比較 root 與 rootless 時用同一個執行檔（build host 上交叉編譯的 aarch64 musl 執行檔）。`postgres` 不在範圍內（有 `RUN`，另有規則）。
- **podman store**：每次打包都用自己新建的 store（`CONTAINERS_STORAGE_CONF` 指向工作目錄內的 `storage.conf`），用完刪除。沒有列出、刪除或清理共用的 store，也沒有使用 Docker。評估目錄在結束後以 `eval.sh cleanup` 刪除。

## 指令

從 worktree 根目錄經 rcheck 執行（`rcheck.sh` 是 coordinator 的 check queue，不在 repo 內；不經它時，在有 podman 與 sudo 的 x86_64 或 arm64 Linux 主機的 repo 根目錄直接執行引號內的指令）：

```sh
D=docs/test-reports/data/2026-10-15-384-rootless-independent-digest
# 1. 同一個執行檔在每種設定下以 root 與 rootless 各打包一次：build host 的 podman 5.7.0，
#    以及 ubuntu:24.04 容器內 runner 的 podman 4.9.3（pack-variants.sh、pack-one.sh、variants-container.sh）
rcheck.sh 384 "bash $D/eval.sh variants"
# 2. 改動後的 build.ts 以 rootless 與 root（sudo）各跑一次，連同 scripts 的檢查
rcheck.sh 384 "bun install --cwd scripts --frozen-lockfile; bun run --cwd scripts typecheck; bun test scripts; \
  bun scripts/ci/check-workflows.ts; bun scripts/ci/check-secret-handling.ts; bash $D/eval.sh buildts final"
# 刪除 build host 上的評估目錄與其中的 store
rcheck.sh 384 "bash $D/eval.sh cleanup"
```

輸出存成 [run-variants-1.txt](data/2026-10-15-384-rootless-independent-digest/run-variants-1.txt)、[run-variants-2.txt](data/2026-10-15-384-rootless-independent-digest/run-variants-2.txt)、[run-buildts.txt](data/2026-10-15-384-rootless-independent-digest/run-buildts.txt)，家目錄與主機路徑已由 rcheck 換成 `~/` 與 `<host copy>`。`analyze.ts` 解開每個 OCI 壓縮檔，列出映像 digest、COPY 層的 `diff_id` 與 xattr、history 的 `COPY file:<hash>` 與 `FROM <id>` 註解、`io.buildah.version` 標籤，並逐欄比較 root 與 rootless 的 config。

第一次 `variants`（run-variants-1.txt）的腳本與現在不同：`pack-one.sh` 的 `storage.conf` 只有 `graphroot`，沒有 `rootless_storage_path`，也還沒有 `podman info` 檢查，另有兩個 `COPY --chown=0:0` 與 `--identity-label=false` 的變體。那次 4.9.3 的 rootless vfs 變體其實沒有用 vfs，見下方第 2 節，所以改了 `pack-one.sh` 再跑第二次（run-variants-2.txt）。執行之後只改了一處：刪除 store 前卸載掛載點的篩選，原本以路徑前綴比對，現在只取該目錄本身與其下的路徑（不會選到名稱相同開頭的同層目錄）。

## 結果

### 1. podman 5.7.0（build host）：本來就不受影響

12 個變體中，root 與 rootless 兩兩相同（run-variants-2.txt 的 `SAME host-…`），包括 build.ts 原本的做法（overlay、有中間映像）：`sha256:3a1217a9…`。5.7.0 打包時：

- COPY 層沒有 xattr；
- history 沒有 `FROM <id>` 註解；
- 加了 `--timestamp` 時，config 沒有 `io.buildah.version` 標籤；
- build context 改由別的使用者擁有時，`COPY file:<hash>` 也不變（`14a9634f…`）。

所以這個問題出在 runner 的 4.9.3，修正要以 4.9.3 驗證。

### 2. podman 4.9.3（runner 的版本）

第二次 `variants` 的結果（run-variants-2.txt）。「COPY 層」是 COPY 層的 `diff_id`，「xattr」是 COPY 層的 PAX 標頭帶的 xattr：

| 變體 | storage driver | 選項 | root | rootless | COPY 層（root / rootless） | xattr（rootless） |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | overlay | （build.ts 原本） | `e38b39d7…` | `045b8bd0…`，**不同** | `fc577a2d…` / `8472e1b4…` | `usr/`：`user.overlay.origin=` |
| 2 | overlay | `--layers=false` | `15a6ee9b…` | `34350649…`，**不同** | `fc577a2d…` / `8472e1b4…` | 同上 |
| 3 | vfs | （無） | `e38b39d7…` | `e38b39d7…`，**相同** | `fc577a2d…` / `fc577a2d…` | 無 |
| 4 | vfs | `--layers=false` | `15a6ee9b…` | `15a6ee9b…`，相同 | 同上 | 無 |
| 5 | vfs | `--layers=false`，context 由別的使用者擁有 | `c814a692…` | `c814a692…`，相同 | 同上 | 無 |
| 6 | vfs | 同 5，Containerfile 加 `COPY --chown=0:0` | `c814a692…` | `c814a692…`，相同 | 同上 | 無 |

- 差異只有一個來源：rootless 的 overlay 在 COPY 層的 `usr/` 留下 `user.overlay.origin` xattr。它改變 COPY 層，連帶改變 history 裡中間映像的 ID（變體 1 的三個 `FROM <id>` 註解）。root 的 overlay 用 `trusted.overlay.*`，不會匯出。
- **只要改用 vfs（變體 3），rootless 就打包出 root 一直以來打包出的映像**（`e38b39d7…`，與變體 1 的 root 相同）。`--layers=false` 只拿掉 `FROM <id>` 註解，不是必要條件（變體 2 仍不同、變體 3 已相同）。
- build context 的擁有者：build.ts 讓執行 podman 的使用者自己建立 context，所以 podman 在兩種身分下都看到 0:0，`COPY file:<hash>` 相同（`a3e7d9eb…`）。context 屬於別人時 hash 會變（變體 5 是 `14a9634f…`），但在 root 與 rootless 之間仍然一致；`COPY --chown=0:0` 不改變這個 hash（變體 6 與 5 相同），所以不需要。`--identity-label=false` 只拿掉 `io.buildah.version`，與 root 或 rootless 無關（第一次 `variants`），也不需要。

第一次 `variants` 的教訓（run-variants-1.txt）：

- `storage.conf` 只設 `graphroot` 時，podman 4.9.3 的 rootless 使用者不採用它，資料庫留在預設的 store（`~/.local/share/containers/storage`）。下一次用另一個 driver 時，podman 印出 `User-selected graph driver "vfs" overwritten by graph driver "overlay" from database`，實際用的是 overlay，所以那次 rootless 的 vfs 變體仍然帶著 xattr。
- 加上 `rootless_storage_path` 之後，`podman info` 回報的 graph root 與 driver 都是指定的（`pack-one.sh` 每次都檢查），第二次就沒有這個訊息。
- 所以 build.ts 的 store 同時寫 `graphroot` 與 `rootless_storage_path`，並以 `podman info` 確認，不符就停止，不會悄悄用到使用者的 store。

### 3. podman 4.9.3 與 5.7.0 之間：不同，原因明確

同一個執行檔、同樣的設定（vfs、有中間映像），4.9.3 是 `e38b39d7…`，5.7.0 是 `3a1217a9…`（run-variants-2.txt 的 `DIFF host-rootless-3-vfs-layers / c493-rootless-3-vfs-layers`）。差在：

- base image 的四個層（重新壓縮的 gzip 不同）與 COPY 層的壓縮檔，未壓縮的 COPY 層 `diff_id` 則相同（`fc577a2d…`）；
- config 的 `io.buildah.version`（4.9.3 寫 `1.33.7`，5.7.0 在 `--timestamp` 時不寫）；
- history 的 `created_by`（5.7.0 多了 `|force-mtime=<epoch>`），以及 4.9.3 的 `FROM <id>` 註解。

這些都是 podman 與 buildah 版本本身的輸出，與 root 或 rootless 無關，也無法以選項消除。所以要重現某次發佈的 digest，打包用的 podman 要與 runner 同一版本。runner 映像不由本 repo 釘選 podman，因此 `build.json` 現在記錄打包用的 podman、buildah 版本（有 dpkg 時連同 Ubuntu 套件版本），publish 寫進 GitHub Release。

### 4. build.ts 以 root 與 rootless 各跑一次（改動後）

run-buildts.txt。同一份 snapshot（rcheck 的快照 commit `41484e41…`，`SOURCE_DATE_EPOCH` 1792038547，版本 `0.0.0-rc.1`；快照的檔案就是本分支當時的工作目錄），`plan.ts --dry-run` 之後先以 rootless 執行 build.ts，再以 `sudo` 執行。root 那次：

- 用同一個 checkout、工具鏈與 Cargo target 目錄，Cargo 沒有以 root 重新編譯任何 crate（`crates Cargo compiled as root: 0`）；
- 使用者目錄裡沒有留下 root 擁有的檔案（`root-owned files … 0`）。

兩次的 log 都印出 `packing with podman 5.7.0, buildah 1.42.1; packages: …`，build.ts 也都以 `podman info` 確認了自己的 vfs store。

| 映像 | rootless | root |
| --- | --- | --- |
| `identity` | `sha256:b2fe2e3e75178622e6b1d39db798c8513919ef68f64cb6d52ed21dcc918be8a0` | 相同 |
| `migrate` | `sha256:3aa42299bb64960f916d1ef98d54138bf943c15dbf4a82f59b8f4fd5ad7fd1db` | 相同 |
| `assets` | `sha256:47752a72a116a46f02d6c314114dc66e08908c1cbced07035dd979f37fe68646` | 相同 |

同一個 job 的 `bun test scripts` 879 個通過、0 個失敗，typecheck、`check-workflows.ts`、`check-secret-handling.ts` 都通過。

## 改動

- `scripts/release/pack.ts`（從 `build.ts` 拆出打包的部分，`build.ts` 只剩編譯）：
  - Rust 映像建在這次執行自己的 podman store，driver 是 vfs，`storage.conf` 同時設 `graphroot` 與 `rootless_storage_path`；
  - 以 `podman info` 確認 podman 用的就是這個 store 與 driver，否則停止；
  - 建完以 `podman unshare rm -rf`（rootless）或直接刪除（root）移除；
  - 自帶 Containerfile 的映像仍用使用者的 store。
  - 拆出來是為了讓 rc 比對（`pack-binaries.ts`）以與 `build.ts` 完全相同的程式打包發佈出來的執行檔。
- `build.json` 增加 `builder`（podman、buildah 版本與 dpkg 的套件版本）。`publish.ts` 讀它，以 `lib.ts` 的 `parseBuilder` 檢查（只接受版本字串，build job 執行第三方程式碼，交出的資料要先檢查），再由 `renderReleaseBody` 寫進 GitHub Release。release.yml 不用改。
- 試過、不採用：
  - `--layers=false`：4.9.3 不需要，只拿掉 `FROM <id>` 註解；
  - `COPY --chown=0:0`：不改變 4.9.3 的 COPY hash；
  - `--identity-label=false`：與 root 或 rootless 無關；
  - 偵測到 root 就拒絕或警告：有乾淨的修法，不必退而求其次。
- 文件：`docs/architecture/ci-release.md` 的「交叉編譯與可重現建置」不再要求以 rootless 打包，改寫新規則與 podman 版本的影響；「發佈內容」加上 podman 與 buildah 版本；`scripts/README.md` 的本機發佈流程加一段。

## arm64 runner 上的 rc（驗收條件 2）

**待測**：要等 coordinator 在本分支打下一個 rc tag，由 release.yml 在 arm64 runner 上以 rootless podman 4.9.3 發佈之後才能比對。比對指令（digest 取自 `gh release view <tag> --repo Laysi/trustiot-v2`）：

```sh
rcheck.sh 384 "bash $D/eval.sh rc <tag> identity=<digest> assets=<digest> migrate=<digest>"
```

`eval.sh rc` 依序做下列事情：

1. 匿名 clone 該 tag。
2. 在 build host 既有的 qemu-user binfmt 下起 arm64 的 `ubuntu:24.04` 容器，照 release.yml 的 plan 與 build 步驟跑 `plan.ts --tag` 與 `build.ts`（`rc-inner.sh`）。容器內是 podman 4.9.3 以 root 執行；qemu-user 下 rootless podman 無法啟動，見 Laysi/trustiot-v2#372。
3. 把那裡建出的 arm64 執行檔以該 tag 的 `pack.ts`（`pack-binaries.ts`）在 build host 上 rootless 與 root 各打包一次（podman 5.7.0）。
4. 每個 digest 都與發佈的比較。

預期如下：

- arm64 容器內（4.9.3、root）的三個 digest 都與 rc 相同：同版本 podman 以 root 打包，得到 runner 以 rootless 打包的結果；
- build host（5.7.0）的 rootless 與 root 彼此相同，但與 rc 不同，原因見第 3 節。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 同一個 commit 以 root 與 rootless 各執行一次 `build.ts`，`identity` 與 `migrate` 的 digest 相同，指令與 digest 記錄在本報告 | **通過**：第 4 節（build host，podman 5.7.0）。runner 的 podman 4.9.3 下同樣的打包設定（vfs）root 與 rootless 相同，見第 2 節變體 3；以 4.9.3 執行 `build.ts` 本身，在 rc 的比對（arm64 容器內 root）與 runner（rootless）各一次 |
| 改動後的第一個 rc 在 arm64 runner 上發佈的 digest，與本機以新規則打包的相同，或差異有明確原因 | **待測**：等 rc，見上一節 |
| `bun test scripts/release` 通過，並涵蓋新選項或檢查 | **通過**：`scripts/release/pack.test.ts`（store 的設定、`podman info` 的檢查）、`parseBuilder`、`parseDpkgVersions`（`lib.ts`，測試在 `pack.test.ts`），release body 的版本行（`lib.test.ts`）；`bun test scripts` 879 通過 |
| `ci-release.md` 不再把「以 rootless podman 打包」列為重現的條件 | **通過**：見「改動」 |
