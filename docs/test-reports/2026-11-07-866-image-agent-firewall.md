# 閘道器映像：安裝 fieldbus 放行元件與預設拒絕的規則（2026-11-07）

對應 issue：Laysi/trustiot-v2#866。這份報告記錄映像安裝 agent 的防火牆（預設拒絕、fieldbus 放行元件、DNS 放行、`IPAddressDeny=` 的確認）之後的證據：

- **映像建置與映像檢查**（`edge/image/build.ts`、`check.ts`）：在 arm64 建置主機上建出映像，以不開機的方式檢查。
- **開機測試**（`edge/image/boot-test.ts`）：在 QEMU 裡開機映像的副本，7 實際走一遍 agent 的防火牆。
- **agent 的容器測試**：rootless 跑全部，以 root（rootful podman）再跑兩個需要載入 cgroup BPF 程式的 `IPAddressDeny=` 測試。

接法照 [`edge/agent/README.md`](../../edge/agent/README.md)「給 Laysi/trustiot-v2#866 的接法」；`IPAddressDeny=` 沒有掛上時的處理與沒有選的做法寫在 [`edge/image/README.md`](../../edge/image/README.md)「`IPAddressDeny=` 沒有掛上時」。實機（Laysi/trustiot-v2#811）與 lab 的閘道器都沒有動。

先講結果：

- **映像檢查**：24 項全部通過。四個執行檔、`edge/agent/systemd` 的每個檔案、以 `systemd-analyze cat-config` 算出的有效設定（三處都載入含預設拒絕的規則檔、agent 只以 `Wants=`／`After=` 帶起 socket 與 path unit、`ExecStartPre=` 的三行）都和預期相同。
- **開機測試**：48 次開機、105 項，104 項通過、1 項不通過。不通過的是 10-unrecorded-tpm-b 的 setup 看 TPM 的時間（最後一次在第一次之後 181 秒開始，上限 180 秒），屬於 Laysi/trustiot-v2#867 的資料分割區 setup，與這個 issue 改的檔案無關（見「結果」的「不通過的一項」）。agent 的防火牆的每一項都通過：agent 啟動過的每次開機都有兩個 table、socket 與 path unit 由 agent 帶起，`IPAddressDeny=` 每次都確認掛上；7 證明 `IPAddressDeny=` 擋下 agent 的 cgroup 送出的封包，沒有掛上時記錯誤、agent 照常執行。
- **agent 的測試**：`cargo test -p agent` 全部通過（rootless）；以 root 執行的兩個 `IPAddressDeny=` 測試通過。
- **其他必要檢查**都通過：`edge/image` 的型別檢查與 231 項單元測試、workspace 的 clippy、`cargo fmt --check`、`cargo test -p repo-check`、`bun test scripts/ci/changes.test.ts`、secret handling 與 gitleaks。

## 環境

