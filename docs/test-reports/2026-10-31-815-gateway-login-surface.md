# 閘道器映像的登入面：建置與不開機的映像檢查（2026-10-31）

對應 issue：Laysi/trustiot-v2#815（父 issue Laysi/trustiot-v2#96）。這份報告記錄 `edge/image` 關閉閘道器本機登入面之後的一次從零建置，以及 `check.ts` 對完成的映像做的不開機檢查。登入面的設計與決定（序列埠登入關閉、USB 的規則與維修例外、已知的限制）在 [edge/image/README.md](../../edge/image/README.md) 的「登入面」。實機燒錄與開機在 Laysi/trustiot-v2#811，不在這裡；lab 的閘道器只做了唯讀查看（序列埠的狀態，見「結果」），沒有改動。

結果先講：從零建置成功（10 分 22 秒），`check.ts` 的 21 項不開機檢查全部通過，其中 5 項是本 issue 新增或擴充的（sshd 拒絕 agent 的服務帳號、主控台、開機失敗、USB、sshd 的本機 socket）；`edge/image` 的型別檢查通過，單元測試除了已知與本 issue 無關的一項之外全部通過。實機上的行為（螢幕沒有登入提示、USB 鍵盤與隨身碟沒有作用、initramfs 失敗時重開機）待 Laysi/trustiot-v2#811。

## 環境

