# 閘道器 LUKS 實體拔卡驗證：SD 卡插到另一台電腦（2026-10-14）

對應 issue：Laysi/trustiot-v2#74。補做 [2026-10-12 的測試](2026-10-12-74-tpm-feasibility.md)以複製容器檔模擬的「SD 卡移到別台電腦無法解鎖」：把 lab 的閘道器的 SD 卡實際拔下，插到另一台有 TPM 的電腦開啟卡上的容器。原始資料在 [data/2026-10-14-74-sd-card-physical/](data/2026-10-14-74-sd-card-physical/)，結論與設計影響見[可行性測試結果（2026-10-12）](../feasibility/2026-10-12-tpm-poc.md)的「LUKS 由 TPM 自動解鎖」一節。

## 環境

- **lab 的閘道器**：與 2026-10-12 相同，裝置樹回報的型號是 Raspberry Pi 4 Model B Rev 1.4，Raspberry Pi OS Lite arm64（Debian 13），systemd 257.13、cryptsetup 2.7.5；TPM 為 Infineon SLB9670，接在 SPI0 的 CE0。持有 `~/e2e.lock`（owner `Laysi/trustiot-v2#74`）的時段：

  | 時段（UTC，2026-10-14） | 內容 |
  | --- | --- |
  | 06:57:54–07:00 | 重建 LUKS 容器、TPM 登錄、只用 TPM 開啟、寫入開機設定；06:58:52 重開機與開機後驗證；記錄容器的指紋 |
  | 07:00–07:19 | 07:00 之後閘道器在運作中斷電，07:08 要正常關機時已經連不上（見「結果」）；之後由人到場把 SD 卡拔下 |
  | 07:19:45 | 卡插在工作站上測試（鎖目錄在卡上，這段期間閘道器沒有開機） |
  | 07:24–07:28:28 | 卡插回閘道器開機、驗證、還原設定、釋放鎖 |

- **另一台電腦**：x86\_64 工作站，Fedora 44（kernel 7.2.8），cryptsetup 2.8.8、systemd 259.9、tpm2-tss 4.1.3、util-linux 2.41.5、udisks2 2.11.2、fish 4.6.0。TPM 是主機板內建的 firmware TPM 2.0（ACPI `MSFT0101`，驅動 `tpm_crb`），與 lab 的閘道器的 SLB9670 是不同的 TPM。SD 卡經讀卡機接上，出現為 `/dev/sdc`。
- LUKS 測試密碼沿用 2026-10-12 在工作站產生的那一個，只存在工作站，沒有進 repo，也沒有留在閘道器上；原始資料裡的 LUKS salt、digest 與 TPM 封裝資料已遮蔽，閘道器的主機名稱與登入帳號（兩者同名）改成 `<gateway>`。

## 指令

lab 的閘道器上，建立、登錄與開機設定與 [2026-10-12 報告](2026-10-12-74-tpm-feasibility.md)「指令」一節的 LUKS 指令相同（容器 `/var/lib/trustiot-poc74/data.luks`、crypttab、fstab 與 drop-in 都一樣），密碼由工作站經 ssh 的 stdin 傳入。之後記錄容器的指紋，作為卡插到工作站時比對是否為同一個容器的依據：

```sh
sudo cryptsetup luksUUID /var/lib/trustiot-poc74/data.luks
sudo cryptsetup token export --token-id 0 /var/lib/trustiot-poc74/data.luks | sha256sum
sudo cryptsetup luksDump --dump-json-metadata /var/lib/trustiot-poc74/data.luks | sha256sum
```

卡從閘道器拔下、插到工作站後，在工作站的 fish 執行（`sudo` 只問一次密碼）。它以 PARTUUID 找到卡的根分割區，先卸載桌面自動掛上的分割區，再以唯讀、不重播 ext4 journal 的方式（`ro,noload`）掛載，所以不會寫入卡片：

```fish
begin
    set part (realpath /dev/disk/by-partuuid/0ff2576b-02)
    set boot (realpath /dev/disk/by-partuuid/0ff2576b-01)
    set img /mnt/poc74-card/var/lib/trustiot-poc74/data.luks
    echo "== "(date -u +%FT%TZ)" card rootfs $part, bootfs $boot"
    for p in $boot $part
        test -b $p; and findmnt -rn -S $p >/dev/null; and udisksctl unmount -b $p
    end
    if not test -b $part
        echo "!! card not found"
    else if findmnt -rn -S $part >/dev/null; or findmnt -rn -S $boot >/dev/null
        echo "!! card still mounted; close Files and run again"
    else if sudo mkdir -p /mnt/poc74-card; and sudo mount -o ro,noload $part /mnt/poc74-card
        findmnt -no SOURCE,OPTIONS /mnt/poc74-card
        echo "== this computer's TPM"
        systemd-cryptenroll --tpm2-device=list
        echo "== container (on the gateway: bf5a105c-5f2e-4199-ae0e-9e6106c3afa6, token 6a12d50f..., metadata 9fe72952...)"
        sudo cryptsetup luksUUID $img
        sudo cryptsetup token export --token-id 0 $img | sha256sum
        sudo cryptsetup luksDump --dump-json-metadata $img | sha256sum
        echo "== A: TPM only, cryptsetup (expect failure)"
        sudo cryptsetup open --test-passphrase --token-only -v $img
        echo "exit $status"
        echo "== B: TPM only, systemd-cryptsetup as the gateway does at boot (expect failure)"
        sudo /usr/lib/systemd/systemd-cryptsetup attach poc74card $img none tpm2-device=auto,headless=true,read-only
        echo "exit $status"
        test -e /dev/mapper/poc74card; and sudo /usr/lib/systemd/systemd-cryptsetup detach poc74card
        echo "== C: control, test passphrase (expect success)"
        sudo cryptsetup open --test-passphrase --key-slot 0 --key-file <密碼檔> $img
        echo "exit $status"
        sudo umount /mnt/poc74-card; and sudo rmdir /mnt/poc74-card; and echo "== card unmounted, safe to remove"
    end
end 2>&1 | tee <log>
```

