# 閘道器映像（edge/image）

TrustIoT 閘道器的 SD 卡映像：以官方 Raspberry Pi OS Lite（64 位元，Debian 13 trixie）為底，加上資料分割區、agent、TPM overlay、NTP 設定與只收 SSH 公鑰的登入（Laysi/trustiot-v2#809），以及資料分割區的加密：第一次開機時閘道器自己以 LUKS2 加密、之後由 TPM 自動解鎖，解不開就不掛上、agent 不啟動（Laysi/trustiot-v2#810，「資料分割區加密」）。建置不開機，也不需要閘道器；映像檢查同樣不開機（「映像檢查」）；開機測試在 QEMU 裡以軟體 TPM 開機（「開機測試」）。

本目錄**不做**的事：解不開時清除資料分割區與「回到出廠」的標記（Laysi/trustiot-v2#867）、燒錄 lab 的閘道器並實機開機（Laysi/trustiot-v2#811）、防火牆（Laysi/trustiot-v2#813、Laysi/trustiot-v2#814）、螢幕、USB 與序列埠主控台（Laysi/trustiot-v2#815）、A/B 分割區、唯讀 root、secure boot 與發佈（Laysi/trustiot-v2#216）、工廠寫入的 `/etc/trustiot/agent.toml`、閘道器憑證與根憑證（Laysi/trustiot-v2#80）。

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

1. initramfs 裡官方的 `resize_early` 把 root（分割區 2）延伸到卡片結尾，`set_partuuid` 給卡片一個隨機的磁碟識別碼，並改寫 `/etc/fstab` 與 `cmdline.txt` 裡所有的 PARTUUID（包含資料分割區那一行），之後把 `resize` 從 `cmdline.txt` 拿掉；接著 `systemd-growfs-root` 延伸 root 的檔案系統。
2. machine ID 是 `uninitialized`，所以這是第一次開機：`regenerate_ssh_host_keys` 產生這台閘道器自己的 SSH 主機金鑰（映像裡沒有任何主機金鑰）。
3. `trustiot-data-setup` 在全是零的資料分割區上建立 LUKS2、登錄這台閘道器的 TPM、在裡面做出檔案系統，`systemd-cryptsetup` 以 TPM 解鎖後掛在 `/var/lib/trustiot-agent`（「資料分割區加密」）；沒有 TPM 時分割區維持全零、不掛上，agent 不啟動。systemd-timesyncd 向 `time.cloudflare.com` 校時，sshd 只接受 `trustiot-admin` 的公鑰登入。
4. agent 已啟用，但工廠（Laysi/trustiot-v2#80）寫入 `/etc/trustiot/agent.toml` 之前會因找不到設定而結束，依 unit 的 `Restart=always` 每分鐘重試一次。

實機的第一次開機在 Laysi/trustiot-v2#811 驗證；本目錄的檢查以映像自己的 `parted` 在一份分割表上重做 `resize_early` 的那一步（「映像檢查」），資料分割區的加密、解鎖與解不開時的行為由開機測試在 QEMU 裡證明（「開機測試」）。

燒錄要寫入映像的每一個 byte（上面的 `dd` 或 Imager 都是）：只寫非零區塊的工具會在資料分割區留下卡片原本的內容，例如之前那台閘道器的 LUKS 標頭：放進別台閘道器時解不開、不掛上（fail closed），放回原來那台則解開舊的資料；兩種情況都不會重新加密。要重用的卡，先整張清成零再燒錄（之後的清除流程在 Laysi/trustiot-v2#867）。

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
   - LUKS2、UUID 是釘選的 `9da7f680-5912-4d5c-bca6-d46c83e064f8`、標籤 `trustiot-data-new`：上次的加密沒做完（那時還沒有任何資料），從頭再做。
   - 同一個 UUID、標籤 `trustiot-data`：已經加密好，只確認 keyslot 只有 TPM 的一個。
   - 其他任何內容（別的檔案系統、別的 LUKS、認不出但不是零）：拒絕，不格式化。unit 失敗，解鎖因此不執行，分割區不掛上，agent 不啟動。
