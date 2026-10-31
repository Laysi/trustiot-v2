# 閘道器映像（edge/image）

TrustIoT 閘道器的 SD 卡映像：以官方 Raspberry Pi OS Lite（64 位元，Debian 13 trixie）為底，加上資料分割區、agent、TPM overlay、NTP 設定與只收 SSH 公鑰的登入（Laysi/trustiot-v2#809），以及關掉螢幕、序列埠主控台與 USB 鍵盤、儲存裝置的登入面（Laysi/trustiot-v2#815，「登入面」）。建置不開機，也不需要閘道器；映像檢查同樣不開機（「映像檢查」）。

本目錄**不做**的事：資料分割區的 LUKS 加密與 TPM 自動解鎖（Laysi/trustiot-v2#810）、燒錄 lab 的閘道器並實機開機（Laysi/trustiot-v2#811）、防火牆（Laysi/trustiot-v2#813、Laysi/trustiot-v2#814）、A/B 分割區、唯讀 root、secure boot 與發佈（Laysi/trustiot-v2#216）、工廠寫入的 `/etc/trustiot/agent.toml`、閘道器憑證與根憑證（Laysi/trustiot-v2#80）。

## 做法

官方映像不重新產生：建置把官方映像的兩個分割區各自取出成檔案，以 fuse2fs 掛上 root、chroot 進去安裝套件與設定，以 mtools 改 boot 分割區，以 `mke2fs` 做出資料分割區，最後以 `sfdisk` 在新的映像檔上寫分割表、把三個分割區放到各自的位置。全程不用 loop device、不需要 sudo，也不改建置主機上的任何東西，所以：

- 一次建置的所有步驟都在一個釘選 digest 的 Debian 13 容器（rootless podman）裡做，容器結束時所有掛載一起消失；建置主機只需要 Bun 與 podman。
- e2fsprogs（`mke2fs`、`debugfs`、`e2fsck`）、fuse2fs、`sfdisk`、mtools、dosfstools、xz 不能由 proto 管理：它們沒有官方的預先編譯執行檔可供 proto 下載與驗證，只能從發行版套件安裝。所以它們裝在容器裡，版本由 snapshot.debian.org 的固定時間點決定（「釘選」），不裝在建置主機上。
- 映像裡的套件以映像自己的 apt 與 dpkg 安裝（同樣從固定時間點的 snapshot），套件的安裝腳本在映像裡執行；這些都是 arm64 程式，所以建置主機必須能執行 arm64 程式碼（「建置主機」）。

另外考慮過的做法：以 pi-gen 從頭產生（要 root 與 loop device、要建置主機整套 debootstrap 工具，也不再是官方發佈的那個映像），以及在第一次開機時才建立資料分割區（要寫自己的 initramfs 步驟，實機以外無法完整驗證，而且第一次開機前映像就不完整）。

## 建置主機

- arm64（aarch64）Linux，rootless podman 可用 `/dev/fuse`（Fedora 與 Debian 的 podman 預設可以）。2026-10-31 在 arm64 Linux 的建置主機上實測。
- x86_64 主機要先註冊 qemu-user 的 binfmt 才能執行映像裡的 arm64 程式，這會改主機核心的設定，所以不支援：`build.ts` 與 `check.ts` 在非 arm64 主機上直接拒絕。
- Bun 由 proto 管理（`.prototools`）：`proto install bun`。
- 磁碟：下載快取約 0.6 GB，一次建置的暫存檔最多約 9 GB（建置結束即刪除），輸出的映像約 5 GB（稀疏檔，實際占用約 3 GB）。快取預設在 `~/.cache/trustiot-v2-image/`，`--cache` 可改；映像、快取與 SSH 公鑰都必須在 repo 之外，`build.ts` 拒絕 repo 內的路徑。

## 從零建置

在 repo 根目錄執行。

1. agent：以 edge/agent 的 aarch64 release 建置，不加任何 feature（[edge/agent/README.md](../agent/README.md) 的交叉建置指令）。這一步可以在別台主機做（2026-10-31 在 x86_64 Linux 建置），再把執行檔複製到 arm64 建置主機：

   ```sh
   proto install zig && proto install cargo-zigbuild
   CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
     PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
     cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
   sha256sum target/aarch64-unknown-linux-gnu/release/trustiot-agent
   ```

2. 管理者的 SSH 公鑰：放在 repo 之外的檔案，一行一把（ssh-ed25519、ecdsa-sha2-nistp256/384/521 或對應的 FIDO `sk-` 金鑰；RSA 不收）。每把公鑰都要完整：`build.ts` 逐欄解析金鑰資料（型別、曲線、長度，後面不能多出位元組），寫進映像後再由映像自己的 `ssh-keygen` 讀一次並比對指紋，讀不懂就中止建置，截斷的公鑰不會讓閘道器沒有登入方式。公鑰後面的註解（常是 `使用者@工作站`）不會寫進映像。私鑰、金鑰前的選項（`command=` 等）一律拒絕。

3. 建置，再檢查：

   ```sh
   bun edge/image/build.ts --agent <trustiot-agent> --ssh-key <公鑰檔> --out <輸出目錄>
   bun edge/image/check.ts --image <輸出目錄>/trustiot-gateway.img --agent <trustiot-agent> \
     --ssh-key <公鑰檔> --report <報告目錄>
   ```

   `build.ts` 第一次執行時從官方網站下載來源映像，比對釘選的 SHA-256 才使用（不符就刪除），之後每次使用前都再比對一次。`--out` 必須是新的或空的目錄，完成後裡面有 `trustiot-gateway.img`、`trustiot-gateway.img.sha256` 與 `build-info.json`（來源、工具容器、套件版本、agent 的 SHA-256 與 `--version`、公鑰指紋、分割表、映像的 SHA-256）。建置失敗時不留下任何映像檔。`check.ts` 全部通過時結束碼為 0，有一項不通過為 1，無法執行為 2 或 3；`--report` 收到每一項的結果（`checks.json`）與建置改了來源映像的哪些路徑（`delta.tsv`、`delta-boot.tsv`）。

