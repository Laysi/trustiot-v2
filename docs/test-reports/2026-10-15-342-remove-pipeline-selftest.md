# 刪除發佈流程的自測映像：本機發佈流程與 C 交叉編譯器的證明（2026-10-15）

對應 issue：Laysi/trustiot-v2#342。原始資料在 [data/2026-10-15-342-remove-pipeline-selftest/](data/2026-10-15-342-remove-pipeline-selftest/)：執行的腳本 [job.sh](data/2026-10-15-342-remove-pipeline-selftest/job.sh) 與它的輸出 [job-output.txt](data/2026-10-15-342-remove-pipeline-selftest/job-output.txt)（去掉了 `Compiling` 行、apt 的下載與解包行、映像層的 `Copying blob` 行）。依據與規則寫在 [docs/architecture/ci-release.md](../architecture/ci-release.md) 的「交叉編譯與可重現建置」。

## 環境

- **程式碼**：分支 `chore/342-remove-release-selftest`，commit `5421853`（base 為 main 的 `8534a60`）的工作樹，加上這份報告的 `job.sh`。檢查佇列把工作樹複製到建置主機、做成只有一個 snapshot commit 的 git repo 再執行，所以輸出裡 plan 與映像標籤的 commit（`68a9e530…`）是那個 snapshot，不是分支上的 commit；映像 digest 也因此只對這次執行有意義。
- **建置主機**：x86_64 Linux，經共用的檢查佇列（`rcheck.sh`）執行：`CI=1`、`CARGO_BUILD_JOBS=10`、這個 worktree 專用的 cargo target 目錄、nice 10、記憶體上限 12 GiB。podman 5.7.0（rootless）、skopeo 1.21.0-dev 來自系統；bun 1.4.2、helm 4.3.0、zig 0.16.0、cargo-zigbuild 0.23.4、cargo-auditable 0.7.6、syft 1.52.0 由 `.prototools` 鎖定、以 `proto install` 安裝；Rust 1.99.0（`rust-toolchain.toml`）加上 `aarch64-unknown-linux-musl` target（`build.ts` 自己加）。
- **隔離**：podman 用工作目錄裡自己的 store（`CONTAINERS_STORAGE_CONF` 指向的 `storage.conf`），映像壓縮檔與實驗的 target 目錄也在同一個暫存目錄（job 副本的 `target/` 下，git 忽略）。job 結束時（含失敗，`trap`）在 podman 的 user namespace 裡卸載並刪除 store，再刪除整個目錄；之後檢查沒有留下目錄，也沒有留下掛載。沒有啟動 registry 或任何容器：驗收只要求 plan 與 build。

## 指令

在 repo 根目錄：

```sh
bash /path/to/rcheck.sh 342 'bash docs/test-reports/data/2026-10-15-342-remove-pipeline-selftest/job.sh'
```

`job.sh` 依序執行（每一步印出 `=== <名稱> ===` 與結束碼，應該失敗的實驗成功時 job 失敗）：

1. repo 的檢查：`cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo test --locked --workspace`、`cargo deny --locked check`、`bun install --cwd scripts --frozen-lockfile`、`bun run --cwd scripts typecheck`、`bun test scripts`、`bun test scripts/release`、`bun scripts/ci/check-workflows.ts`、`bun scripts/ci/check-secret-handling.ts`。沒有改 workflow，所以不跑 `lint-workflows.ts`。
2. 三個 Rust 映像裡誰編譯 C：`cargo tree --locked --target aarch64-unknown-linux-musl -p <identity|assets|migrate> -e normal,build -i cc --depth 2`。
3. 本機發佈流程，照 [scripts/README.md](../../scripts/README.md)「本機跑一次完整流程」的前兩步：
   ```sh
   bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run \
     --registry 127.0.0.1:5068/laysi/trustiot-v2 --output "$work/plan.json"
   bun scripts/release/build.ts --plan "$work/plan.json" --out "$work/out"
   ```
4. C 交叉編譯器的實驗，每個都只建 `ring@0.17.14`，各用自己的 target 目錄，指令與 `scripts/release/rust-build.ts` 相同（`cargo auditable zigbuild --locked --release --target aarch64-unknown-linux-musl`，PATH 與 `CARGO_ZIGBUILD_ZIG_PATH` 也相同）：
   1. 對照：釘選的 Zig，加 `CC_ENABLE_DEBUG_OUTPUT=1` 讓 `cc` 印出它讀到的變數與執行的指令。
   2. `CARGO_ZIGBUILD_ZIG_PATH` 指向包裝腳本：其他子指令照常交給釘選的 Zig，只有 `zig cc` 遇到 C 或組合語言原始檔時失敗。a：所有原始檔都失敗；b：只有 `ring-<版本>/` 下的原始檔失敗。
   3. 不經 cargo-zigbuild：`cargo build --locked --release --target aarch64-unknown-linux-musl -p ring@0.17.14`，主機上沒有 `aarch64-linux-musl-gcc`。

## 結果

### 檢查

