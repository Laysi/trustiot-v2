# 身分服務映像：aarch64 musl 建置、arm64 啟動與 nightly 輔助程式（2026-10-11）

對應 issue：Laysi/trustiot-v2#308。原始資料在 [data/2026-10-11-308-identity-image/](data/2026-10-11-308-identity-image/)。映像的宣告方式見 [deploy/images/README.md](../../deploy/images/README.md)，nightly 的用法與成本見 [docs/architecture/ci-release.md](../architecture/ci-release.md) 的「nightly.yml」。

## 環境

- **commit**：`9c183ad`（分支 `feat/308-identity-image`，base 為 main 的 `dddd9ed`）。映像的 `org.opencontainers.image.revision` 就是這個 commit。之後的 commit 只改文件（含 `scripts/README.md`）與這份報告，沒有改 Rust 與 `scripts/` 的程式。
- **建置機**：本機 x86_64 的 Fedora 45（kernel 7.2.8），8 核，與其他 agent 共用（建置期間負載 3–5）。經共用的檢查佇列（`rcheck.sh`）執行：`CI=1`、`CARGO_BUILD_JOBS=6`、worktree 自己的 `target/`（開始時不存在，即冷建置）、nice 10。podman 6.1.3、skopeo 1.24.1 來自系統；bun 1.4.2、helm 4.3.0、zig 0.16.0、cargo-zigbuild 0.23.4、cargo-auditable 0.7.6、syft 1.52.0 由 `.prototools` 鎖定、以 proto 安裝；Rust 1.99.0（`rust-toolchain.toml`）加上 `aarch64-unknown-linux-musl` target。沒有 docker：nightly 輔助程式的建置以 podman 代替 docker 執行（`podman build` 與 `docker build` 的參數相同，都來自 `containerBuildArgs`）。
- **arm64 環境**：lab 的 Mac mini M1（`trustiot-mac-m1`），Fedora Linux Asahi Remix 44（kernel 7.1.13-402.asahi.fc44.aarch64+16k）、podman 5.8.7。依 lab 規則取得 `~/e2e.lock`（2026-10-11T15:12:39Z）後使用，用完釋放。lab 上沒有編譯任何東西，只載入本機建好的映像。
- **基底映像**：`gcr.io/distroless/static-debian13:nonroot@sha256:e2e927ec666bae08560abb3c55d0659eceabb657f56b6782ab500a9fc7f555e3`，釘在 `deploy/images/rust-binary.Containerfile`（本 issue 沒有改它）。

## 指令

本機建置（[build-all.sh](data/2026-10-11-308-identity-image/build-all.sh)，輸出 [local-build.txt](data/2026-10-11-308-identity-image/local-build.txt)），在 repo 根目錄：

```sh
bash rcheck.sh 308 'bash build-all.sh'
# build-all.sh 做的事：
bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run --output ~/.cache/trustiot-v2-308/plan.json
bun scripts/release/build.ts --plan ~/.cache/trustiot-v2-308/plan.json --out ~/.cache/trustiot-v2-308/out
# nightly 的 buildServiceImage，以 podman 代替 docker（nightly-build-podman.ts）
bun nightly-build-podman.ts "$PWD" ~/.cache/trustiot-v2-308/nightly-helper.log
# 兩個映像各取出 /usr/local/bin/identity
podman create --platform linux/arm64 <image>; podman cp <container>:/usr/local/bin/identity <file>
file <file>; sha256sum <file>
```

arm64 啟動（[lab-run.sh](data/2026-10-11-308-identity-image/lab-run.sh)，輸出 [lab-run.txt](data/2026-10-11-308-identity-image/lab-run.txt)）：

```sh
ssh trustiot-mac-m1 'mkdir -m 700 ~/e2e-308 && cat > ~/e2e-308/identity.oci.tar' < out/images/identity.oci.tar
ssh trustiot-mac-m1 'bash -s' < lab-run.sh
```