單元測試與型別檢查（CI 也跑，不下載映像、不需要 podman）：

```sh
bun install --cwd edge/image --frozen-lockfile
bun run --cwd edge/image typecheck
bun run --cwd edge/image test
```

## 燒錄與第一次開機

先比對 `trustiot-gateway.img.sha256`，再以 Raspberry Pi Imager 的「Use custom」燒錄（不要用 Imager 的 OS 自訂設定：映像遮蔽了 cloud-init，那些設定不會生效），或以 `dd`（以能寫入該裝置的身分執行；`SD_DEVICE` 是整張卡的裝置，寫錯裝置會清掉它）：

```sh
: "${SD_DEVICE:?set SD_DEVICE to the SD card's whole-disk device, for example /dev/sdX}"
sha256sum -c trustiot-gateway.img.sha256
dd if=trustiot-gateway.img of="$SD_DEVICE" bs=4M conv=fsync status=progress
```

卡片至少 8 GB。第一次開機時：

1. initramfs 裡官方的 `resize_early` 把 root（分割區 2）延伸到卡片結尾，`set_partuuid` 給卡片一個隨機的磁碟識別碼，並改寫 `/etc/fstab` 與 `cmdline.txt` 裡所有的 PARTUUID（包含資料分割區那一行），之後把 `resize` 從 `cmdline.txt` 拿掉（`sed 's| resize||g'`，後面的 `panic=10` 留著）；接著 `systemd-growfs-root` 延伸 root 的檔案系統。
2. machine ID 是 `uninitialized`，所以這是第一次開機：`regenerate_ssh_host_keys` 產生這台閘道器自己的 SSH 主機金鑰（映像裡沒有任何主機金鑰）。
3. 資料分割區掛在 `/var/lib/trustiot-agent`，systemd-timesyncd 向 `time.cloudflare.com` 校時，sshd 只接受 `trustiot-admin` 的公鑰登入。
4. agent 已啟用，但工廠（Laysi/trustiot-v2#80）寫入 `/etc/trustiot/agent.toml` 之前會因找不到設定而結束，依 unit 的 `Restart=always` 每分鐘重試一次。

實機的第一次開機在 Laysi/trustiot-v2#811 驗證；本目錄的檢查以映像自己的 `parted` 在一份分割表上重做 `resize_early` 的那一步（「映像檢查」）。

## 分割區

| 表內編號 | 位置（sector，512 bytes） | 大小 | 內容 |
| --- | --- | --- | --- |
| 1 | 16384 起 | 512 MiB | 官方的 boot 分割區（FAT32），加上 TPM overlay 與 `config.txt` 的兩行 |
| 3 | 1064960 起 | 2 GiB | 資料分割區：ext4，標籤 `trustiot-data`，掛在 `/var/lib/trustiot-agent` |
| 2 | 5259264 起，映像的最後 | 映像中約 2.4 GiB，第一次開機延伸到卡片結尾 | 官方的 root 分割區（ext4） |

- **root 維持表內第 2 個、放在最後**：官方 initramfs 的 `resize_early` 只在 root 是分割區 2 時才延伸，而且延伸到卡片結尾；資料分割區夾在 boot 與 root 之間，第一次開機不會移動它，官方的延伸與隨機磁碟識別碼照常運作。分割表的項目因此不依磁碟上的順序排列（`fdisk` 會提示，這是刻意的）。
- **MBR 的開機碼區沿用來源映像的**（前 440 bytes）：parted（3.6）寫 MBR 時，若開機碼區第一個位元組是 0，會清掉整個 MBR 並換一個隨機的磁碟識別碼，而 `sfdisk` 在新的映像檔上留下的開機碼區全是 0。第一次開機的 `resize_early` 若因此換掉識別碼，`set_partuuid` 就找不到舊識別碼、不會改寫 `/etc/fstab` 與 `cmdline.txt`，下一次開機找不到 root。建置在寫完分割表後把來源映像的開機碼區複製過來，檢查比對兩者並實際跑一次 parted。
- **大小固定在映像裡**：資料分割區在建置時建立，第一次開機沒有自己寫的分割步驟。2 GiB 給 agent 的離線緩衝（`buffer.disk_limit_mib` 預設 1024，見 [edge/agent/README.md](../agent/README.md)）、desired config 與憑證，並留下檔案系統與日後 LUKS2 標頭（16 MiB）的空間；工廠的設定不可讓 `disk_limit_mib` 超過約 1800。卡片多出來的空間全部給 root（日誌與系統更新）。
- **掛載**：`PARTUUID=27b94958-03 /var/lib/trustiot-agent ext4 defaults,noatime,nodev,nosuid,noexec,nofail 0 2`。以 PARTUUID 指定，第一次開機換磁碟識別碼時 `set_partuuid` 一起改寫；`nofail` 讓資料分割區掛不上時閘道器仍能開機、可以遠端處理，而 agent 不會啟動：unit 的 `StateDirectory=trustiot-agent` 隱含 `RequiresMountsFor=/var/lib/trustiot-agent`（systemd.exec(5)「Implicit Dependencies」），所以 agent 不會把資料寫到 root 上。分割區沒掛上時，掛載點本身是 root 擁有、0700 的空目錄。
- **檔案系統**：根目錄屬於 `trustiot-agent`、0700，只有 `lost+found`；不保留 root 的區塊（`-m 0`），UUID 與 hash seed 固定（`src/pins.ts`），建置時間固定為來源映像的發佈日。