3. 加密：需要 `/dev/tpmrm0`，沒有 TPM 就拒絕（分割區維持全零，下一次有 TPM 的開機才加密）。以 `/dev/random` 的 48 bytes 做暫時金鑰，只放在 `/run`（記憶體）；`cryptsetup luksFormat`（LUKS2、aes-xts-plain64、512 位元金鑰、4096 bytes 的 sector、釘選的 UUID、標籤 `trustiot-data-new`、keyslot 0；這把金鑰的 KDF 用 pbkdf2 1000 次，因為它在幾秒後就被清掉，argon2 在 2 GB 的閘道器上只多花記憶體與時間）。`systemd-cryptenroll --tpm2-device=auto --tpm2-pcrs= --tpm2-public-key= --tpm2-pcrlock= --tpm2-with-pin=no`：把 volume key 密封在 TPM 裡，明確不綁 PCR（systemd 預設綁 PCR 7）、不要 PIN，用 systemd 放在 0x81000001 的 SRK，不碰閘道器身分金鑰的 0x81000100。接著以 TPM token 開啟（證明 TPM 解得開才往下）、做 ext4、關閉；`cryptsetup luksKillSlot` 清掉暫時金鑰的 keyslot 0，確認只剩 TPM 的 keyslot，最後把標籤改成 `trustiot-data`。
4. 每次開機最後確認釘選的 UUID 只出現在這個分割區上，`/dev/disk/by-uuid/` 也指向它：解鎖以這個 UUID 找 volume，另一個磁碟若帶著同樣的 UUID 就拒絕。這只在 setup 執行的當下確認，之後才接上的磁碟不在範圍內；能在開機時接上磁碟的人拿得到閘道器本體，本來就能改 root（「檢查證明什麼、不證明什麼」）。

LUKS 標頭因此只有一個 keyslot 與一個 `systemd-tpm2` token（`tpm2-pcrs` 是空的），`systemd-cryptenroll <分割區>` 只列出 `tpm2`；開機測試以 `cryptsetup luksDump --dump-json-metadata` 檢查（`src/data.ts` 的 `luksHeaderProblems`）。每次開機的確認只看 keyslot 的種類，也就是沒有密碼 keyslot；token 的數量、PCR 與 PIN 不在每次開機確認：能改標頭的人已經是閘道器上的 root，而綁了 PCR 或要 PIN 的 token 在 `headless=true` 下只會解不開（fail closed）。

### 解鎖

- `/etc/crypttab`：`trustiot_data UUID=9da7f680-5912-4d5c-bca6-d46c83e064f8 none luks,tpm2-device=auto,headless=true,nofail,x-systemd.device-timeout=15min`。以 LUKS 標頭的 UUID 指定：第一次開機 `set_partuuid` 換掉的 PARTUUID 不影響它，映像裡的 crypttab 也不必在第一次開機後改寫。這個 UUID 每台閘道器都一樣，不是祕密。
- 等 volume 出現的時間：systemd 從開機起就等 volume（以 UUID）與解鎖後的 `/dev/mapper/trustiot_data` 出現，`nofail` 的項目預設只等 90 秒。第一次開機的 setup 要讀完整個分割區再加密，在慢的卡上可能超過 90 秒，那次開機 volume 就不會解鎖、agent 不會啟動（雖然 setup 成功了）。crypttab 與 fstab 因此都設 `x-systemd.device-timeout=15min`：比 setup 自己的上限（10 分鐘）加上等 TPM 的 90 秒長，仍是有限的。generator 把它變成那兩個 device 的 `JobRunningTimeoutSec=`，不傳給 `systemd-cryptsetup`。開機測試以限速的磁碟做第一次開機，setup 在開機後兩分鐘以上才完成，同一次開機仍解鎖、掛上。
- systemd 的 `systemd-cryptsetup-generator` 由它產生 `systemd-cryptsetup@trustiot_data.service`；drop-in（`files/data/systemd-cryptsetup-trustiot-data.conf`）讓它 `Requires=`/`After=` setup、`After=` TPM 的裝置，`TimeoutStartSec=2min` 是上限，`LimitCORE=0`。`/etc/fstab` 從 `/dev/mapper/trustiot_data` 掛上。
- 不綁 PCR：更新核心、韌體或 `config.txt` 都不影響解鎖。代價：拿到閘道器本體、能改 root 或 `cmdline.txt` 的人，可以讓它在下一次開機時解鎖後讀取資料；防這一點要 secure boot 加上 PCR 的綁定（Laysi/trustiot-v2#216）。本 issue 擋的是只拿到 SD 卡的人：volume key 只有這顆 TPM 解得開，TPM 被清除（`TPM2_Clear`）後也解不開。
- 解不開時資料不救：沒有 recovery key，閘道器回到工廠重新登錄；清除資料分割區與「回到出廠」的標記在 Laysi/trustiot-v2#867。

### 解不開時（fail closed）

