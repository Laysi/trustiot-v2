# 閘道器映像：資料分割區的 LUKS2 加密與 TPM 解鎖（2026-10-31）

對應 issue：Laysi/trustiot-v2#810（父 issue Laysi/trustiot-v2#95）。這份報告記錄資料分割區加密的映像檢查（`check.ts`，不開機）與開機測試（`boot-test.ts`，在 QEMU 裡以軟體 TPM 開機映像的副本）。實機燒錄與開機（SPI 上的 TPM、SD 卡的速度）在 Laysi/trustiot-v2#811，不在這裡；lab 的閘道器全程沒有碰。

結果先講：開機測試 12 次開機、23 項全部通過：第一次有 TPM 的開機加密、以 TPM 解鎖並掛上，換了 TPM、拿掉 TPM、TPM 被清除時都解不開，開機不停在密碼提示、資料分割區不掛上、agent 不啟動；分割區上不是這個映像的內容一律拒絕且不變。映像檢查 19 項通過 18 項，未通過的一項是 main 上既有的問題（agent 的出站規則檔，Laysi/trustiot-v2#828 加入），與本 issue 無關，見「結果」。

## 環境

- **建置主機**：arm64 Linux 虛擬機（沒有 KVM），rootless podman，Bun 1.4.2（`.prototools`）。映像的建置、檢查與開機測試都在這台。
- **工具容器與套件**：都是 `edge/image/src/pins.ts` 的釘選值，snapshot.debian.org `20261030T000000Z`。開機測試另外裝的套件（`BOOT_TEST`）：

  | 套件 | 版本 |
  | --- | --- |
  | linux-image-6.12.113+deb13-arm64、linux-headers-6.12.113+deb13-arm64、linux-source-6.12 | 6.12.113-1 |
  | qemu-system-arm | 1:10.0.13+ds-0+deb13u1 |
  | swtpm | 0.7.1-1.5 |
  | libtss2-tcti-swtpm0t64 | 4.1.3-1.2 |
  | tpm2-tools | 5.7-1+b1 |
  | cryptsetup-bin | 2:2.7.5-2 |
  | make | 4.4.1-2 |

- **映像裡加的套件**：cryptsetup-bin 2:2.7.5-2、systemd-cryptsetup 257.13-1~deb13u1、tpm-udev 0.6+nmu1、tpm2-tools 5.7-1+b1。
- **虛擬機**：`qemu-system-aarch64 -machine virt -cpu max,pauth-impdef=on -smp 4 -m 2048 -accel tcg,thread=multi`，Debian 的 arm64 核心（`tpm_tis` 模組由同版核心原始碼編譯，見 `edge/image/README.md` 的「開機測試」），TPM 是 swtpm 經 `tpm-tis-device`，沒有網路。
- **agent**：main 的 `edge/agent`（含 Laysi/trustiot-v2#828），在建置主機以 `cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent` 建置，SHA-256 `005b8cb1483da5cbdd4b9c251ee6af01f479f27efd1dcafd6289347f5a8ba062`。
- **程式**：分支 `feat/810-data-partition-luks`。映像的建置與開機測試以 commit `d00e85e3` 的內容執行（那次執行在提交前一分鐘開始，工作樹與這個 commit 相同）；映像檢查以 commit `09ad0bcf` 的 `check.ts` 對同一個映像重跑（`09ad0bcf` 只改了 `check.ts` 與 README，映像的檔案相同）。
- **管理者公鑰**：執行時以 `ssh-keygen` 產生的拋棄式 ed25519 金鑰，放在 repo 之外。
- **時段**：2026-10-31（+08:00）。建置、檢查與開機測試 19:40 開始、20:48 結束（開機測試 3359 秒）；映像檢查的重跑 21:50 結束。

## 指令

在 repo 根目錄、arm64 Linux 的建置主機上執行。`<金鑰目錄>`、`<輸出>`、`<報告>`、`<開機報告>` 都是 repo 之外的目錄。

```sh
bun install --cwd edge/image --frozen-lockfile
ssh-keygen -q -t ed25519 -N '' -C '' -f <金鑰目錄>/admin
bun edge/image/build.ts --agent <trustiot-agent> --ssh-key <金鑰目錄>/admin.pub --out <輸出>
bun edge/image/check.ts --image <輸出>/trustiot-gateway.img --agent <trustiot-agent> \
  --ssh-key <金鑰目錄>/admin.pub --report <報告>
bun edge/image/boot-test.ts --image <輸出>/trustiot-gateway.img --report <開機報告>

# CI 會跑的部分
bun run --cwd edge/image typecheck
bun run --cwd edge/image test
```