### 給 LUKS（Laysi/trustiot-v2#810）

在同一個分割區（表內第 3 個、起點與大小不變）上做 LUKS2，分割表不用改。要注意：

- 第一次開機會換磁碟識別碼，`set_partuuid` 只改 `/etc/fstab` 與 `cmdline.txt`。`/etc/crypttab` 若以 `PARTUUID=` 指定分割區，要在第一次開機之後才寫，或改以 LUKS 標頭的 UUID 指定（`cryptsetup luksFormat --uuid`）。
- fstab 那一行的來源改成 `/dev/mapper/<名稱>`，掛載點與選項（含 `nofail`）不變，agent 依然靠 `StateDirectory=` 等到掛載完成。

## 映像內容

建置對官方映像做的事（`src/inside/build.ts`，依序）：

| 項目 | 內容 |
| --- | --- |
| 套件 | 從 snapshot.debian.org 安裝 `tpm-udev`（`tss` 群組與 `/dev/tpmrm0` 的 udev 規則，agent 的 unit 以 `SupplementaryGroups=tss` 使用）與 `tpm2-tools`（帶入 tpm2-tss 函式庫，Laysi/trustiot-v2#810 的 `systemd-cryptenroll` 需要；也供現場診斷），版本見「釘選」。`dpkg --audit` 必須沒有未完成的套件 |
| agent | 執行檔 `/usr/bin/trustiot-agent`（0755）、[edge/agent/systemd](../agent/systemd) 的每個檔案原樣安裝（目前是 agent 的 unit 與 sysusers 設定，以及 agent 帳號的出口規則：規則檔、載入它的 unit 與 `nftables.service` 的 drop-in，安裝位置照 [edge/agent/README.md](../agent/README.md)「出口規則」；那裡多了檔案，例如 agent 依賴的另一個 unit，`bun run --cwd edge/image test` 就失敗，直到 `src/pins.ts` 的 `AGENT_SYSTEMD_FILES` 列出它的安裝位置），`systemd-sysusers` 建立 `trustiot-agent` 使用者，unit 設為開機啟動；在映像裡執行一次 `trustiot-agent --version`，確認它能在映像的 C 函式庫上執行 |
| 帳號 | 刪除官方的預設帳號 `pi`；建立 `trustiot-admin`：密碼欄是 `*`（沒有任何密碼），`~/.ssh/authorized_keys` 是建置時給的公鑰，`/etc/sudoers.d/trustiot-admin` 允許免密碼 sudo（帳號沒有密碼，sudo 無從詢問；工廠的 `--remote-sudo` 需要。正式產線的縮限在 Laysi/trustiot-v2#219）。root 的密碼欄維持官方的 `*` |
| SSH | `/etc/ssh/sshd_config.d/00-trustiot.conf`：`PermitRootLogin no`、`PasswordAuthentication no`、`KbdInteractiveAuthentication no`、`PermitEmptyPasswords no`、`AuthenticationMethods publickey`、`AllowUsers trustiot-admin`、`X11Forwarding no`。sshd 對每個關鍵字採用第一個讀到的值，這個檔案最先被讀到。刪除官方指向改名精靈的 `rename_user.conf`，啟用 `ssh.service`；主機金鑰在第一次開機產生。建置以 `sshd -G` 與對四個連線（`trustiot-admin`、`root`、`pi`、agent 的服務帳號 `trustiot-agent`）的 `sshd -T -C` 確認有效設定與 `AllowUsers` 不放行後三個帳號，並確認所有 sshd 設定檔都沒有 `Match` 區塊（`Match` 只對符合的連線生效，`sshd -G` 不會算進去） |
| 首次設定 | cloud-init 會從 boot 分割區讀帳號、公鑰與密碼（拿得到 SD 卡的人都能寫），`userconfig.service` 是主控台上的建立帳號精靈，也讀 boot 分割區的 `userconf.txt`。建置寫入 `/etc/cloud/cloud-init.disabled`，並停用、遮蔽（`systemctl mask`）cloud-init 的每個 unit 與 `userconfig.service`：只有標記檔時，kernel 參數 `cloud-init=enabled` 仍會讓 cloud-init 執行，遮蔽的 unit 則不會被啟動。boot 分割區上 cloud-init 讀的 `user-data`、`network-config`、`meta-data`（官方內容全是註解）一併刪除。cloud-init 的套件不移除：移除它就得一起移除 `rpi-cloud-init-mods`，而那個套件帶著 netplan 交給 NetworkManager 的設定（`/usr/lib/netplan/00-network-manager-all.yaml`），第一次開機的網路就和官方映像不同 |
| 登入面 | 遮蔽每個在主控台上給登入提示或 shell 的 unit，寫入 logind 的 drop-in 與 USB 的 udev 規則（「登入面」）；boot 分割區的 `cmdline.txt` 最後加上 `panic=10` |
| 校時 | `/etc/systemd/timesyncd.conf.d/50-trustiot.conf`：`NTP=time.cloudflare.com`、`FallbackNTP=`（清掉內建的 Debian pool，防火牆不放行它） |
| 資料分割區 | `/etc/fstab` 加一行（「分割區」）、建立掛載點 |
| TPM | 編譯 [`files/tpm-slb9670-ce0-overlay.dts`](files/tpm-slb9670-ce0-overlay.dts)（以映像裡的 `dtc`）放到 boot 分割區的 `overlays/`，`config.txt` 最後加上 `[all]`、`dtparam=spi=on`、`dtoverlay=tpm-slb9670-ce0`。官方的 `tpm-slb9670` 把 TPM 放在 CE1，閘道器的模組在 CE0（[TPM 可行性測試](../../docs/feasibility/2026-10-12-tpm-poc.md)）。不碰 TPM 裡的任何金鑰 |
| 整理 | 安裝套件寫的日誌清回官方映像的空檔，刪除 `useradd` 留下的 `/etc/passwd-` 等備份；確認 machine ID 仍是 `uninitialized` |

