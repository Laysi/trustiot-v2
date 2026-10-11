# deploy/images

release.yml 發佈的容器映像。每個子目錄是一個映像，目錄名就是映像名，也是 chart 裡的服務名；發佈為 `ghcr.io/laysi/trustiot-v2/<目錄名>:<版本>`，只有 linux/arm64。流程、簽章與驗證見 [CI 與發佈](../../docs/architecture/ci-release.md) 的「release.yml」。

目錄名用小寫英文與數字，以單一連字號連接（例如 `field-context`），最多 63 字元；`charts` 保留給 Helm chart。每個子目錄只能有下列兩種宣告之一，本目錄下的檔案（例如本 README 與 `rust-binary.Containerfile`）不是映像。

## Rust 服務

子目錄只放 `image.toml`：

```toml
[rust]
package = "identity"        # Cargo 套件名
# bin = "identity"          # 套件有多個執行檔時才需要
```

套件的執行檔交叉編譯成 `aarch64-unknown-linux-musl` 的靜態執行檔，再以 [rust-binary.Containerfile](rust-binary.Containerfile) 包成映像：distroless static 基底、UID 65532、`ENTRYPOINT` 就是該執行檔。未知的欄位會讓發佈失敗，不會被忽略。

## 自帶 Containerfile 的映像

子目錄放 `Containerfile`，以該目錄為 build context，以 `podman build --platform linux/arm64` 建置。release.yml 在 x86 的 GitHub 代管 runner 上建置，而且不使用模擬器，所以 arm64 那一層不能有 `RUN`：要編譯的東西放在 `FROM --platform=$BUILDPLATFORM` 的建置階段交叉編譯後 `COPY` 進來，或取用上游已有的 arm64 映像。基底映像以 tag 加 digest 釘選。範例是 [postgres](postgres/README.md)（CloudNativePG 的 Postgres 加交叉編譯的 Apache AGE）。

SBOM 由 syft 從映像產生，只列得出套件資料庫（例如 dpkg）或執行檔本身宣告的東西。自己編譯後 `COPY` 進來的程式不在任何套件資料庫裡，要在連結時加上 ELF package note（systemd 的 ELF package metadata，例如 `-Xlinker '--package-metadata={"type":"generic","name":"<名稱>","version":"<版本>"}'`），syft 才會把它列進 SBOM；postgres 映像的 AGE 就是這樣做。

## 本機試建

`bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run --output plan.json` 列出會發佈的映像，`bun scripts/release/build.ts --plan plan.json --out <目錄>` 在本機建出映像與 SBOM（需要 podman、skopeo 與 `proto install` 裝好的工具），見 [scripts/README.md](../../scripts/README.md)。