- `headless=true`：`systemd-cryptsetup` 絕不詢問密碼，volume 也沒有密碼 keyslot 可以退回。TPM 不在時，setup 先等 `/dev/tpmrm0` 出現（systemd 預設等 device 的 90 秒），`systemd-cryptsetup` 再等 TPM 30 秒（crypttab 預設的 `token-timeout=`），等不到就失敗（開機測試的日誌：`Timed out waiting for security device` 後 `No passphrase or recovery key registered.`）；換了 TPM 或 TPM 被清除時解封失敗（`Failed to unseal secret using TPM2: State not recoverable`），一秒內結束；`TimeoutStartSec=2min` 是上限。
- `nofail`（crypttab 與 fstab 都有）：開機照常完成，ssh 可以登入診斷；資料分割區不掛上。
- agent 不啟動：drop-in `files/data/trustiot-agent-data.conf` 給 agent 的 unit `RequiresMountsFor=/var/lib/trustiot-agent`（unit 的 `StateDirectory=` 本來就隱含它）與 `AssertPathIsMountPoint=/var/lib/trustiot-agent`：掛載失敗時 agent 的啟動因相依失敗而取消；就算有人手動啟動，掛載點不是掛載點時 assert 失敗，agent 不會把資料寫到 root。
- 這裡只做到「不掛上、不啟動」，不清除也不重新格式化（Laysi/trustiot-v2#867）。

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
| agent | 執行檔 `/usr/bin/trustiot-agent`（0755）、[edge/agent/systemd](../agent/systemd) 的檔案原樣安裝（目前是 agent 的 unit 與 sysusers 設定，以及 agent 帳號的出口規則：規則檔、載入它的 unit 與 `nftables.service` 的 drop-in，安裝位置照 [edge/agent/README.md](../agent/README.md)「出口規則」。例外是 fieldbus 放行集合的 socket 與 service（Laysi/trustiot-v2#864）：建置還沒有建出元件的執行檔，它們列在 `src/pins.ts` 的 `AGENT_SYSTEMD_NOT_INSTALLED`，由 Laysi/trustiot-v2#866 與執行檔一起安裝。那裡多了檔案，例如 agent 依賴的另一個 unit，`bun run --cwd edge/image test` 就失敗，直到 `AGENT_SYSTEMD_FILES` 列出它的安裝位置；agent 的 unit 依賴一個沒有安裝的 unit 時也失敗），`systemd-sysusers` 建立 `trustiot-agent` 使用者，unit 設為開機啟動；在映像裡執行一次 `trustiot-agent --version`，確認它能在映像的 C 函式庫上執行 |
| 帳號 | 刪除官方的預設帳號 `pi`；建立 `trustiot-admin`：密碼欄是 `*`（沒有任何密碼），`~/.ssh/authorized_keys` 是建置時給的公鑰，`/etc/sudoers.d/trustiot-admin` 允許免密碼 sudo（帳號沒有密碼，sudo 無從詢問；工廠的 `--remote-sudo` 需要。正式產線的縮限在 Laysi/trustiot-v2#219）。root 的密碼欄維持官方的 `*` |
| SSH | `/etc/ssh/sshd_config.d/00-trustiot.conf`：`PermitRootLogin no`、`PasswordAuthentication no`、`KbdInteractiveAuthentication no`、`PermitEmptyPasswords no`、`AuthenticationMethods publickey`、`AllowUsers trustiot-admin`、`X11Forwarding no`。sshd 對每個關鍵字採用第一個讀到的值，這個檔案最先被讀到。刪除官方指向改名精靈的 `rename_user.conf`，啟用 `ssh.service`；主機金鑰在第一次開機產生。建置以 `sshd -G` 與對三個連線（`trustiot-admin`、`root`、`pi`）的 `sshd -T -C` 確認有效設定，並確認所有 sshd 設定檔都沒有 `Match` 區塊（`Match` 只對符合的連線生效，`sshd -G` 不會算進去） |
| 首次設定 | cloud-init 會從 boot 分割區讀帳號、公鑰與密碼（拿得到 SD 卡的人都能寫），`userconfig.service` 是主控台上的建立帳號精靈，也讀 boot 分割區的 `userconf.txt`。建置寫入 `/etc/cloud/cloud-init.disabled`，並停用、遮蔽（`systemctl mask`）cloud-init 的每個 unit 與 `userconfig.service`：只有標記檔時，kernel 參數 `cloud-init=enabled` 仍會讓 cloud-init 執行，遮蔽的 unit 則不會被啟動。boot 分割區上 cloud-init 讀的 `user-data`、`network-config`、`meta-data`（官方內容全是註解）一併刪除。cloud-init 的套件不移除：移除它就得一起移除 `rpi-cloud-init-mods`，而那個套件帶著 netplan 交給 NetworkManager 的設定（`/usr/lib/netplan/00-network-manager-all.yaml`），第一次開機的網路就和官方映像不同 |
| 校時 | `/etc/systemd/timesyncd.conf.d/50-trustiot.conf`：`NTP=time.cloudflare.com`、`FallbackNTP=`（清掉內建的 Debian pool，防火牆不放行它） |
| 資料分割區 | `/etc/fstab` 加一行（「分割區」）、建立掛載點（root、0700）；[`files/data/`](files/data) 的每個檔案原樣安裝到 `src/pins.ts` 的 `DATA_FILES` 指定的位置：setup 的程式與 unit、`/etc/crypttab`、解鎖與 agent 的 drop-in、journald 與 rpi-swap 的設定（「資料分割區加密」、「日誌與置換空間」）。分割區本身不寫，全是零 |
| TPM | 編譯 [`files/tpm-slb9670-ce0-overlay.dts`](files/tpm-slb9670-ce0-overlay.dts)（以映像裡的 `dtc`）放到 boot 分割區的 `overlays/`，`config.txt` 最後加上 `[all]`、`dtparam=spi=on`、`dtoverlay=tpm-slb9670-ce0`。官方的 `tpm-slb9670` 把 TPM 放在 CE1，閘道器的模組在 CE0（[TPM 可行性測試](../../docs/feasibility/2026-10-12-tpm-poc.md)）。不碰 TPM 裡的任何金鑰 |
| 整理 | 安裝套件寫的日誌清回官方映像的空檔，刪除 `useradd` 留下的 `/etc/passwd-` 等備份；確認 machine ID 仍是 `uninitialized` |

