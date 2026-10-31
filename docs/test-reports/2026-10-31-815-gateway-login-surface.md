# 閘道器映像的登入面：建置、映像檢查與 QEMU 模擬開機（2026-10-31 至 2026-11-01）

對應 issue：Laysi/trustiot-v2#815（父 issue Laysi/trustiot-v2#96）。這份報告記錄 `edge/image` 關閉閘道器本機登入面之後的驗證：

- 一次從零建置；
- `check.ts` 對完成的映像做的不開機檢查；
- Laysi/trustiot-v2#810 的開機測試（`boot-test.ts`），分支合併了它；
- 以 QEMU 模擬開機同一個映像，證明 USB 裝置不被授權、螢幕與序列埠沒有登入提示。

登入面的設計與決定（序列埠登入關閉、USB 全部不授權、已知的限制）在 [edge/image/README.md](../../edge/image/README.md) 的「登入面」。實機燒錄與開機在 Laysi/trustiot-v2#811，不在這裡；lab 的閘道器只做了唯讀查看（見「結果」），沒有改動。

2026-10-31 的審查之後，USB 從「依介面類別擋鍵盤與儲存裝置的 udev 規則，加上維修例外」改為核心參數 `usbcore.authorized_default=0`：全部 USB 裝置不授權，沒有維修例外。這份報告取代同一檔名的前一版（commit `4c8560d0`），前一版驗的是 udev 規則；這一版的測試在 2026-11-01 完成，沿用原檔名。

結果先講：

- 從零建置成功；
- `check.ts` 的 23 項不開機檢查全部通過，包括在 main 上不通過的「祕密與內網名稱」（見「祕密與內網名稱：前後對照」）；
- 開機測試 12 次開機、23 項全部通過；
- QEMU 模擬開機時，開機前接上與開機後熱插拔的 USB 鍵盤、隨身碟與 hub 都不被授權：沒有輸入裝置、沒有磁碟，序列埠與螢幕都沒有登入提示。只拿掉這個參數的對照組則出現鍵盤的輸入裝置與磁碟。

實機上的行為（VL805、RP1 與開機韌體，以及 initramfs 失敗時重開機）待 Laysi/trustiot-v2#811。

## 環境

- **建置主機（arm64 Linux）**：rootless podman（`edge/image` 的要求），Bun 1.4.2（`.prototools`）；QEMU 不用 KVM，全部是軟體模擬。建置、檢查、開機測試與模擬開機都在這台，四項都用同一個映像檔。
- **來源映像與釘選**：工具容器、snapshot 時間點與套件版本都是 `edge/image/src/pins.ts` 的釘選值。
  - 來源映像 `2026-10-06-raspios-trixie-arm64-lite.img.xz`，SHA-256 `483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843`（每次使用前重新比對）。
  - 映像裡是 systemd 257.13-1~deb13u1、openssh-server 1:10.0p1-7+deb13u4，核心 `6.18.50+rpt-rpi-v8` 與 `6.18.50+rpt-rpi-2712`。
- **agent**：同一台建置主機以 `cargo build --release --locked -p agent` 原生建置（Rust 依 `rust-toolchain.toml`），SHA-256 `f36c6fbacc18f1217c3a624300a8d0cd907b54d8a5fdcffd8ed78dbb1d3674f2`。
- **程式**：分支 `feat/815-gateway-login-surface` 的 commit `5f8f9413`，也就是合併 main `dbb41618`（含 Laysi/trustiot-v2#810）之後的樹。之後的 commit 只改文件與本報告。
- **管理者公鑰**：執行時以 `ssh-keygen` 產生的拋棄式 ed25519 金鑰，放在 repo 之外，建置只拿到公鑰；檢查完成後刪除。
- **QEMU**：`qemu-system-arm` 1:10.0.13+ds-0+deb13u1，與 mtools、fdisk、socat、netpbm 一起從同一個 snapshot 裝在拋棄式的工具容器裡（容器用 `pins.ts` 的工具映像），不裝在建置主機上。
- **時段**：2026-11-01（+08:00）。
  - 建置、檢查與開機測試：01:44 開始、03:27 結束。建置 10 分 23 秒，檢查 11 分 4 秒，開機測試 55 分 28 秒。
  - 模擬開機兩次（見「QEMU 模擬開機」）：第 1 次在同一個工作裡、開機測試之後，03:01 開始、03:27 結束；第 2 次 05:09 開始、06:55 結束。
