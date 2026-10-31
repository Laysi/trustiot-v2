# 閘道器取用新的根 CRL

Laysi/trustiot-v2#563：agent 經 HTTPS 取得較新的根 CRL、只往前換上、跨重啟保留且不能被刪檔或換舊檔降級、換上後下一條連線是完整交握、心跳回報根 CRL 的編號與是否過期。

## 環境

- 建置主機（x86_64 Linux），rcheck 佇列：工作樹的快照、`CI=1`（需要容器的測試在沒有容器時失敗而不是略過），podman（swtpm 容器給 `provision_install`）。
- 工具版本依 `.prototools` 與 `rust-toolchain.toml`。
- 分支 `feat/563-root-crl-fetch`；commit 見 PR。

## 指令

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=true cargo test --locked -p agent
cargo test --locked -p protocol
cargo test --locked -p root-crl
cargo test --locked --workspace --exclude agent
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
bash docs/test-reports/data/2026-11-01-563-root-crl-update/run-mutations.sh
```

## 結果

RESULTS

## 結論

CONCLUSION