`lab-run.sh` 載入 release 建出的 OCI 壓縮檔（就是 publish 會原封不動推送的那個檔案），在 lab 當場產生拋棄式的設定：Ed25519 私鑰（`openssl genpkey`，只存在 `~/e2e-308/keys/`）、由它算出的公鑰組成的 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`（`kid` 為 `e2e-308`）、一條 cookie 路由、指向不存在主機 `postgres.e2e-308.invalid` 的 Postgres 設定與隨機的資料庫密碼（`openssl rand`），以 `--env-file` 交給容器。容器以映像自己的使用者（65532）、唯讀根檔案系統、`--cap-drop=ALL`、`no-new-privileges` 執行，健康埠 8081 對應到 lab 的 `127.0.0.1:18308`，API 埠 8080 對應到 `127.0.0.1:18309`。結束時（含失敗時，`trap`）刪除容器、映像與 `~/e2e-308`；私鑰與密碼沒有印出、沒有離開 lab，也不在這份報告裡。

單元測試：

```sh
bun test scripts/release   # 含新的 staticElfProblem、toolchainChannel、rustCrossCompileCommand 與 identity 映像宣告
bun test scripts/nightly   # 含新的 service-image.test.ts
```

## 結果

### 建置

| 項目 | 結果 |
| --- | --- |
| plan | 列出 `identity`（Rust package `identity`）與 `release-selftest` 兩個映像 |
| release 建置時間 | 196 秒：plan（含 `helm lint`）、從空的 `target/` 交叉編譯兩個套件（約 214 個 crate）、打包兩個映像、syft SBOM。`target/` 結束時 497 MB |
| 映像 | `ghcr.io/laysi/trustiot-v2/identity:0.0.0-rc.1` → `sha256:43fc4baee16a5712aeefb2879f216e3d507175806dd591a5777c616daaa428eb`（[build.json](data/2026-10-11-308-identity-image/build.json)），只有 linux/arm64 |
| 大小 | OCI 壓縮檔 4,192,256 bytes（壓縮後的層共 4,168,512 bytes）；解開後 9,790,206 bytes；執行檔 6,362,000 bytes |
| 映像設定 | `User` 65532:65532，`Entrypoint` `/usr/local/bin/identity` |
| SBOM | SPDX-2.3，213 個套件（含 cargo-auditable 嵌入的 crate 清單） |
| 執行檔 | `ELF 64-bit LSB executable, ARM aarch64, version 1 (SYSV), statically linked, stripped`（`file`）；`build.ts` 的 `aarch64ElfProblem` 與新增的 `staticElfProblem` 都通過 |
| nightly 輔助程式 | `buildServiceImage` 建出 `localhost/trustiot-e2e/identity:nightly`（linux/arm64、65532、同一個 entrypoint）。cargo 沿用剛才的建置（`Finished … in 0.24s`），整段 6 秒 |
| 兩條路的執行檔 | SHA-256 相同：`17bb1421ce36aa7c756c9cc116a1ac10f003ca512596a534e03346493a41e3d6`。nightly 用的就是發佈會建出的執行檔 |

建置中唯一的警告是 zig 連結器的 `ignoring deprecated linker optimization setting '1'`（`linker_messages`），`release-selftest` 也有，與本 issue 無關。

### arm64 啟動

| 檢查 | 結果 |
| --- | --- |
| 載入 | `podman load` 得到 `sha256:5c483ac9975c…`，與本機建置的映像 ID 相同；`os=linux arch=arm64` |
| `GET /healthz`（健康埠） | **200**，容器啟動後 1 秒內；`cache-control: no-store` |
| `GET /readyz`（健康埠） | 503：資料庫主機不存在，連線池開不起來，符合服務的設計（`/readyz` 等常駐連線開好才回 200） |
| 沒有 cookie 的 `GET /internal/ext-authz/`（API 埠） | 401，日誌記 `denied the ext_authz check`、`reason: no_session_cookie`（不查資料庫） |
| 容器狀態 | `running`，重啟 0 次，使用者 65532:65532 |
| 啟動日誌 | `signing internal identity tokens`，`kid` 為 `e2e-308`、`routes` 為 1：服務用當場產生的私鑰找到自己的 `kid` |
| 日誌不含密鑰 | 日誌裡沒有資料庫密碼，也沒有 `PRIVATE KEY` |
| 清理 | 容器、映像（`5c483ac9975c`）與 `~/e2e-308` 都不在了；lab 的鎖已釋放 |

### 單元測試

| 指令 | 結果 |
| --- | --- |
| `bun test scripts/release` | 通過（含 identity 映像宣告、`staticElfProblem`、`toolchainChannel`、`rustCrossCompileCommand`） |
| `bun test scripts/nightly` | 通過（含 `service-image.test.ts`：以假的 rustup、cargo、docker 與 k3d 驗證指令、build context 只有執行檔與 Containerfile、拒絕動態連結與非 aarch64 的執行檔、拉基底映像失敗時重試、匯入後逐節點確認） |

最終的檢查結果與次數記在 PR 內文。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| `deploy/images/identity/image.toml` 存在，release 的 plan 會納入它；`bun test scripts/release` 通過 | 通過：plan 列出 `identity`，`lib.test.ts` 的「the repository's own images are valid」檢查它 |
| 在 arm64 環境建出映像並啟動，`/healthz` 回 200，binary 是靜態連結的 aarch64；指令與輸出存到 `docs/test-reports/` | 通過：本機以 release 流程交叉編譯並建出 arm64 映像（沒有模擬器，也不在 arm64 上編譯），在 lab 的 arm64 podman 啟動，`/healthz` 200；`file` 與 `staticElfProblem` 確認是靜態連結的 aarch64 |
| nightly 有建置並匯入身分服務映像的輔助程式，附單元測試；`bun test scripts/nightly` 通過 | 通過：`scripts/nightly/service-image.ts` 的 `loadServiceImage`；建置那一半在本機以 podman 實跑過，匯入那一半（`importImageIntoCluster`）只在 CI 有 k3d，這裡以單元測試驗證。nightly 尚未呼叫它（不在本 issue 範圍） |
| README 已更新 | 通過：`deploy/images/README.md`、`services/identity/README.md`、`scripts/README.md`、`docs/architecture/ci-release.md` |