- **lab 的閘道器**（唯讀，2026-10-31）：BCM2711 平台的 TrustIoT gateway，核心 `6.18.50+rpt-rpi-v8`，燒的是本 issue 之前的映像。

## 指令

在 repo 根目錄、建置主機上執行。`<快取>`、`<金鑰目錄>`、`<輸出>`、`<報告>`、`<開機報告>` 都是 repo 之外的目錄。

```sh
# 1. agent
cargo build --release --locked -p agent

# 2. 拋棄式管理者金鑰
ssh-keygen -q -t ed25519 -N '' -C throwaway-test-key -f <金鑰目錄>/admin

# 3. 建置、檢查與 Laysi/trustiot-v2#810 的開機測試
bun install --cwd edge/image --frozen-lockfile
bun edge/image/build.ts --agent <trustiot-agent> --ssh-key <金鑰目錄>/admin.pub --out <輸出> --cache <快取>
bun edge/image/check.ts --image <輸出>/trustiot-gateway.img --agent <trustiot-agent> \
  --ssh-key <金鑰目錄>/admin.pub --report <報告> --cache <快取>
bun edge/image/boot-test.ts --image <輸出>/trustiot-gateway.img --report <開機報告> --cache <快取>

# 4. CI 會跑的部分（不下載映像）
bun run --cwd edge/image typecheck
bun run --cwd edge/image test
cargo test --locked -p repo-check
bun install --cwd scripts --frozen-lockfile
bun test scripts/ci/changes.test.ts
bun scripts/ci/check-secret-handling.ts
```

### QEMU 模擬開機

這是一次性的手動測試，程式沒有進 repo。步驟如下：

1. 在拋棄式的 rootless podman 容器裡安裝上述套件，映像以唯讀掛入。
2. 以 mtools 從 boot 分割區取出 `kernel8.img`、`initramfs8`、`bcm2710-rpi-3-b.dtb` 與 `cmdline.txt`。
3. 開機四次。每次用映像的一份副本當 SD 卡，副本延伸到 8 GiB（QEMU 的 SD 卡大小必須是 2 的次方）。

```sh
qemu-system-aarch64 -M raspi3b -m 1G \
  -kernel kernel8.img -initrd initramfs8 -dtb bcm2710-rpi-3-b.dtb \
  -drive file=sd.img,if=sd,format=raw \
  -drive if=none,id=stick0,file=stick0.img,format=raw \
  -drive if=none,id=stick1,file=stick1.img,format=raw \
  -append "<見下>" \
  -serial file:serial-pl011.log -serial file:serial.log \
  -display none -monitor unix:mon.sock,server=on,wait=off \
  -no-reboot -d guest_errors -D qemu-guest-errors.log \
  <開機前接上的裝置>
```

`stick0.img`、`stick1.img` 是 64 MiB、有 MBR 分割表與一個分割區的磁碟。`-append` 是映像的 `cmdline.txt`，加上三個模擬環境需要的調整，`panic=10` 與 `usbcore.authorized_default=0` 原樣保留：

```text
映像的 cmdline.txt：
console=serial0,115200 console=tty1 root=PARTUUID=27b94958-02 rootfstype=ext4 fsck.repair=yes rootwait resize panic=10 usbcore.authorized_default=0

模擬開機（deny）：
8250.nr_uarts=1 initcall_blacklist=bcm2835_wdt_driver_init ignore_loglevel console=ttyS0,115200 console=tty1 root=PARTUUID=27b94958-02 rootfstype=ext4 fsck.repair=yes rootwait resize panic=10 usbcore.authorized_default=0

對照組（control）：同上，只拿掉 usbcore.authorized_default=0
```

