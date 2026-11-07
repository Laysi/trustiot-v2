# 閘道器映像（edge/image）

TrustIoT 閘道器的 SD 卡映像：以官方 Raspberry Pi OS Lite（64 位元，Debian 13 trixie）為底，加上資料分割區、agent、TPM overlay、NTP 設定與只收 SSH 公鑰的登入（Laysi/trustiot-v2#809）；資料分割區的加密：第一次開機時閘道器自己以 LUKS2 加密、之後由 TPM 自動解鎖，解不開就不掛上、agent 不啟動（Laysi/trustiot-v2#810，「資料分割區加密」），換了 TPM 或 TPM 被清除而永遠解不開時清除重建、標記回產線重新登錄（Laysi/trustiot-v2#867，「TPM 永久解不開時」）；以及關掉螢幕與序列埠主控台的登入、核心不授權任何 USB 裝置的登入面（Laysi/trustiot-v2#815，「登入面」）。建置不開機，也不需要閘道器；映像檢查同樣不開機（「映像檢查」）；開機測試在 QEMU 裡以軟體 TPM 開機（「開機測試」）。

本目錄**不做**的事：產線移除「回產線重新登錄」標記的工具（「TPM 永久解不開時」的「產線要做什麼」）、燒錄 lab 的閘道器並實機開機（Laysi/trustiot-v2#811）、防火牆（Laysi/trustiot-v2#813、Laysi/trustiot-v2#814）、A/B 分割區、唯讀 root、secure boot 與發佈（Laysi/trustiot-v2#216）、工廠寫入的 `/etc/trustiot/agent.toml`、閘道器憑證與根憑證（Laysi/trustiot-v2#80）。

## 做法

官方映像不重新產生：建置把官方映像的兩個分割區各自取出成檔案，以 fuse2fs 掛上 root、chroot 進去安裝套件與設定，以 mtools 改 boot 分割區，資料分割區留成全零（第一次開機時閘道器自己加密），最後以 `sfdisk` 在新的映像檔上寫分割表、把三個分割區放到各自的位置。全程不用 loop device、不需要 sudo，也不改建置主機上的任何東西，所以：

- 一次建置的所有步驟都在一個釘選 digest 的 Debian 13 容器（rootless podman）裡做，容器結束時所有掛載一起消失；建置主機只需要 Bun 與 podman。
- e2fsprogs（`mke2fs`、`debugfs`、`e2fsck`）、fuse2fs、`sfdisk`、mtools、dosfstools、xz 不能由 proto 管理：它們沒有官方的預先編譯執行檔可供 proto 下載與驗證，只能從發行版套件安裝。所以它們裝在容器裡，版本由 snapshot.debian.org 的固定時間點決定（「釘選」），不裝在建置主機上。
- 映像裡的套件以映像自己的 apt 與 dpkg 安裝（同樣從固定時間點的 snapshot），套件的安裝腳本在映像裡執行；這些都是 arm64 程式，所以建置主機必須能執行 arm64 程式碼（「建置主機」）。

另外考慮過的做法：以 pi-gen 從頭產生（要 root 與 loop device、要建置主機整套 debootstrap 工具，也不再是官方發佈的那個映像），以及在第一次開機時才建立資料分割區（要寫自己的 initramfs 步驟，實機以外無法完整驗證，而且第一次開機前映像就不完整）。

## 建置主機

- arm64（aarch64）Linux，rootless podman 可用 `/dev/fuse`（Fedora 與 Debian 的 podman 預設可以）。2026-10-31 在 arm64 Linux 的建置主機上實測。
- x86_64 主機要先註冊 qemu-user 的 binfmt 才能執行映像裡的 arm64 程式，這會改主機核心的設定，所以不支援：`build.ts`、`check.ts` 與 `boot-test.ts` 在非 arm64 主機上直接拒絕。
- Bun 由 proto 管理（`.prototools`）：`proto install bun`。
- 磁碟：下載快取約 0.6 GB，一次建置的暫存檔最多約 9 GB（建置結束即刪除），輸出的映像約 5 GB（稀疏檔，實際占用約 3 GB）。快取預設在 `~/.cache/trustiot-v2-image/`，`--cache` 可改；映像、快取與 SSH 公鑰都必須在 repo 之外，`build.ts` 拒絕 repo 內的路徑。

## 從零建置

在 repo 根目錄執行。

1. agent：以 edge/agent 的 aarch64 release 建置，不加任何 feature（[edge/agent/README.md](../agent/README.md) 的交叉建置指令）。一次建置做出 agent 的 package 的四個執行檔（`src/pins.ts` 的 `AGENT_EXECUTABLES`）：`trustiot-agent`，以及 unit 以 root 或 `CAP_NET_ADMIN` 執行的 `trustiot-agent-allowlist`、`trustiot-agent-nameservers` 與 `trustiot-agent-ip-deny-check`（「agent 的防火牆」）。這一步可以在別台主機做（2026-10-31 在 x86_64 Linux 建置），再把四個執行檔放在同一個目錄、複製到 arm64 建置主機：`--agent` 指向其中的 `trustiot-agent`，建置與檢查從同一個目錄取另外三個，少了一個或不是 aarch64 ELF 就以用法錯誤結束：

   ```sh
   proto install zig && proto install cargo-zigbuild
   CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
     PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
     cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
   sha256sum target/aarch64-unknown-linux-gnu/release/trustiot-agent{,-allowlist,-nameservers,-ip-deny-check}
   ```

2. 管理者的 SSH 公鑰：放在 repo 之外的檔案，一行一把（ssh-ed25519、ecdsa-sha2-nistp256/384/521 或對應的 FIDO `sk-` 金鑰；RSA 不收）。每把公鑰都要完整：`build.ts` 逐欄解析金鑰資料（型別、曲線、長度，後面不能多出位元組），寫進映像後再由映像自己的 `ssh-keygen` 讀一次並比對指紋，讀不懂就中止建置，截斷的公鑰不會讓閘道器沒有登入方式。公鑰後面的註解（常是 `使用者@工作站`）不會寫進映像。私鑰、金鑰前的選項（`command=` 等）一律拒絕。

3. 建置，再檢查：

   ```sh
   bun edge/image/build.ts --agent <trustiot-agent> --ssh-key <公鑰檔> --out <輸出目錄>
   bun edge/image/check.ts --image <輸出目錄>/trustiot-gateway.img --agent <trustiot-agent> \
     --ssh-key <公鑰檔> --report <報告目錄>
   ```

   `build.ts` 第一次執行時從官方網站下載來源映像，比對釘選的 SHA-256 才使用（不符就刪除），之後每次使用前都再比對一次。`--out` 必須是新的或空的目錄，完成後裡面有 `trustiot-gateway.img`、`trustiot-gateway.img.sha256` 與 `build-info.json`（來源、工具容器、套件版本、agent 的 SHA-256 與 `--version`、四個執行檔各自的 SHA-256、公鑰指紋、分割表、映像的 SHA-256）。建置失敗時不留下任何映像檔。`check.ts` 全部通過時結束碼為 0，有一項不通過為 1，無法執行為 2 或 3；`--report` 收到每一項的結果（`checks.json`）與建置改了來源映像的哪些路徑（`delta.tsv`、`delta-boot.tsv`）。

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

1. initramfs 裡官方的 `resize_early` 把 root（分割區 2）延伸到卡片結尾，`set_partuuid` 給卡片一個隨機的磁碟識別碼，並改寫 `/etc/fstab` 與 `cmdline.txt` 裡所有的 PARTUUID（包含資料分割區那一行），之後把 `resize` 從 `cmdline.txt` 拿掉（`sed 's| resize||g'`，後面的 `panic=10 usbcore.authorized_default=0` 留著）；接著 `systemd-growfs-root` 延伸 root 的檔案系統。
2. machine ID 是 `uninitialized`，所以這是第一次開機：`regenerate_ssh_host_keys` 產生這台閘道器自己的 SSH 主機金鑰（映像裡沒有任何主機金鑰）。
3. `trustiot-data-setup` 在全是零的資料分割區上建立 LUKS2、登錄這台閘道器的 TPM、在裡面做出檔案系統，`systemd-cryptsetup` 以 TPM 解鎖後掛在 `/var/lib/trustiot-agent`（「資料分割區加密」）；沒有 TPM 時分割區維持全零、不掛上，agent 不啟動。systemd-timesyncd 向 `time.cloudflare.com` 校時，sshd 只接受 `trustiot-admin` 的公鑰登入。
4. agent 已啟用，但工廠（Laysi/trustiot-v2#80）寫入 `/etc/trustiot/agent.toml` 之前會因找不到設定而結束，依 unit 的 `Restart=always` 每分鐘重試一次。

實機的第一次開機在 Laysi/trustiot-v2#811 驗證；本目錄的檢查以映像自己的 `parted` 在一份分割表上重做 `resize_early` 的那一步（「映像檢查」），資料分割區的加密、解鎖與解不開時的行為由開機測試在 QEMU 裡證明（「開機測試」）。

燒錄要寫入映像的每一個 byte（上面的 `dd` 或 Imager 都是）：只寫非零區塊的工具會在資料分割區留下卡片原本的內容，例如之前那台閘道器的 LUKS 標頭。放回原來那台會解開舊的資料；放進別台已登錄的閘道器（TPM 裡有身分金鑰）時只是解不開、不掛上；放進 TPM 是新的或被清除過的閘道器時，setup 分不出這和「這台換了 TPM」，就清除重建，並標記這台閘道器回產線重新登錄、agent 不啟動（「TPM 永久解不開時」；標頭沒有記下 owner hierarchy 的舊卡則只是解不開、不掛上）。要重用的卡，先整張清成零再燒錄。

## 分割區

| 表內編號 | 位置（sector，512 bytes） | 大小 | 內容 |
| --- | --- | --- | --- |
| 1 | 16384 起 | 512 MiB | 官方的 boot 分割區（FAT32），加上 TPM overlay 與 `config.txt` 的兩行 |
| 3 | 1064960 起 | 2 GiB | 資料分割區：映像裡全是零；第一次開機成為 LUKS2 volume，裡面是 ext4，解鎖後掛在 `/var/lib/trustiot-agent` |
| 2 | 5259264 起，映像的最後 | 映像中約 2.4 GiB，第一次開機延伸到卡片結尾 | 官方的 root 分割區（ext4） |

- **root 維持表內第 2 個、放在最後**：官方 initramfs 的 `resize_early` 只在 root 是分割區 2 時才延伸，而且延伸到卡片結尾；資料分割區夾在 boot 與 root 之間，第一次開機不會移動它，官方的延伸與隨機磁碟識別碼照常運作。分割表的項目因此不依磁碟上的順序排列（`fdisk` 會提示，這是刻意的）。
- **MBR 的開機碼區沿用來源映像的**（前 440 bytes）：parted（3.6）寫 MBR 時，若開機碼區第一個位元組是 0，會清掉整個 MBR 並換一個隨機的磁碟識別碼，而 `sfdisk` 在新的映像檔上留下的開機碼區全是 0。第一次開機的 `resize_early` 若因此換掉識別碼，`set_partuuid` 就找不到舊識別碼、不會改寫 `/etc/fstab` 與 `cmdline.txt`，下一次開機找不到 root。建置在寫完分割表後把來源映像的開機碼區複製過來，檢查比對兩者並實際跑一次 parted。
- **大小固定在映像裡**：分割區在建置時就在分割表裡，第一次開機沒有自己寫的分割步驟。2 GiB 給 agent 的離線緩衝（`buffer.disk_limit_mib` 預設 1024，見 [edge/agent/README.md](../agent/README.md)）、desired config 與憑證，並留下檔案系統與 LUKS2 標頭（16 MiB）的空間；工廠的設定不可讓 `disk_limit_mib` 超過約 1800。卡片多出來的空間全部給 root（系統更新）。
- **掛載**：`/dev/mapper/trustiot_data /var/lib/trustiot-agent ext4 defaults,noatime,nodev,nosuid,noexec,nofail,x-systemd.device-timeout=15min 0 2`，只掛解鎖後的 volume，不以 PARTUUID 或標籤指定分割區本身。`nofail` 讓資料分割區解不開或掛不上時閘道器仍能開機、可以遠端處理，而 agent 不會啟動（「資料分割區加密」）；`x-systemd.device-timeout=15min` 見「解鎖」。分割區沒掛上時，掛載點本身是 root 擁有、0700 的空目錄。
- **檔案系統**：第一次開機時在解鎖後的 volume 裡建立：標籤 `trustiot-data`，根目錄屬於 `trustiot-agent`、0700，只有 `lost+found`；不保留 root 的區塊（`-m 0`）。

## 資料分割區加密

資料分割區（表內第 3 個）是 LUKS2 volume，只有這台閘道器的 TPM 打得開：沒有密碼、沒有 recovery key，也沒有人看過金鑰。要防的是 SD 卡被拔走、或被放到另一台機器讀取；檔案在 [`files/data/`](files/data)，安裝位置在 `src/pins.ts` 的 `DATA_FILES`。

### 何時加密

在閘道器第一次開機時，不在建置時：TPM 的登錄要用那台閘道器自己的 TPM，而每台閘道器燒的是同一個映像，所以映像裡的資料分割區全是零。`trustiot-data-setup.service`（沒有預設相依，排在解鎖之前）每次開機都執行 `/usr/libexec/trustiot/trustiot-data-setup`：

1. 找出分割區：root 檔案系統所在磁碟的第 3 個分割區，起點與大小要和映像的相同；不以標籤或 UUID 找，所以插著的另一個磁碟不會被格式化。
2. 依內容決定：
   - 認不出格式、整個分割區全是零（只在第一次開機讀一次）：第一次開機，加密（下一步）。
   - LUKS2、UUID 是釘選的 `9da7f680-5912-4d5c-bca6-d46c83e064f8`、標籤 `trustiot-data-new`：上次的加密沒做完（那時還沒有任何資料），從頭再做；標頭帶著回產線的標記時，把它帶到再做的標頭裡。`cryptsetup` 讀到的標籤也必須是 `trustiot-data-new`（`blkid` 讀第一份 metadata，cryptsetup 讀序號較大的那份），讀不出標頭有沒有標記時也一樣拒絕，不重做；兩者都當成讀不出標記，寫 `/run/trustiot-back-to-factory`（「斷電時」）。
   - 同一個 UUID、標籤 `trustiot-data`：已經加密好。確認 keyslot 只有 TPM 的一個、釘選的 UUID 只在這個分割區上（第 4 步的確認，在這裡先做一次），再看 TPM 解不解得開：解得開就照常解鎖；解不開時重試，暫時解不開就不動它（fail closed），永遠解不開才清除重建（「TPM 永久解不開時」）。
   - 其他任何內容（別的檔案系統、別的 LUKS、認不出但不是零）：拒絕，不格式化。unit 失敗，解鎖因此不執行，分割區不掛上，agent 不啟動。