全部通過（[job-output.txt](data/2026-10-15-342-remove-pipeline-selftest/job-output.txt) 開頭）：fmt、clippy、`cargo test --workspace`（沒有失敗）、`cargo deny`（advisories、bans、licenses、sources ok）、typecheck、`bun test scripts`（873 pass、4 skip、0 fail；skip 是需要特定環境的測試，例如以 podman 起整套開發環境的 e2e 與只在 arm64 主機執行的測試）、`bun test scripts/release`（153 pass、1 skip、0 fail；skip 是 `lib.test.ts` 只在 arm64 主機執行的 ELF 測試）、workflow policy 與 secret handling 無違規。

### 本機發佈流程

plan 只列出四個映像，沒有自測映像：

| 映像 | 種類 | 壓縮檔大小 | SBOM 套件數 | SBOM 有 `ring` |
| --- | --- | --- | --- | --- |
| `assets` | Rust（package `assets`） | 4,399,616 bytes | 214 | 是 |
| `identity` | Rust（package `identity`） | 4,516,864 bytes | 214 | 是 |
| `migrate` | Rust（package `migrate`） | 3,926,528 bytes | 215 | 是 |
| `postgres` | Containerfile（`deploy/images/postgres`） | 87,012,352 bytes | 142 | 否 |

加上 chart `charts/trustiot:0.0.0-rc.1`。build 以一次 Cargo 執行交叉編譯三個 Rust 套件，四個映像都建成 OCI 壓縮檔並產生 SPDX 2.3 SBOM，`build.ts` 結束碼 0。唯一的編譯警告是 zig 連結器既有的 `ignoring deprecated linker optimization setting '1'`。postgres 的建置階段下載 AGE 時第一個鏡像站逾時（`curl: (28)`），腳本照設計改從 archive 站下載並通過 SHA-512 檢查，與這張 issue 無關。

### 三個 Rust 映像裡編譯 C 的 crate

三個套件的結果相同，`cc` 只被 `ring` 當成 build dependency 使用：

```text
cc v1.7.0
[build-dependencies]
└── ring v0.17.14
    ├── rustls v0.23.46
    └── rustls-webpki v0.103.15
```

另外在工作站上以 `cargo tree --locked --target aarch64-unknown-linux-musl -p <套件> -e normal,build --prefix none` 列出三個套件的完整相依，找 `cmake`、`aws-lc-sys`、`openssl`、`bindgen`、`pkg-config` 等其他編譯原生程式碼的 crate，都沒有。

### C 交叉編譯器的實驗

| 實驗 | 結果 |
| --- | --- |
| 1. 釘選的 Zig（對照） | 成功。`cc` 讀到 `CC_aarch64_unknown_linux_musl = Some(~/.cache/cargo-zigbuild/0.23.4/wrappers/…/zigcc-aarch64-unknown-linux-musl-….sh)`，以它執行 24 次編譯：13 個 C 檔與 11 個 aarch64 組合語言檔（`aesv8-armx-linux64.S`、`sha256-armv8-linux64.S` 等） |
| 2a. `zig cc` 不編譯任何檔案 | 失敗，結束碼 1：cargo-zigbuild 開始前探測 `zig cc -v` 就停下（`Failed to collect zig cc options`） |
| 2b. `zig cc` 不編譯 `ring` 的原始檔 | 失敗，結束碼 101：`error: failed to run custom build command for ring v0.17.14`，原因是 `error occurred in cc-rs: command did not execute successfully` |
| 3. 不經 cargo-zigbuild（沒有目標的 C 編譯器） | 失敗，結束碼 101：`error: failed to run custom build command for ring v0.17.14`，原因是 `error occurred in cc-rs: failed to find tool "aarch64-linux-musl-gcc"`；`cc` 沒有退回主機的編譯器 |

## 結論

- 驗收條件「repo 中除了過去的測試報告，不再引用 `release-selftest`；`cargo test --workspace`、`bun test scripts/release` 都通過」：**通過**。`git grep release-selftest -- . ':!docs/test-reports'` 沒有結果；兩項測試見上方「檢查」。
- 驗收條件「交叉編譯會用到目標的 C 編譯器仍有測試或建置檢查涵蓋」：**通過**，由建置本身涵蓋。`identity`、`assets`、`migrate` 都經 rustls 相依 `ring` 0.17，它的 build script 以 `cc` 用目標的 C 編譯器（cargo-zigbuild 設定的 Zig）編譯 C 與 aarch64 組合語言；C 編譯器壞掉（實驗 2a、2b）或不存在（實驗 3）時，建置失敗而不是警告，`cc` 也不退回主機的編譯器。這正是原本自測映像的 build script 檢查的事（交叉編譯時要有目標的 C 編譯器，C 程式真的以它編譯）；它在執行時比對 C 與 Rust 指標寬度的檢查只在 arm64 上手動執行過，不是發佈流程的一部分。
- 驗收條件「以本機流程跑一次 `plan.ts --dry-run` 與 `build.ts`：plan 只有服務映像，建置成功」：**通過**。plan 只有 `assets`、`identity`、`migrate`、`postgres` 四個 chart 部署的映像，建置成功。
- 驗收條件「文件已更新」：見 PR 的變更清單（`docs/architecture/ci-release.md`、`docs/architecture/repository-layout.md`、`deploy/images/README.md`、`scripts/README.md`、`tools/README.md`）。
