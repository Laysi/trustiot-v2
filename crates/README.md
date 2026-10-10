# crates

服務與閘道器程式共用的 Rust library crate：服務間契約、閘道器↔雲端訊息格式、服務骨架、事件框架、OpenAI 相容 client、測試工具。各 crate 的職責見[專案目錄](../docs/architecture/repository-layout.md)，每個 crate 用自己的 `README.md` 說明用法與規則。

- 每個子目錄都是 workspace 成員（根目錄 `Cargo.toml` 的 `crates/*`），必須有 `Cargo.toml`，並以 `edition.workspace = true` 與 `[lints] workspace = true` 繼承 workspace 設定（`cargo test -p repo-check` 會檢查）。
- 第三方相依套件先宣告在根目錄 `Cargo.toml` 的 `[workspace.dependencies]`，crate 內以 `<name>.workspace = true` 引用。
- 閘道器會用到的 crate（例如 `protocol`）不得引入只能在雲端編譯的相依套件，要能編到 aarch64。