## 結果

原始資料在 [data/2026-10-31-810-data-partition-luks/](data/2026-10-31-810-data-partition-luks/)：

- `boot-test.txt`：開機測試的輸出（每次開機的 QEMU 時間與每一項的結果）；`boot-test.json`：每一項的結果。
- `<開機>.probe.txt`：每次開機的探針輸出（unit 狀態、掛載、LUKS 標頭、keyslot、密碼提示、置換空間、日誌目錄、core dump 與 setup、解鎖、掛載、agent 的日誌）。
- `luks-header.json`：第一次開機寫出的 LUKS2 標頭（建置主機上以 `cryptsetup luksDump --dump-json-metadata` 讀出）。
- `checks.json`：映像檢查每一項的結果。
- `before-device-timeout.txt`：修正等待時間之前的一次開機（見「第一次開機的等待時間」）。

探針輸出與 LUKS 標頭裡 token 的 `tpm2-blob`（密封在那顆 swtpm 的 volume key）與 `tpm2_srk`（那顆 swtpm 的 SRK 公鑰）寫成 `(left out)`；swtpm 的狀態與磁碟副本在測試結束時隨容器刪除。

### 開機測試

```text
bun edge/image/boot-test.ts --image <輸出>/trustiot-gateway.img --report <開機報告>
23 of 23 checks passed
exit 0 at 20:48:06 (build host)
```

| 開機 | 磁碟與 TPM | QEMU（秒） | 看到的 |
| --- | --- | --- | --- |
| 1-foreign-fs | 資料分割區是另一個 ext4，TPM F | 32 | setup 拒絕（`not this image's data volume; not formatting it`），分割區的 SHA-256 不變 |
| 2-foreign-luks | 另一個 UUID 的 LUKS2，TPM F | 25 | 同上 |
| 3-not-empty | 全零，只有 1 GiB 處的 4 KiB 隨機位元組，TPM F | 26 | setup 拒絕（`has no known format but is not empty; not formatting it`），分割區不變 |
| 4-interrupted-setup | 釘選的 UUID、標籤 `trustiot-data-new`、一個密碼 keyslot，TPM C | 26 | `holds a setup that did not finish ... starting again`，重新加密，TPM C 解鎖、掛上；標頭只剩 TPM 的 keyslot 1 |
| 5-first-boot-no-tpm | 映像原樣，沒有 TPM | 110 | setup 等 `/dev/tpmrm0` 90 秒後拒絕（`no TPM (/dev/tpmrm0)`），分割區仍全是零 |
| 6-first-boot-tpm-a | TPM A，磁碟讀取限速 16 MiB/s | 148 | setup 在 10.0 秒開始、讀完分割區後 141.6 秒判定「第一次開機」、143.8 秒完成；解鎖 144.3 秒、掛上 144.5 秒；agent 啟動；標記寫入且不出現在磁碟的原始位元組裡 |
| 7-tpm-a-again | TPM A | 19 | `is encrypted and opens only with the TPM`，解鎖、掛上，讀回標記 |
| 8-other-tpm | TPM B | 19 | `Failed to unseal secret using TPM2: State not recoverable` → `No passphrase or recovery key registered.`，解鎖開始後約 0.5 秒失敗 |
| 9-tpm-removed | 沒有 TPM | 130 | setup 等 TPM 90 秒後確認標頭；`systemd-cryptsetup` 等 TPM 30 秒（`Timed out waiting for security device`）後失敗 |
| 10-tpm-a-cleared | TPM A 的副本，以 `tpm2_clear` 清除 | 19 | 同 8-other-tpm |
| 11-tpm-a-last | TPM A | 19 | 解鎖、讀回標記；LUKS 標頭與第一次開機寫的相同；標記仍不在原始位元組裡 |
| 12-duplicate-uuid | TPM A，另接一個同 UUID 的 LUKS2 磁碟 | 19 | `the volume UUID ... is on '/dev/vda3 /dev/vdc', expected only /dev/vda3`，解鎖因相依失敗而不執行，兩個磁碟都不變 |