不改的：核心、`cmdline.txt` 原有的參數（含兩個 `console=`，見「登入面」）、網路設定（NetworkManager 與 netplan，和官方映像相同；實機上的行為在 Laysi/trustiot-v2#811 確認）、avahi、Wi-Fi 與藍牙服務（防火牆與對外介面留給 Laysi/trustiot-v2#96 的子 issue）、主機名稱 `raspberrypi`、套件來源設定、apt 的套件清單。

### 套件更新

建置不安裝來源映像之後的更新：Raspberry Pi 的套件庫沒有 snapshot 服務，裝了就無法重現；Debian 的更新雖然可以從 snapshot 重現，但更新的套件若帶 initramfs hook，就要重新產生 boot 分割區上的 initramfs，建置目前不做這一步。建置記錄 snapshot 裡有更新版本的套件（`build-info.json` 的 `upgradable`，2026-10-31 的建置是 12 個，多數是 Debian 的安全更新）；目前更新的方式是換新的官方映像。建置時套用 Debian 安全更新是待決定的後續事項。

## 登入面

Laysi/trustiot-v2#815。資料分割區的加密不綁 PCR（[TPM 可行性測試](../../docs/feasibility/2026-10-12-tpm-poc.md)），整台被偷時閘道器照樣開機、自動解鎖，所以閘道器本體上不能有不用 SSH 金鑰就進得去的入口。SSH 只收公鑰、沒有任何帳號有密碼見「映像內容」的「帳號」與「SSH」；本節是本機的其他入口。預期值與產生檔案的函式在 `src/login.ts`。

### 螢幕與主控台

- 遮蔽（`systemctl mask`）下列 unit。遮蔽 template 就遮蔽它的每個 instance，所以 getty 不論由誰啟動（`getty.target`、logind、systemd 的 getty generator）都不會執行：
  - `getty@.service`，以及它的別名 `autovt@.service`（logind 在切換到虛擬主控台時啟動）、`getty-static.service`（Debian 在沒有 logind 時為 tty2 到 tty6 啟動 getty）。
  - `serial-getty@.service`（序列埠，見下）、`console-getty.service`、`container-getty@.service`。
  - `debug-shell.service`：tty9 上不用密碼的 root shell，kernel 參數 `systemd.debug_shell` 會啟動它。
  - `systemd-firstboot.service`：第一次開機時在主控台詢問語系、鍵盤、時區與 root 密碼；官方映像把它放在 `sysinit.target.wants`，而本映像的第一次開機正是 machine ID 為 `uninitialized` 的那一次。
  - `systemd-storagetm.service`：只由 kernel 參數啟動，把所有本機磁碟以 NVMe-TCP 開放到網路上，並在主控台顯示狀態。
- `/etc/systemd/logind.conf.d/50-trustiot.conf`：`NAutoVTs=0`、`ReserveVT=0`，logind 不再嘗試在虛擬主控台啟動 getty。
- 螢幕上仍有 kernel 與 systemd 的開機訊息（`console=tty1`），但沒有登入提示。
- 緊急與救援模式（`emergency.service`、`rescue.service`）保留：它們執行 sulogin，而 root 的密碼欄是 `*`（鎖定），sulogin 只印出 root 帳號已鎖定，不給 shell。`SYSTEMD_SULOGIN_FORCE` 會讓 sulogin 照樣給 shell，映像裡沒有任何 unit 或 drop-in 設定它。
- `cmdline.txt` 最後的 `panic=10`：initramfs 失敗時（例如找不到 root），initramfs-tools 的 `panic()` 原本會在主控台開一個不用密碼的 root shell；有 `panic=` 時改為 10 秒後重開機，kernel panic 也在 10 秒後重開機。這個 shell 出現在 USB 規則生效之前（規則不在 initramfs 裡，鍵盤的驅動程式又編譯在核心裡），接上鍵盤就能輸入。閘道器無人值守，重開機本來就比停在 shell 有用。Laysi/trustiot-v2#810 會重新產生 initramfs：TPM 解不開時的路徑也要以重開機收尾而不是 shell，那時要確認。

### 序列埠主控台：登入關閉

序列埠上沒有登入：`serial-getty@.service` 遮蔽，kernel 參數 `console=serial0,115200` 只輸出開機訊息，systemd 的 getty generator 為它加上的 getty 不會執行。閘道器目前的 SoC（BCM2711）上 `serial0` 是預設停用的 mini UART，`config.txt` 沒有 `enable_uart=1` 時連輸出也沒有（lab 的閘道器 2026-10-31 唯讀確認：沒有 `/dev/serial0`，`/proc/consoles` 只有 tty1）。映像也帶 BCM2712 的核心，那個平台的除錯 UART（`ttyAMA10`）預設是開的：它的 `serial-getty@ttyAMA10.service` 同樣是 `masked`。

Laysi/trustiot-v2#815 原本要每台隨機的 serial 緊急密碼；2026-10-31 決定先關閉 serial 登入，理由：

1. 現階段擋不住拿到閘道器的人：secure boot（Laysi/trustiot-v2#216）之前，改 boot 分割區的 `cmdline.txt`（`init=/bin/sh`）就能取得 root，而接得到序列埠腳位的人也拿得到 SD 卡。緊急密碼只多開一個入口，沒有多擋住誰。
2. 沒有可行的保管方式：每台一組的密碼要有存放處、誰能取得的規則與取用紀錄。平台沒有存放裝置祕密的地方（閘道器的私鑰在 TPM 裡、不匯出）；產線工作站的目錄是操作員都讀得到的檔案，不是保管處；印在機身標籤上等於交給偷走閘道器的人；內部祕密也不由人設定（[secrets.md](../../docs/architecture/secrets.md)）。
3. 維修用不到：現場維修走 SSH 公鑰（「維修時的做法」）；SSH 不通時重新燒錄，或換機（TPM 或主機板更換視同新設備、回產線登錄，[決策 12](../../docs/architecture/decisions.md)），不在現場登入除錯。
4. 映像維持「沒有任何帳號有可用的密碼」（「映像檢查」的「帳號」），sulogin 也因此不給 shell。