3. 加密：需要 `/dev/tpmrm0`，沒有 TPM 就拒絕（分割區維持全零，下一次有 TPM 的開機才加密）；分割區正被使用（`/sys/class/block/<分割區>/holders` 不是空的，例如 setup 在開好機的閘道器上又執行一次）時也拒絕。以 `/dev/random` 的 48 bytes 做暫時金鑰，只放在 `/run`（記憶體）；先讀這顆 TPM 的 owner hierarchy 的指紋（「TPM 永久解不開時」），讀不到就拒絕；0x81000001 已經有一把 key、而它不是這個 owner hierarchy 的標準 SRK（name 不等於指紋）或讀不到它時，也拒絕：systemd-cryptenroll 以 0x81000001 現有的 key 密封 volume key（那裡沒有東西時才放一把標準的），而那把 key 可能是別人放進去、他自己知道私密部分的（開機測試的 34）；清除重建時，這時的指紋還必須等於判定時看到的那個；`cryptsetup luksFormat --header` 把標頭做在 `/run` 的檔案裡（LUKS2、aes-xts-plain64、512 位元金鑰、4096 bytes 的 sector、釘選的 UUID、標籤 `trustiot-data-new`、keyslot 0；資料從 16 MiB 開始，前面是兩份 16 KiB 的 metadata 與 keyslot 區，也就是 cryptsetup 對這個 volume 的預設，明確寫出；這把金鑰的 KDF 用 pbkdf2 1000 次，因為它在幾秒後就被清掉，argon2 在 2 GB 的閘道器上只多花記憶體與時間），再以 `cryptsetup token import` 把 setup 自己的 token 放進那個檔案：指紋，清除重建時還有回產線的標記。確認那個檔案正好 16 MiB、`blkid` 讀得出釘選的 UUID 與標籤 `trustiot-data-new` 之後，才以 `dd` 寫到分割區：先寫兩份 metadata（32 KiB），寫到裝置上（fsync）之後再寫 keyslot 區；兩個 `dd` 都以 `count` 限定範圍，不會寫過 16 MiB。分割區不會先被清成零，`luksFormat` 也不直接寫分割區，原因見「斷電時」。`systemd-cryptenroll --tpm2-device=auto --tpm2-pcrs= --tpm2-public-key= --tpm2-pcrlock= --tpm2-with-pin=no`：把 volume key 密封在 TPM 裡，明確不綁 PCR（systemd 預設綁 PCR 7）、不要 PIN，用 systemd 放在 0x81000001 的 SRK，不碰閘道器身分金鑰的 0x81000100；登錄之後再確認 0x81000001 那把的 name 等於指紋，不等就拒絕（那時標籤還是 `trustiot-data-new`，volume 沒有用過，下一次開機從頭再做、再確認）。接著以 TPM token 開啟（證明 TPM 解得開才往下）、做 ext4、關閉；`cryptsetup luksKillSlot` 清掉暫時金鑰的 keyslot 0，確認只剩 TPM 的 keyslot，最後把標籤改成 `trustiot-data`。
4. 每次開機最後確認釘選的 UUID 只出現在這個分割區上，`/dev/disk/by-uuid/` 也指向它：解鎖以這個 UUID 找 volume，另一個磁碟若帶著同樣的 UUID 就拒絕。這只在 setup 執行的當下確認，之後才接上的磁碟不在範圍內；能在開機時接上磁碟的人拿得到閘道器本體，本來就能改 root（「檢查證明什麼、不證明什麼」）。最後依標頭有沒有回產線的標記，寫入或刪掉讓 agent 不啟動的檔案；讀不出有沒有標記時也寫入（「TPM 永久解不開時」）。

LUKS 標頭因此只有一個 keyslot 與一個 `systemd-tpm2` token（`tpm2-pcrs` 是空的），`systemd-cryptenroll <分割區>` 只列出 `tpm2`；另外有 setup 自己的 token 1（指紋），清除重建過的還有 token 2（標記），兩者都沒有 keyslot。開機測試以 `cryptsetup luksDump --dump-json-metadata` 檢查（`src/data.ts` 的 `luksHeaderProblems`）。每次開機的確認只看 keyslot 的種類，也就是沒有密碼 keyslot；token 的數量、PCR 與 PIN 不在每次開機確認：能改標頭的人（拿到卡片，或已經是閘道器上的 root）本來就能改 root 檔案系統（「檢查證明什麼、不證明什麼」），而綁了 PCR 或要 PIN 的 token 在 `headless=true` 下只會解不開（fail closed）。

### 解鎖

- `/etc/crypttab`：`trustiot_data UUID=9da7f680-5912-4d5c-bca6-d46c83e064f8 none luks,tpm2-device=auto,headless=true,nofail,x-systemd.device-timeout=15min`。以 LUKS 標頭的 UUID 指定：第一次開機 `set_partuuid` 換掉的 PARTUUID 不影響它，映像裡的 crypttab 也不必在第一次開機後改寫。這個 UUID 每台閘道器都一樣，不是祕密。
- 等 volume 出現的時間：systemd 從開機起就等 volume（以 UUID）與解鎖後的 `/dev/mapper/trustiot_data` 出現，`nofail` 的項目預設只等 90 秒。第一次開機的 setup 要讀完整個分割區再加密，在慢的卡上可能超過 90 秒，那次開機 volume 就不會解鎖、agent 不會啟動（雖然 setup 成功了）。crypttab 與 fstab 因此都設 `x-systemd.device-timeout=15min`：比 setup 自己的上限（10 分鐘）加上等 TPM 的 90 秒長，仍是有限的。generator 把它變成那兩個 device 的 `JobRunningTimeoutSec=`，不傳給 `systemd-cryptsetup`。開機測試以限速的磁碟做第一次開機，setup 在開機後兩分鐘以上才完成，同一次開機仍解鎖、掛上。
- systemd 的 `systemd-cryptsetup-generator` 由它產生 `systemd-cryptsetup@trustiot_data.service`；drop-in（`files/data/systemd-cryptsetup-trustiot-data.conf`）讓它 `Requires=`/`After=` setup、`After=` TPM 的裝置，`TimeoutStartSec=2min` 是上限，`LimitCORE=0`。`/etc/fstab` 從 `/dev/mapper/trustiot_data` 掛上。
- 不綁 PCR：更新核心、韌體或 `config.txt` 都不影響解鎖。代價：拿到閘道器本體、能改 root 或 `cmdline.txt` 的人，可以讓它在下一次開機時解鎖後讀取資料；防這一點要 secure boot 加上 PCR 的綁定（Laysi/trustiot-v2#216）。本 issue 擋的是只拿到 SD 卡的人：volume key 只有這顆 TPM 解得開，TPM 被清除（`TPM2_Clear`）後也解不開。
- 解不開時資料不救：沒有 recovery key。暫時解不開時不動它（下一節），永遠解不開時清除重建、閘道器回產線重新登錄（「TPM 永久解不開時」）。

### 解不開時（fail closed）

- `headless=true`：`systemd-cryptsetup` 絕不詢問密碼，volume 也沒有密碼 keyslot 可以退回。TPM 不在時，setup 先等 `/dev/tpmrm0` 出現（systemd 預設等 device 的 90 秒），再每 10 秒看一次、看 180 秒（「TPM 永久解不開時」），`systemd-cryptsetup` 再等 TPM 30 秒（crypttab 預設的 `token-timeout=`），等不到就失敗（開機測試的日誌：`Timed out waiting for security device` 後 `No passphrase or recovery key registered.`）；TPM 在、卻是另一個 owner hierarchy 而無法判定（例如沒有記下指紋的舊 volume）時解封失敗（`Failed to unseal secret using TPM2: State not recoverable`），一秒內結束；`TimeoutStartSec=2min` 是上限。
- `nofail`（crypttab 與 fstab 都有）：開機照常完成，ssh 可以登入診斷；資料分割區不掛上。
- agent 不啟動：drop-in `files/data/trustiot-agent-data.conf` 給 agent 的 unit `RequiresMountsFor=/var/lib/trustiot-agent`（unit 的 `StateDirectory=` 本來就隱含它）與 `AssertPathIsMountPoint=/var/lib/trustiot-agent`：掛載失敗時 agent 的啟動因相依失敗而取消；就算有人手動啟動，掛載點不是掛載點時 assert 失敗，agent 不會把資料寫到 root。
- 這裡只做到「不掛上、不啟動」，不清除也不重新格式化；資料還在，TPM 恢復後的下一次開機照常解鎖。清除重建只在下一節的條件下發生。

### TPM 永久解不開時

Laysi/trustiot-v2#867。TPM 解不開 volume 有兩種：

- **暫時**：TPM 還沒出現（驅動還沒載入）、暫時沒有回應、指令逾時。等一下、重開機或修好接線就可能解得開，資料還在。
- **永久**：volume key 密封在 systemd 放在 0x81000001 的 SRK 之下，而 SRK 是 TPM 的 owner hierarchy 由它的 seed 衍生的 primary key。seed 換了，就再也做不出那把 SRK，任何 TPM 都解不開這個 volume：換了一顆 TPM（每顆 TPM 有自己的 seed），或這顆 TPM 被清除（`TPM2_Clear` 換掉 owner seed）。卡片從別台閘道器拿來的也一樣。

TPM 或主機板更換視同新設備，回產線重新登錄（[決策 12](../../docs/architecture/decisions.md)）；TPM 被清除時，閘道器身分金鑰（0x81000100，同一個 owner hierarchy）也不在了，同樣要重新登錄。所以永久解不開、而且這顆 TPM 沒有任何金鑰（新的 TPM，或被清除過）時，setup 清除資料分割區、以這顆 TPM 重建（資料本來就救不回），在新的標頭留下回產線重新登錄的標記，agent 在標記移除之前不啟動。這顆 TPM 有金鑰時，它多半是另一台已登錄閘道器的 TPM，也就是這張卡從別台拿來：原來那台還解得開，所以不清除（「判定依據：TPM 裡有沒有金鑰」）。暫時解不開時照上一節不動它。分不清是哪一種時，一律當成暫時，不清除。

#### 判定依據：owner hierarchy 的指紋

setup 每次做標頭（第一次開機、重新開始、清除重建）都在標頭記下這顆 TPM 的 owner hierarchy 的指紋：以 TCG 標準的 ECC storage key 範本（systemd-cryptenroll 放在 0x81000001 的 SRK 用的範本）在 owner hierarchy 做一把暫時的 primary key，取它的 name（SHA-256，`000b` 開頭的 34 bytes，以 16 進位寫）：

```sh
tpm2_createprimary --hierarchy=o --hash-algorithm=sha256 --key-algorithm=ecc256:aes128cfb \
  --attributes='fixedtpm|fixedparent|sensitivedataorigin|userwithauth|noda|restricted|decrypt' --key-context=<檔案>
tpm2_readpublic --object-context=<檔案> --name=<name 檔>
```

primary key 只由 hierarchy 的 seed 與範本決定，所以同一顆 TPM、seed 沒變時永遠是同一個 name；換了 TPM 或 `TPM2_Clear` 之後就不同。這把 key 是暫時的：resource manager 在指令結束時清掉它，TPM 裡不多留任何東西。指紋放在 token 1（type `trustiot-tpm-owner`，`{"type":"trustiot-tpm-owner","keyslots":[],"owner":"000b…"}`）：沒有 keyslot，cryptsetup 與 systemd-cryptsetup 不認得這個 type，不拿它解鎖；systemd 的 token 是 0。

2026-11-06 在建置主機的拋棄式容器裡，以 swtpm、映像同版的 tpm2-tools 5.7、cryptsetup 2.7.5 與 systemd-cryptenroll 257 實測：

- 同一顆 TPM 重複做出的指紋相同，而且等於 systemd-cryptenroll 放在 0x81000001 的 SRK 的 name。
- 移除 0x81000001 的 SRK 之後、TPM 在 dictionary attack lockout 時（`inLockout: 1`），指紋照樣做得出來，而且不變。
- 另一顆 TPM、同一顆 TPM 在 `TPM2_Clear` 之後，指紋不同。
- owner hierarchy 設了密碼時做不出來（`0x9A2`，認證失敗）。沒有 TPM 時 tpm2-tools 結束碼 1。
- `cryptsetup token import` 把兩個 token 放進 `/run` 上的標頭檔，id 不變，之後的 systemd-cryptenroll 取 token 0。

容器裡的 swtpm 沒有 `/dev/tpmrm0`，所以 `opens` 的那個指令（經 systemd 的 TPM token）只在開機測試的虛擬機裡驗證。開機測試在虛擬機裡也再比一次指紋：每次解鎖的開機，標頭記的指紋都要等於 0x81000001 那把 SRK 的 name（探針的 `srk-name`）。

#### 判定依據：TPM 裡有沒有金鑰

指紋只說得出「這顆 TPM 不是密封 volume 的那顆」，說不出是哪一邊換了：這台閘道器換了 TPM 或 TPM 被清除（該清除），還是這張卡從另一台閘道器拿來（原來那台還解得開，不該清除）。所以指紋不同時，setup 再看 TPM 的持久 handle（`tpm2_getcap handles-persistent`）：

- 產線工作站把閘道器的身分金鑰放在 owner 與 endorsement 範圍的持久 handle（`0x81000000` 到 `0x817fffff`，不含 SRK 的 `0x81000001`，預設 `0x81000100`；[`factory/workstation`](../../factory/workstation/README.md) 的 `--handle`）。agent 在閘道器上只使用這把金鑰，不建立它（[`edge/agent`](../agent/README.md)「金鑰佈建（產線工具用）」）。
- `TPM2_Clear` 清掉 owner 與 endorsement hierarchy 的所有持久物件，新的 TPM 一般也沒有。

所以這個範圍裡有這顆 TPM 的標準 SRK 以外的物件時（回答 `keyed`），這顆 TPM 不是新的、也沒被清除過，多半是另一台已登錄閘道器的：不清除，照上一節解不開、不掛上，日誌多寫一行 warning：`this TPM holds keys (<handle>) but is not the one the volume was sealed under: most likely another gateway's TPM, so this card is not this gateway's own`。範圍裡只有這顆 TPM 自己的標準 SRK、或什麼都沒有，才可能清除。這樣分：

