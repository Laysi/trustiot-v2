# Cargo workspace 與 repo 骨架驗收（2026-10-10）

對應 issue：Laysi/trustiot-v2#48 建立 Cargo workspace、.prototools 與 repo 骨架。原始資料在 [data/2026-10-10-48-workspace-skeleton/](data/2026-10-10-48-workspace-skeleton/)。

## 環境

- 機器：本機開發機，Fedora Linux 45（kernel 7.2.8），x86_64。本 issue 不需要 lab 端到端測試。
- 受測版本：分支 `build/48-workspace-skeleton` 的 commit `99eb6b1`，從 `https://github.com/Laysi/trustiot-v2.git` 全新 clone。
- 工具：proto 0.60.2。`.prototools` 鎖定 Rust 1.99.0（2026-10-01 發佈的 stable）與 Bun 1.4.2；`rust-toolchain.toml` 鎖定同一個 Rust 版本，含 clippy 與 rustfmt。
- 模擬沒有任何工具鏈的新機器：proto、rustup、cargo 的家目錄都指向空目錄，`PATH` 只放 proto 執行檔本身，不繼承任何環境變數。安裝前 `which cargo rustup bun` 三者都找不到（[fresh-clone-tools-before.txt](data/2026-10-10-48-workspace-skeleton/fresh-clone-tools-before.txt)）。

## 指令

```sh
# 1. 全新 clone
git clone --branch build/48-workspace-skeleton --single-branch \
  https://github.com/Laysi/trustiot-v2.git "$F/repo"

# 2. 之後每個指令都在隔離環境、於 "$F/repo" 執行；"$F/bin" 只有指向 proto 執行檔的 symlink
env -i HOME="$F/home/user" PROTO_HOME="$F/home/proto" RUSTUP_HOME="$F/home/rustup" \
  CARGO_HOME="$F/home/cargo" CARGO_BUILD_JOBS=3 \
  PATH="$F/home/proto/shims:$F/home/proto/bin:$F/home/cargo/bin:$F/bin:/usr/local/bin:/usr/bin" \
  <指令>

# 3. 依序執行的 <指令>
proto install --yes
cargo --version; rustc --version; cargo clippy --version; cargo fmt --version; bun --version
cargo fmt --all --check
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p repo-check toolchain_pins_agree

# 4. 負向檢查（在開發用的 worktree 執行，每項跑完立即還原）
#    .prototools 改成 rust = "1.98.1" 後跑 cargo test -p repo-check --test repository toolchain_pins_agree
#    touch AGENTS.md 後跑 cargo test -p repo-check --test repository repository_has_no_agents_md
#    在 tools/repo-check/src/lib.rs 加入 unwrap 與 u32→u8 的 as 轉型後跑 cargo clippy

# 5. 頂層目錄對照：docs/architecture/repository-layout.md 目錄樹的第一層 vs git ls-tree --name-only HEAD
```

## 結果

| 步驟 | 結果 |
| --- | --- |
| `proto install --yes` | exit 0；proto 自行下載 rustup 安裝程式裝好 rustup，再安裝 Rust 1.99.0（rustc 1.99.0 b940084d7 2026-09-28）與 Bun 1.4.2，共 32 秒（[fresh-clone-proto-install.txt](data/2026-10-10-48-workspace-skeleton/fresh-clone-proto-install.txt)） |
| 版本 | cargo 1.99.0、rustc 1.99.0、clippy 0.1.99、rustfmt 1.10.0、bun 1.4.2 |
| `cargo fmt --all --check` | exit 0 |
| `cargo build --workspace` | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0，沒有警告 |
| `cargo test --workspace` | exit 0；repo-check 單元測試 12 個、整合測試 4 個全數通過 |
| `cargo test -p repo-check toolchain_pins_agree` | exit 0；只跑 1 個測試，其餘 15 個被篩掉 |
| 負向：兩處 Rust 版本不一致 | 測試失敗，訊息 `.prototools pins rust 1.98.1 but rust-toolchain.toml pins 1.99.0; change both together` |
| 負向：repo 根目錄有 `AGENTS.md` | 測試失敗，訊息列出該檔路徑 |
| 負向：成員程式碼用 `unwrap()` 與截斷的 `as` 轉型 | clippy 失敗：`unwrap_used`、`cast_possible_truncation`（來自 `[workspace.lints]`） |
| 頂層目錄對照 | 目錄樹第一層的 18 個項目全部存在；repo 另有 `.gitignore`、`Cargo.lock`、`README.md`、`clippy.toml` 四個根目錄檔案；`git ls-files` 中不分大小寫的 `AGENTS.md` 為 0 個。第二層以下（例如 `.github/workflows/`、`api/openapi.json`、各 crate 與服務）由各自的 issue 建立 |

完整輸出見 [fresh-clone-checks.txt](data/2026-10-10-48-workspace-skeleton/fresh-clone-checks.txt) 與 [negative-checks.txt](data/2026-10-10-48-workspace-skeleton/negative-checks.txt)。

安裝紀錄中 rustup 印出的 `$HOME differs from euid-obtained home directory` 是測試刻意把 `HOME` 指到空目錄造成的提示，不影響安裝；一般機器不會出現。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 全新 clone 後 `proto install` 安裝 `.prototools` 鎖定的 rust 與 bun 版本 | 通過：沒有任何工具鏈的環境中裝好 Rust 1.99.0 與 Bun 1.4.2，cargo 依 `rust-toolchain.toml` 使用 1.99.0 |
| `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` 皆通過 | 通過 |
| CLAUDE.md 列出 build、lint、test 與單一測試指令，且照著執行可成功 | 通過：CLAUDE.md「Build, lint, and test」段的每個指令都在全新 clone 中執行成功 |
| 頂層目錄與 docs/architecture/repository-layout.md 一致；repo 內沒有 AGENTS.md | 通過：另由 `repo-check` 的 `repository_has_no_agents_md` 測試持續檢查 |

沒有待測項目。