每次開機：`systemctl is-system-running --wait` 結束（結果是 `degraded`：虛擬機沒有 Raspberry Pi 的 EEPROM，`rpi-eeprom-update.service` 失敗；解鎖的開機 agent 因映像沒有 `/etc/trustiot/agent.toml` 而反覆重啟，這是預期的，設定由工廠寫入）、`ssh.service` 在執行、`/run/systemd/ask-password` 是空的、日誌沒有 `Please enter`、`/var/log/journal` 是空的、置換空間只有 `/dev/zram0`、沒有 core 檔。解鎖的開機另外看到：`/dev/mapper/trustiot_data` 以 `rw,nosuid,nodev,noexec,noatime` 掛在 `/var/lib/trustiot-agent`、根目錄 `trustiot-agent trustiot-agent 700`、`systemd-cryptenroll` 只列出 `1 tpm2`、`blkid` 是 `crypto_LUKS`、釘選的 UUID 與標籤 `trustiot-data`、兩個 device 的 `JobRunningTimeoutUSec=15min`。解不開的開機：解鎖是 `failed/exit-code`（不是逾時）、掛載與 agent 的啟動因相依失敗而取消（agent 從未啟動）、root 上的 `/var/lib/trustiot-agent` 是 root 擁有、0700 的空目錄。

第一次開機的 setup 在日誌裡（`6-first-boot-tpm-a.probe.txt`）：

```text
[   10.030380] systemd[1]: Starting trustiot-data-setup.service - TrustIoT data partition encryption...
[  141.560679] trustiot-data-setup[439]: /dev/vda3 is empty: first boot
[  141.607548] trustiot-data-setup[439]: encrypting /dev/vda3 (LUKS2, aes-xts-plain64)
[  142.069702] trustiot-data-setup[439]: enrolling this gateway's TPM (no PCRs)
[  142.574790] systemd-cryptenroll[758]: New TPM2 token enrolled as key slot 1.
[  143.380559] trustiot-data-setup[439]: wiping the key's keyslot: from now on only this TPM opens the volume
[  143.413733] trustiot-data-setup[774]: Nothing to read on input.
[  143.645681] trustiot-data-setup[439]: done: /dev/vda3 is encrypted and opens only with this gateway's TPM
[  144.260895] systemd[1]: Finished systemd-cryptsetup@trustiot_data.service - Cryptography Setup for trustiot_data.
[  144.512493] systemd[1]: Mounted var-lib-trustiot\x2dagent.mount - /var/lib/trustiot-agent.
```

`Nothing to read on input.` 是 `cryptsetup luksKillSlot --batch-mode` 讀不到剩下的密碼時的訊息，keyslot 0 照樣清掉（之後只剩 keyslot 1）。

### 第一次開機的等待時間

`before-device-timeout.txt` 是同一天較早的一次開機測試（commit `b85a6dfc` 的映像，crypttab 與 fstab 還沒有 `x-systemd.device-timeout`）。沒有 TPM 的第一次開機裡，解鎖在 96.69 秒因「相依失敗」取消，比 setup 開始（96.75 秒）還早：volume 的 device 等到 systemd 預設的 90 秒就放棄了。setup 若比這更晚完成（慢的卡上讀完 2 GiB 再加密），那次開機 volume 就不會解鎖。加上 15 分鐘的等待後，同樣的開機（5-first-boot-no-tpm）解鎖在 setup 拒絕之後（106.68 秒）才取消；限速的第一次開機（6-first-boot-tpm-a）setup 在 143.8 秒完成，同一次開機解鎖、掛上。

### 映像檢查

```text
bun edge/image/check.ts --image <輸出>/trustiot-gateway.img --agent <trustiot-agent> --ssh-key <金鑰目錄>/admin.pub --report <報告>
PASS  data partition is all zeros (the gateway encrypts it on its first boot): 2147483648 bytes of zeros, no format blkid knows
PASS  data partition encryption, unlock and mount are installed: 7 files as in files/data; /etc/crypttab trustiot_data UUID=9da7f680-5912-4d5c-bca6-d46c83e064f8 none luks,tpm2-device=auto,headless=true,nofail,x-systemd.device-timeout=15min; generated systemd-cryptsetup@trustiot_data.service matches; device timeout 15min for the volume and the unlocked volume
PASS  data partition is mounted at the agent's state directory, and only from the unlocked volume: /dev/mapper/trustiot_data /var/lib/trustiot-agent ext4 defaults,noatime,nodev,nosuid,noexec,nofail,x-systemd.device-timeout=15min; the agent requires the mount and asserts it
PASS  logs, swap and core dumps stay off the root partition: journald Storage=volatile, ForwardToSyslog=no, no syslog daemon; rpi-swap Mechanism=zram, no /var/swap; LimitCORE=0 for the setup, the unlock and the agent
PASS  added packages at the pinned versions: cryptsetup-bin 2:2.7.5-2; systemd-cryptsetup 257.13-1~deb13u1; tpm-udev 0.6+nmu1; tpm2-tools 5.7-1+b1
FAIL  no private keys, secrets or LAN names in what the build changed: /usr/share/trustiot-agent/trustiot-agent-egress.nft names a LAN address or host (line 51, LAN_PATTERNS[3])
18 of 19 checks passed
exit 0 at 21:50:24 (build host)
```

