# factory

產線用的程式。

- [`workstation/`](workstation/README.md)：產線工作站工具 `trustiot-workstation`：從 TPM 取出 CSR 與 EK，以工作站憑證加平台管理員帳號呼叫登錄 API（決策 12，Laysi/trustiot-v2#80）。目前有工作站自己的私鑰、CSR 與工作站憑證（`init`、`enroll`、`install-certificate`），以及平台管理員的登入與登出（`login`、`logout`）；簽章的登錄（Laysi/trustiot-v2#615）與經 ssh 登錄閘道器（Laysi/trustiot-v2#616）還沒有。建置：`cargo build --release --locked -p workstation`。

Rust 子目錄都是 workspace 成員（根目錄 `Cargo.toml` 的 `factory/*`）；非 Rust 子目錄要加進根目錄 `Cargo.toml` 的 `exclude`。