- 0x81000001 上這個 owner hierarchy 的標準 SRK 不算（它的 name 就是這顆 TPM 的指紋）：systemd 為任何 TPM 都放一把，清除重建時這台的 systemd-cryptenroll 也會放。0x81000001 上是別的 key 時算（`keyed 0x81000001`）：清除重建會把新的 volume key 密封在它之下，所以「何時加密」第 3 步也拒絕在它之下加密；0x81000001 有東西卻讀不到它的 name 時是 `unknown-keys`。
- platform 範圍（`0x81800000` 以上）不算：那是平台韌體放的，`TPM2_Clear` 不清它，說不出 TPM 有沒有登錄過。
- 列出的任何一行不是 `- 0x` 加 8 個 16 進位字元，就當成讀不到（`unknown-keys`），不清除。
- TPM 出廠就放了 EK（例如 `0x81010001`）時也算有金鑰：換上這種 TPM 的閘道器不清除，只是解不開，由產線處理（「產線要做什麼」）。這是分不清時不清除的那一邊。

開機測試的 14 以另一台已登錄閘道器那樣的 TPM（0x81000001 的 SRK 加 0x81000100 的金鑰）開機，不清除；15 的 TPM A 在清除之前也先放了 0x81000100 的金鑰，清除之後 `tpm2_getcap handles-persistent` 什麼都沒有，setup 照樣清除重建。34 以 0x81000001 放了 RSA storage key 的新 TPM 做第一次開機，setup 拒絕加密，分割區仍全是零。0x81000001 上是別的 key 時的 `keyed 0x81000001` 只有單元測試（`tests/data-setup.test.ts`）。

#### 每一次看 TPM 的回答

已經加密好（標籤 `trustiot-data`）的 volume，setup 每次開機先看一次 TPM（`observe`），依序判斷，第一個成立的就是回答。每個 TPM 指令最多 60 秒（`timeout`）：

| 回答 | 條件 | 會不會清除 |
| --- | --- | --- |
| `opens` | `cryptsetup open --test-passphrase --token-only --token-type systemd-tpm2` 成功：TPM 解得開（不建立 mapping） | 不會，照常解鎖 |
| `absent` | 沒有 `/dev/tpmrm0` | 不會 |
| `silent` | `tpm2_getcap properties-variable` 失敗或逾時：TPM 沒有回應 | 不會 |
| `unrecorded` | 標頭沒有指紋，或格式不對（這個變更之前做的 volume，或被改過的標頭） | 不會：無法證明解不開是永久的 |
| `unknown-owner` | 做不出指紋（例如 owner hierarchy 有密碼） | 不會 |
| `same-owner` | 指紋和記下的相同；或指紋不同，但 0x81000001 那把 SRK 的 name 和記下的相同（範本做出的 key 變了，例如工具更新，以 SRK 為準） | 不會：同一個 owner hierarchy，不是換了 TPM |
| `unknown-keys` | 指紋和記下的不同，0x81000001 也不是記下的那把，但 `tpm2_getcap handles-persistent` 失敗、逾時或列出看不懂的內容，或 0x81000001 有東西卻讀不到它的 name | 不會 |
| `keyed <handle,…>` | 指紋和記下的不同，0x81000001 也不是記下的那把，而 TPM 在 `0x81000000` 到 `0x817fffff` 有這顆 TPM 的標準 SRK 以外的持久物件（0x81000001 上的別的 key 也算） | 不會：多半是另一台已登錄閘道器的 TPM，這張卡是從那裡拿來的 |
| `other-owner <指紋>` | 指紋和記下的不同，0x81000001 也不是記下的那把，TPM 在那個範圍沒有標準 SRK 以外的持久物件：新的 TPM，或這顆被清除過 | 只有這個可能導致清除 |

#### 重試的條件

- 第一次就是 `opens`：照常。每次正常開機因此多一次 TPM 解封。
- 否則每 10 秒再看一次，最後一次最晚在第一次之後 180 秒開始。每一次都寫進 setup 的日誌：`the TPM (look <第幾次>, <第一次之後幾秒> s): <回答>`。
- 任何一次是 `opens`：停止，照常解鎖（TPM 晚出現時就是這樣）。
- 連續 3 次都是 `other-owner`、而且是同一個指紋：永久解不開，清除重建（下一段）。中間有任何別的回答或別的指紋，就從頭數。
- 180 秒內兩者都沒發生：不清除。日誌寫 `the TPM did not open <分割區> within 180 s (last: <最後的回答>); not wiping it: it stays locked`（warning；最後的回答是 `keyed` 時再加上一節那行），setup 成功結束，接著的解鎖照上一節失敗。下一次開機再從頭看。
- setup 開始之前，systemd 已經等 `/dev/tpmrm0` 最多 90 秒，所以沒有 TPM 時，資料分割區的解鎖最多晚約 90 + 180 秒才失敗。開機的其他部分不等它（crypttab 與 fstab 都是 `nofail`），ssh 照常可以登入。setup 的上限 `TimeoutStartSec=10min` 仍涵蓋：清除重建最晚在開始看之後 180 秒內決定，接著的加密不讀整個分割區（那只在第一次開機做）。

數值釘在 `src/pins.ts` 的 `DATA_CHECK`，單元測試比對 setup 的常數。

#### 清除重建

1. 判定之前，setup 已確認這是映像的 volume（釘選的 UUID、標籤 `trustiot-data`）、只有 TPM 的 keyslot、UUID 只在這個分割區上（「何時加密」的第 2 步）；任何一項不符就照原本拒絕，不看 TPM、不清除。
2. 日誌寫兩行 warning：`the TPM's owner hierarchy is <新的指紋>, the volume was sealed under <記下的指紋>: another TPM, or this one cleared; no TPM can open it again`，以及 `wiping <分割區> and making it anew for this TPM; the gateway goes back to the factory to be registered again`。
3. `/dev/mapper/trustiot_data` 已經開著時（setup 在開好機的閘道器上又執行一次）拒絕，不清除。以第一次開機的同一套步驟重建（「何時加密」的第 3 步，含它的拒絕條件：做標頭之前再做一次指紋，必須等於判定時看到的那個，0x81000001 也必須是空的或這顆 TPM 的標準 SRK；不符就拒絕，舊的 volume 原封不動）：新的暫時金鑰、新的 `trustiot-data-new` 標頭、這顆 TPM 的指紋，加上 token 2（type `trustiot-back-to-factory`）：`{"type":"trustiot-back-to-factory","keyslots":[],"reason":"tpm-owner-changed","sealed_owner":"<記下的指紋>","found_owner":"<這顆 TPM 的指紋>"}`，沒有 keyslot。兩個 token 在標頭寫到分割區之前就放進 `/run` 裡的標頭檔，所以標頭第一次寫到分割區就帶著標記。新的標頭蓋掉舊的 keyslot 區：舊的 volume key 不再以任何形式存在（本來也只有那個已經不在的 owner hierarchy 解得開）；舊的密文留在分割區上但無法解密，新的檔案系統是空的。
4. 之後與第一次開機相同：登錄這顆 TPM、做 ext4、清掉暫時金鑰的 keyslot、改標籤。同一次開機就以這顆 TPM 解鎖、掛上。

斷電時：新標頭第一次寫到分割區之前斷電，分割區還是舊的 volume，下一次開機再看 TPM、再清除；之後斷電，分割區是帶著標記的 `trustiot-data-new` 標頭，下一次開機重新開始的 setup 把標記帶到再做的標頭裡（「斷電時」）。

#### 標記與 agent

- 每次開機最後（不論這次有沒有清除），setup 讀標頭的 token 2。有標記就在日誌寫 warning：`<分割區> was wiped (tpm-owner-changed) and carries the mark to go back to the factory: the agent stays off until the factory registers the gateway again and removes the mark`，並寫 `/run/trustiot-back-to-factory`（root、0644，`reason=`、`sealed_owner=`、`found_owner=` 三行）；沒有標記就刪掉這個檔案。`/run` 在記憶體裡，每次開機都從標頭重新決定。setup 從 `cryptsetup luksDump` 列出的 token 判斷有沒有標記，所以「沒有標記」是讀過標頭之後的結論：讀不出標頭（`luksDump` 失敗），或列著標記卻匯出不了時，當成有標記，寫這個檔案（三個欄位都是 `unknown`），setup 失敗，agent 不啟動；重新開始的 setup 讀不出標記、或 `blkid` 與 cryptsetup 讀到的標籤不同時也一樣（「何時加密」第 2 步）。
- agent 的 drop-in 有 `ConditionPathExists=!/run/trustiot-back-to-factory`：檔案存在時 agent 不啟動（condition 不成立，unit 跳過、不算失敗，手動 `systemctl start` 也一樣），資料分割區照常掛上。用 condition 而不是 assert：有標記是預期的狀態，不是錯誤。
- 看得出來的地方：`journalctl -u trustiot-data-setup` 的 warning、`/run/trustiot-back-to-factory`、`cryptsetup token export --token-id 2 <分割區>`，以及 `systemctl status trustiot-agent` 的 `ConditionPathExists=!/run/trustiot-back-to-factory was not met`。
- 標頭沒有簽章，能寫分割區的人就能加上或移除標記，所以標記只是告知，不是授權：setup 不依它做任何清除，產線也不憑它登錄（「產線要做什麼」）。移除標記只讓 agent 啟動：清除重建只發生在 TPM 沒有金鑰時，那台閘道器沒有身分金鑰，平台不接受它，直到產線重新登錄。從標頭讀到的欄位只接受固定的格式（指紋是 `000b` 加 64 個 16 進位小寫字元，reason 是小寫字母與 `-`），不符就寫成 `unknown`，日誌與 `/run` 的檔案不會出現標頭裡的任意內容。

#### 現場要做什麼

- 平台上看到閘道器離線，ssh 登入後看到上面的 warning 或 `/run/trustiot-back-to-factory`：資料救不回來，不要嘗試；不要自己移除標記。把閘道器帶回產線。
- 日誌是 `not wiping it: it stays locked`（暫時解不開）：資料還在。重開機再看一次，或檢查 TPM 模組；仍解不開就帶回產線，不要清除分割區。
- 日誌還有 `most likely another gateway's TPM`：這張卡多半是另一台閘道器的，資料還在、原來那台解得開。把卡放回它自己的閘道器；這台閘道器換一張新燒錄的卡（舊卡不重用，見[重建清單](../../docs/requirements/rebuild-checklist.md) D4）。

#### 產線要做什麼

1. 確認標記：`cryptsetup token export --token-id 2 /dev/disk/by-uuid/9da7f680-5912-4d5c-bca6-d46c83e064f8`（root），記下 `sealed_owner` 與 `found_owner`。
2. 視同新設備重新登錄（產線工作站，[`factory/workstation`](../../factory/workstation/README.md)）。不能只憑標記：它是閘道器自己寫的、沒有簽章。工作站以 TPM 的 endorsement key（EK）分辨：EK 和這個硬體 ID 的紀錄相同、卻沒有金鑰，是同一顆 TPM 被清除過（工作站的「TPM 被清除過」，要 `--allow-new-key`）；EK 不同就是另一顆 TPM（換了 TPM 或主機板），依決策 12 當新設備登錄。
3. 登錄完成後移除標記並重開機：`cryptsetup token remove --token-id 2 /dev/disk/by-uuid/9da7f680-5912-4d5c-bca6-d46c83e064f8`（root）。之後的開機 setup 不再寫 `/run/trustiot-back-to-factory`，agent 啟動（開機測試的 18）。工作站還沒有這一步，接上它是後續的工作。

產線自己換了 TPM 或清除 TPM 時，先讓閘道器開機一次（setup 在這次開機清除重建、留下標記），再登錄：登錄把金鑰放進 TPM 之後，setup 就把它當成另一台閘道器的 TPM，不再清除，舊的 volume 只會一直解不開。已經這樣時（日誌是 `keyed`），把卡整張清成零再燒錄（「燒錄與第一次開機」），第一次開機以這顆 TPM 加密。TPM 出廠就帶著 EK 的持久物件時也一樣。

TPM 的 0x81000001 出廠就放了別的 key（例如 RSA 的 SRK）時，第一次開機 setup 拒絕加密（日誌 `0x81000001 holds a key that is not this TPM's standard storage key`），資料分割區維持全是零、agent 不啟動：以 `tpm2_clear` 清除 TPM（還沒登錄，沒有東西會丟）之後再開機。

#### 濫用與限制

- 清除只在這台閘道器的 TPM 永遠解不開這個 volume、而且這顆 TPM 沒有金鑰時發生。做得到這一點的人（換掉或清掉 TPM、把卡插到別台、改寫標頭）都拿到了閘道器本體或卡片，或已經是 root，本來就能直接改寫卡片毀掉資料，所以清除不給他多做任何事。清除重建也不會密封在他放進 TPM 的 key 之下：0x81000001 不是這顆 TPM 的標準 SRK 時回答是 `keyed`、不清除；判定之後才換上的，做標頭之前與登錄之後各再確認一次（「何時加密」第 3 步）。
- 換上自己的 TPM 的人，得到的是一個密封給他那顆 TPM 的空 volume，agent 不啟動；產線要以 EK 確認，不能把他的 TPM 當成原本那台登錄。
- agent 以 `tss` 群組使用 TPM（`edge/agent/systemd/trustiot-agent.service` 的 `SupplementaryGroups=tss`）。lockout hierarchy 沒有設密碼時，被入侵的 agent 可以 `TPM2_Clear`，下一次開機 setup 就清除資料分割區。這不比清除本身多造成什麼：`TPM2_Clear` 已經讓資料永遠解不開，也已經刪掉閘道器身分金鑰。platform hierarchy 也能 `TPM2_Clear`：開機鏈裡沒有韌體設定 platform hierarchy 的授權或把它關掉（`phEnable`）時，它的授權是空的，只為 lockout hierarchy 設密碼擋不住；Raspberry Pi 平台的開機韌體一般不處理 TPM，閘道器實機上未確認。產線是否為 lockout hierarchy 設密碼、並設定或關掉 platform hierarchy，是另外的決定。
- 卡片從另一台閘道器拿來、沒有先清成零（「燒錄與第一次開機」）：這台已登錄（TPM 裡有金鑰）時不清除，卡片照樣解不開、不掛上，放回原來那台就解得開（開機測試的 14）。這台的 TPM 是新的或被清除過時，setup 分不出這和「這台換了 TPM」：兩者的 TPM 一樣是另一個 owner hierarchy、一樣沒有金鑰，標頭裡也沒有記下是哪一塊主機板。這時它照「換了 TPM」清除重建並標記回產線，原來那台還解得開的資料就沒了。卡片不在閘道器之間移動（[重建清單](../../docs/requirements/rebuild-checklist.md) D4），而且這台閘道器本來就要回產線登錄，所以接受；要分得出來，標頭得再記下主機板的序號，這要在閘道器上確認序號從哪裡讀（Laysi/trustiot-v2#811），沒有在這裡做。
- 想讓 TPM 永久解不開時不清除的人，只要在 TPM 放一個持久物件：這需要 TPM 的 owner 授權（沒設密碼時，root 或 `tss` 群組就有），結果是 volume 照上一節解不開、不掛上（fail closed），不是資料外洩。
- 沒有記下指紋的 volume（這個變更之前加密的）永遠不會被清除，只會照上一節解不開、不掛上；setup 不替它補記指紋，因為補記的那一刻無法證明 TPM 就是當初密封的那顆。
- 以檔案金鑰執行 agent 的閘道器（`identity.key` 的 `source = "file"`，只給開發與測試）TPM 裡沒有金鑰：卡片移到這種閘道器時，setup 照「換了 TPM」清除重建。
- 只看 owner hierarchy：TPM 的韌體更新若換掉 seed（一般不會），結果與 `TPM2_Clear` 相同。