模擬環境的調整：

- `console=serial0,115200` 改成 `console=ttyS0,115200`，前面加 `8250.nr_uarts=1`：這是開機韌體在真的板子上做的事（把 `serial0` 換成 mini UART、在前面加自己的參數），QEMU 自己載入核心，所以手動做。沒有這兩項時，initramfs 找不到主控台，`run-init` 失敗，核心 panic。
- `initcall_blacklist=bcm2835_wdt_driver_init`：QEMU 的 raspi 機器在 watchdog 一被打開就重置。initramfs 會打開它來解除，systemd 會打開它來啟用，所以這裡不載入 watchdog 的驅動程式。
- `ignore_loglevel`：Raspberry Pi OS 在 systemd 起來後以 sysctl（`kernel.printk`）調低主控台的訊息等級，熱插拔的核心訊息就不會出現在序列埠。這一項讓每則核心訊息都留在主控台。

每次開機的流程：

1. 等序列埠出現 `systemd[1]: Detected architecture`，再等它 180 秒沒有新輸出。
2. 經 monitor 執行 `info usb`、`screendump`（以 `pnmtopng` 轉成 PNG）。
3. 熱插拔。每一步之後等 5 秒，再等序列埠 15 秒沒有新輸出。
4. 等到開機後 1500 秒（只有第 2 次）。沒有 TPM 時，資料分割區的 device job 最多等 15 分鐘（README「資料分割區加密」），這樣最後一張螢幕畫面是開機結束之後的。
5. 再做一次 `info usb` 與 `screendump`。

四次開機：

| 開機 | `-append` | 開機前接上 | 開機後依序熱插拔（QEMU monitor） |
| --- | --- | --- | --- |
| control-a | 對照組 | `-device usb-kbd,port=1,id=kbd0` | `device_del kbd0`；`device_add usb-storage,drive=stick1,port=1,id=st1`；`device_del st1`；`device_add usb-hub,port=1,id=hub1`；`device_add usb-kbd,port=1.1,id=kbd1` |
| a | deny | 同上 | 同上 |
| control-b | 對照組 | `-device usb-storage,drive=stick0,port=1,id=st0` | `device_del st0`；`device_add usb-kbd,port=1,id=kbd1`；`device_del kbd1`；`device_add usb-hub,port=1,id=hub1`；`device_add usb-storage,drive=stick1,port=1.1,id=st1` |
| b | deny | 同上 | 同上 |

QEMU 的 raspi3b 只有一個 USB 根埠（dwc2；映像的核心用 Raspberry Pi 的 `dwc_otg` 驅動程式），所以裝置一律接在 `port=1`，要接第二個裝置就先接一個 hub。

## 結果

原始資料在 [data/2026-10-31-815-gateway-login-surface/](data/2026-10-31-815-gateway-login-surface/)：

- `checks.json`：`check.ts` 23 項檢查的結果。
- `login-surface.txt`：建置的登入面步驟與 `cmdline.txt`、檢查的登入相關輸出、delta 裡登入面的檔案。
- `lan-scan-before-after.txt`：「祕密與內網名稱」的前後對照。
- `boot-test.txt`、`boot-test.json`：開機測試的輸出與每一項的結果。
- `qemu/`：模擬開機。
  - `qemu.txt` 是兩次執行的測試程式輸出：套件、boot 檔案的 SHA-256、每次開機的 QEMU 指令與摘要。
  - 每次開機的目錄裡有序列埠紀錄（`serial.log`，mini UART）、monitor 的指令（`monitor.log`）、開機後與最後的 `info usb`，以及 `screens.txt`（兩張螢幕畫面的手動文字轉錄）。
  - `b-run2-sd-fault.txt`：第 2 次的 b 沒有開進 systemd 的紀錄摘錄。