不改的：核心、`cmdline.txt`（序列埠主控台留給 Laysi/trustiot-v2#815）、網路設定（NetworkManager 與 netplan，和官方映像相同；實機上的行為在 Laysi/trustiot-v2#811 確認）、avahi、Wi-Fi 與藍牙服務（防火牆與對外介面留給 Laysi/trustiot-v2#96 的子 issue）、主機名稱 `raspberrypi`、套件來源設定、apt 的套件清單。

### 套件更新

建置不安裝來源映像之後的更新：Raspberry Pi 的套件庫沒有 snapshot 服務，裝了就無法重現；Debian 的更新雖然可以從 snapshot 重現，但更新的套件若帶 initramfs hook，就要重新產生 boot 分割區上的 initramfs，建置目前不做這一步。建置記錄 snapshot 裡有更新版本的套件（`build-info.json` 的 `upgradable`，2026-10-31 的建置是 12 個，多數是 Debian 的安全更新）；目前更新的方式是換新的官方映像。建置時套用 Debian 安全更新是待決定的後續事項。

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

`check.ts` 只讀完成的映像、來源映像、agent 執行檔與公鑰檔，和建置共用預期值（`src/pins.ts`、`src/files.ts`），但不共用建置的步驟。每一項：

| 檢查 | 內容 |
| --- | --- |
| 分割表 | 磁碟識別碼、三個分割區的位置、大小與類型和預期完全相同；4 MiB 對齊、不重疊、boot 在最前、root 在最後；映像檔大小 |
| 檔案系統 | root 的 `e2fsck -fn`、boot 的 `fsck.vfat -n` 都沒有錯誤 |
| 資料分割區 | 整個分割區全是零（`cmp`），`blkid -p` 認不出任何格式；fstab 那一行（`/dev/mapper/trustiot_data`、ext4、`nodev,nosuid,noexec,nofail,x-systemd.device-timeout=15min`、fsck 順序 2），root 與 boot 仍以 PARTUUID 指定；掛載點 root 擁有、0700，root 上的掛載點裡沒有任何檔案；agent 的 unit 有 `StateDirectory=trustiot-agent`，drop-in 有 `RequiresMountsFor=` 與 `AssertPathIsMountPoint=` |
| 資料分割區加密 | `files/data/` 的每個檔案與 repo 逐位元組相同、權限正確；`/etc/crypttab` 只有一行：`trustiot_data`、釘選的 UUID、`none`、`luks,tpm2-device=auto,headless=true,nofail,x-systemd.device-timeout=15min`；在映像裡執行映像自己的 `systemd-cryptsetup-generator`，產生的 unit 以那個 UUID 與選項（`x-systemd.*` 以外的）attach、沒有預設相依，只被 `cryptsetup.target` wants（不是 requires）；那個 generator 與 `systemd-fstab-generator` 給 volume 與 `/dev/mapper/trustiot_data` 的 device 寫了 `JobRunningTimeoutSec=15min`；以 `systemd-analyze verify` 載入 setup、解鎖與 agent 的 unit；agent 已啟用 |
| 日誌與置換空間 | `systemd-analyze cat-config` 算出 journald 的 `Storage=volatile`、`ForwardToSyslog=no`；沒有安裝 syslog daemon；`rpi-systemd-config` 算出 rpi-swap 的 `Mechanism=zram`，root 上沒有 `/var/swap`；setup、解鎖與 agent 的 unit 都有 `LimitCORE=0` |
| 第一次開機 | 以映像的 `parted` 對 16 GiB 的檔案（只有映像的分割表）執行 `resize_early` 的指令：root 延伸到結尾，boot 與資料分割區不動；machine ID 是 `uninitialized`、沒有 SSH 主機金鑰、主機金鑰的產生服務已啟用、`cmdline.txt` 有 `resize` 與分割區 2 的 root |
| agent | 執行檔與給的檔案 SHA-256 相同、root 擁有、0755、aarch64 ELF、在映像裡執行 `--version` 成功；unit 與 sysusers 和 repo 的檔案逐位元組相同、unit 已啟用、`trustiot-agent` 使用者、`tss` 群組、tpmrm 的 udev 規則；映像裡沒有 `/etc/trustiot/agent.toml` |
| 套件 | cryptsetup-bin、systemd-cryptsetup、tpm-udev、tpm2-tools 是釘選版本且已完成安裝，`dpkg --audit` 為空 |
| TPM overlay | boot 分割區上的 `.dtbo` 與 repo 的 `.dts` 重新編譯的結果相同；`config.txt` 對所有型號有那兩行；以 `dtmerge` 疊到 `bcm2711-rpi-4-b.dtb` 後，spi0 有 reg 0 的 `infineon,slb9670`、spidev0 停用 |
| NTP | `systemd-analyze cat-config` 算出的有效設定只有釘選的伺服器、沒有 fallback；timesyncd 已啟用 |
| sshd | `sshd -G`，以及以暫時的主機金鑰（放在 chroot 的 tmpfs，不進映像）對 `trustiot-admin`、`root`、`pi` 三個連線各跑一次 `sshd -T -C`：有效設定是「映像內容」的每一項；所有 sshd 設定檔沒有 `Match` 區塊；`ssh.service` 已啟用、改名精靈的設定已刪 |
| 管理者 | `authorized_keys` 正好是給的公鑰（不含註解），映像自己的 `ssh-keygen` 讀得懂每一把且指紋相同；家目錄與 `.ssh` 0700、`authorized_keys` 0600；密碼欄是 `*`；sudoers 檔的內容、0440、`visudo --check --strict`；`/etc/sudoers.d` 只多了這個檔案，`/etc/sudoers` 與來源映像原有的規則檔都沒改；`sudo` 群組沒有成員 |
| 帳號 | 沒有任何帳號有可用的密碼（密碼欄是雜湊或空白都算可用）、沒有群組密碼、`pi` 已刪、沒有 `/etc/shadow-` 等備份 |
| 首次設定 | 映像裡每個 cloud-init 的 unit 與 `userconfig.service` 都是 `masked`、有 `/etc/cloud/cloud-init.disabled`、boot 分割區沒有 `user-data`、`network-config`、`meta-data` |
| 祕密與內網名稱 | 建置新增或改過的每個檔案（root 與 boot，與來源映像逐檔比對得出）：文字檔沒有私鑰區塊，設定檔（`/etc`、`/home`、`/root` 與建置寫的檔案）沒有私有網段位址或只在內網解析的名稱（報告只寫行號與規則，不寫出比對到的內容）；這些檔案再以 `.prototools` 釘選的 gitleaks（`gitleaks dir`）掃描；沒有 `~/.ssh/id_*`。gitleaks 的發現只有 `src/pins.ts` 的 `KNOWN_FINDINGS` 列出的才接受：路徑、規則、`dpkg-query -S` 查到的套件與整個檔案的 SHA-256 都要相同，建置自己寫的檔案一律不接受。目前列的是 libtss2-fapi1t64 4.1.3-1.2 的兩個 TPM FAPI profile（`/etc/tpm2-tss/fapi-profiles/P_ECCP384SHA384.json`、`/etc/tpm2-tss/fapi-profiles/P_RSA3072SHA384.json`）：`generic-api-key` 比對到的是 `authPolicy`，NV index 授權策略的公開摘要，不是祕密（2026-10-31 檢視）。套件版本一變，檔案的 SHA-256 跟著變，檢查就不通過，要重新檢視後更新清單 |