### 斷電時

第一次開機的 setup 在任何一步之後斷電，下一次開機看到的分割區都必須是 setup 能安全重新開始的兩種之一：全是零（還沒寫），或釘選 UUID、標籤 `trustiot-data-new` 的 LUKS2 標頭（上次沒做完）。重新開始的 setup 再斷電也一樣。「認不出格式但不是零就拒絕」的規則不放寬：它擋的是 setup 自己沒留下的內容（例如只寫非零區塊的燒錄工具留下的舊資料），寧可不掛上也不格式化。

**原本的問題（Laysi/trustiot-v2#917，2026-11-02 修正）**：setup 原本先以 `dd` 把開頭 16 MiB 清成零，再在分割區上 `luksFormat`。上一次的 setup 已經做過 `mke2fs` 時，分割區後段有它的密文；重新開始的 setup 清掉標頭之後斷電，分割區就是「開頭是零、後段不是零」，之後每次開機都被拒絕，要有人清掉分割區或回工廠才能恢復（fail closed，沒有機密性的影響）。`luksFormat` 自己也有同樣的時段：cryptsetup 2.7.5 在裝置上 `luksFormat` 時，先把整個 16 MiB 寫成零並 fsync，再把 keyslot 區（32 KiB 之後）寫成亂數並 fsync，最後才寫標頭（2026-11-02 以 strace 觀察，見[測試報告](../../docs/test-reports/2026-11-02-917-data-setup-power-cut.md)）。所以 issue 的推論成立：第一次開機若在 `luksFormat` 裡、標頭寫好之前斷電，分割區是「標頭的位置是零、keyslot 區是亂數」，同樣每次被拒絕。

考慮過的做法：

| 做法 | 內容 | 評估 |
| --- | --- | --- |
| A. 先清資料區 | 重新開始時保留 `trustiot-data-new` 標頭，先把標頭後面的資料區（2032 MiB）清成零並 fsync，再清標頭、`luksFormat` | 修好重新開始時的那一步，但 `luksFormat` 自己的時段還在（第一次開機與重新開始都是）。每次重新開始多寫 2032 MiB：SD 卡以 Class 10 保證的最低循序寫入 10 MB/s 估約 3.5 分鐘（setup 的上限是 10 分鐘；實機的寫入速度在 Laysi/trustiot-v2#811 量測） |
| B. 紀錄 | 破壞標頭之前，把「這個分割區由 setup 重新開始」寫到 root 並 fsync（綁定分割區與資料區開頭的雜湊）；下一次開機看到紀錄，就接受認不出的內容 | 不多寫資料，但未加密 root 上的一個檔案成了「可以格式化認不出內容」的授權：紀錄過期、從別的卡複製來、或比對的範圍不夠時，setup 會格式化它沒有證明是自己留下的內容，正是那條規則要擋的。setup 還得等 root 以讀寫掛上（它現在沒有預設相依），斷電時多一個要保證寫入順序的檔案系統 |
| **C. 標頭先做在記憶體（採用）** | `luksFormat --header` 把標頭做在 `/run` 的檔案（資料裝置是同樣大小的稀疏檔，`luksFormat` 不寫它），確認之後以 `dd` 寫到分割區：先 32 KiB 的兩份 metadata（fsync），再 keyslot 區（fsync） | 分割區上原本的內容（全是零，或舊的 `trustiot-data-new` 標頭）一直留到新的 `trustiot-data-new` 標頭蓋上去為止，任何一步之後都不會出現被拒絕的狀態，包含 `luksFormat` 的那一段（現在只寫 `/run`）。`main()` 的判斷一個字都沒改，也不需要紀錄。寫到分割區的只有 16 MiB 一次（原本是 16 MiB 零、`luksFormat` 再寫 16 MiB 零與約 16 MiB 亂數），所以重新開始不多花時間（測試報告的「重新開始多花的時間」）。代價：setup 進行時 `/run` 多放一個 16 MiB 的檔案（有暫時金鑰的 keyslot，與金鑰檔放在同一個 0700 目錄、寫完就刪） |

採用 C：它是唯一同時修好兩個時段、又不改「什麼內容可以格式化」的做法，也不多花時間。

C 沒有涵蓋的（一步做到一半時斷電、卡片遺失或重排寫入，延後在 Laysi/trustiot-v2#918）：

- 第一次開機寫那 32 KiB 時斷電，而卡片只寫進了其中一部分：結果取決於哪些 bytes 寫進了。`blkid` 讀的 magic、UUID、標籤都在主標頭與第二份標頭各自的第一個 512 bytes 裡；從其中一份讀得到釘選的 UUID 與標籤 `trustiot-data-new`，setup 就重新開始。兩份都讀不到（例如都沒寫進，而後面的部分寫進了；卡片也不保證一個 sector 整個寫進或整個沒寫，可能 magic 寫進了、UUID 或標籤還沒），或讀到的 UUID、標籤不對時，分割區會被拒絕。重新開始時沒有這個問題：新舊標頭的 magic、UUID 與標籤相同，不論寫進哪幾個 sector、一個 sector 寫進多少，`blkid` 讀到的都是 `trustiot-data-new`。
- 卡片自己的快取不理會 fsync，或寫入的順序被重排。
- 遇到時仍是 fail closed：不解鎖、不掛上、agent 不啟動，ssh 可以登入。「TPM 永久解不開時」的清除只處理加密好的 volume（標籤 `trustiot-data`、記下了指紋），不處理這些狀態。

清除重建（「TPM 永久解不開時」）用的是同一套步驟，所以它在每一步之後斷電的結果與上面相同，只多一件事：標記不能丟。標頭在 `/run` 的檔案裡就帶著標記，分割區上的舊 volume 一直留到新標頭第一次寫上去為止，分界就在那裡（寫到一半時斷電見最後一點）：

- 之前斷電（例如 `luksFormat` 之後）：分割區還是舊的 volume、沒有標記，下一次開機再看 TPM，同樣連續 3 次看到另一個 owner hierarchy，再清除一次。
- 之後斷電：分割區是帶著標記的 `trustiot-data-new` 標頭，下一次開機的 setup 重新開始，把標頭裡的標記（只取驗證過格式的欄位）放進再做的標頭；改完標籤之後斷電，下一次開機照常以這顆 TPM 解鎖，標記還在。
- 寫兩份 metadata 寫到一半時斷電（Laysi/trustiot-v2#918 的範圍，開機測試不模擬）：`blkid` 讀第一份，cryptsetup 讀序號較大的那份，兩者可能不同。`blkid` 讀到 `trustiot-data-new`、cryptsetup 卻讀到別的標籤時，cryptsetup 讀的可能是被取代的舊 volume，看不到標記，所以 setup 拒絕、不重做；讀不出有沒有標記時也拒絕。兩者都是 fail closed（另外寫 `/run/trustiot-back-to-factory`，欄位是 `unknown`），標記不會在重做時遺失。

開機測試在每一步之後斷電，第一次開機與重新開始的 setup 都測（「開機測試」的 19 到 29），清除重建則在上面三個點斷電（31 到 33）。

### 其他決定

- 密碼學：aes-xts-plain64、512 位元金鑰（AES-256）、4096 bytes 的 sector，cryptsetup 2.7 的預設。閘道器的 CPU 沒有 ARMv8 的 AES 指令，[TPM 可行性測試](../../docs/feasibility/2026-10-12-tpm-poc.md)在 lab 的閘道器上量到 `cryptsetup benchmark` 的 aes-xts（512 位元）約 83 MiB/s、Adiantum 約 240 MiB/s。選 AES-XTS：83 MiB/s 已經比 SD 卡的讀寫快，agent 的寫入量（遙測緩衝與設定）只用到其中很小的一部分；AES-XTS 是標準的磁碟加密模式（NIST SP 800-38E），稽核與客戶問到時不必另外解釋。XTS 沒有完整性保護：改動密文只會讓解出的資料變成亂碼，讀不到內容；不加 dm-integrity。
- 不綁 PCR 與威脅模型的實測依據在同一份可行性測試的「PCR 綁定與威脅模型」：閘道器的開機鏈不量測，PCR 永遠是 0，綁了也沒有保護效果。
- core dump：映像沒有 systemd-coredump，核心預設的 `core_pattern` 會把 core 寫進程式的工作目錄（root 分割區）。setup（暫時金鑰）、解鎖（volume key）與 agent（資料）的 unit 都有 `LimitCORE=0`。
- 不用的做法：建置時加密（每台閘道器的映像就不同、金鑰經過建置主機）；保留一把密碼或 recovery key（有人持有金鑰，還要保管與輪替；解不開時的處理是回到工廠）；`x-systemd.makefs`（掛不上時會格式化，可能清掉資料）；綁 PCR（見上）。

## 日誌與置換空間

資料分割區加密之後，其他寫到 root（未加密）的東西不能帶著它的內容：

- 系統日誌只在記憶體：`files/data/journald.conf` 裝成 `/etc/systemd/journald.conf.d/zz-trustiot.conf`，`Storage=volatile`、`ForwardToSyslog=no`（journald 依檔名排序套用所有目錄的 drop-in，systemd 自己的 `/usr/lib/systemd/journald.conf.d/syslog.conf` 設了 `ForwardToSyslog=yes`，這個檔名排在它後面；映像檢查比對的是 `systemd-analyze cat-config` 算出的有效值）；映像沒有 syslog daemon。來源映像的 `raspberrypi-sys-mods` 已經設了 `Storage=volatile`，自己的 drop-in 讓這個決定不依賴它。代價：重開機後日誌消失，事後的調查要靠遠端的日誌（另案）；資料分割區鎖住時日誌照常記在記憶體，可以 ssh 登入看。不採用把日誌放進資料分割區：分割區鎖住或還沒解鎖時正是最需要日誌的時候。
- 置換空間只用壓縮的記憶體：`files/data/rpi-swap.conf` 裝成 `/etc/rpi/swap.conf.d/60-trustiot.conf`，`Mechanism=zram`。rpi-swap 的預設（`auto`，目前等於 `zram+file`）會把閒置的分頁寫到 root 上的 `/var/swap`，那些分頁可能是 agent 從資料分割區讀出的內容。代價：沒有寫到卡上的置換空間。

## 映像內容

建置對官方映像做的事（`src/inside/build.ts`，依序）：