- **映像的建置與檢查**：建置主機（arm64 Linux），rootless podman，Bun 1.4.2（`.prototools`）。工具容器、snapshot 時間點、套件版本與來源映像都是 `edge/image/src/pins.ts` 的釘選值，與 [Laysi/trustiot-v2#809 的報告](2026-10-31-809-gateway-image.md)相同：來源映像 `2026-10-06-raspios-trixie-arm64-lite.img.xz`，SHA-256 `483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843`（每次使用前重新比對）。映像裡是 systemd 257.13-1~deb13u1、openssh-server 1:10.0p1-7+deb13u4，核心 `6.18.50+rpt-rpi-v8` 與 `6.18.50+rpt-rpi-2712`。
- **agent**：同一台建置主機以 `cargo build --release --locked -p agent` 原生建置（Rust 依 `rust-toolchain.toml`），SHA-256 `869eebe16c4382db53f0692d963b89da093c4fe70646e3206122111d4799c376`。
- **程式**：分支 `feat/815-gateway-login-surface` 的 commit `19183ed0`（base：main `30da7053`）。
- **管理者公鑰**：執行時以 `ssh-keygen` 產生的拋棄式 ed25519 金鑰，放在 repo 之外，建置只拿到公鑰；檢查完成後與映像一起刪除。
- **時段**：2026-10-31（+08:00）。建置 19:42 開始、19:52 結束；檢查 19:52 開始、20:03 結束。
- **lab 的閘道器**（唯讀，2026-10-31 稍早）：在 Raspberry Pi 4 Model B 平台上開發的 TrustIoT gateway，核心 `6.18.50+rpt-rpi-v8`。

## 指令

在 repo 根目錄執行。`<快取>`、`<金鑰目錄>`、`<輸出>`、`<報告>` 都是 repo 之外的目錄。

```sh
# 1. agent（arm64 Linux 建置主機，以下同）
cargo build --release --locked -p agent

# 2. 拋棄式管理者金鑰
ssh-keygen -q -t ed25519 -N '' -C throwaway-test-key -f <金鑰目錄>/admin

# 3. 建置與檢查
bun install --cwd edge/image --frozen-lockfile
bun edge/image/build.ts --agent <trustiot-agent> --ssh-key <金鑰目錄>/admin.pub --out <輸出> --cache <快取>
bun edge/image/check.ts --image <輸出>/trustiot-gateway.img --agent <trustiot-agent> \
  --ssh-key <金鑰目錄>/admin.pub --report <報告> --cache <快取>

# 4. CI 會跑的部分（不下載映像）
bun run --cwd edge/image typecheck
bun run --cwd edge/image test
cargo test --locked -p repo-check
bun install --cwd scripts --frozen-lockfile
bun test scripts/ci/changes.test.ts
bun scripts/ci/check-secret-handling.ts
```

`systemd-ssh-generator` 的行為（「sshd 的本機 socket」）以一次性的探查確認：把來源映像的 root 掛進同一個工具容器、chroot 進去，以 `systemd-ssh-generator <normal> <early> <late>` 對三個暫存目錄執行一次，再列出它寫出的檔案。探查程式沒有進 repo。

## 結果

原始資料在 [data/2026-10-31-815-gateway-login-surface/](data/2026-10-31-815-gateway-login-surface/)：`checks.json`（21 項檢查的結果）、`login-surface.txt`（建置的登入面步驟、`cmdline.txt`、檢查的登入相關輸出與每個 `sshd -T` 連線的判定、delta 裡主控台、logind 與 USB 規則的檔案；這次的篩選沒有包含 generator 的連結與三個 socket 的遮蔽，它們由檢查「sshd has no local socket」確認）、`ssh-generator-probe.txt`（來源映像裡 `systemd-ssh-generator` 的輸出）。

### 建置

建置的登入面步驟遮蔽 9 個主控台 unit 與 3 個 sshd 的 socket，寫入 logind 的 drop-in 與 USB 的 udev 規則（`udevadm verify` 通過），再確認每個 unit 與 19 個 instance 都是 `masked`；boot 分割區的 `cmdline.txt` 變成：

```text
console=serial0,115200 console=tty1 root=PARTUUID=27b94958-02 rootfstype=ext4 fsck.repair=yes rootwait resize panic=10
```

映像 SHA-256 `eae3fb36d494b3a8f3a847986a00f32d6a8a938fe955c76fec4ce427b5cf93b0`（5226102784 位元組）。

### 檢查

21 項全部通過（`21 of 21 checks passed`，`exit 0 at 20:03:33 (build host)`）。本 issue 相關的幾項：

| 檢查 | 結果 |
| --- | --- |
| sshd accepts public keys only, for the administrator only | 對 `trustiot-admin`、`root`、`pi`、`trustiot-agent` 四個連線的 `sshd -T -C` 都是 `allowusers trustiot-admin`、`authenticationmethods publickey`，沒有 `denyusers`，所有 sshd 設定檔沒有 `Match` 區塊；`AllowUsers` 不放行 `trustiot-agent`、`root`、`pi`。 |
| no login prompt or shell on the screen or the serial console | 9 個 unit 與 19 個 instance 都是 `masked`；接上終端機的 service 除了 `emergency.service`、`rescue.service`（sulogin）都遮蔽；沒有任何設定提到 `SYSTEMD_SULOGIN_FORCE`；root 鎖定；logind `NAutoVTs=0`、`ReserveVT=0` |
| a failed boot reboots instead of opening the initramfs shell | `cmdline.txt` 只有 `panic=10`；`initramfs8` 與 `initramfs_2712` 的 `init` 讀 `panic=`，`panic()` 在開 shell 之前重開機 |
| USB keyboards and storage get no driver | 規則檔內容與權限正確、`udevadm verify` 通過、沒有被同名檔覆蓋、沒有其他規則寫入 `authorized`；兩個核心都把 `usbhid`、`usb-storage`、`uas` 編譯在核心裡 |
| sshd has no local socket (only SSH over the network) | `systemd-ssh-generator` 連到 `/dev/null`，`sshd-extra.socket`、`sshd-unix-local.socket`、`sshd-vsock.socket` 都是 `masked` |

agent 服務帳號的 sshd 判定（`login-surface.txt`）：

```text
sshd -T -C user=trustiot-agent,host=gateway.example,addr=192.0.2.2,laddr=192.0.2.2,lport=22: allowusers trustiot-admin; denyusers (none); authenticationmethods publickey
```

`AllowUsers` 沒有指定位址，映像也沒有 `Match` 區塊，所以從本機（127.0.0.1）連線的判定相同；測試連線依 `edge/image` 的規則只用文件用的位址（RFC 5737）。

### sshd 的本機 socket

在來源映像（遮蔽之前）執行 `systemd-ssh-generator` 的結果：它寫出 `sshd-unix-local.socket`（`ListenStream=/run/ssh-unix-local/socket`、`Accept=yes`，沒有 `SocketMode=`）並放進 `sockets.target.wants`，印出 `Binding SSH to AF_UNIX socket /run/ssh-unix-local/socket`；不在 VM 裡，所以沒有 AF_VSOCK。也就是說不遮蔽時，閘道器上任何行程（包括 agent 的服務帳號）都能經這個 socket 連到 sshd。完成的映像遮蔽了 generator 與三個 socket（上表）。

### lab 的閘道器（唯讀）

`/proc/consoles` 只有 `tty1`，沒有 `/dev/serial0`（`config.txt` 沒有 `enable_uart`），`getty@tty1` 在執行；這是燒錄本 issue 的映像之前的狀態，只用來確認序列埠在這個平台上預設就沒有輸出。

### CI 會跑的部分

| 指令 | 結果 |
| --- | --- |
| `bun run --cwd edge/image typecheck` | 通過 |
| `bun run --cwd edge/image test` | 94 項通過、1 項失敗：`sfdisk > without boot code parted replaces the disk identifier`，主機的 parted 3.7.x 與測試預期不同，main 上同樣失敗，與本 issue 無關（Laysi/trustiot-v2#872） |
| `cargo test --locked -p repo-check` | 10 項通過 |
| `bun test scripts/ci/changes.test.ts` | 306 項通過 |
| `bun scripts/ci/check-secret-handling.ts` | 沒有違規 |
| gitleaks（分支的 commit 與 `git archive HEAD` 的樹） | 沒有發現 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 螢幕沒有登入提示 | 映像通過（getty、autovt、getty-static、console-getty 遮蔽，logind 不啟動 getty）；實機確認待 Laysi/trustiot-v2#811 |
| USB 鍵盤與儲存裝置停用，維修例外有文件 | 映像通過（udev 規則、沒有覆蓋）；維修例外寫在 README「維修時的做法」；實機確認（接上鍵盤與隨身碟沒有作用、例外的指令可用）待 Laysi/trustiot-v2#811；initramfs 階段仍可用是已知限制，記在 README |
| serial 緊急密碼每台隨機並有保管紀錄，或關閉 serial 登入並記錄理由 | 選擇關閉：`serial-getty@.service` 遮蔽（映像通過），理由與重新考慮的時機寫在 README「序列埠主控台：登入關閉」、`docs/architecture/secrets.md` 與 `docs/requirements/rebuild-checklist.md` 的 D5 |
| sshd 拒絕 agent 的服務帳號（Laysi/trustiot-v2#826 審查後加入） | 通過（`sshd -T -C user=trustiot-agent,...` 的 `allowusers trustiot-admin`） |
| `sshd-unix-local.socket` 的決定（Laysi/trustiot-v2#826 審查後加入） | 映像不帶：generator 與三個 socket 遮蔽，通過 |
| initramfs 失敗時不給 shell | 映像通過（`panic=10`，兩個 initramfs 的 `panic()` 都會重開機）；實機確認待 Laysi/trustiot-v2#811 |