- `ssh-generator-probe.txt`：來源映像裡 `systemd-ssh-generator` 的輸出（前一版的探查，見「sshd 的本機 socket」）。
- `lab-gateway-cmdline.txt`：lab 的閘道器 `/proc/cmdline` 與 `cmdline.txt` 的參數名稱（不含值）。

螢幕畫面的 PNG 不放進 repo：本目錄的規則只收文字格式（[README](README.md)「原始資料」），所以報告附的是轉錄。

### 建置

建置的登入面步驟遮蔽 9 個主控台 unit 與 3 個 sshd 的 socket，寫入 logind 的 drop-in；boot 分割區的 `cmdline.txt` 變成：

```text
console=serial0,115200 console=tty1 root=PARTUUID=27b94958-02 rootfstype=ext4 fsck.repair=yes rootwait resize panic=10 usbcore.authorized_default=0
```

建置以 `exit 0` 結束（10 分 23 秒）。映像 SHA-256 `b8dd2f32f2ac0c947d1be94e15cf450490c4aff507b1db0b351edcecf60dd5c6`，5226102784 位元組。

### 檢查

23 項全部通過（`23 of 23 checks passed`，`check exit 0`）。本 issue 相關的幾項：

| 檢查 | 結果 |
| --- | --- |
| sshd accepts public keys only, for the administrator only | 對 `trustiot-admin`、`root`、`pi`、`trustiot-agent` 四個連線，`sshd -T -C` 的結果都是 `allowusers trustiot-admin`、`authenticationmethods publickey`，沒有 `denyusers`；所有 sshd 設定檔都沒有 `Match` 區塊 |
| no login prompt or shell on the screen or the serial console | 9 個 unit 與 19 個 instance 都是 `masked`；接上終端機的 service 除了 `emergency.service`、`rescue.service`（sulogin）都遮蔽；沒有任何設定提到 `SYSTEMD_SULOGIN_FORCE`；root 鎖定；logind `NAutoVTs=0`、`ReserveVT=0` |
| a failed boot reboots instead of opening the initramfs shell | `cmdline.txt` 只有 `panic=10`；`initramfs8` 與 `initramfs_2712` 的 `init` 讀 `panic=`，`panic()` 在開 shell 之前重開機 |
| no USB device is authorized (usbcore.authorized_default=0) | 見下方 |
| sshd has no local socket (only SSH over the network) | `systemd-ssh-generator` 連到 `/dev/null`，`sshd-extra.socket`、`sshd-unix-local.socket`、`sshd-vsock.socket` 都是 `masked` |
| no private keys, secrets or LAN names in what the build changed | 掃描 460 個新增或改過的檔案：沒有私鑰區塊，設定檔沒有內網名稱，gitleaks 只有 `KNOWN_FINDINGS` 列的兩個 TPM FAPI profile |

USB 一項的輸出（`login-surface.txt`）：

```text
PASS  no USB device is authorized (usbcore.authorized_default=0): cmdline.txt: one line, no --, panic=10 and usbcore.authorized_default=0 once each, no gadget module; config.txt has no cmdline=, os_prefix=, include or dwc2 outside host mode; no autoboot.txt; usbcore built into 6.18.50+rpt-rpi-v8, 6.18.50+rpt-rpi-2712, none with CONFIG_CMDLINE_FORCE or usbcore in a built-in command line; 74 udev rules files in /etc/udev/rules.d, /run/udev/rules.d, /usr/local/lib/udev/rules.d, /usr/lib/udev/rules.d: none writes "authorized", 26 programs they run are in the image and the 5 scripts among them do not name it; 31 tmpfiles.d files and 0 sysfsutils files: none writes it; usbguard not installed; none of 10992 text files under /etc, /usr/local, /usr/bin, /usr/sbin, /usr/libexec and /usr/lib, nor 184 in initramfs8, initramfs_2712, names the attribute
```

agent 服務帳號的 sshd 判定：