| 項目 | 內容 |
| --- | --- |
| 套件 | 從 snapshot.debian.org 安裝 `cryptsetup-bin` 與 `systemd-cryptsetup`（`cryptsetup`、`systemd-cryptenroll`、`systemd-cryptsetup` 與它的 TPM2 token 外掛，和映像的 systemd 同一個版本，不升級任何套件；「資料分割區加密」）、`tpm-udev`（`tss` 群組與 `/dev/tpmrm0` 的 udev 規則，agent 的 unit 以 `SupplementaryGroups=tss` 使用）與 `tpm2-tools`（帶入 tpm2-tss 函式庫，`systemd-cryptenroll` 需要；也供現場診斷），版本見「釘選」。`dpkg --audit` 必須沒有未完成的套件 |
| agent | 執行檔 `/usr/bin/trustiot-agent` 與另外三個執行檔 `/usr/libexec/trustiot-agent/allowlist`、`nameservers`、`ip-deny-check`（root 擁有、0755，目錄 root 擁有、0755；`src/pins.ts` 的 `AGENT_EXECUTABLES`），[edge/agent/systemd](../agent/systemd) 的每個檔案原樣安裝：agent 的 unit 與 sysusers 設定；agent 帳號的出口規則（規則檔、載入它的 unit、`nftables.service` 的 drop-in，安裝位置照 [edge/agent/README.md](../agent/README.md)「出口規則」）；agent 執行中規則被清掉時把它載回的檢查 unit 與每 10 秒啟動它的 timer（Laysi/trustiot-v2#878）；預設拒絕的規則檔（Laysi/trustiot-v2#879）；fieldbus 放行集合的 socket 與 service（Laysi/trustiot-v2#864）；填入預設拒絕 DNS 放行的 path unit 與 service（Laysi/trustiot-v2#879）。timer、socket 與 path unit 都沒有 `[Install]`，由 agent 的 unit 帶起，不另外設為開機啟動（「agent 的防火牆」）。`edge/agent/systemd` 多了檔案，`bun run --cwd edge/image test` 就失敗，直到 `AGENT_SYSTEMD_FILES` 列出它的安裝位置；agent 的 unit 依賴一個沒有安裝的 unit、unit 執行或規則 `include` 一個沒有安裝的路徑、該執行 `/usr/libexec/trustiot-agent` 執行檔的 unit 只在註解裡提到它（Laysi/trustiot-v2#1188）、`edge/agent/Cargo.toml` 多了 `[[bin]]` 時也失敗。`systemd-sysusers` 建立 `trustiot-agent` 使用者，agent 的 unit 設為開機啟動；在映像裡執行一次 `trustiot-agent --version`，另外三個各以一個參數執行一次（它們不接受參數，什麼都不做就以 2 結束），確認它們能在映像的 C 函式庫上執行 |
| 帳號 | 刪除官方的預設帳號 `pi`；建立 `trustiot-admin`：密碼欄是 `*`（沒有任何密碼），`~/.ssh/authorized_keys` 是建置時給的公鑰，`/etc/sudoers.d/trustiot-admin` 允許免密碼 sudo（帳號沒有密碼，sudo 無從詢問；工廠的 `--remote-sudo` 需要。正式產線的縮限在 Laysi/trustiot-v2#219）。root 的密碼欄維持官方的 `*` |
| SSH | `/etc/ssh/sshd_config.d/00-trustiot.conf`：`PermitRootLogin no`、`PasswordAuthentication no`、`KbdInteractiveAuthentication no`、`PermitEmptyPasswords no`、`AuthenticationMethods publickey`、`AllowUsers trustiot-admin`、`X11Forwarding no`。sshd 對每個關鍵字採用第一個讀到的值，這個檔案最先被讀到。刪除官方指向改名精靈的 `rename_user.conf`，啟用 `ssh.service`；主機金鑰在第一次開機產生。建置以 `sshd -G` 與對四個連線（`trustiot-admin`、`root`、`pi`、agent 的服務帳號 `trustiot-agent`）的 `sshd -T -C` 確認有效設定與 `AllowUsers` 不放行後三個帳號，並確認所有 sshd 設定檔都沒有 `Match` 區塊（`Match` 只對符合的連線生效，`sshd -G` 不會算進去） |
| 首次設定 | cloud-init 會從 boot 分割區讀帳號、公鑰與密碼（拿得到 SD 卡的人都能寫），`userconfig.service` 是主控台上的建立帳號精靈，也讀 boot 分割區的 `userconf.txt`。建置寫入 `/etc/cloud/cloud-init.disabled`，並停用、遮蔽（`systemctl mask`）cloud-init 的每個 unit 與 `userconfig.service`：只有標記檔時，kernel 參數 `cloud-init=enabled` 仍會讓 cloud-init 執行，遮蔽的 unit 則不會被啟動。boot 分割區上 cloud-init 讀的 `user-data`、`network-config`、`meta-data`（官方內容全是註解）一併刪除。cloud-init 的套件不移除：移除它就得一起移除 `rpi-cloud-init-mods`，而那個套件帶著 netplan 交給 NetworkManager 的設定（`/usr/lib/netplan/00-network-manager-all.yaml`），第一次開機的網路就和官方映像不同 |
| 登入面 | 遮蔽每個在主控台上給登入提示或 shell 的 unit，以及讓 sshd 聽本機 socket 的 `systemd-ssh-generator` 與它的 socket，寫入 logind 的 drop-in（「登入面」）；boot 分割區的 `cmdline.txt` 最後加上 `panic=10 usbcore.authorized_default=0` |
| 校時 | `/etc/systemd/timesyncd.conf.d/50-trustiot.conf`：`NTP=time.cloudflare.com`、`FallbackNTP=`（清掉內建的 Debian pool，防火牆不放行它） |
| 資料分割區 | `/etc/fstab` 加一行（「分割區」）、建立掛載點（root、0700）；[`files/data/`](files/data) 的每個檔案原樣安裝到 `src/pins.ts` 的 `DATA_FILES` 指定的位置：setup 的程式與 unit、`/etc/crypttab`、解鎖與 agent 的 drop-in、journald 與 rpi-swap 的設定（「資料分割區加密」、「日誌與置換空間」）。分割區本身不寫，全是零 |
| TPM | 編譯 [`files/tpm-slb9670-ce0-overlay.dts`](files/tpm-slb9670-ce0-overlay.dts)（以映像裡的 `dtc`）放到 boot 分割區的 `overlays/`，`config.txt` 最後加上 `[all]`、`dtparam=spi=on`、`dtoverlay=tpm-slb9670-ce0`。官方的 `tpm-slb9670` 把 TPM 放在 CE1，閘道器的模組在 CE0（[TPM 可行性測試](../../docs/feasibility/2026-10-12-tpm-poc.md)）。不碰 TPM 裡的任何金鑰 |
| 整理 | 安裝套件寫的日誌清回官方映像的空檔，刪除 `useradd` 留下的 `/etc/passwd-` 等備份；確認 machine ID 仍是 `uninitialized` |

### agent 的防火牆

照 [edge/agent/README.md](../agent/README.md)「給 Laysi/trustiot-v2#866 的接法」（Laysi/trustiot-v2#866）：

- 規則檔 `/usr/share/trustiot-agent/trustiot-agent-egress.nft` 的最後一行以絕對路徑 `include` 預設拒絕 `/usr/share/trustiot-agent/trustiot-agent-default-deny.nft`。出口規則的 unit、agent 的 `ExecStartPre=` 與 `nftables.service` 的 reload 三處都載入規則檔，所以兩個 table（`inet trustiot_agent` 與 `inet trustiot_agent_allow`）每一處都在同一個 transaction 裡載入，任一個失敗就整個不載入。
- 每次載入之後填回 DNS 的放行：agent 的 `ExecStartPre=+/usr/libexec/trustiot-agent/nameservers` 排在重新載入規則那一行之後，`nftables.service` 的 drop-in 有第二行 `ExecReload=` 執行同一個執行檔；`/etc/resolv.conf` 改變時由 path unit 填。
- agent 的 unit 以 `Wants=` 與 `After=` 帶起 `trustiot-agent-nameservers.path` 與 `trustiot-agent-allowlist.socket`。用 `Wants=` 而不是 `Requires=`：socket 或 path unit 起不來時 agent 照樣啟動、雲端連得上，可以遠端修復。socket 起不來時清單送不出去，閘門不讓任何設備連線（fail closed）；核心的集合這時仍是上一次執行留下的清單（開機之後是空的），要等新的清單回 `ok` 才換掉（[edge/agent/README.md](../agent/README.md)「給 Laysi/trustiot-v2#866 的接法」）。
- agent 的 unit 每次啟動時確認 `IPAddressDeny=` 掛上了（下一節）。

#### `IPAddressDeny=` 沒有掛上時

agent 的 unit 有 `IPAddressDeny=`（Laysi/trustiot-v2#878），規則被清掉、檢查還沒載回的那段時間（最多 11 秒）靠它拒絕 loopback、metadata、多播與受限廣播位址。systemd 把它做成掛在 unit 的 cgroup 上的 BPF 程式（ingress 與 egress 各一個）；掛不上時（核心沒有 cgroup BPF、不是 cgroup v2、沒有權限載入 BPF 程式）systemd 照樣啟動 unit，只在 debug 層級記一行，這一層會悄悄不生效。

**做法：每次啟動確認，不生效時記錯誤、agent 照常啟動。** agent 的 unit 的最後一行 `ExecStartPre=-+/usr/libexec/trustiot-agent/ip-deny-check`：在 unit 自己的 cgroup 裡以 root 執行（查詢要 `CAP_NET_ADMIN`），從 `/proc/self/cgroup` 讀出 cgroup，只接受 `trustiot-agent.service` 的，對那個 cgroup 以 `bpf(BPF_PROG_QUERY)` 問核心 ingress 與 egress 各掛了幾個程式。兩邊都有時記一行 `IPAddressDeny= is attached to /system.slice/trustiot-agent.service: 1 ingress, 1 egress programs`；任一邊沒有、或查詢失敗時以 error 層級（journal 的 priority 3）記下原因並以 1 結束，`-` 讓 systemd 不因此停下，agent 照常啟動。它不接受參數、不讀環境變數、不寫檔案、不啟動其他程式（[原始碼](../agent/src/bin/trustiot-agent-ip-deny-check.rs)）。

這樣選的理由：這一層只補規則被清掉之後的那幾秒，而且必須先有 root 清掉規則；agent 不啟動的代價是閘道器從雲端消失，只能到現場處理。所以不生效時寧可留著雲端的 443 與 ssh，讓人能遠端修，並留下一行會被看到的錯誤。

**沒有選的做法**：

- **不生效時 agent 不啟動**（`ExecStartPre=` 不加 `-`）：核心或 systemd 的條件是持續的，agent 會一直起不來，閘道器從雲端消失，只能到現場修；換到的保護只是規則被清掉之後的 11 秒。
- **在每 10 秒一次的出口規則檢查裡確認**：條件持續時每 10 秒記一次錯誤，灌滿 journal；檢查的沙箱要多放行 `bpf`（屬於 `@privileged`）並寫死 agent 的 cgroup 路徑；agent 重啟、cgroup 重建時會和檢查賽跑而誤報。
- **另一個由 agent 帶起的 oneshot unit**：失敗時看得到 `failed` 的 unit，但它不在 agent 的 cgroup 裡，要寫死路徑，而且在 agent 的 cgroup 建立之前或重啟之間執行時會誤報；多兩個檔案。
- **重啟 agent 直到掛上**：條件持續時無限重啟，和不啟動一樣斷線，還多了重啟的雜訊。
- **以 `bpftool` 或 `IPAccounting=` 的計數確認**：`bpftool` 是映像要多裝、多釘選的套件；計數器只說明有流量經過計數程式，不證明拒絕的程式掛著。
- **在心跳裡回報給雲端**：要改 agent 與雲端的協定，超出這個 issue。

**留下的風險**：不生效只出現在 journal（error 層級），unit 不會變成 `failed`；在閘道器上定期看它，例如 `scripts/check-gateway.sh` 加一項檢查，屬於 Laysi/trustiot-v2#1132。開機測試的核心是 Debian 的 6.12（「開機測試」），不是閘道器的；閘道器上的確認在 Laysi/trustiot-v2#811（2026-11-01 唯讀確認 lab 的閘道器：`CONFIG_CGROUP_BPF=y`、cgroup v2、systemd 257，條件具備）。

不改的：核心、`cmdline.txt` 原有的參數（含兩個 `console=`，見「登入面」）、網路設定（NetworkManager 與 netplan，和官方映像相同；實機上的行為在 Laysi/trustiot-v2#811 確認）、avahi、Wi-Fi 與藍牙服務（Laysi/trustiot-v2#888）、主機名稱 `raspberrypi`、套件來源設定、apt 的套件清單。

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
- `cmdline.txt` 最後的 `panic=10`：initramfs 失敗時（例如找不到 root），initramfs-tools 的 `panic()` 原本會在主控台開一個不用密碼的 root shell；有 `panic=` 時改為 10 秒後重開機，kernel panic 也在 10 秒後重開機。USB 鍵盤從核心啟動起就不被授權（「USB」），但這個 shell 也開在序列埠主控台上，而 BCM2712 的除錯 UART 預設是開的（「序列埠主控台：登入關閉」）。閘道器無人值守，重開機本來就比停在 shell 有用。資料分割區的解鎖（Laysi/trustiot-v2#810）不在 initramfs 裡：root 掛上之後由 `systemd-cryptsetup` 以 `headless=true` 解鎖，解不開時不詢問密碼、也不開 shell，開機照常完成（「資料分割區加密」），initramfs 沒有改。

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

閘道器不接受任何 USB 裝置：boot 分割區的 `cmdline.txt` 最後有 `usbcore.authorized_default=0`。`usbcore` 編譯在核心裡（兩個核心 `6.18.50+rpt-rpi-v8`、`6.18.50+rpt-rpi-2712` 的 `modules.builtin` 都列出它，映像檢查確認），所以從核心啟動起（含 initramfs），除了 root hub 之外，每個 USB 裝置一出現就是未授權：核心不為它設定組態，所以沒有介面，鍵盤、儲存裝置、網路卡等介面的驅動程式都不會綁上，也不會出現輸入裝置或磁碟。被擋的不只鍵盤、滑鼠與儲存裝置，也包括 hub、USB 網路卡、數據機與 USB 轉序列埠；BCM2711 板上 VL805 控制器內含的 USB 2.0 hub 本身也是 USB 裝置，同樣不授權，它後面的埠因此全部無效。網路線照常可用：BCM2711 與 BCM2712 板上的乙太網路不經過 USB。兩個核心共用同一份 `cmdline.txt`。映像只支援這兩種 SoC：BCM2837 的板子乙太網路經 USB，燒上這個映像就沒有網路，也連不上 SSH。

- 現場不接任何 USB 裝置。現場設備經 RS485 轉換器以 TCP 連線（[現場硬限制](../../docs/requirements/field-constraints.md)），閘道器用不到 USB。
- 檔案只經 SSH 以 `scp` 或 `sftp` 傳。
- root 仍然可以經 sysfs 授權個別裝置，但那不是維修程序，本文件不提供做法；能以 root 登入的人本來就能改任何設定。
- 硬體若內建需要的 USB 裝置（例如 USB 介面的行動網路模組），要在那時決定放行清單（依連接埠位置授權，並由映像檢查驗證）；目前的硬體不需要。

開機韌體會在 `cmdline.txt` 的參數前面加上自己的參數。`usbcore.authorized_default` 這類核心模組參數出現兩次時以後面的為準，而 `cmdline.txt` 的參數排在最後。lab 的閘道器（BCM2711）2026-10-31 唯讀確認：韌體加上的參數在前、其中沒有 `usbcore`；BCM2712 的韌體在 Laysi/trustiot-v2#811 確認。映像檢查也確認沒有別的東西取代或蓋過 `cmdline.txt`：

- `cmdline.txt` 只有一行（韌體只讀第一行），沒有 `--`（核心不讀它後面的參數）。
- `config.txt` 沒有 `cmdline=`、`os_prefix=`（會讓韌體讀別的檔案）或 `include`；boot 分割區沒有 `autoboot.txt`（bootloader 會改從別的分割區開機）。
- 每個核心的建置設定（`/boot/config-<版本>`）沒有 `CONFIG_CMDLINE_FORCE`，內建的命令列也沒有提到 `usbcore`。

映像裡沒有任何東西會再授權 USB 裝置（映像檢查的「USB」）：

- udev 規則（`/etc/udev/rules.d`、`/run/udev/rules.d`、`/usr/local/lib/udev/rules.d`、`/usr/lib/udev/rules.d`）不寫 `authorized` 或 `authorized_default`；規則執行的每個程式都在映像裡，其中的腳本也沒有提到它們。
- tmpfiles.d（`/etc/tmpfiles.d`、`/run/tmpfiles.d`、`/usr/local/lib/tmpfiles.d`、`/usr/lib/tmpfiles.d`）與 sysfsutils（`/etc/sysfs.conf`、`/etc/sysfs.d`）不寫它們。
- `usbguard` 沒有安裝（裝了就要 `masked`）。
- `/etc`、`/usr/local`、`/usr/bin`、`/usr/sbin`、`/usr/libexec`、`/usr/lib`（韌體與核心模組除外）底下 1 MiB 以內的文字檔，以及 boot 分割區上每個 initramfs 裡的文字檔，都沒有提到這個屬性（例如 unit 的 `ExecStart=` 或 initramfs 的腳本寫入 sysfs）。

不涵蓋：