重新考慮的時機：secure boot（Laysi/trustiot-v2#216）擋下改開機分割區之後，而且有了保管的地方（D6 的復原金鑰託管與雙人核准）。那時的做法：密碼在閘道器上產生（不經命令列參數），只把雜湊寫進 `/etc/shadow`，明文只交給保管處，工廠流程（[factory/workstation](../../factory/workstation/README.md)）跟著改。現在的工廠流程不變。

### sshd 的本機 socket

systemd 257 的 `systemd-ssh-generator` 在每次開機時加上 `sshd-unix-local.socket`：sshd 聽 `/run/ssh-unix-local/socket`，沒有設 `SocketMode=`，閘道器上任何行程都連得到（`ssh .host` 就是走它；2026-10-31 在來源映像的 chroot 裡執行這個 generator 確認）。agent 的服務帳號 `trustiot-agent` 也連得到：它的出口規則（[edge/agent](../agent/README.md)「出口規則」）只管網路，不管 AF_UNIX。sshd 在這條路上照樣只收 `trustiot-admin` 的公鑰，但被入侵的 agent 不必經過網路就能碰到 sshd 認證前的程式碼。維修只走網路上的 SSH，所以映像遮蔽這個 generator（`/etc/systemd/system-generators/systemd-ssh-generator` 連到 `/dev/null`，systemd.generator(7)），也遮蔽它會產生的 `sshd-unix-local.socket`、`sshd-vsock.socket`（只在 VM）與 `sshd-extra.socket`（kernel 參數 `systemd.ssh_listen=`）；`/etc` 的遮蔽優先於 generator 的輸出。Laysi/trustiot-v2#826 另外讓 agent 的 sandbox 看不到 `/run/ssh-unix-local`，兩層各自成立。

sshd 拒絕 agent 的服務帳號：`AllowUsers` 只有 `trustiot-admin`，而且沒有 `Match` 區塊，不論從哪個位址連線（含本機的 127.0.0.1）都一樣；`trustiot-agent` 也沒有密碼與登入 shell（`/usr/sbin/nologin`）。建置與檢查都以 `sshd -T -C user=trustiot-agent,...` 確認。

### USB

鍵盤與儲存裝置不能用，其他 USB 裝置照常。`/usr/lib/udev/rules.d/70-trustiot-usb-deny.rules` 在介面出現（`add`，含開機時已接上、由 `systemd-udev-trigger` 重播的裝置）與驅動程式綁上（`bind`）時，把下列介面的 `authorized` 設為 0，核心就解除綁定，也不讓其他驅動程式綁上：

- 介面類別 03（HID：鍵盤、滑鼠、條碼掃描器等輸入裝置）與 08（mass storage）。
- 驅動程式 `usbhid`、`usb-storage`、`uas`、`ums-*`，不論裝置宣告哪個類別（核心以廠商與產品 ID 認得的儲存裝置）。

照常可用的例如：hub、USB 網路卡與數據機（CDC）、USB 轉序列埠（廠商自訂類別）。現場設備經 RS485 轉換器以 TCP 連線（[現場硬限制](../../docs/requirements/field-constraints.md)），閘道器不需要 USB 鍵盤或儲存裝置。

為什麼是 udev 規則而不是封鎖核心模組：Raspberry Pi 的兩個核心（`6.18.50+rpt-rpi-v8`、`6.18.50+rpt-rpi-2712`）把 `usbhid`、`usb-storage`、`uas` 編譯在核心裡（`modules.builtin`），`modprobe.d` 擋不到。也因此驅動程式在裝置出現時先綁上，udev 處理事件時才解除：這段很短的時間裡，鍵盤的輸入沒有程式讀（沒有 getty），儲存裝置不會被掛載（映像沒有自動掛載），但核心會讀它的分割表。要完全不給驅動程式，得用不含這些驅動程式的核心，或讓 USB 介面預設不授權、再逐一放行（授權後要手動觸發驅動程式探測，多介面的裝置容易出錯，實機以外驗證不了）；這留給 secure boot 與自訂核心（Laysi/trustiot-v2#216）。

initramfs 裡規則還沒生效（規則不在 initramfs 裡），USB 鍵盤與儲存裝置都能用，這是已知的限制：

- 鍵盤：initramfs 失敗時本來會開的 shell 已由 `panic=10` 關掉（「螢幕與主控台」）。
- 儲存裝置：initramfs 以 `root=PARTUUID=…` 找 root。接上一支分割區帶相同 PARTUUID 的隨身碟再重開機，兩個裝置誰先出現就用誰；usb-storage 預設延遲 1 秒才掃描，SD 卡多半先出現，但攻擊者可以重試，成功時就以自己的 root 在帶著 TPM 的閘道器上執行（資料分割區的 TPM 解鎖不綁 PCR，照樣解鎖）。PARTUUID 從螢幕上的開機訊息就讀得到。拿得到 SD 卡時，這與改 `cmdline.txt` 同級；卡槽封在機殼裡、或改用 eMMC 的機型，只要 USB 埠在外面就比改 `cmdline.txt` 容易。