（`exit 0` 是包裝腳本的結束碼；`check.ts` 自己的結束碼是 1，因為上面那一項。其他 13 項（分割表、MBR、檔案系統、第一次開機、agent、TPM overlay、NTP、sshd、管理者、帳號、首次設定）都通過，全文在 `checks.json`。）

未通過的一項：agent 的出站規則檔 `edge/agent/systemd/trustiot-agent-egress.nft`（Laysi/trustiot-v2#828，已在 main）第 51 行的註解寫了雲端 metadata 服務的位址（169.254.x.x），規則本身也擋 `169.254.169.254`；映像檢查的內網名稱規則（`LAN_PATTERNS`）把它當成內網位址。這個檔案與規則都不是本分支改的，main 上建出的映像也會在這一項不通過；處理方式另案提出。

### 單元測試

```text
bun run --cwd edge/image typecheck
exit 0
bun run --cwd edge/image test
 109 pass
 1 fail
(fail) sfdisk > without boot code parted replaces the disk identifier (why the build copies it)
```

未通過的一項是已知的 Laysi/trustiot-v2#872（工作站上的 parted 3.7.x 行為不同），與本 issue 無關。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 以軟體 TPM 測試自動解鎖並掛上、換 TPM（或清掉登錄）解不開 | 通過：6、7、11 以 TPM A 解鎖並掛上，4 以 TPM C；8（TPM B）與 10（TPM A 被 `tpm2_clear`）解不開 |
| LUKS header 沒有可用的密碼 keyslot、只有 TPM token | 通過：`luks-header.json` 只有 keyslot 1 與它的 `systemd-tpm2` token（`tpm2-pcrs` 空、沒有 PIN）；虛擬機裡 `systemd-cryptenroll` 只列出 `tpm2`；4（原本有密碼 keyslot）重新加密後也是 |
| 模擬 TPM 解不開時開機不卡在密碼提示、資料分割區不掛上 | 通過：8、9、10 開機完成、ssh 在執行、沒有等待中的密碼提示、日誌沒有 `Please enter`，解鎖以 `exit-code` 失敗而不是逾時，分割區不掛上 |
| 資料分割區沒掛上時 agent 不啟動、不寫到根分割區 | 通過：所有解不開與拒絕的開機 agent 都沒有啟動（啟動因相依失敗而取消），root 上的掛載點是空的。有人手動啟動 agent 時的 `AssertPathIsMountPoint=` 只由映像檢查確認 drop-in 存在，沒有在開機測試裡實際啟動（延後，見下） |
| 系統日誌不把資料分割區的內容留在未加密的分割區 | 通過：映像檢查算出 journald 的 `Storage=volatile`、`ForwardToSyslog=no`、沒有 syslog daemon、rpi-swap 只用 zram、三個 unit 都有 `LimitCORE=0`；每次開機 `/var/log/journal` 是空的、只有 zram 置換空間、沒有 core 檔；寫在分割區裡的標記不出現在磁碟的原始位元組裡 |
| `edge/image/README.md` 寫明加密的時機與解鎖的流程 | 通過：「資料分割區加密」（何時加密、解鎖、解不開時、其他決定）與「日誌與置換空間」 |
| 測試需要 root 或特殊裝置時寫明在哪裡跑、怎麼跑 | 通過：README 的「開機測試」寫明在 arm64 Linux 的建置主機以 rootless podman 執行、不需要 root、KVM 或 device mapper、不在 CI 跑 |

待測：

- 實機（SPI 上的 SLB9670、SD 卡）：第一次開機的加密與之後的解鎖、拔掉 TPM 模組時的行為、setup 在真實 SD 卡上的時間（2 GiB 的讀取加上加密，必須遠小於 setup 的 10 分鐘上限）。等 Laysi/trustiot-v2#811 的實機燒錄。
- 資料分割區鎖住時手動啟動 agent（`AssertPathIsMountPoint=` 擋下）、第一次開機在各步驟之間斷電：屬進階驗證，延後另開 issue。