### 檢查證明什麼、不證明什麼

- 拿得到 SD 卡或閘道器本體的人，等同拿到 root：可以改 boot 分割區的 `cmdline.txt`（例如 `init=/bin/sh`），或直接改 root 檔案系統。遮蔽 cloud-init 與 userconfig 只是拿掉官方映像現成的入口，不是對實體存取的防護。資料分割區加密只擋拿到 SD 卡的人；拿到閘道器本體的人能改 root，讓下一次開機解鎖後讀取資料（TPM 沒有綁 PCR，「資料分割區加密」），那要等 secure boot（Laysi/trustiot-v2#216），Laysi/trustiot-v2#216 應把這一點列為前提。
- `check.ts` 不開機：它證明加密、解鎖與 fail closed 需要的檔案與設定都在映像裡、而且是 systemd 讀得懂的形式；它們開機時真的那樣運作，由開機測試證明（「開機測試」）。
- `check.ts` 驗的是以 `build.ts` 從已知輸入做出的映像，不是第三方映像的驗收工具：它列出建置改了來源映像的哪些路徑（`delta.tsv`），但不判斷每個改動是否惡意，例如被換掉的執行檔。燒錄前要確認的是映像的 SHA-256 與建置記錄（`trustiot-gateway.img.sha256`、`build-info.json`）相同，不是重跑檢查。

