## 驗證結果

2026-10-12T06:52:12Z 由 release workflow 的 verify job 以唯讀權限從 registry 檢查，以本機測試金鑰驗證簽章（本機測試，不是正式發佈）：

- 映像 `identity`：tag 指向 `sha256:8080c5f5ea6154d6d15375ddbc213b1bddf694b7994448e07725ec3d8b55ef21`；平台 linux/arm64；cosign 簽章驗證通過；SBOM attestation 驗證通過（SPDX-2.3，213 個套件）。
- 映像 `postgres`：tag 指向 `sha256:040a7fad94dfa3bf8230f884ac588631efaaa8ce274e9c2af04e767a96759b5a`；平台 linux/arm64；cosign 簽章驗證通過；SBOM attestation 驗證通過（SPDX-2.3，142 個套件）。
- 映像 `release-selftest`：tag 指向 `sha256:43f2cb6ad0d5d690a8f7b891ee26c1006beb2cbbe5bc6f1973ce38bdee88a0dd`；平台 linux/arm64；cosign 簽章驗證通過；SBOM attestation 驗證通過（SPDX-2.3，13 個套件）。
- Helm chart `trustiot`：tag 指向 `sha256:536de43f9275c19fc8a47de5f9c473fa299aa120bf5c4c52b4d983c05cdee76c`；cosign 簽章驗證通過；拉下的 chart 就是發佈的檔案；Chart.yaml 的 version 與 appVersion 都是 `0.0.0-rc.1`。