完整擋下的做法是讓這些內建驅動程式不初始化（kernel 參數 `initcall_blacklist=` 列出它們的 init 函式，或不含它們的核心），同時也關掉上面「先綁上、再解除」的空窗；函式名稱要從核心的符號表查證，並在實機上確認開機不受影響，所以留給 Laysi/trustiot-v2#216（實機驗證在 Laysi/trustiot-v2#811）。

不涵蓋：藍牙鍵盤（藍牙服務留給 Laysi/trustiot-v2#96 的子 issue）、開機韌體從 USB 開機（EEPROM 的 `BOOT_ORDER`，屬於 Laysi/trustiot-v2#216）。可程式化的 USB HID 裝置在上述空窗或 initramfs 裡送的按鍵（Ctrl-Alt-Del 重開機、Alt+↑ 的 `kbrequest.target`（sulogin 一樣拒絕）、`kernel.sysrq` 允許的 SysRq）只能讓閘道器重開或停住，不給登入；拿得到閘道器的人本來就能拔電，`ctrl-alt-del.target`、`kbrequest.target` 與 `kernel.sysrq` 的收緊也留給 Laysi/trustiot-v2#216。

### 維修時的做法

- 維修一律經 SSH 以 `trustiot-admin` 的公鑰登入（「映像內容」的「SSH」）；閘道器本體上沒有任何登入方式，螢幕、鍵盤與序列埠都不是維修管道。SSH 不通時重新燒錄（「燒錄與第一次開機」）或換機，再回產線登錄。
- 檔案經 SSH 以 `scp` 或 `sftp` 傳，不用 USB 隨身碟。
- 例外：真的需要在現場用 USB 儲存裝置（或其他被擋的裝置）時，經 SSH 暫時解除，重開機後自動恢復。udev 依檔名決定用哪個規則檔，`/run/udev/rules.d` 的同名檔案優先於 `/usr/lib/udev/rules.d`（udev(7)），而 `/run` 在重開機時清空：

  ```sh
  sudo mkdir -p /run/udev/rules.d
  sudo ln -s /dev/null /run/udev/rules.d/70-trustiot-usb-deny.rules
  sudo udevadm control --reload
  # 接上裝置；解除前就接著的裝置要拔掉再接
  ```

  用完立刻恢復（或重開機）；解除期間接上的裝置要拔掉：

  ```sh
  sudo rm /run/udev/rules.d/70-trustiot-usb-deny.rules
  sudo udevadm control --reload
  ```

  解除期間接上的任何鍵盤或儲存裝置都能用，所以只在有人在現場、而且經 SSH 登入時做。即使接上鍵盤，閘道器上也沒有可以登入的地方。

## 釘選

改任何一項都會改變映像；改 `src/pins.ts` 時同一個變更更新本表（`bun run --cwd edge/image test` 檢查本文件列出每一個釘選值）。

| 項目 | 值 |
| --- | --- |
| 來源映像 | Raspberry Pi OS Lite（64 位元）2026-10-06，Debian 13（trixie），核心 6.18.50：https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2026-10-06/2026-10-06-raspios-trixie-arm64-lite.img.xz |
| 來源 SHA-256 | `483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843`（與官方網站同目錄的 `.sha256` 相同，2026-10-31 核對） |
| 工具容器 | `docker.io/library/debian:trixie@sha256:2dc419c6d7fe3dad0a2f9fd469c34cc90b0f9ba642d8fde837fe3d7d34a0e7f1`（trixie，多架構 index。寫成 `名稱:tag@sha256:…`，Renovate 讀得到，更新時與 `src/pins.ts` 在同一個 PR 一起換） |
| Debian 套件時間點 | snapshot.debian.org `20261030T000000Z`（容器的工具與映像的套件都從這裡安裝；apt 照常驗證套件庫簽章） |
| 工具容器的套件 | ca-certificates 20250419+deb13u1、dosfstools 4.2-1.2、e2fsprogs 1.47.2-3+b12、fdisk 2.41.5-0+deb13u1、fuse2fs 1.47.2-3+b12、mtools 4.0.48-1、xz-utils 5.8.1-1+deb13u2（以 `名稱=版本` 安裝。容器裡還沒有 CA 憑證，ca-certificates 只能以 HTTP 取得：apt 照常驗證套件庫簽章與檔案雜湊，固定版本則讓重放的舊索引無法換成較舊的套件；其餘套件之後以 HTTPS 取得） |
| 映像加裝的套件 | tpm-udev 0.6+nmu1、tpm2-tools 5.7-1+b1（相依：libtss2-* 4.1.3-1.2） |
| NTP | time.cloudflare.com（Cloudflare 公布的位址：162.159.200.1、162.159.200.123、2606:4700:f1::1、2606:4700:f1::123，見 https://developers.cloudflare.com/time-services/ntp/usage/ ，2026-10-31 查閱）。Cloudflare 不做閏秒抹平（leap smear），所以不與會抹平的伺服器（例如 Google）混用 |
| 資料分割區 | 2 GiB，ext4，標籤 `trustiot-data`，UUID `6f3c1b2e-5a4d-4c1e-9b7a-2d8e0f4a9c31` |

lab 的閘道器目前跑的是 2026-09-15 的官方映像（raspi-firmware 1:1.20260915-1），與本映像的來源同一個核心版本（6.18.50）；兩者的差異在 Laysi/trustiot-v2#811 實機開機時確認。

## 映像檢查

`check.ts` 只讀完成的映像、來源映像、agent 執行檔與公鑰檔，和建置共用預期值（`src/pins.ts`、`src/files.ts`、`src/login.ts`），但不共用建置的步驟。每一項：

