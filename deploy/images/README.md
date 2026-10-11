# deploy/images

release.yml 發佈的容器映像。每個子目錄是一個映像，目錄名就是映像名，也是 chart 裡的服務名；發佈為 `ghcr.io/laysi/trustiot-v2/<目錄名>:<版本>`，只有 linux/arm64。流程、簽章與驗證見 [CI 與發佈](../../docs/architecture/ci-release.md) 的「release.yml」。

目錄名用小寫英文與數字，以單一連字號連接（例如 `field-context`），最多 63 字元；`charts` 保留給 Helm chart。每個子目錄只能有下列兩種宣告之一，本目錄下的檔案（例如本 README 與 `rust-binary.Containerfile`）不是映像。

目前的映像：

| 目錄 | 內容 |
| --- | --- |
| `identity` | 身分服務（`services/identity`），chart 的 `services.identity`；2026-10-11 加入（Laysi/trustiot-v2#308），arm64 啟動驗證見[測試報告](../../docs/test-reports/2026-10-11-308-identity-image.md) |
| `release-selftest` | 發佈流程的自測映像（`tools/release-selftest`），不是服務，不部署 |

## Rust 服務

子目錄只放 `image.toml`：

```toml
[rust]
package = "identity"        # Cargo 套件名
# bin = "identity"          # 套件有多個執行檔時才需要
```

套件的執行檔交叉編譯成 `aarch64-unknown-linux-musl` 的靜態執行檔，再以 [rust-binary.Containerfile](rust-binary.Containerfile) 包成映像：distroless static 基底、UID 65532、`ENTRYPOINT` 就是該執行檔。未知的欄位會讓發佈失敗，不會被忽略。建置會檢查執行檔是 aarch64 的 ELF，而且沒有 program interpreter（動態連結的執行檔需要載入器與 libc，distroless static 沒有，容器會啟動失敗），任一項不符就停止（`scripts/release/lib.ts` 的 `aarch64ElfProblem`、`staticElfProblem`）。

### 在 nightly 的 k3d 使用

`scripts/nightly/service-image.ts` 的 `loadServiceImage(platform, "<目錄名>")` 以發佈相同的 Cargo 指令（`lib.ts` 的 `rustCrossCompileCommand`）與同一份 Containerfile 建出映像，標為 `localhost/trustiot-e2e/<目錄名>:nightly`（不推送），再以 `importImageIntoCluster` 匯入每個 k3d 節點並逐一確認。chart 以 `serviceImageValues("<目錄名>")` 的 `repository`、`tag` 與 `pullPolicy: IfNotPresent` 使用它。目前沒有呼叫端；nightly 啟用 `services.identity` 時（Laysi/trustiot-v2#65、Laysi/trustiot-v2#77）才接上，那時 workflow 要另外提供 rustup 與 `.prototools` 的 zig、cargo-zigbuild、cargo-auditable，成本見 [CI 與發佈](../../docs/architecture/ci-release.md) 的「nightly.yml」。

## 自帶 Containerfile 的映像

子目錄放 `Containerfile`，以該目錄為 build context，以 `podman build --platform linux/arm64` 建置。release.yml 在 x86 的 GitHub 代管 runner 上建置，而且不使用模擬器，所以 arm64 那一層不能有 `RUN`：要編譯的東西放在 `FROM --platform=$BUILDPLATFORM` 的建置階段交叉編譯後 `COPY` 進來，或取用上游已有的 arm64 映像。基底映像以 tag 加 digest 釘選。

## 本機試建

`bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run --output plan.json` 列出會發佈的映像，`bun scripts/release/build.ts --plan plan.json --out <目錄>` 在本機建出映像與 SBOM（需要 podman、skopeo 與 `proto install` 裝好的工具），見 [scripts/README.md](../../scripts/README.md)。
