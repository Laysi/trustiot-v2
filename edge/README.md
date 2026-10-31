# edge

在現場閘道器上執行的程式與閘道器映像。

- `agent/`：閘道器 agent，由 systemd 管理（決策 10）。
- `updater/`：自建 OTA 更新程式：驗證簽章、切換版本、健康檢查失敗自動回滾（決策 11）。
- `image/`：閘道器映像（2026-10-31 起，Laysi/trustiot-v2#809）：以官方 Raspberry Pi OS Lite（arm64）為底，加上資料分割區、agent、TPM overlay、NTP 與只收金鑰的 SSH，用 Bun 寫成，在 arm64 Linux 建置主機的 rootless podman 容器裡建置與檢查，見 [image/README.md](image/README.md)。LUKS（Laysi/trustiot-v2#810）、防火牆與 console/USB/serial 的強化由後續 issue 加上。

Rust 子目錄都是 workspace 成員（根目錄 `Cargo.toml` 的 `edge/*`），要能交叉編譯到 aarch64。`image/` 不是 Rust 套件，已列在根目錄 `Cargo.toml` 的 `exclude`；之後新增其他非 Rust 子目錄時也要加進 `exclude`，否則 cargo 會找不到它的 `Cargo.toml` 而失敗。