- 開機韌體從 USB 開機（EEPROM 的 `BOOT_ORDER`），它在核心之前執行，留給 Laysi/trustiot-v2#216。
- 閘道器自己當 USB 裝置（gadget）：`authorized_default` 只管主機端。映像檢查確認 `config.txt` 沒有讓 dwc2 進入裝置或 OTG 模式（`dr_mode=host` 以外的 `dtoverlay=dwc2`）、`cmdline.txt` 沒有載入 gadget 驅動程式；改 SD 卡上的設定屬於 Laysi/trustiot-v2#216。
- 列舉本身：核心仍會讀取並解析未授權裝置的描述元，udev 仍處理它的 `add` 事件，所以描述元解析這類核心漏洞的攻擊面還在。
- 藍牙鍵盤、Wi-Fi 與 avahi 留給 Laysi/trustiot-v2#888。

### 維修時的做法

- 維修一律經 SSH 以 `trustiot-admin` 的公鑰登入（「映像內容」的「SSH」）；閘道器本體上沒有任何登入方式，螢幕、鍵盤與序列埠都不是維修管道。SSH 不通時重新燒錄（「燒錄與第一次開機」）或換機，再回產線登錄。
- 檔案經 SSH 以 `scp` 或 `sftp` 傳。USB 沒有例外程序：現場不接 USB 裝置。

## 釘選

改任何一項都會改變映像；改 `src/pins.ts` 時同一個變更更新本表（`bun run --cwd edge/image test` 檢查本文件列出每一個釘選值）。

| 項目 | 值 |
| --- | --- |
| 來源映像 | Raspberry Pi OS Lite（64 位元）2026-10-06，Debian 13（trixie），核心 6.18.50：https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2026-10-06/2026-10-06-raspios-trixie-arm64-lite.img.xz |
| 來源 SHA-256 | `483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843`（與官方網站同目錄的 `.sha256` 相同，2026-10-31 核對） |
| 工具容器 | `docker.io/library/debian:trixie@sha256:2dc419c6d7fe3dad0a2f9fd469c34cc90b0f9ba642d8fde837fe3d7d34a0e7f1`（trixie，多架構 index。寫成 `名稱:tag@sha256:…`，Renovate 讀得到，更新時與 `src/pins.ts` 在同一個 PR 一起換） |
| Debian 套件時間點 | snapshot.debian.org `20261030T000000Z`（容器的工具與映像的套件都從這裡安裝；apt 照常驗證套件庫簽章） |
| 工具容器的套件 | ca-certificates 20250419+deb13u1、dosfstools 4.2-1.2、e2fsprogs 1.47.2-3+b12、fdisk 2.41.5-0+deb13u1、fuse2fs 1.47.2-3+b12、mtools 4.0.48-1、xz-utils 5.8.1-1+deb13u2（以 `名稱=版本` 安裝。容器裡還沒有 CA 憑證，ca-certificates 只能以 HTTP 取得：apt 照常驗證套件庫簽章與檔案雜湊，固定版本則讓重放的舊索引無法換成較舊的套件；其餘套件之後以 HTTPS 取得） |
| 映像加裝的套件 | cryptsetup-bin 2:2.7.5-2、systemd-cryptsetup 257.13-1~deb13u1（與映像的 systemd 同版本）、tpm-udev 0.6+nmu1、tpm2-tools 5.7-1+b1（相依：libtss2-* 4.1.3-1.2；libcryptsetup12 2:2.7.5-2 映像原本就有） |
| NTP | time.cloudflare.com（Cloudflare 公布的位址：162.159.200.1、162.159.200.123、2606:4700:f1::1、2606:4700:f1::123，見 https://developers.cloudflare.com/time-services/ntp/usage/ ，2026-10-31 查閱）。Cloudflare 不做閏秒抹平（leap smear），所以不與會抹平的伺服器（例如 Google）混用 |
| 資料分割區 | 2 GiB，映像裡全是零；第一次開機的 LUKS2：UUID `9da7f680-5912-4d5c-bca6-d46c83e064f8`、aes-xts-plain64、512 位元金鑰、4096 bytes 的 sector，volume 名稱 `trustiot_data`，標籤 `trustiot-data-new`／`trustiot-data`；裡面的 ext4 標籤 `trustiot-data`（`src/pins.ts` 的 `LUKS`；setup 程式與 crypttab 寫著同樣的值，`bun run --cwd edge/image test` 比對） |
| 開機測試的套件 | 只裝在開機測試的容器裡（「開機測試」）：核心 6.12.113+deb13-arm64（linux-image-6.12.113+deb13-arm64 6.12.113-1、linux-headers-6.12.113+deb13-arm64 6.12.113-1、linux-source-6.12 6.12.113-1）、make 4.4.1-2、qemu-system-arm 1:10.0.13+ds-0+deb13u1、swtpm 0.7.1-1.5、libtss2-tcti-swtpm0t64 4.1.3-1.2、tpm2-tools 5.7-1+b1、cryptsetup-bin 2:2.7.5-2 |

lab 的閘道器目前跑的是 2026-09-15 的官方映像（raspi-firmware 1:1.20260915-1），與本映像的來源同一個核心版本（6.18.50）；兩者的差異在 Laysi/trustiot-v2#811 實機開機時確認。

## 映像檢查

`check.ts` 只讀完成的映像、來源映像、agent 執行檔與公鑰檔，和建置共用預期值（`src/pins.ts`、`src/files.ts`、`src/login.ts`），但不共用建置的步驟。每一項：

| 檢查 | 內容 |
| --- | --- |
| 分割表 | 磁碟識別碼、三個分割區的位置、大小與類型和預期完全相同；4 MiB 對齊、不重疊、boot 在最前、root 在最後；映像檔大小 |
| 檔案系統 | root 的 `e2fsck -fn`、boot 的 `fsck.vfat -n` 都沒有錯誤 |
| 資料分割區 | 整個分割區全是零（`cmp`），`blkid -p` 認不出任何格式；fstab 那一行（`/dev/mapper/trustiot_data`、ext4、`nodev,nosuid,noexec,nofail,x-systemd.device-timeout=15min`、fsck 順序 2），root 與 boot 仍以 PARTUUID 指定；掛載點 root 擁有、0700，root 上的掛載點裡沒有任何檔案；agent 的 unit 有 `StateDirectory=trustiot-agent`，drop-in 有 `RequiresMountsFor=`、`AssertPathIsMountPoint=` 與 `ConditionPathExists=!/run/trustiot-back-to-factory` |
| 資料分割區加密 | `files/data/` 的每個檔案與 repo 逐位元組相同、權限正確；`/etc/crypttab` 只有一行：`trustiot_data`、釘選的 UUID、`none`、`luks,tpm2-device=auto,headless=true,nofail,x-systemd.device-timeout=15min`；在映像裡執行映像自己的 `systemd-cryptsetup-generator`，產生的 unit 以那個 UUID 與選項（`x-systemd.*` 以外的）attach、沒有預設相依，只被 `cryptsetup.target` wants（不是 requires）；那個 generator 與 `systemd-fstab-generator` 給 volume 與 `/dev/mapper/trustiot_data` 的 device 寫了 `JobRunningTimeoutSec=15min`；以 `systemd-analyze verify` 載入 setup、解鎖與 agent 的 unit；agent 已啟用 |
| 日誌與置換空間 | `systemd-analyze cat-config` 算出 journald 的 `Storage=volatile`、`ForwardToSyslog=no`；沒有安裝 syslog daemon；`rpi-systemd-config` 算出 rpi-swap 的 `Mechanism=zram`，root 上沒有 `/var/swap`；setup、解鎖與 agent 的 unit 都有 `LimitCORE=0` |
| 第一次開機 | 以映像的 `parted` 對 16 GiB 的檔案（只有映像的分割表）執行 `resize_early` 的指令：root 延伸到結尾，boot 與資料分割區不動；machine ID 是 `uninitialized`、沒有 SSH 主機金鑰、主機金鑰的產生服務已啟用、`cmdline.txt` 有 `resize` 與分割區 2 的 root |
| agent | 四個執行檔（`AGENT_EXECUTABLES`）各與給的檔案 SHA-256 相同、root 擁有、0755、所在目錄 root 擁有、0755、aarch64 ELF，在映像裡執行成功（`trustiot-agent --version` 以 0 結束，另外三個帶一個參數以 2 結束）；`edge/agent/systemd` 的每個檔案和 repo 的逐位元組相同、0644；agent 的 unit 已啟用，`trustiot-agent-allowlist.socket` 與 `trustiot-agent-nameservers.path` 是 `static`（沒有 `[Install]`）；`trustiot-agent` 使用者、`tss` 群組、tpmrm 的 udev 規則；映像裡沒有 `/etc/trustiot/agent.toml` |
| agent 的防火牆 | 以映像的 `systemd-analyze cat-config` 讀 agent、出口規則的 unit 與 `nftables.service`（含映像裡的每個 drop-in，`src/firewall.ts`）：agent 的 `ExecStartPre=` 正好是載入規則檔、`nameservers`、`-+` 的 `ip-deny-check` 三行；出口規則的 unit 的 `ExecStart=` 載入同一個規則檔；`nftables.service` 的 `ExecReload=` 正好是在一個 transaction 裡載入 `/etc/nftables.conf` 與規則檔、再執行 `nameservers`；agent 以 `Wants=` 與 `After=` 帶起 socket 與 path unit，沒有 `Requires=`、`Requisite=`、`BindsTo=`、`PartOf=`、`Upholds=`；規則檔的最後一行、也是唯一的 `include`，是預設拒絕的絕對路徑；`systemd-analyze verify` 載入 agent、出口規則、檢查、socket、path 與兩個 service 的 unit（unit 執行的程式都要在映像裡） |
| 套件 | cryptsetup-bin、systemd-cryptsetup、tpm-udev、tpm2-tools 是釘選版本且已完成安裝，`dpkg --audit` 為空 |
| TPM overlay | boot 分割區上的 `.dtbo` 與 repo 的 `.dts` 重新編譯的結果相同；`config.txt` 對所有型號有那兩行；以 `dtmerge` 疊到 `bcm2711-rpi-4-b.dtb` 後，spi0 有 reg 0 的 `infineon,slb9670`、spidev0 停用 |
| NTP | `systemd-analyze cat-config` 算出的有效設定只有釘選的伺服器、沒有 fallback；timesyncd 已啟用 |
| sshd | `sshd -G`，以及以暫時的主機金鑰（放在 chroot 的 tmpfs，不進映像）對 `trustiot-admin`、`root`、`pi`、`trustiot-agent` 四個連線各跑一次 `sshd -T -C`：有效設定是「映像內容」的每一項，`AllowUsers` 不放行 `root`、`pi`、`trustiot-agent`（不能有萬用字元）；所有 sshd 設定檔沒有 `Match` 區塊；`ssh.service` 已啟用、改名精靈的設定已刪 |
| 管理者 | `authorized_keys` 正好是給的公鑰（不含註解），映像自己的 `ssh-keygen` 讀得懂每一把且指紋相同；家目錄與 `.ssh` 0700、`authorized_keys` 0600；密碼欄是 `*`；sudoers 檔的內容、0440、`visudo --check --strict`；`/etc/sudoers.d` 只多了這個檔案，`/etc/sudoers` 與來源映像原有的規則檔都沒改；`sudo` 群組沒有成員 |
| 帳號 | 沒有任何帳號有可用的密碼（密碼欄是雜湊或空白都算可用）、沒有群組密碼、`pi` 已刪、沒有 `/etc/shadow-` 等備份 |
| 首次設定 | 映像裡每個 cloud-init 的 unit 與 `userconfig.service` 都是 `masked`、有 `/etc/cloud/cloud-init.disabled`、boot 分割區沒有 `user-data`、`network-config`、`meta-data` |
| 主控台 | 「登入面」遮蔽的每個 unit，以及 tty1 到 tty6 的 `getty@`、`autovt@`，序列埠 ttyS0、ttyS1、ttyAMA0、ttyAMA1、ttyAMA10、hvc0 的 `serial-getty@` 都是 `masked`；unit 目錄（`/etc`、`/usr/local/lib`、`/usr/lib` 的 `systemd/system`）裡每個以 `StandardInput=tty*` 或 `TTYPath=` 接上終端機、或 `Exec*=` 提到 getty、sulogin、`sh -i` 或終端機裝置的 service（含 drop-in）都是 `masked`，例外只有執行 `systemd-sulogin-shell` 的 `emergency.service` 與 `rescue.service`，而它們只有官方的 unit 檔（沒有覆蓋或 drop-in）；`/etc`、`/usr/lib/systemd`、`/usr/local/lib/systemd` 沒有任何文字檔提到 `SYSTEMD_SULOGIN_FORCE`（含 manager 的 `DefaultEnvironment=` 與 `EnvironmentFile=` 指的檔案）；unit 目錄裡沒有連到目錄外的 unit；建置沒有新增 `/etc/init.d` 的腳本或 `/etc/rc.local`（`/etc/rc?.d` 的連結只是啟用 init.d 的腳本，`systemctl enable`、`mask` 會為 Debian 的腳本寫入）；root 的密碼欄鎖定；`systemd-analyze cat-config` 算出的 logind 設定是 `NAutoVTs=0`、`ReserveVT=0` |
| 開機失敗 | `cmdline.txt` 只有一個 `panic=`，是 `panic=10`；以映像的 `unmkinitramfs` 解開 boot 分割區上的每個 initramfs：`init` 讀 `panic=`，`scripts/functions` 的 `panic()` 在 `panic=` 有值時重開機，而且在那之前不開 shell |
| sshd 本機 socket | `/etc/systemd/system-generators/systemd-ssh-generator` 是指向 `/dev/null` 的連結；`sshd-extra.socket`、`sshd-unix-local.socket`、`sshd-vsock.socket` 都是 `masked`；unit 目錄裡沒有其他沒遮蔽、以 `ListenStream=`、`ListenSequentialPacket=` 或 `ListenDatagram=` 的路徑、抽象名稱或 `vsock:` 位址讓 sshd 聽本機 socket 的 socket unit（服務依 `Service=` 或 socket 的名稱，在每個 unit 目錄找） |
| USB | `cmdline.txt` 只有一行、沒有 `--`，`panic=` 與 `usbcore.authorized_default=` 各只出現一次，值是 `panic=10` 與 `usbcore.authorized_default=0`（參數名稱的 `-` 與 `_` 視為相同），沒有載入 gadget 驅動程式；`config.txt` 沒有 `cmdline=`、`os_prefix=`、`include`，也沒有 `dr_mode=host` 以外的 `dtoverlay=dwc2`；boot 分割區沒有 `autoboot.txt`；每個核心的 `modules.builtin` 都有 `usbcore`，`/boot/config-<版本>` 沒有 `CONFIG_CMDLINE_FORCE`、內建命令列沒有 `usbcore`；所有 udev 規則檔（四個目錄，連結就在映像裡跟到它指的檔案，讀不到的連結算問題）沒有寫入 `authorized` 或 `authorized_default`（`ATTR{}` 的 `=`、`:=`、`+=`，或 `RUN`、`PROGRAM`、`IMPORT` 提到它），規則執行的每個程式（`/bin`、`/sbin` 換成 `/usr` 底下；直譯器另取它的腳本）都在映像裡，其中的腳本沒有提到它；tmpfiles.d 與 sysfsutils 的設定沒有寫入它；`usbguard` 沒有安裝，或它的 unit 是 `masked`；`/etc`、`/usr/local`、`/usr/bin`、`/usr/sbin`、`/usr/libexec`、`/usr/lib`（`firmware`、`modules` 除外）底下 1 MiB 以內的文字檔，以及以映像的 `unmkinitramfs` 解開的每個 initramfs 裡的文字檔，都沒有提到這個屬性 |
| 祕密與內網名稱 | 建置新增或改過的每個檔案（root 與 boot，與來源映像逐檔比對得出）：文字檔沒有私鑰區塊，設定檔（`/etc`、`/home`、`/root` 與建置寫的檔案）沒有私有網段位址或只在內網解析的名稱（報告只寫行號與規則，不寫出比對到的內容）；從 `edge/agent/systemd` 原樣複製的檔案不比對位址：「agent」一項已確認它們與 repo 的檔案逐位元組相同，而出口規則本來就寫著它要擋的位址（metadata 的 `169.254.169.254` 等）；這些檔案再以 `.prototools` 釘選的 gitleaks（`gitleaks dir`）掃描；沒有 `~/.ssh/id_*`。gitleaks 的發現只有 `src/pins.ts` 的 `KNOWN_FINDINGS` 列出的才接受：路徑、規則、`dpkg-query -S` 查到的套件與整個檔案的 SHA-256 都要相同，建置自己寫的檔案一律不接受。目前列的是 libtss2-fapi1t64 4.1.3-1.2 的兩個 TPM FAPI profile（`/etc/tpm2-tss/fapi-profiles/P_ECCP384SHA384.json`、`/etc/tpm2-tss/fapi-profiles/P_RSA3072SHA384.json`）：`generic-api-key` 比對到的是 `authPolicy`，NV index 授權策略的公開摘要，不是祕密（2026-10-31 檢視）。套件版本一變，檔案的 SHA-256 跟著變，檢查就不通過，要重新檢視後更新清單 |