| 檢查 | 內容 |
| --- | --- |
| 分割表 | 磁碟識別碼、三個分割區的位置、大小與類型和預期完全相同；4 MiB 對齊、不重疊、boot 在最前、root 在最後；映像檔大小 |
| 檔案系統 | root 與資料的 `e2fsck -fn`、boot 的 `fsck.vfat -n` 都沒有錯誤 |
| 資料分割區 | 標籤、UUID、根目錄的擁有者（`trustiot-agent`）與 0700、內容是空的；fstab 那一行（PARTUUID、ext4、`nodev,nosuid,noexec,nofail`、fsck 順序 2）、掛載點、unit 有 `StateDirectory=trustiot-agent` |
| 第一次開機 | 以映像的 `parted` 對 16 GiB 的檔案（只有映像的分割表）執行 `resize_early` 的指令：root 延伸到結尾，boot 與資料分割區不動；machine ID 是 `uninitialized`、沒有 SSH 主機金鑰、主機金鑰的產生服務已啟用、`cmdline.txt` 有 `resize` 與分割區 2 的 root |
| agent | 執行檔與給的檔案 SHA-256 相同、root 擁有、0755、aarch64 ELF、在映像裡執行 `--version` 成功；unit 與 sysusers 和 repo 的檔案逐位元組相同、unit 已啟用、`trustiot-agent` 使用者、`tss` 群組、tpmrm 的 udev 規則；映像裡沒有 `/etc/trustiot/agent.toml` |
| 套件 | tpm-udev、tpm2-tools 是釘選版本且已完成安裝，`dpkg --audit` 為空 |
| TPM overlay | boot 分割區上的 `.dtbo` 與 repo 的 `.dts` 重新編譯的結果相同；`config.txt` 對所有型號有那兩行；以 `dtmerge` 疊到 `bcm2711-rpi-4-b.dtb` 後，spi0 有 reg 0 的 `infineon,slb9670`、spidev0 停用 |
| NTP | `systemd-analyze cat-config` 算出的有效設定只有釘選的伺服器、沒有 fallback；timesyncd 已啟用 |
| sshd | `sshd -G`，以及以暫時的主機金鑰（放在 chroot 的 tmpfs，不進映像）對 `trustiot-admin`、`root`、`pi`、`trustiot-agent` 四個連線各跑一次 `sshd -T -C`：有效設定是「映像內容」的每一項，`AllowUsers` 不放行 `root`、`pi`、`trustiot-agent`（不能有萬用字元）；所有 sshd 設定檔沒有 `Match` 區塊；`ssh.service` 已啟用、改名精靈的設定已刪 |
| 管理者 | `authorized_keys` 正好是給的公鑰（不含註解），映像自己的 `ssh-keygen` 讀得懂每一把且指紋相同；家目錄與 `.ssh` 0700、`authorized_keys` 0600；密碼欄是 `*`；sudoers 檔的內容、0440、`visudo --check --strict`；`/etc/sudoers.d` 只多了這個檔案，`/etc/sudoers` 與來源映像原有的規則檔都沒改；`sudo` 群組沒有成員 |
| 帳號 | 沒有任何帳號有可用的密碼（密碼欄是雜湊或空白都算可用）、沒有群組密碼、`pi` 已刪、沒有 `/etc/shadow-` 等備份 |
| 首次設定 | 映像裡每個 cloud-init 的 unit 與 `userconfig.service` 都是 `masked`、有 `/etc/cloud/cloud-init.disabled`、boot 分割區沒有 `user-data`、`network-config`、`meta-data` |
| 主控台 | 「登入面」遮蔽的每個 unit，以及 tty1 到 tty6 的 `getty@`、`autovt@`，序列埠 ttyS0、ttyS1、ttyAMA0、ttyAMA1、ttyAMA10、hvc0 的 `serial-getty@` 都是 `masked`；unit 目錄（`/etc`、`/usr/local/lib`、`/usr/lib` 的 `systemd/system`）裡每個以 `StandardInput=tty*` 或 `TTYPath=` 接上終端機、或 `Exec*=` 提到 getty、sulogin、`sh -i` 或終端機裝置的 service（含 drop-in）都是 `masked`，例外只有執行 `systemd-sulogin-shell` 的 `emergency.service` 與 `rescue.service`，而它們只有官方的 unit 檔（沒有覆蓋或 drop-in）；`/etc`、`/usr/lib/systemd`、`/usr/local/lib/systemd` 沒有任何文字檔提到 `SYSTEMD_SULOGIN_FORCE`（含 manager 的 `DefaultEnvironment=` 與 `EnvironmentFile=` 指的檔案）；unit 目錄裡沒有連到目錄外的 unit；建置沒有新增 `/etc/init.d`、`/etc/rc?.d` 的腳本或 `/etc/rc.local`；root 的密碼欄鎖定；`systemd-analyze cat-config` 算出的 logind 設定是 `NAutoVTs=0`、`ReserveVT=0` |
| 開機失敗 | `cmdline.txt` 只有一個 `panic=`，是 `panic=10`；以映像的 `unmkinitramfs` 解開 boot 分割區上的每個 initramfs：`init` 讀 `panic=`，`scripts/functions` 的 `panic()` 在 `panic=` 有值時重開機，而且在那之前不開 shell |
| sshd 本機 socket | `/etc/systemd/system-generators/systemd-ssh-generator` 是指向 `/dev/null` 的連結；`sshd-extra.socket`、`sshd-unix-local.socket`、`sshd-vsock.socket` 都是 `masked`；unit 目錄裡沒有其他沒遮蔽、以 `ListenStream=`、`ListenSequentialPacket=` 或 `ListenDatagram=` 的路徑、抽象名稱或 `vsock:` 位址讓 sshd 聽本機 socket 的 socket unit（服務依 `Service=` 或 socket 的名稱，在每個 unit 目錄找） |
| USB | 規則檔的內容、0644、root 擁有，映像的 `udevadm verify` 通過；`/etc/udev/rules.d` 與 `/usr/local/lib/udev/rules.d` 沒有同名的檔案或連結；所有 udev 規則檔（連結就在映像裡跟到它指的檔案，讀不到的連結算問題）裡只有這一個寫入 `authorized`（`ATTR{}`，或 `RUN`、`PROGRAM`、`IMPORT` 提到它）；排在它前面的規則檔沒有 `last_rule`；結果列出每個核心的 `modules.builtin` 裡有哪些被擋的驅動程式 |
| 祕密與內網名稱 | 建置新增或改過的每個檔案（root 與 boot，與來源映像逐檔比對得出）：文字檔沒有私鑰區塊，設定檔（`/etc`、`/home`、`/root` 與建置寫的檔案）沒有私有網段位址或只在內網解析的名稱（報告只寫行號與規則，不寫出比對到的內容）；從 `edge/agent/systemd` 原樣複製的檔案不比對位址：「agent」一項已確認它們與 repo 的檔案逐位元組相同，而出口規則本來就寫著它要擋的位址（metadata 的 `169.254.169.254` 等）；這些檔案再以 `.prototools` 釘選的 gitleaks（`gitleaks dir`）掃描；沒有 `~/.ssh/id_*`。gitleaks 的發現只有 `src/pins.ts` 的 `KNOWN_FINDINGS` 列出的才接受：路徑、規則、`dpkg-query -S` 查到的套件與整個檔案的 SHA-256 都要相同，建置自己寫的檔案一律不接受。目前列的是 libtss2-fapi1t64 4.1.3-1.2 的兩個 TPM FAPI profile（`/etc/tpm2-tss/fapi-profiles/P_ECCP384SHA384.json`、`/etc/tpm2-tss/fapi-profiles/P_RSA3072SHA384.json`）：`generic-api-key` 比對到的是 `authPolicy`，NV index 授權策略的公開摘要，不是祕密（2026-10-31 檢視）。套件版本一變，檔案的 SHA-256 跟著變，檢查就不通過，要重新檢視後更新清單 |