## 開機測試

`boot-test.ts` 在 QEMU 裡開機映像的副本，證明資料分割區的加密、解鎖與解不開時的行為。不需要閘道器、KVM、root、loop device 或建置主機上的 device mapper：虛擬機的核心自己載入 dm-crypt，TPM 是 swtpm（軟體 TPM），全部在和建置相同的 rootless 工具容器裡。在 arm64 Linux 的建置主機上執行（「建置主機」）；它不在 CI 裡跑：

```sh
bun edge/image/boot-test.ts --image <輸出目錄>/trustiot-gateway.img --report <報告目錄>
```

結束碼：全部通過為 0，有一項不通過為 1，用法錯誤為 2，無法執行為 3。`--report`（repo 之外、新的或空的目錄）收到每次開機的探針輸出（`*.probe.txt`）、序列埠主控台的紀錄（`*.serial.log`）、第一次開機寫出的 LUKS 標頭（`luks-header.json`）與每一項的結果（`boot-test.json`）。

測試的副本只比映像多兩樣東西，boot 與資料分割區逐位元組相同：

- 核心：QEMU 跑不了 Raspberry Pi 的核心，改用 Debian 的 arm64 核心（版本見「釘選」），模組複製到副本的 root，以 `-kernel`／`-initrd` 開機（不經過官方的 initramfs，所以第一次開機的 `resize_early` 與 `set_partuuid` 不執行；它們在「映像檢查」與 Laysi/trustiot-v2#811 驗證）。TPM 是 QEMU 的 `tpm-tis-device`（記憶體映射的 TIS 介面；閘道器的是 SPI 上的 TIS），它的驅動 `tpm_tis` 在 Debian 的 arm64 核心沒有編譯，測試以同一版核心的原始碼（`linux-source-6.12`）與 headers 只編譯這一個模組。
- 探針：[`boot-test/probe.sh`](boot-test/probe.sh) 與它的 unit。開機完成後（`systemctl is-system-running --wait`）記錄 setup、解鎖、掛載、agent 與 ssh 的 unit 狀態、掛載點、LUKS 標頭、keyslot、等待中的密碼提示、置換空間、日誌目錄、core dump 與相關的日誌，寫到另一個虛擬磁碟後關機。核心參數 `trustiot_test.start-agent=1` 讓它在記錄之後再以 root 手動啟動 agent 兩次（`systemctl start trustiot-agent`，以及略過相依的 `--job-mode=ignore-dependencies`），記錄結果。`src/boot-results.ts` 判斷每次開機（單元測試在 `tests/boot-results.test.ts`）。

斷電的開機（13 以後）用另一份「斷電副本」：測試副本再加上 [`boot-test/power-cut.sh`](boot-test/power-cut.sh)，以 `/usr/local/sbin/` 裡的 `cryptsetup`、`systemd-cryptenroll`、`mke2fs`、`dd` 連結指向它（systemd 給 service 的 PATH 先找 `/usr/local/sbin`）。沒有核心參數 `trustiot_test.power-cut=<步驟>`、或呼叫者不在 `trustiot-data-setup.service` 裡時，它直接執行真正的程式；有的話，setup 做完那一步（`before-relabel` 是改標籤之前），它把 `power-cut <步驟>` 寫到結果磁碟，接著以 sysrq `b` 立刻重置核心（不 sync、不卸載；QEMU 以 `-no-reboot` 執行，所以就此結束）：虛擬機還沒交給虛擬磁碟的資料都不見。它模擬的是在兩步之間斷電，不是真的斷電：已經交給虛擬磁碟的寫入都留著（磁碟是 `cache=unsafe`），不模擬一步做到一半時斷電，也不模擬卡片自己的快取遺失、寫一半或重排寫入。斷電之後在建置主機讀分割區（`blkid` 與 LUKS 標頭），確認停在那一步留下的狀態，再以同一顆 TPM 開下一次機。