### 檢查證明什麼、不證明什麼

- 拿得到 SD 卡或閘道器本體的人，等同拿到 root：可以改 boot 分割區的 `cmdline.txt`（例如 `init=/bin/sh`），或直接改 root 檔案系統。遮蔽 cloud-init 與 userconfig 只是拿掉官方映像現成的入口，不是對實體存取的防護。資料分割區加密只擋拿到 SD 卡的人；拿到閘道器本體的人能改 root，讓下一次開機解鎖後讀取資料（TPM 沒有綁 PCR，「資料分割區加密」），那要等 secure boot（Laysi/trustiot-v2#216），Laysi/trustiot-v2#216 應把這一點列為前提。
- 登入面的檢查看的是映像裡的設定：unit 的狀態、規則檔與開機參數。核心真的不授權 USB 鍵盤與隨身碟、螢幕與序列埠上沒有登入提示，另以 QEMU 模擬開機驗證過一次：開機前接上與開機後熱插拔的鍵盤、隨身碟與 hub 都不被授權，拿掉這個參數的對照組則出現輸入裝置與磁碟（[測試報告](../../docs/test-reports/2026-10-31-815-gateway-login-surface.md)；QEMU 的 `raspi3b` 只證明核心參數的效果，不涵蓋 BCM2711 的 VL805、BCM2712 的 RP1 與開機韌體）；在閘道器上開機驗證、以及 initramfs 失敗時重開機，在 Laysi/trustiot-v2#811。USB 的掃描以字串比對：規則執行的程式若是二進位檔，檢查不讀它的內容；刻意避開比對的寫法（例如先切換目錄再寫入 `authorized`）也比對不到。閘道器上之後的更新（例如套件的 hook 重寫 `cmdline.txt`）不在映像檢查的範圍。
- `check.ts` 不開機：它證明加密、解鎖與 fail closed 需要的檔案與設定都在映像裡、而且是 systemd 讀得懂的形式；它們開機時真的那樣運作，由開機測試證明（「開機測試」）。agent 的防火牆也一樣：檢查證明三處載入的都是含預設拒絕的規則檔；兩個 table 真的載入、`IPAddressDeny=` 真的掛上並擋下封包，由開機測試證明。
- `check.ts` 驗的是以 `build.ts` 從已知輸入做出的映像，不是第三方映像的驗收工具：它列出建置改了來源映像的哪些路徑（`delta.tsv`），但不判斷每個改動是否惡意，例如被換掉的執行檔。燒錄前要確認的是映像的 SHA-256 與建置記錄（`trustiot-gateway.img.sha256`、`build-info.json`）相同，不是重跑檢查。

## 開機測試

`boot-test.ts` 在 QEMU 裡開機映像的副本，證明資料分割區的加密、解鎖、解不開時的行為，以及永久解不開時的清除重建與標記。不需要閘道器、KVM、root、loop device 或建置主機上的 device mapper：虛擬機的核心自己載入 dm-crypt，TPM 是 swtpm（軟體 TPM），全部在和建置相同的 rootless 工具容器裡。在 arm64 Linux 的建置主機上執行（「建置主機」）；它不在 CI 裡跑：

```sh
bun edge/image/boot-test.ts --image <輸出目錄>/trustiot-gateway.img --report <報告目錄>
```

結束碼：全部通過為 0，有一項不通過為 1，用法錯誤為 2，無法執行為 3。`--report`（repo 之外、新的或空的目錄）收到每次開機的探針輸出（`*.probe.txt`）、序列埠主控台的紀錄（`*.serial.log`）、第一次開機寫出的 LUKS 標頭（`luks-header.json`）、15 清除重建後的 LUKS 標頭（`luks-header-wiped.json`）與每一項的結果（`boot-test.json`）。

測試的副本只比映像多三樣東西，boot 與資料分割區逐位元組相同：

- 核心：開機測試要 TPM，用 QEMU 的 `virt` 機器，而它跑不了 Raspberry Pi 的核心（Raspberry Pi 的核心在 QEMU 的 `raspi3b` 上開得起來，登入面的模擬開機就是這樣做的，但 `raspi3b` 不能接 TPM），所以改用 Debian 的 arm64 核心（版本見「釘選」），模組複製到副本的 root，以 `-kernel`／`-initrd` 開機（不經過官方的 initramfs，所以第一次開機的 `resize_early` 與 `set_partuuid` 不執行；它們在「映像檢查」與 Laysi/trustiot-v2#811 驗證）。TPM 是 QEMU 的 `tpm-tis-device`（記憶體映射的 TIS 介面；閘道器的是 SPI 上的 TIS），它的驅動 `tpm_tis` 在 Debian 的 arm64 核心沒有編譯，測試以同一版核心的原始碼（`linux-source-6.12`）與 headers 只編譯這一個模組。
- 探針：[`boot-test/probe.sh`](boot-test/probe.sh) 與它的 unit。開機完成後（`systemctl is-system-running --wait`）記錄 setup、解鎖、掛載、agent 與 ssh 的 unit 狀態（agent 的含 condition 的結果）、掛載點、LUKS 標頭、keyslot、0x81000001 那把 SRK 的 name、`/run/trustiot-back-to-factory`、等待中的密碼提示、置換空間、日誌目錄、core dump 與相關的日誌，寫到另一個虛擬磁碟後關機。核心參數 `trustiot_test.start-agent=1` 讓它在記錄之後再以 root 手動啟動 agent 兩次（`systemctl start trustiot-agent`，以及略過相依的 `--job-mode=ignore-dependencies`），記錄結果。探針也記錄 agent 的防火牆：`nft list tables`、socket 與 path unit 的狀態、socket 的擁有者與權限，以及 `ip-deny-check` 這次開機記的每一行。核心參數 `trustiot_test.firewall=1` 讓它在記錄之後再實際走一遍（見下方「agent 的防火牆」）。`src/boot-results.ts` 判斷每次開機（單元測試在 `tests/boot-results.test.ts`）。
- 晚出現的 TPM：[`boot-test/tpm-late.sh`](boot-test/tpm-late.sh) 與它的 unit（`sysinit.target` 啟動，沒有預設相依）。只在核心參數有 `trustiot_test.tpm-late=<秒>` 時動作，測試同時給 `modprobe.blacklist=tpm_tis`：udev 不為 TPM 裝置載入驅動，開機後那麼多秒它才以名稱 `modprobe tpm_tis`（blacklist 只擋以別名載入），在那之前沒有 `/dev/tpmrm0`，就像 TPM 暫時沒有回應。

斷電的開機（19 到 33）與 34 用另一份「斷電副本」：測試副本再加上 [`boot-test/power-cut.sh`](boot-test/power-cut.sh)，以 `/usr/local/sbin/` 裡的 `cryptsetup`、`systemd-cryptenroll`、`mke2fs`、`dd` 連結指向它（systemd 給 service 的 PATH 先找 `/usr/local/sbin`）。沒有核心參數 `trustiot_test.power-cut=<步驟>`、或呼叫者不在 `trustiot-data-setup.service` 裡時，它直接執行真正的程式；有的話，setup 做完那一步（步驟依 `format()` 的順序：`luks-format`、`write-header`（寫兩份 metadata 的 `dd`）、`write-keyslots`（寫 keyslot 區的 `dd`）、`tpm-enroll`、`mkfs`、`kill-slot`、`before-relabel`（改標籤之前）、`after-relabel`），它把 `power-cut <步驟>` 寫到結果磁碟，接著以 sysrq `b` 立刻重置核心（不 sync、不卸載；QEMU 以 `-no-reboot` 執行，所以就此結束）：虛擬機還沒交給虛擬磁碟的資料都不見。它模擬的是在兩步之間斷電，不是真的斷電：已經交給虛擬磁碟的寫入都留著（磁碟是 `cache=unsafe`），不模擬一步做到一半時斷電，也不模擬卡片自己的快取遺失、寫一半或重排寫入。斷電之後在建置主機讀分割區（`blkid`、LUKS 標頭，與整個分割區是否全是零），確認停在那一步留下的狀態，再以同一顆 TPM 開下一次機。

依序開機（swtpm 的狀態目錄就是一顆 TPM，A、B、C、F、K、O、W、X、P-<步驟> 與 P-wipe-<步驟> 是不同的 TPM，A-cleared 是 A 的副本清除後）。1 到 4 各用映像的一份副本，5 到 18 都是同一份（10 用它的一份副本），19 到 26 各用斷電副本的一份副本（資料分割區全是零），27 到 29 各用 23a 斷電之後那份磁碟與 TPM 的一份副本，30 用斷電副本的一份副本，31 到 33 各用 30 之後那份磁碟的一份副本與一顆新的 TPM，34 用斷電副本的一份副本：