```text
sshd -T -C user=trustiot-agent,host=gateway.example,addr=192.0.2.2,laddr=192.0.2.2,lport=22: allowusers trustiot-admin; denyusers (none); authenticationmethods publickey
```

`AllowUsers` 沒有指定位址，映像也沒有 `Match` 區塊，所以從本機（127.0.0.1）連線的判定相同。依 `edge/image` 的規則，測試連線只用文件用的位址（RFC 5737）。

### 祕密與內網名稱：前後對照

- **前（main `dbb41618`）**：這一項不通過。main 的 `check.ts` 把從 `edge/agent/systemd` 複製的檔案當成建置寫的設定檔，比對內網位址。agent 的出口規則檔刻意寫著它要擋的位址，所以 `/usr/share/trustiot-agent/trustiot-agent-egress.nft` 有 3 個發現：第 57 行有兩個 metadata 位址（`LAN_PATTERNS[3]`、`[4]`），第 48 行有 ULA 的例子（`LAN_PATTERNS[5]`）。Laysi/trustiot-v2#810 的報告記錄了同一個失敗（[2026-10-31-810](2026-10-31-810-data-partition-luks.md)，當時是第 51 行）。
- **後（本分支）**：這一項通過。與 repo 逐位元組相同的副本不比對位址：「agent」一項確認了它們逐位元組相同，再以 gitleaks 掃描。
- **證據**：`lan-scan-before-after.txt` 以 repo 自己的 `textProblems()` 對 `edge/agent/systemd` 的每個檔案，分別用 main 與本分支的呼叫方式各跑一次。main 的方式在 `trustiot-agent-egress.nft` 得到上述 3 個發現，其他檔案 0 個；本分支的方式都是 0 個。完整建置的檢查也通過（上表）。

### Laysi/trustiot-v2#810 的開機測試

12 次開機、23 項全部通過（`23 of 23 checks passed`，`boot-test exit 0`，55 分 28 秒），每次開機的結果見 `boot-test.txt`。

這個測試用 QEMU 的 `virt` 機器與 Debian 的核心，不經過映像的 `cmdline.txt`，所以不涵蓋 `usbcore.authorized_default=0`。它證明登入面的遮蔽沒有影響資料分割區的加密、解鎖與解不開時的行為，包括每次開機 `ssh.service` 在執行、沒有等待中的密碼提示。

### QEMU 模擬開機

同一個映像（SHA-256 `b8dd2f32…`，兩次執行前都重新比對）跑了兩次，每次四次開機：

- **第 1 次**（03:01–03:27）：四次開機都完成，熱插拔的結果都在。這一次的最後一張螢幕畫面在熱插拔之後就擷取，那時開機還在等資料分割區，所以不是開機結束的畫面。
- **第 2 次**（05:09–06:55）：加上「等到開機後 1500 秒」（「每次開機的流程」第 4 步），讓最後一張畫面是開機結束之後的。control-a、a、control-b 完成。b 沒有開進 systemd：核心啟動後 5.8 秒，模擬的 SD 卡控制器（bcm2835-sdhost）出錯，之後讀不到 SD 卡（QEMU 記錄 114676 次 `sd_read_byte: DAT read illegal for command READ_MULTIPLE_BLOCK`），與 USB 無關：開機前接上的隨身碟在 1.879 秒已被拒絕，和其他 deny 的開機一樣（`qemu/b-run2-sd-fault.txt`）。這次沒有重跑 b。

`qemu/` 裡 control-a、a、control-b 的目錄來自第 2 次，b 來自第 1 次；兩次的完整輸出都在 `qemu/qemu.txt`。

序列埠紀錄的計數（`qemu.txt` 每次開機的摘要；兩次執行的 control-a、a、control-b 計數相同）：

