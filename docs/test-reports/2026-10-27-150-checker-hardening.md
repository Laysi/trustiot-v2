# AI 儀表板：檢查服務、沙箱、映像與 nightly 的安全強化

對應 issue：Laysi/trustiot-v2#150（AI 儀表板）。測試日期 2026-10-27（Asia/Taipei）。

這份報告是 #150 安全審查之後，檢查服務（`services/dashboard-check`）、沙箱 runtime（`frontend/sandbox`）、檢查服務的映像（`deploy/images/dashboard-check`）與 nightly 隔離證明（`scripts/nightly/dashboard-check.ts`）的強化證據。每一項都有修正前與修正後的結果。原始輸出在 [data/2026-10-27-150-checker-hardening/](data/2026-10-27-150-checker-hardening/)。

## 環境

- **受測程式碼**：本報告所在的 commit（分支 `feat/150-y-hardening-checker`，從 #150 的整合分支開始）。「修正前」是同一個測試對修正前的程式碼跑的結果。
- **執行的地方**：開發工作站。容器引擎是 Docker，平台 `linux/arm64`；起容器的測試一次只跑一個。沒有設 `CI`。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2、zig 0.16.0、cargo-zigbuild 0.23.4。

## 1. 檢查之間的行程清理（PID 1）

`tests/pid1_cleanup.rs` 的主機端（建映像、起容器）只在 Linux 主機上執行（CI）。這次在開發工作站把同一個測試執行檔以 cargo-zigbuild 交叉編譯成 `aarch64-unknown-linux-musl`，用與主機端相同的容器設定直接跑容器內的部分（PID 1 的角色）。

### 指令

```sh
# 交叉編譯測試執行檔（CARGO_ZIGBUILD_ZIG_PATH 指向 .prototools 釘選的 zig）
cargo zigbuild --locked --target aarch64-unknown-linux-musl -p dashboard-check --test pid1_cleanup

# 映像：gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2
# 加上 COPY 進來的測試執行檔（ENTRYPOINT 是它），以測試主機端相同的設定執行：
docker run --name <name> --platform linux/arm64 --read-only --tmpfs /tmp:rw,mode=1777 \
  --user 65532:65532 --cap-drop ALL --security-opt no-new-privileges --network none \
  --env DASHBOARD_CHECK_PID1_TEST_ROLE=init <image>

# 清理的單元測試在 Linux 上（不是 PID 1：以 docker 的 --init 當 PID 1）
docker run --rm --init --platform linux/arm64 --read-only --tmpfs /tmp:rw,mode=1777 \
  --user 65532:65532 --cap-drop ALL --security-opt no-new-privileges --network none \
  <交叉編譯的 dashboard_check 單元測試執行檔> cleanup
```

### 結果

容器裡的 PID 1 起一個行程：它離開 session、起一個每 10 ms 寫一次計數的執行緒，再只結束主執行緒（`SYS_exit`，不是 `exit_group`）。

| | 確認 | 清理的結果 | 清理後 `/proc` 裡除了 PID 1 還有 | 結束碼 |
| --- | --- | --- | --- | --- |
| 修正前 | 該行程 `State: Z (zombie)`，另一個執行緒 `S (sleeping)`，計數 2 → 4 | `Ok(())` | `6 Z (zombie)` | 1 |
| 修正後 | 同上，計數 2 → 4 | `Ok(())` | 沒有 | 0 |

- 審查的說法成立：主執行緒結束、其他執行緒還在跑的行程，在 `/proc/<pid>/status` 顯示為 zombie，計數還在增加，表示它的程式還在執行。修正前的清理把 zombie 當成已經結束，不殺也不算殘留。
- 修正前與修正後的輸出：[pid1-cleanup-before.txt](data/2026-10-27-150-checker-hardening/pid1-cleanup-before.txt)、[pid1-cleanup-after.txt](data/2026-10-27-150-checker-hardening/pid1-cleanup-after.txt)。
- 清理的單元測試在 Linux 上 6 個全過，包括「不是 PID 1 時碰不到對所有行程的訊號」與「列出 `/proc` 裡其他所有行程」。單元測試的建置不含真的 `kill(-1, …)`，只計數。

### 結論

- 服務是 PID 1 時，清理後 PID namespace 裡只剩服務自己，主執行緒已結束的行程也一樣：通過（Linux 容器，交叉編譯的同一個測試）。
- 不是 PID 1 時走不到對所有行程的訊號：通過（單元測試，開發工作站與 Linux 容器）。
- 測試主機端在 Linux 上建映像、起容器的部分：待測，由 CI 的 `cargo test --workspace`（設了 `CI`，沒有容器引擎時失敗）跑。