| 開機 | 磁碟與 TPM | 必須看到 |
| --- | --- | --- |
| 1-foreign-fs | 資料分割區放了另一個 ext4，TPM F | setup 拒絕（unit 失敗）、分割區逐位元組不變、不掛上、agent 沒有啟動 |
| 2-foreign-luks | 資料分割區放了另一個 UUID 的 LUKS2，TPM F | 同上 |
| 3-not-empty | 資料分割區全是零，只有 1 GiB 處的 4 KiB 隨機位元組（`blkid` 認不出格式），TPM F | 同上（「認不出但不是零」） |
| 4-interrupted-setup | 資料分割區是釘選的 UUID、標籤 `trustiot-data-new`、有一個密碼 keyslot 的 LUKS2（上次的加密沒做完），TPM C | 重新加密，TPM C 解鎖、掛上，標頭只剩 TPM 的 keyslot |
| 5-first-boot-no-tpm | 映像原樣，沒有 TPM | setup 拒絕、分割區仍全是零、不掛上、agent 沒有啟動 |
| 6-first-boot-tpm-a | TPM A，磁碟讀取限速 16 MiB/s | 加密、只剩 TPM 的 keyslot、有指紋的 token（LUKS 標頭在虛擬機裡與建置主機上各讀一次），指紋等於 0x81000001 那把 SRK 的 name、以 `/dev/mapper/trustiot_data` 掛上（`nodev,nosuid,noexec,noatime`）、根目錄 `trustiot-agent` 0700、agent 啟動、沒有標記也沒有 `/run/trustiot-back-to-factory`；setup 在開機兩分鐘以上才完成（超過 systemd 預設等 device 的 90 秒），同一次開機仍解鎖、掛上；在分割區裡寫一個隨機的標記檔，它不出現在磁碟的原始位元組裡 |
| 7-tpm-a-again | TPM A；開機後探針走一遍 agent 的防火牆（`trustiot_test.firewall=1`） | 不重新格式化，解鎖、掛上，讀回標記檔；防火牆的每一項（見下方「agent 的防火牆」） |
| 8-tpm-removed | 沒有 TPM；開機後探針手動啟動 agent | setup 每 10 秒看一次、看完 180 秒（每次都是 `absent`，間隔至少 10 秒、最後一次不晚於 180 秒），不清除；解不開：解鎖失敗而不是等到時限、沒有密碼提示、不掛上、agent 沒有啟動、root 上的掛載點是空的。手動啟動：`systemctl start` 因相依的 job 失敗而失敗（它再拉起一次解鎖，之後仍沒有等待中的密碼提示），略過相依的那次因 `AssertPathIsMountPoint=` 失敗（assert 有評估、`AssertResult=no`）；兩次都在時限內結束，agent 從未離開 inactive、沒有程序執行 `/usr/bin/trustiot-agent`，仍不掛上，root 上的掛載點仍是空的。LUKS 標頭和第一次開機寫的相同 |
| 9-tpm-a-late | TPM A，驅動在開機 150 秒後才載入（`tpm-late.sh`） | setup 先看到沒有 TPM（`absent`），TPM 出現後的那次 `opens`，不清除；setup 在 150 秒之後才結束，解鎖、掛上、讀回標記檔，LUKS 標頭和第一次開機寫的相同 |
| 10-unrecorded-tpm-b | 這份磁碟的副本，在建置主機上移除指紋的 token；TPM B | 每次都是 `unrecorded`、看完 180 秒、不清除；解不開、不掛上、agent 沒有啟動；資料分割區逐位元組不變 |
| 11-owner-password | TPM O：新的 TPM，owner hierarchy 設了密碼 | 每次都是 `unknown-owner`、看完 180 秒、不清除；解不開、不掛上、agent 沒有啟動；LUKS 標頭和第一次開機寫的相同 |
| 12-tpm-a-last | TPM A | 之前的失敗沒有破壞任何東西：解鎖、讀回標記檔，LUKS 標頭和第一次開機寫的相同，標記檔仍不在磁碟的原始位元組裡 |
| 13-duplicate-uuid | TPM B，另接一個帶著同樣 UUID 的 LUKS2 磁碟 | setup 在看 TPM 之前就拒絕、不清除、不解鎖、不掛上、agent 沒有啟動，兩個磁碟都不變 |
| 14-tpm-with-a-key | TPM K：另一台已登錄閘道器的 TPM（新的 TPM，在建置主機上先放 0x81000001 的 SRK 與 0x81000100 的金鑰，`tpm2_getcap handles-persistent` 正好列出這兩個） | 每次都是 `keyed 0x81000100`、看完 180 秒、不清除，日誌寫 `most likely another gateway's TPM`；解不開、不掛上、agent 沒有啟動；LUKS 標頭和第一次開機寫的相同 |
| 15-tpm-a-cleared | A-cleared：TPM A 的副本，先放 0x81000100 的金鑰（像登錄過的閘道器），再以 `tpm2_clear` 清除；清除之後 `tpm2_getcap handles-persistent` 什麼都沒有 | 連續 3 次同一個 `other-owner`（間隔至少 10 秒），清除重建：以這顆 TPM 解鎖、掛上，標頭有指紋與標記（虛擬機裡與建置主機上各讀一次），標記的 `sealed_owner` 是第一次開機記下的指紋、`found_owner` 是新的指紋；`/run/trustiot-back-to-factory` 是 `reason=tpm-owner-changed`；agent 沒有啟動，而且是 condition 擋下的（`ConditionResult=no`，有評估）；沒有密碼提示；讀不到舊的標記檔（新的空 volume） |
| 16-tpm-a-cleared-again | A-cleared | 第一次就 `opens`，不再清除；標記還在、agent 仍沒有啟動；寫一個新的標記檔並讀回；LUKS 標頭和 15 寫的相同 |
| 17-tpm-b | TPM B | 再清除重建一次，給 TPM B；新標記的 `sealed_owner` 是 15 記下的指紋；agent 沒有啟動，讀不到 16 的標記檔 |
| 18-mark-removed | 在建置主機上以 `cryptsetup token remove --token-id 2` 移除標記；TPM B | 照常解鎖、掛上，沒有 `/run/trustiot-back-to-factory`，agent 啟動，標頭沒有標記 |
| 19a-power-cut-luks-format | 斷電副本，TPM P-luks-format，`luksFormat` 之後斷電（標頭只在 `/run` 的檔案裡） | 結果磁碟是 `power-cut luks-format`；分割區仍全是零 |
| 19b-after-luks-format | 同一份磁碟與 TPM | 第一次開機重來（`is empty: first boot`），加密、解鎖、掛上，標頭只剩 TPM 的 keyslot 與指紋（虛擬機裡與建置主機上各讀一次） |
| 20a-power-cut-write-header | 寫完兩份 metadata 之後斷電 | 標籤 `trustiot-data-new`、只有 keyslot 0、只有指紋的 token |
| 20b-after-write-header | 同上 | 重新加密（`holds a setup that did not finish`），解鎖、掛上，標頭只剩 TPM 的 keyslot 與指紋（虛擬機裡與建置主機上各讀一次） |
| 21a-power-cut-write-keyslots | 寫完 keyslot 區之後斷電 | 同 20a |
| 21b-after-write-keyslots | 同上 | 同 20b |
| 22a-power-cut-tpm-enroll | TPM 登錄之後斷電 | 標籤 `trustiot-data-new`、keyslot 0 與 1、keyslot 1 上的 TPM token、指紋的 token |
| 22b-after-tpm-enroll | 同上 | 同 20b |
| 23a-power-cut-mkfs | `mke2fs` 之後斷電（解開的 volume 還開著） | 同 22a；這份磁碟與 TPM 另存一份給 27 到 29 |
| 23b-after-mkfs | 同上 | 同 20b |
| 24a-power-cut-kill-slot | 清掉暫時金鑰的 keyslot 之後斷電 | 標籤 `trustiot-data-new`、只有 keyslot 1 與它的 TPM token、指紋的 token |
| 24b-after-kill-slot | 同上 | 同 20b |
| 25a-power-cut-before-relabel | 改標籤之前斷電 | 同 24a |
| 25b-after-before-relabel | 同上 | 同 20b |
| 26a-power-cut-after-relabel | 改標籤之後斷電 | 標籤 `trustiot-data`、只有 keyslot 1 與它的 TPM token、指紋的 token |
| 26b-after-after-relabel | 同上 | 不重新加密（`is encrypted and opens only with the TPM`），解鎖、掛上，標頭只有 TPM 的 keyslot 與指紋 |
| 27a-power-cut-again-luks-format | 23a 之後的磁碟與 TPM 的副本（標頭是 `trustiot-data-new`，後面有上一次做出的加密檔案系統，所以標頭後面不是零）；重新開始的 setup 在 `luksFormat` 之後斷電 | 先確認標頭後面不是零；斷電後分割區沒變：同 22a |
| 27b-after-again-luks-format | 同上 | 同 20b |
| 28a-power-cut-again-write-header | 同 27a 的副本；重新開始的 setup 寫完兩份 metadata 之後斷電（Laysi/trustiot-v2#917 之前的 setup 在這裡清掉標頭） | 同 20a：新的 `trustiot-data-new` 標頭，不是零 |
| 28b-after-again-write-header | 同上 | 同 20b（Laysi/trustiot-v2#917 之前這次開機拒絕分割區） |
| 29a-power-cut-again-write-keyslots | 同 27a 的副本；重新開始的 setup 寫完 keyslot 區之後斷電 | 同 20a |
| 29b-after-again-write-keyslots | 同上 | 同 20b |
| 30-wipe-base-tpm-w | 斷電副本的一份副本，TPM W | 第一次開機加密，31 到 33 從這份磁碟開始 |
| 31a-power-cut-wipe-luks-format | 30 的副本，新的 TPM P-wipe-luks-format：setup 判定永久解不開、清除重建，在 `luksFormat` 之後斷電 | 分割區還是 W 的 volume：標籤 `trustiot-data`、keyslot 1 與它的 TPM token、指紋的 token，沒有標記 |
| 31b-after-wipe-luks-format | 同一份磁碟與 TPM | 再看一次 TPM、再清除重建（`and making it anew for this TPM`），解鎖、掛上，標頭有 TPM 的 keyslot、指紋與標記，agent 沒有啟動 |
| 32a-power-cut-wipe-write-header | 30 的副本，新的 TPM；清除重建寫完兩份 metadata 之後斷電 | 標籤 `trustiot-data-new`、只有 keyslot 0、指紋與標記的 token |
| 32b-after-wipe-write-header | 同上 | 重新開始（`holds a setup that did not finish`），標記帶到再做的標頭；其餘同 31b |
| 33a-power-cut-wipe-after-relabel | 30 的副本，新的 TPM；清除重建改標籤之後斷電 | 標籤 `trustiot-data`、只有 keyslot 1 與它的 TPM token、指紋與標記的 token |
| 33b-after-wipe-after-relabel | 同上 | 照常解鎖（`is encrypted and opens only with the TPM`），標記還在；其餘同 31b |
| 34-foreign-storage-key | 斷電副本的一份副本（資料分割區全是零），TPM X：新的 TPM，在建置主機上於 0x81000001 放一把 RSA 的 storage key（不是 setup 與 systemd 用的 ECC 範本；`tpm2_getcap handles-persistent` 只列出它） | setup 拒絕（`0x81000001 holds a key that is not this TPM's standard storage key`，unit 失敗）、分割區仍全是零、不掛上、agent 沒有啟動 |

**agent 的防火牆（Laysi/trustiot-v2#866）。** agent 啟動過的每次開機（解鎖而且標頭沒有標記的開機，例如 4、6、7、9、12、18 與斷電之後的 b 開機）都檢查：`nft list tables` 有 `inet trustiot_agent` 與 `inet trustiot_agent_allow`；`trustiot-agent-allowlist.socket` 是 active/listening、`trustiot-agent-nameservers.path` 是 active/waiting，兩個都是 `static`（是 agent 的 unit 帶起的）；socket 是 `root trustiot-agent 660`；agent 的每次啟動中至少有一次記下 `IPAddressDeny= is attached to /system.slice/trustiot-agent.service`，而且 `ip-deny-check` 沒有記任何錯誤。7 再走一遍（`boot-test/probe.sh`）：agent 本身沒有出廠設定會立刻結束，所以探針以 `/run` 的 drop-in 只把 `ExecStart=` 換成一個一直執行、忽略 `SIGHUP` 的替身，`ExecStartPre=`、`IPAddressDeny=` 與其他設定都是映像的。

- 啟動 agent：`ip-deny-check` 記下 ingress 與 egress 都至少一個程式；root 的程序移進 agent 的 cgroup 後送往 `127.0.0.1` 的 UDP 以 `Operation not permitted` 失敗，在 cgroup 外送同樣的封包成功（出口規則只比對 agent 的帳號，所以擋下它的只有 `IPAddressDeny=`）。
- 對照：drop-in 清空 `IPAddressDeny=` 再重啟（等同 systemd 掛不上程式）：`ip-deny-check` 記下 `is not attached ... (0 ingress, 0 egress programs)`，agent 仍是 active/running，cgroup 裡送出的 UDP 成功。
- `/etc/resolv.conf` 以改名換成寫著 `192.0.2.53` 的檔案：path unit 把它填進預設拒絕的 `nameserver4`；`systemctl reload nftables.service` 之後兩個 table 都在、集合仍有它。
- 停掉檢查的 timer、刪掉 `inet trustiot_agent_allow`、重啟 agent：兩個 table 都回來、集合仍有那個位址，而檢查的日誌沒有載回規則的紀錄（是 agent 的 `ExecStartPre=` 載回的）。探針最後把 `/etc/resolv.conf` 放回原樣。

每次開機都檢查：開機完成、`ssh.service` 在執行（閘道器仍可遠端處理）、沒有等待中的密碼提示、`/var/log/journal` 沒有檔案、置換空間只有 zram、沒有 core dump，而且 `/run/trustiot-back-to-factory` 只在標頭有標記時存在。解鎖的開機另外檢查 `blkid` 讀到的類型、UUID 與最終標籤、標頭記的指紋等於 0x81000001 那把 SRK 的 name，以及兩個 device 的等待時間是 15 分鐘（`systemctl show`）；有標記的開機另外檢查標記的內容（`found_owner` 是 volume 現在的指紋、`sealed_owner` 是另一個）。setup 看 TPM 的過程以它的日誌檢查（`src/boot-results.ts` 的 `tpmLookProblems`）：每次的編號連續、間隔至少 10 秒、最後一次不晚於第一次之後 180 秒、每次的回答都是那個情境允許的。報告裡的 LUKS 標頭與探針輸出不含 token 的 `tpm2-blob` 與 `tpm2_srk`（`src/boot-results.ts` 的 `redactTpmFields`）；指紋與標記是軟體 TPM 的，照原樣留著。

斷電的開機不經過探針：結果磁碟只有 `power-cut <步驟>`（報告裡的 `*.cut.txt`）。重新開始的 setup 只在它寫好新標頭與 keyslot 區之前的每一步斷電：之後它做的和第一次開機的 22 到 26 相同。清除重建只在標記可能丟掉的三個點斷電：之間的步驟與第一次開機相同。手動啟動 agent 不增加開機次數；斷電的情境（含 30）共 29 次開機，實測的時間在[測試報告](../../docs/test-reports/2026-11-06-867-tpm-permanent-failure-wipe.md)（之前的版本見 [Laysi/trustiot-v2#917 的報告](../../docs/test-reports/2026-11-02-917-data-setup-power-cut.md)）。

## 可重現性

映像不是逐位元相同。同樣的輸入（來源映像、工具容器、snapshot 時間點、agent 執行檔、公鑰、本目錄）預期得到同樣的檔案、權限與擁有者，內容除了下列幾處也相同：

- root 檔案系統經 fuse2fs 修改，新增與改過的檔案、目錄的 inode 時間是建置當時的時間，inode 與區塊的配置也可能不同。
- 內容帶有建置日期或時間的檔案：`/etc/shadow` 裡建置建立或修改的帳號（`trustiot-admin`、`trustiot-agent`）的密碼最後變更日（以日計；建置沒有設定 `SOURCE_DATE_EPOCH`），`/var/cache/ldconfig/aux-cache`（記錄函式庫檔案的 inode 與時間），以及 dpkg、apt 與 debconf 在映像裡留下的狀態（`/var/lib/dpkg/status-old`、`/var/cache/debconf`）。
- boot 分割區的 FAT 項目時間固定為來源映像的發佈日；資料分割區全是零。

上面是依建置步驟推得的；兩次建置的逐檔比對（下面的 manifest）2026-10-31 還沒有實際做過。

比對兩次建置以檔案為單位：`check.ts --manifest yes --report <目錄>` 會在報告目錄寫出 root 檔案系統每個檔案的類型、權限、擁有者、大小與 SHA-256（`manifest-root.tsv`），兩份直接 `diff`。

## 容器

`build.ts`、`check.ts` 與 `boot-test.ts` 以 root 身分執行，或 podman 不是 rootless（`podman info` 的 `Rootless` 不是 `true`）時直接拒絕：容器裡跑的是映像自己的程式（套件的安裝腳本、`sshd -G`、agent），只有 rootless 時它們才關在一般使用者的權限裡。`src/host.ts` 的 `containerArgs` 以 rootless podman 執行工具容器：`--cap-add=SYS_ADMIN` 只在容器自己的 user namespace 內有效，讓 fuse2fs、chroot 需要的 bind mount 與 tmpfs 能掛上；`--device=/dev/fuse` 給 fuse2fs；`--security-opt=no-new-privileges`；`--security-opt=label=disable` 讓 SELinux 主機上的容器讀得到掛進去的主機路徑（不改主機檔案的標籤）。輸入一律唯讀掛載，Bun 本身從主機以唯讀掛入，不在容器裡下載。容器要連網（snapshot.debian.org）。