| | control-a | a | control-b | b |
| --- | --- | --- | --- | --- |
| `usbcore.authorized_default=0` | 無 | 有 | 無 | 有 |
| 出現的 USB 裝置（`new ... USB device number`） | 3 | 3 | 3 | 3 |
| `Device is not authorized for usage` | 0 | 3 | 0 | 3 |
| 輸入裝置（`input:`） | 1 | 0 | 1 | 0 |
| `hid-generic` 鍵盤 | 1 | 0 | 1 | 0 |
| `USB Mass Storage device detected` | 1 | 0 | 1 | 0 |
| `[sda] Attached SCSI disk` | 1 | 0 | 1 | 0 |
| `USB hub found`（含 root hub） | 2 | 1 | 2 | 1 |
| 序列埠的 `login:` | 0 | 0 | 0 | 0 |
| initramfs 的 shell（`(initramfs)`、`BusyBox`） | 0 | 0 | 0 | 0 |

- **deny（a、b）**：開機前接上的裝置，以及熱插拔的鍵盤或隨身碟與 hub，三個都記錄 `Device is not authorized for usage`。核心沒有為它們設定組態：沒有輸入裝置、沒有 `sd*` 磁碟，hub 也沒有啟用（只有 root hub 的 `USB hub found`）。QEMU 的 `info usb` 仍列出這些裝置：它們接在模擬的匯流排上，只是核心不用。
- **對照組（control-a、control-b）**：只拿掉 `usbcore.authorized_default=0`，開機前接上的鍵盤或隨身碟，以及熱插拔的隨身碟或鍵盤，都出現輸入裝置（`input0`、`hid-generic ... Keyboard`）或磁碟（`[sda] Attached SCSI disk`），hub 也啟用（`hub 1-1:1.0: USB hub found`）。所以 deny 組裡沒有裝置，是因為這個參數，不是模擬環境本身列舉不了。
- **hub 後面的裝置**：熱插拔到 hub 埠 1.1 的鍵盤或隨身碟，在對照組也一直沒有列舉（第 2 次在 hub 接上後又等了 1000 秒以上）。這是 QEMU 的 dwc2 模擬的限制，所以這次只證明 hub 本身不被授權；hub 不啟用，核心就不會列舉它後面的埠。
- **螢幕**：第 2 次 control-a、a、control-b 的最後一張畫面在 1500 秒時擷取。三張都已到 `Reached target multi-user.target`、`Reached target graphical.target`，沒有登入提示，也沒有 shell；資料分割區因為沒有 TPM 而沒有解鎖（`trustiot-data-setup.service` 失敗、agent 不啟動），這是 README「資料分割區加密」描述的行為。b 的兩張畫面（第 1 次）同樣沒有登入提示，但那是開機結束之前的畫面。轉錄見各目錄的 `screens.txt`。
- **序列埠**：四次開機都沒有 `login:`，也沒有 initramfs 的 shell。systemd 的訊息在 `systemd-journald.service` 啟動（開機後約 18 秒）之後就不再送到序列埠，之後只有核心訊息（`ignore_loglevel`）；systemd 的狀態訊息送到螢幕（`console=tty1`）。
- **emergency 與 rescue**：第 2 次的三次開機在螢幕上都到 `multi-user.target`，沒有進 emergency 或 rescue 模式。序列埠紀錄也沒有這類訊息，但它在 journald 啟動之後就沒有 systemd 的訊息，所以這一項以螢幕為準。

模擬開機證明的是核心參數在這個核心上的效果，不涵蓋：

- 真的板子的 USB 控制器（BCM2711 的 VL805、BCM2712 的 RP1）與開機韌體：QEMU 的 `raspi3b` 是 BCM2837 加 dwc2，核心用同一個 `kernel8.img`。
- initramfs 失敗時重開機（`panic=10`）：這次沒有讓 initramfs 失敗。第 2 次的 b 是模擬的 SD 卡控制器出錯，核心最後執行了重開機（`reboot: Restarting system`），但那不是 initramfs 的 `panic()` 路徑，不算這一項的證據。

以上都在 Laysi/trustiot-v2#811 的實機驗證。

### sshd 的本機 socket

