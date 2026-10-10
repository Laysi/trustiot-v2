# factory

產線用的程式。

- `workstation/`：產線工作站程式：從 TPM 取出 CSR 與 EK，以工作站憑證加平台管理員帳號呼叫登錄 API（決策 12）。

Rust 子目錄都是 workspace 成員（根目錄 `Cargo.toml` 的 `factory/*`）；非 Rust 子目錄要加進根目錄 `Cargo.toml` 的 `exclude`。
