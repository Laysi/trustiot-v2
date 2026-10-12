# deploy/images

release.yml 發佈的容器映像。每個子目錄是一個映像，目錄名就是映像名，也是 chart 裡的服務名；發佈為 `ghcr.io/laysi/trustiot-v2/<目錄名>:<版本>`，只有 linux/arm64。流程、簽章與驗證見 [CI 與發佈](../../docs/architecture/ci-release.md) 的「release.yml」。

目錄名用小寫英文與數字，以單一連字號連接（例如 `field-context`），最多 63 字元；`charts` 保留給 Helm chart。每個子目錄只能有下列兩種宣告之一，本目錄下的檔案（例如本 README 與 `rust-binary.Containerfile`）不是映像。

目前的映像：

| 目錄 | 內容 |
| --- | --- |
| `identity` | 身分服務（`services/identity`），chart 的 `services.identity`；2026-10-11 加入（Laysi/trustiot-v2#308），arm64 啟動驗證見[測試報告](../../docs/test-reports/2026-10-11-308-identity-image.md) |
| `migrate` | migration Job（`tools/migrate`），chart 的 `migrate.image`；Job 執行 `migrate run`，每個服務 pod 的 init container 執行 `migrate wait-for-job`。2026-10-13 加入（Laysi/trustiot-v2#60），nightly 從原始碼建置它 |
| `release-selftest` | 發佈流程的自測映像（`tools/release-selftest`），不是服務，不部署 |

發佈的 chart 以 digest 固定它部署的映像：`postgres`，以及名稱是 chart `services` 的 key 的映像（`scripts/release/chart-images.ts`，見 [chart README](../chart/README.md)「映像的 digest」）。chart 不部署的映像要列在該檔的 `NOT_DEPLOYED_BY_CHART`，否則發佈停止，`bun test scripts` 也會失敗。

## Rust 服務

子目錄只放 `image.toml`：

```toml
[rust]
package = "identity"        # Cargo 套件名
# bin = "identity"          # 套件有多個執行檔時才需要
```

套件的執行檔交叉編譯成 `aarch64-unknown-linux-musl` 的靜態執行檔，再以 [rust-binary.Containerfile](rust-binary.Containerfile) 包成映像：distroless static 基底、UID 65532、`ENTRYPOINT` 就是該執行檔。未知的欄位會讓發佈失敗，不會被忽略。建置會檢查執行檔是 aarch64 的 ELF，而且沒有 program interpreter（動態連結的執行檔需要載入器與 libc，distroless static 沒有，容器會啟動失敗），任一項不符就停止（`scripts/release/lib.ts` 的 `aarch64ElfProblem`、`scripts/release/rust-build.ts` 的 `staticElfProblem`）。

### 在 nightly 的 k3d 使用

`scripts/nightly/service-image.ts` 的 `loadServiceImage(platform, "<目錄名>")` 以發佈相同的 Cargo 指令（`scripts/release/rust-build.ts` 的 `rustCrossCompileCommand`）與同一份 Containerfile 建出映像，標為 `localhost/trustiot-e2e/<目錄名>:nightly`（不推送），再以 `importImageIntoCluster` 匯入每個 k3d 節點並逐一確認。chart 以 `serviceImageValues("<目錄名>")` 的 `repository`、`tag` 與 `pullPolicy: IfNotPresent` 使用它。`scripts/nightly/run.ts` 以它建置 `migrate`（Laysi/trustiot-v2#60），nightly.yml 為此提供 rustup 與 `.prototools` 的 zig、cargo-zigbuild、cargo-auditable；服務映像（`services.identity` 等）由 Laysi/trustiot-v2#65、Laysi/trustiot-v2#77 接上。成本見 [CI 與發佈](../../docs/architecture/ci-release.md) 的「nightly.yml」。

## 自帶 Containerfile 的映像

子目錄放 `Containerfile`，以該目錄為 build context，以 `podman build --platform linux/arm64` 建置。arm64 那一層不能有 `RUN`：要編譯的東西放在 `FROM --platform=$BUILDPLATFORM` 的建置階段交叉編譯後 `COPY` 進來，或取用上游已有的 arm64 映像。release.yml 改在 arm64 runner 上建置後（Laysi/trustiot-v2#310），在那裡 arm64 那一層的 `RUN` 其實能執行；這條規則仍然保留，讓 x86 的機器不用模擬器也能跑本機發佈流程，所以改由 plan 檢查，違反就停止（`scripts/release/containerfile.ts` 的 `targetStageRunProblems`）。理由見 [CI 與發佈](../../docs/architecture/ci-release.md) 的「交叉編譯與可重現建置」。基底映像以 tag 加 digest 釘選。範例是 [postgres](postgres/README.md)（CloudNativePG 的 Postgres 加交叉編譯的 Apache AGE）。

SBOM 由 syft 從映像產生，只列得出套件資料庫（例如 dpkg）或執行檔本身宣告的東西。自己編譯後 `COPY` 進來的程式不在任何套件資料庫裡，要在連結時加上 ELF package note（systemd 的 ELF package metadata，例如 `-Xlinker '--package-metadata={"type":"generic","name":"<名稱>","version":"<版本>"}'`），syft 才會把它列進 SBOM；postgres 映像的 AGE 就是這樣做。

## 本機試建

`bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run --output plan.json` 列出會發佈的映像，`bun scripts/release/build.ts --plan plan.json --out <目錄>` 在本機建出映像與 SBOM（需要 podman、skopeo 與 `proto install` 裝好的工具），見 [scripts/README.md](../../scripts/README.md)。