B 不帶 `tpm2-measure-pcr`，即使解開也不會 extend 工作站 TPM 的 PCR。卡插回閘道器開機後，以上面的指紋指令與 `systemctl is-active systemd-cryptsetup@poc74data.service mnt-poc74data.mount` 驗證，最後還原：停掉 unit、以備份還原 `/etc/fstab`、刪除 `/etc/crypttab`、drop-in、容器檔與掛載點，然後 `systemctl daemon-reload`，並釋放鎖。

## 結果

### 拔卡前（lab 的閘道器）

- 建立與登錄：keyslot 0 是測試密碼（argon2id，256 MiB），keyslot 1 與 token 0 是 `systemd-tpm2`，不綁 PCR、`tpm2-srk: true`，與 2026-10-12 相同（[gateway-luks-setup.log](data/2026-10-14-74-sd-card-physical/gateway-luks-setup.log)）。
- 只用 TPM 開啟：1.393、1.389、1.386 秒；token 的封裝資料在 lab 的閘道器的 TPM 上 `tpm2_load` 成功。
- 06:58:52 重開機後自動解鎖並掛載：開機 6.41 秒開始、8.27 秒完成（blame 1.863 秒），沒有密碼提示，標記檔兩行都在。
- 容器指紋：UUID `bf5a105c-5f2e-4199-ae0e-9e6106c3afa6`，token 0 的 sha256 `6a12d50f9e20…`，LUKS2 metadata 的 sha256 `9fe72952b4f0…`（[gateway-before-card-test.log](data/2026-10-14-74-sd-card-physical/gateway-before-card-test.log)）。

### 卡插在另一台電腦（工作站）

[workstation-card-test.log](data/2026-10-14-74-sd-card-physical/workstation-card-test.log)：

| 項目 | 結果 |
| --- | --- |
| 掛載 | 桌面自動掛上的開機分割區先被卸載；根分割區以 `ro,relatime,seclabel,norecovery` 掛上 |
| 這台電腦的 TPM | `/dev/tpmrm0`，`MSFT0101:00`，`tpm_crb_acpi` |
| 卡上容器的指紋 | UUID、token 0 與 metadata 的 sha256 都與拔卡前在閘道器上記錄的相同，是同一個容器 |
| A：只用 TPM，`cryptsetup open --test-passphrase --token-only` | 拒絕，exit 1：`Esys_Load()` 回 `0x000001df`（`TPM_RC_INTEGRITY`），`TPM key integrity check failed. Key enrolled in superblock most likely does not belong to this TPM.`，之後 `沒有可用的 token。` |
| B：只用 TPM，與閘道器開機時相同的 `systemd-cryptsetup attach ... tpm2-device=auto,headless=true` | 拒絕，exit 1：同樣的完整性錯誤，最後 `Password querying disabled via 'headless' option.`，沒有改問密碼 |
| C：對照組，以測試密碼開啟 keyslot 0 | 成功，exit 0：卡上的容器完好，A、B 打不開是因為 TPM 不同 |
| 卸載 | `card unmounted, safe to remove` |

錯誤與 2026-10-12 在 swtpm 上模擬的相同（`TPM2_Load` 回 `TPM_RC_INTEGRITY`）：封裝資料由 lab 的閘道器那顆 TPM 的 SRK 保護，別顆 TPM 的 SRK 由它自己的儲存種子推導，完整性檢查通不過。

### 卡插回 lab 的閘道器

[gateway-after-card-return.log](data/2026-10-14-74-sd-card-physical/gateway-after-card-return.log)：

- 開機自動解鎖並掛載：開機 6.24 秒開始、8.12 秒完成（blame 1.887 秒），8.25 秒掛載，`Automatically discovered security TPM2 token unlocks volume`，沒有密碼提示；容器內的 ext4 fsck 為 clean，標記檔兩行都在。
- 容器指紋與拔卡前相同。
- 拔卡前閘道器是在運作中斷電，不是正常關機：這次開機根分割區出現 `EXT4-fs (mmcblk0p2): orphan cleanup on readonly fs`，由核心自動處理；開機分割區的 `fsck.fat` 沒有報錯。前一次開機的 journal 沒有留下，斷電原因查不出來。工作站以 `norecovery` 掛載，沒有替閘道器重播 journal，所以這個修復發生在閘道器自己開機時。

### 還原

[gateway-revert.log](data/2026-10-14-74-sd-card-physical/gateway-revert.log)：`/etc/crypttab` 不存在、`/etc/fstab` 與備份相同、drop-in、容器檔與掛載點都已刪除，`/dev/mapper` 只剩 `control`；TPM 的持久 handle 仍是 `0x81000001`、`0x81000100`；07:28:28 釋放鎖。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 開機自動解鎖 LUKS；SD 卡移到別台電腦無法解鎖 | 通過：拔卡前與卡插回後，lab 的閘道器開機都不輸入密碼就自動解鎖（1.86、1.89 秒）；同一張 SD 卡插到有另一顆 TPM 的電腦，`cryptsetup` 與 `systemd-cryptsetup` 只用 TPM 都打不開（`TPM_RC_INTEGRITY`），以測試密碼開得起來，證明打不開是因為 TPM 不同 |

沒有 TPM 的電腦這次沒有實體測，只有 2026-10-12 以複製容器檔模擬的結果（打不開）。