依序開機（swtpm 的狀態目錄就是一顆 TPM，A、B、C、F 與 P-<步驟> 是不同的 TPM）。1 到 4 各用映像的一份副本，5 到 12 都是同一份，13 以後各用斷電副本的一份副本（資料分割區全是零）：

| 開機 | 磁碟與 TPM | 必須看到 |
| --- | --- | --- |
| 1-foreign-fs | 資料分割區放了另一個 ext4，TPM F | setup 拒絕（unit 失敗）、分割區逐位元組不變、不掛上、agent 沒有啟動 |
| 2-foreign-luks | 資料分割區放了另一個 UUID 的 LUKS2，TPM F | 同上 |
| 3-not-empty | 資料分割區全是零，只有 1 GiB 處的 4 KiB 隨機位元組（`blkid` 認不出格式），TPM F | 同上（「認不出但不是零」） |
| 4-interrupted-setup | 資料分割區是釘選的 UUID、標籤 `trustiot-data-new`、有一個密碼 keyslot 的 LUKS2（上次的加密沒做完），TPM C | 重新加密，TPM C 解鎖、掛上，標頭只剩 TPM 的 keyslot |
| 5-first-boot-no-tpm | 映像原樣，沒有 TPM | setup 拒絕、分割區仍全是零、不掛上、agent 沒有啟動 |
| 6-first-boot-tpm-a | TPM A，磁碟讀取限速 16 MiB/s | 加密、只剩 TPM 的 keyslot（LUKS 標頭在虛擬機裡與建置主機上各讀一次）、以 `/dev/mapper/trustiot_data` 掛上（`nodev,nosuid,noexec,noatime`）、根目錄 `trustiot-agent` 0700、agent 啟動；setup 在開機兩分鐘以上才完成（超過 systemd 預設等 device 的 90 秒），同一次開機仍解鎖、掛上；在分割區裡寫一個隨機的標記，它不出現在磁碟的原始位元組裡 |
| 7-tpm-a-again | TPM A | 不重新格式化，解鎖、掛上，讀回標記 |
| 8-other-tpm | TPM B；開機後探針手動啟動 agent | 解不開：解鎖失敗而不是等到時限、沒有密碼提示、不掛上、agent 沒有啟動、root 上的掛載點是空的。手動啟動：`systemctl start` 因相依的 job 失敗而失敗（它再拉起一次解鎖，之後仍沒有等待中的密碼提示），略過相依的那次因 `AssertPathIsMountPoint=` 失敗（assert 有評估、`AssertResult=no`）；兩次都在時限內結束，agent 從未離開 inactive、沒有程序執行 `/usr/bin/trustiot-agent`，仍不掛上，root 上的掛載點仍是空的 |
| 9-tpm-removed | 沒有 TPM | 同上 |
| 10-tpm-a-cleared | TPM A 的副本，先以 `tpm2_clear` 清除 | 同上 |
| 11-tpm-a-last | TPM A | 之前的失敗沒有破壞任何東西：解鎖、讀回標記，LUKS 標頭和第一次開機寫的相同，標記仍不在磁碟的原始位元組裡 |
| 12-duplicate-uuid | TPM A，另接一個帶著同樣 UUID 的 LUKS2 磁碟 | setup 拒絕、不解鎖、不掛上、agent 沒有啟動，兩個磁碟都不變 |
| 13a-power-cut-luks-format | 斷電副本，TPM P-luks-format，`luksFormat` 之後斷電 | 結果磁碟是 `power-cut luks-format`；分割區是標籤 `trustiot-data-new`、只有 keyslot 0、沒有 token |
| 13b-after-luks-format | 同一份磁碟與 TPM | 重新加密（`holds a setup that did not finish`），解鎖、掛上，標頭只剩 TPM 的 keyslot（虛擬機裡與建置主機上各讀一次） |
| 14a-power-cut-tpm-enroll | TPM 登錄之後斷電 | 標籤 `trustiot-data-new`、keyslot 0 與 1、keyslot 1 上的 TPM token |
| 14b-after-tpm-enroll | 同上 | 同 13b |
| 15a-power-cut-mkfs | `mke2fs` 之後斷電（解開的 volume 還開著） | 同 14a |
| 15b-after-mkfs | 同上 | 同 13b |
| 16a-power-cut-kill-slot | 清掉暫時金鑰的 keyslot 之後斷電 | 標籤 `trustiot-data-new`、只有 keyslot 1 與它的 TPM token |
| 16b-after-kill-slot | 同上 | 同 13b |
| 17a-power-cut-before-relabel | 改標籤之前斷電 | 同 16a |
| 17b-after-before-relabel | 同上 | 同 13b |
| 18a-power-cut-after-relabel | 改標籤之後斷電 | 標籤 `trustiot-data`、只有 keyslot 1 與它的 TPM token |
| 18b-after-after-relabel | 同上 | 不重新加密（`is encrypted and opens only with the TPM`），解鎖、掛上，標頭只有 TPM 的 keyslot |
| 19a-power-cut-zero-header-again | 15a 斷電後的磁碟與 TPM 的副本；重新開始的 setup 把分割區開頭 16 MiB 清成零之後斷電 | 分割區開頭是零、`blkid` 認不出格式，後面還有上一次做出的（加密的）檔案系統 |
| 19b-after-zero-header-again | 同上 | **已知問題（KNOWN）**：setup 拒絕（`has no known format but is not empty`），應該重新加密卻沒有；至少 fail closed：不解鎖、不掛上、agent 沒有啟動，分割區不變。Bug：Laysi/trustiot-v2#917；見[測試報告](../../docs/test-reports/2026-11-01-890-boot-test-power-cut.md)的「已知問題」 |