### 檢查證明什麼、不證明什麼

- 拿得到 SD 卡或閘道器本體的人，等同拿到 root：可以改 boot 分割區的 `cmdline.txt`（例如 `init=/bin/sh`），或直接改 root 檔案系統。遮蔽 cloud-init 與 userconfig 只是拿掉官方映像現成的入口，不是對實體存取的防護；那要等 secure boot 與資料分割區加密（Laysi/trustiot-v2#216、Laysi/trustiot-v2#810），Laysi/trustiot-v2#216 應把這一點列為前提。
- 登入面的檢查看的是映像裡的設定：unit 的狀態、規則檔與開機參數。螢幕上真的沒有登入提示、接上 USB 鍵盤與隨身碟沒有作用、initramfs 失敗時重開機，要在閘道器上開機驗證（Laysi/trustiot-v2#811）。USB 驅動程式在 udev 解除之前短暫綁上的那段時間（「登入面」的「USB」）不在檢查範圍。
- `check.ts` 驗的是以 `build.ts` 從已知輸入做出的映像，不是第三方映像的驗收工具：它列出建置改了來源映像的哪些路徑（`delta.tsv`），但不判斷每個改動是否惡意，例如被換掉的執行檔。燒錄前要確認的是映像的 SHA-256 與建置記錄（`trustiot-gateway.img.sha256`、`build-info.json`）相同，不是重跑檢查。

## 可重現性

映像不是逐位元相同。同樣的輸入（來源映像、工具容器、snapshot 時間點、agent 執行檔、公鑰、本目錄）預期得到同樣的檔案、權限與擁有者，內容除了下列幾處也相同：

- root 檔案系統經 fuse2fs 修改，新增與改過的檔案、目錄的 inode 時間是建置當時的時間，inode 與區塊的配置也可能不同。
- 內容帶有建置日期或時間的檔案：`/etc/shadow` 裡建置建立或修改的帳號（`trustiot-admin`、`trustiot-agent`）的密碼最後變更日（以日計；建置沒有設定 `SOURCE_DATE_EPOCH`），`/var/cache/ldconfig/aux-cache`（記錄函式庫檔案的 inode 與時間），以及 dpkg、apt 與 debconf 在映像裡留下的狀態（`/var/lib/dpkg/status-old`、`/var/cache/debconf`）。
- boot 分割區的 FAT 項目時間固定為來源映像的發佈日，資料分割區以固定的 UUID、hash seed 與時間建立。

上面是依建置步驟推得的；兩次建置的逐檔比對（下面的 manifest）2026-10-31 還沒有實際做過。

比對兩次建置以檔案為單位：`check.ts --manifest yes --report <目錄>` 會在報告目錄寫出 root 檔案系統每個檔案的類型、權限、擁有者、大小與 SHA-256（`manifest-root.tsv`），兩份直接 `diff`。

## 容器

`build.ts` 與 `check.ts` 以 root 身分執行，或 podman 不是 rootless（`podman info` 的 `Rootless` 不是 `true`）時直接拒絕：容器裡跑的是映像自己的程式（套件的安裝腳本、`sshd -G`、agent），只有 rootless 時它們才關在一般使用者的權限裡。`src/host.ts` 的 `containerArgs` 以 rootless podman 執行工具容器：`--cap-add=SYS_ADMIN` 只在容器自己的 user namespace 內有效，讓 fuse2fs、chroot 需要的 bind mount 與 tmpfs 能掛上；`--device=/dev/fuse` 給 fuse2fs；`--security-opt=no-new-privileges`；`--security-opt=label=disable` 讓 SELinux 主機上的容器讀得到掛進去的主機路徑（不改主機檔案的標籤）。輸入一律唯讀掛載，Bun 本身從主機以唯讀掛入，不在容器裡下載。容器要連網（snapshot.debian.org）。