- **建置主機**：arm64 Linux 建置主機（rootless podman，沒有 KVM），Bun 1.4.2（`.prototools`）。agent 的容器測試、映像建置、映像檢查、單元測試與開機測試都在這台跑。以 root 執行的容器測試用一次性的 storage 設定與 `apparmor_profile = "unconfined"` 的暫時設定（[`edge/agent/README.md`](../../edge/agent/README.md)「測試」），用完即刪。
- **工具容器與開機測試的套件**：都是 `edge/image/src/pins.ts` 的釘選值（snapshot.debian.org `20261030T000000Z`），與 [Laysi/trustiot-v2#867 的報告](2026-11-06-867-tpm-permanent-failure-wipe.md)相同：Debian 核心 6.12.113+deb13-arm64、qemu-system-arm 1:10.0.13+ds-0+deb13u1、swtpm 0.7.1-1.5（[`boot-test.txt`](data/2026-11-07-866-image-agent-firewall/boot-test.txt) 開頭）。
- **映像裡的版本**：systemd 257.13-1~deb13u1（[`build-info.json`](data/2026-11-07-866-image-agent-firewall/build-info.json)）。
- **虛擬機**：`qemu-system-aarch64 -machine virt -cpu max,pauth-impdef=on -smp 4 -m 2048 -accel tcg,thread=multi`，沒有網路，TPM 是 swtpm。
- **程式**：分支 `feat/866-image-fieldbus-allowlist` 的 `f004224e`，分支點是 main `df13e01f`。建置主機上 `edge/agent` 與 `edge/image` 的每個檔案與這個 commit 相同（兩邊對 `git ls-files edge/agent edge/image` 逐檔 SHA-256 的雜湊都是 `149bec93…`）；之後只加了這份報告。
  - `trustiot-gateway.img` 的 SHA-256：`95d7b89752786bb99078f0d4f9acd51d592b5bd7c30595bca403e4d367865e40`
  - agent 的四個執行檔以 `cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent` 建置（[`agent-sha256.txt`](data/2026-11-07-866-image-agent-firewall/agent-sha256.txt)，與 `build-info.json` 的 `agent.executables` 相同）。
- **管理者公鑰**：建置時以 `ssh-keygen` 產生的拋棄式 ed25519 金鑰，放在 repo 之外。
- **時段**：2026-11-07（+08:00）。建置 683 秒、映像檢查 883 秒、開機測試 7869 秒（建置主機同時沒有其他工作；開機測試比 Laysi/trustiot-v2#867 的 2634 秒慢很多，見「不通過的一項」）。
- **之前的一次**：同一天以 `ip-deny-check` 改版之前的程式跑過一次建置、檢查與開機測試的前段（到 7），7 也通過；`ip-deny-check` 之後改成查詢時給 ID 緩衝區（見下方），所以停掉重跑。這份報告的數字與原始資料都只來自 `f004224e` 那一次。

## 指令

在 repo 根目錄執行（`<work>` 是 repo 之外的工作目錄）：

```sh
proto install zig && proto install cargo-zigbuild
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
  PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
  cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
mkdir -p <work>/agent
cp target/aarch64-unknown-linux-gnu/release/trustiot-agent{,-allowlist,-nameservers,-ip-deny-check} <work>/agent/
ssh-keygen -q -t ed25519 -N '' -C '' -f <work>/key
bun edge/image/build.ts --agent <work>/agent/trustiot-agent --ssh-key <work>/key.pub --out <work>/out
bun edge/image/check.ts --image <work>/out/trustiot-gateway.img --agent <work>/agent/trustiot-agent \
  --ssh-key <work>/key.pub --report <work>/check-report
bun edge/image/boot-test.ts --image <work>/out/trustiot-gateway.img --report <work>/boot-report

# agent
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p agent
cargo test --locked -p agent --test egress --no-run   # 以 root 執行它，見下方
# 以 root、CONTAINERS_STORAGE_CONF 指向暫時的 storage 設定、CONTAINERS_CONF_OVERRIDE 指向只有
# [containers] apparmor_profile = "unconfined" 的暫時設定：
<egress 測試執行檔> --nocapture --exact with_its_rules_flushed_the_agents_unit_still_refuses_loopback_metadata_multicast_and_broadcast
<egress 測試執行檔> --nocapture --exact every_start_of_the_agent_confirms_ip_address_deny_or_logs_an_error_and_runs

# 其他
bun install --cwd edge/image --frozen-lockfile
bun run --cwd edge/image typecheck
bun run --cwd edge/image test
cargo test -p repo-check
bun test scripts/ci/changes.test.ts
bun scripts/ci/check-secret-handling.ts
```

## 結果

### 映像檢查

24 項全部通過（[`check/checks.json`](data/2026-11-07-866-image-agent-firewall/check/checks.json)、[`check.txt`](data/2026-11-07-866-image-agent-firewall/check.txt)）。與這個 issue 有關的三項：

| 檢查 | 結果 |
| --- | --- |
| agent executables are the given build | `/usr/bin/trustiot-agent` 與 `/usr/libexec/trustiot-agent/{allowlist,nameservers,ip-deny-check}`：SHA-256 與給的檔案相同、root 0755、目錄 root 0755、aarch64 ELF；`trustiot-agent --version` 以 0 結束，另外三個帶一個參數以 2 結束（在映像的 C 函式庫上載入） |
| agent unit and system user are installed and enabled | `edge/agent/systemd` 的 12 個檔案逐位元組相同、0644；agent 已啟用；`trustiot-agent-allowlist.socket` 與 `trustiot-agent-nameservers.path` 是 `static` |
| agent firewall | `ExecStartPre=` 是 `+/usr/sbin/nft -f …/trustiot-agent-egress.nft`、`+/usr/libexec/trustiot-agent/nameservers`、`-+/usr/libexec/trustiot-agent/ip-deny-check`；`nftables.service` 的 `ExecReload=` 是在一個 transaction 裡 `include` 兩個檔案、再執行 `nameservers`；規則檔以 `include "/usr/share/trustiot-agent/trustiot-agent-default-deny.nft"` 結尾；`Wants=`／`After=` 兩個 unit、沒有 `Requires=` 類；`systemd-analyze verify` 載入 7 個 unit 沒有錯誤 |

建置改了來源映像的路徑（[`check/delta.tsv`](data/2026-11-07-866-image-agent-firewall/check/delta.tsv)）多了 `/usr/libexec/trustiot-agent/` 的三個執行檔、預設拒絕的規則檔與四個 unit；gitleaks 沒有新的發現（[`check/gitleaks.json`](data/2026-11-07-866-image-agent-firewall/check/gitleaks.json)）。

### 開機測試

[`boot-test.txt`](data/2026-11-07-866-image-agent-firewall/boot-test.txt)、[`boot/boot-test.json`](data/2026-11-07-866-image-agent-firewall/boot/boot-test.json)，每次開機的探針輸出在 [`boot/`](data/2026-11-07-866-image-agent-firewall/boot/)。

**agent 啟動過的每次開機**（4、6、7、9、12、18 與斷電之後的 b 開機）都通過防火牆的檢查：`nft list tables` 有 `inet trustiot_agent` 與 `inet trustiot_agent_allow`；socket 是 active/listening、path unit 是 active/waiting，兩個都是 `static`；socket 是 `root trustiot-agent 660`；agent 的每次啟動都記下 `IPAddressDeny= is attached to /system.slice/trustiot-agent.service: 1 ingress, 1 egress programs`，沒有錯誤。

**7-tpm-a-again 的防火牆**（[`boot/7-tpm-a-again.probe.txt`](data/2026-11-07-866-image-agent-firewall/boot/7-tpm-a-again.probe.txt)，`fw-` 開頭的段落）：

| 步驟 | 看到的 |
| --- | --- |
| 以替身啟動 agent（只換 `ExecStart=`） | active/running；`ip-deny-check`：`1 ingress, 1 egress programs` |
| root 的程序移進 agent 的 cgroup，送 UDP 到 `127.0.0.1:9` | `write error: Operation not permitted`（`IPAddressDeny=` 擋下；出口規則只比對 agent 的帳號） |
| 同一個封包從 cgroup 外送 | `sent` |
| 對照：drop-in 清空 `IPAddressDeny=`，重啟 | `ip-deny-check` 記下 `IPAddressDeny= is not attached … (0 ingress, 0 egress programs)`（error 層級）；agent 仍是 active/running；cgroup 裡的封包 `sent` |
| `/etc/resolv.conf` 以改名換成 `nameserver 192.0.2.53` | path unit 填入：`nameserver4` 是 `{ 192.0.2.53 }` |
| `systemctl reload nftables.service` | 兩個 table 都在，集合仍是 `{ 192.0.2.53 }` |
| 停掉檢查的 timer，`nft delete table inet trustiot_agent_allow`，重啟 agent | 刪掉之後只剩 `inet trustiot_agent`；重啟之後兩個 table 都在、集合是 `{ 192.0.2.53 }`；檢查的日誌沒有載回的紀錄（是 agent 的 `ExecStartPre=` 載回的） |

**不通過的一項**：10-unrecorded-tpm-b（TPM B、標頭沒有指紋的 volume）。setup 每次都回答 `unrecorded`、不清除、解不開、agent 沒有啟動、分割區逐位元組不變，都符合預期；不符合的是第 17 次看 TPM 在第一次之後 181 秒開始，超過 180 秒的上限（`tpmLookProblems`）。setup（`edge/image/files/data/trustiot-data-setup` 的 `check_tpm`）在睡下一個 10 秒之前判斷「現在加 10 秒是否超過 180 秒」，時間是 `/proc/uptime` 的整數秒；第 16 次在 170 秒，判斷通過，睡 10 秒加上排程延遲與取整，下一次落在 181 秒。這一次建置主機很慢（開機測試共 7869 秒，Laysi/trustiot-v2#867 是 2634 秒；每次看 TPM 間隔 11–14 秒），10 與 11 的間隔都比 10 秒長。這個 issue 沒有改 setup、它的測試或判斷（`git diff df13e01f f004224e -- edge/image/files edge/image/src/data.ts` 是空的，`tpmLookProblems` 也沒改），Laysi/trustiot-v2#867 的那一次通過。要不要把 setup 改成睡完再判斷、或把檢查的上限放寬一個取整的秒數，交給 Laysi/trustiot-v2#867 的負責人決定，不在這份報告處理。

### agent 的測試

- `cargo test -p agent`（rootless）：全部通過（[`agent-test-rootless.txt`](data/2026-11-07-866-image-agent-firewall/agent-test-rootless.txt)）。其中 `egress` 的 `every_start_of_the_agent_confirms_ip_address_deny_or_logs_an_error_and_runs` 在 rootless 時走另一半：systemd 載入不了 BPF 程式，查詢失敗，`ip-deny-check` 以 error 層級記下，agent 是 active、`Result=success`。
- 以 root 執行（[`agent-container-rootful.txt`](data/2026-11-07-866-image-agent-firewall/agent-container-rootful.txt)）：`with_its_rules_flushed_…` 通過；`every_start_…` 通過，記下 `1 ingress, 1 egress programs`，清空 `IPAddressDeny=` 之後記下 `(0 ingress, 0 egress programs)` 的錯誤、agent 仍在執行。這個測試的 unit 保留出貨的 `SystemCallFilter=~@privileged @resources`（測試先確認），`bpf(2)` 在 `@privileged` 裡；查詢照樣有答案，因為 `+` 的那一行不套用 unit 的沙箱（含 seccomp）。
- `cargo clippy --workspace --all-targets -- -D warnings` 與 `cargo fmt --all --check` 沒有問題（[`clippy.txt`](data/2026-11-07-866-image-agent-firewall/clippy.txt)）。

### 其他

- `edge/image`：型別檢查通過，231 項單元測試全部通過（[`image-unit.txt`](data/2026-11-07-866-image-agent-firewall/image-unit.txt)）；新的有 `tests/firewall.test.ts`（有效設定的判斷）、`tests/boot-results.test.ts` 的防火牆段落、`tests/agent-unit.test.ts` 的執行檔清單與 unit 裡的 `/usr/libexec` 路徑。
- `cargo test -p repo-check`、`bun test scripts/ci/changes.test.ts`（映像的測試現在也讀 `edge/agent/Cargo.toml`，規則已加上）、`bun scripts/ci/check-secret-handling.ts`：通過。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 映像裝了元件、它的 unit 與預設拒絕的規則，unit 已啟用（映像檢查） | 通過。元件、nameservers 與 `ip-deny-check` 的執行檔、socket 與 service、path unit 與 service、預設拒絕的規則檔都已安裝；它們沒有 `[Install]`（`static`），由 agent 的 unit（已啟用）帶起，開機測試裡每次 agent 啟動後都是 active |
| `/usr/libexec/trustiot-agent/allowlist` 是 aarch64 執行檔，root 擁有、0755；`AGENT_SYSTEMD_NOT_INSTALLED` 不再列元件的兩個 unit，agent 的 unit 帶得起元件的 socket（映像檢查） | 通過。`AGENT_SYSTEMD_NOT_INSTALLED` 是空的 |
| agent 的規則檔載入時，預設拒絕一起載入（agent 啟動前的重新載入與 `nftables.service` 的 reload 都是；映像檢查） | 通過。映像檢查確認三處都載入以 `include` 結尾的規則檔；開機測試 7 實際看到 reload 之後與刪掉 table 再重啟 agent 之後兩個 table 都在 |
| agent 的 cgroup 掛著 `IPAddressDeny=` 的 BPF 程式；不生效時的處理已實作並寫進 README（開機測試或實機） | 開機測試通過（Debian 6.12 核心）：每次啟動 ingress、egress 各 1 個程式，擋下封包；沒有掛上時記錯誤、agent 照常執行。處理寫在 `edge/image/README.md`「`IPAddressDeny=` 沒有掛上時」。閘道器自己的核心上的確認在 Laysi/trustiot-v2#811（待測，這個 issue 不碰實機） |
| `edge/image` 的測試與型別檢查通過；測試報告放在 `docs/test-reports/` | 通過（本報告） |

開機測試的 10-unrecorded-tpm-b 有一項不通過，原因在 Laysi/trustiot-v2#867 的 setup 時間判斷，不在這個 issue 的範圍（見上方）。