每次開機都檢查：開機完成、`ssh.service` 在執行（閘道器仍可遠端處理）、沒有等待中的密碼提示、`/var/log/journal` 沒有檔案、置換空間只有 zram、沒有 core dump。解鎖的開機另外檢查 `blkid` 讀到的類型、UUID 與最終標籤，以及兩個 device 的等待時間是 15 分鐘（`systemctl show`）。報告裡的 LUKS 標頭與探針輸出不含 token 的 `tpm2-blob` 與 `tpm2_srk`（`src/boot-results.ts` 的 `redactTpmFields`）。

斷電的開機不經過探針：結果磁碟只有 `power-cut <步驟>`（報告裡的 `*.cut.txt`）。標成 KNOWN 的項目把映像現在的錯誤行為釘住：行為不變時算通過但顯示 KNOWN，行為一改變就不通過，修正的人把那一項改回應有的預期。手動啟動 agent 不增加開機次數；斷電的情境多 14 次開機，實測的時間在[測試報告](../../docs/test-reports/2026-11-01-890-boot-test-power-cut.md)。

## 可重現性

映像不是逐位元相同。同樣的輸入（來源映像、工具容器、snapshot 時間點、agent 執行檔、公鑰、本目錄）預期得到同樣的檔案、權限與擁有者，內容除了下列幾處也相同：

- root 檔案系統經 fuse2fs 修改，新增與改過的檔案、目錄的 inode 時間是建置當時的時間，inode 與區塊的配置也可能不同。
- 內容帶有建置日期或時間的檔案：`/etc/shadow` 裡建置建立或修改的帳號（`trustiot-admin`、`trustiot-agent`）的密碼最後變更日（以日計；建置沒有設定 `SOURCE_DATE_EPOCH`），`/var/cache/ldconfig/aux-cache`（記錄函式庫檔案的 inode 與時間），以及 dpkg、apt 與 debconf 在映像裡留下的狀態（`/var/lib/dpkg/status-old`、`/var/cache/debconf`）。
- boot 分割區的 FAT 項目時間固定為來源映像的發佈日；資料分割區全是零。

上面是依建置步驟推得的；兩次建置的逐檔比對（下面的 manifest）2026-10-31 還沒有實際做過。

比對兩次建置以檔案為單位：`check.ts --manifest yes --report <目錄>` 會在報告目錄寫出 root 檔案系統每個檔案的類型、權限、擁有者、大小與 SHA-256（`manifest-root.tsv`），兩份直接 `diff`。

## 容器

`build.ts`、`check.ts` 與 `boot-test.ts` 以 root 身分執行，或 podman 不是 rootless（`podman info` 的 `Rootless` 不是 `true`）時直接拒絕：容器裡跑的是映像自己的程式（套件的安裝腳本、`sshd -G`、agent），只有 rootless 時它們才關在一般使用者的權限裡。`src/host.ts` 的 `containerArgs` 以 rootless podman 執行工具容器：`--cap-add=SYS_ADMIN` 只在容器自己的 user namespace 內有效，讓 fuse2fs、chroot 需要的 bind mount 與 tmpfs 能掛上；`--device=/dev/fuse` 給 fuse2fs；`--security-opt=no-new-privileges`；`--security-opt=label=disable` 讓 SELinux 主機上的容器讀得到掛進去的主機路徑（不改主機檔案的標籤）。輸入一律唯讀掛載，Bun 本身從主機以唯讀掛入，不在容器裡下載。容器要連網（snapshot.debian.org）。
