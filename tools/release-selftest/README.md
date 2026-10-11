# release-selftest

release.yml 的流程自測映像，不是服務：在還沒有任何服務映像時，讓每次發佈都真的走過交叉編譯、建置映像、簽章、SBOM 與驗證整條路。它不在 Helm chart 裡，也不部署。第一個服務映像加入 `deploy/images/` 後，連同 `deploy/images/release-selftest/` 一起刪除。

程式連結一個 C 物件（`src/target.c`，由 build script 以 `cc` 編譯），所以交叉編譯時 C 編譯器與連結器（Zig）也被驗證到。執行時印出一行 JSON：版本、架構、作業系統，以及 Rust 與 C 各自看到的指標寬度；兩者不一致時以結束碼 1 結束。

```sh
cargo run -p release-selftest
# 發佈的映像（GHCR 套件是私有的，先登入）：
podman run --rm ghcr.io/laysi/trustiot-v2/release-selftest:<版本>
```

在 arm64 主機上執行發佈的映像，預期輸出 `"arch":"aarch64"` 與 `"c_object":"ok"`，`version` 是發佈的版本。