這一節沿用前一版的探查，那時在來源映像（遮蔽之前）執行 `systemd-ssh-generator`：

- 它寫出 `sshd-unix-local.socket`（`ListenStream=/run/ssh-unix-local/socket`、`Accept=yes`，沒有 `SocketMode=`），並放進 `sockets.target.wants`；
- 它印出 `Binding SSH to AF_UNIX socket /run/ssh-unix-local/socket`；
- 它不在 VM 裡，所以沒有 AF_VSOCK。

也就是說，不遮蔽時，閘道器上任何行程（包括 agent 的服務帳號）都能經這個 socket 連到 sshd。完成的映像遮蔽了 generator 與三個 socket（上表）。

### lab 的閘道器（唯讀）

2026-10-31 查看了兩件事：

- 序列埠：`/proc/consoles` 只有 `tty1`，沒有 `/dev/serial0`（`config.txt` 沒有 `enable_uart`），`getty@tty1` 在執行。這是燒錄本 issue 的映像之前的狀態，只用來確認序列埠在這個平台上預設就沒有輸出。
- 開機參數：韌體加上的參數排在 `cmdline.txt` 的參數前面，其中沒有 `usbcore`（`lab-gateway-cmdline.txt`，只列名稱）。所以 `cmdline.txt` 最後的 `usbcore.authorized_default=0` 不會被韌體的參數蓋過。BCM2712 的韌體在 Laysi/trustiot-v2#811 確認。

### CI 會跑的部分

| 指令 | 結果 |
| --- | --- |
| `bun run --cwd edge/image typecheck` | 通過 |
| `bun run --cwd edge/image test` | 134 項全部通過 |
| `cargo test --locked -p repo-check` | 28 項通過（兩個測試執行檔各 18、10 項） |
| `bun test scripts/ci/changes.test.ts` | 306 項通過 |
| `bun scripts/ci/check-secret-handling.ts` | 594 個檔案，沒有違規 |
| `bun scripts/ci/check-workflows.ts` | 4 個檔案，沒有違規 |
| gitleaks（分支的 commit 與 `git archive HEAD` 的樹） | 沒有發現 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 螢幕沒有登入提示 | 通過：映像檢查（getty、autovt、getty-static、console-getty 遮蔽，logind 不啟動 getty）與 QEMU 模擬開機（開機結束後的螢幕沒有登入提示）。實機確認待 Laysi/trustiot-v2#811 |
| USB 鍵盤與儲存裝置停用（2026-10-31 改為全部 USB 裝置不授權、沒有維修例外） | 通過：映像檢查與 QEMU 模擬開機（開機前接上與熱插拔的鍵盤、隨身碟、hub 都不被授權；對照組出現輸入裝置與磁碟）。實機（BCM2711 的 VL805、BCM2712 的 RP1、開機韌體）待 Laysi/trustiot-v2#811；開機韌體從 USB 開機屬 Laysi/trustiot-v2#216 |
| serial 緊急密碼每台隨機並有保管紀錄，或關閉 serial 登入並記錄理由 | 選擇關閉：`serial-getty@.service` 遮蔽（映像檢查通過，模擬開機的序列埠沒有 `login:`）。理由與重新考慮的時機寫在 README「序列埠主控台：登入關閉」、`docs/architecture/secrets.md` 與 `docs/requirements/rebuild-checklist.md` 的 D5 |
| sshd 拒絕 agent 的服務帳號（Laysi/trustiot-v2#826 審查後加入） | 通過（`sshd -T -C user=trustiot-agent,...` 的 `allowusers trustiot-admin`） |
| `sshd-unix-local.socket` 的決定（Laysi/trustiot-v2#826 審查後加入） | 映像不帶：generator 與三個 socket 遮蔽，通過 |
| initramfs 失敗時不給 shell | 映像檢查通過（`panic=10`，兩個 initramfs 的 `panic()` 都會重開機）。實機確認待 Laysi/trustiot-v2#811 |
