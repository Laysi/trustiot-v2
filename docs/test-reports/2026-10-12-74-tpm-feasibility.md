# 閘道器 TPM 可行性測試：mTLS、換發挑戰與 LUKS 自動解鎖（2026-10-12）

對應 issue：Laysi/trustiot-v2#74。原始資料在 [data/2026-10-12-74-tpm-feasibility/](data/2026-10-12-74-tpm-feasibility/)，結論與設計影響見[可行性測試結果（2026-10-12）](../feasibility/2026-10-12-tpm-poc.md)。

## 環境

- **lab 的 Pi**：Raspberry Pi 4 Model B Rev 1.4、2 GB，Raspberry Pi OS Lite arm64（Debian 13），kernel 6.18.50+rpt-rpi-v8，systemd 257.13-1~deb13u1；TPM 為 Infineon SLB9670（韌體 7.63.3353.0），接在 SPI0 的 CE0。測試期間另裝 `tpm2-tools` 5.7-1+b1、`cryptsetup-bin` 2:2.7.5-2、`systemd-cryptsetup` 257.13-1~deb13u1（libtss2 4.1.3-1.2）。lab Pi 由 Laysi/trustiot-v2#74、#75、#76 共用，改系統設定、重開機與每次量測都持有 `~/e2e.lock`（owner `Laysi/trustiot-v2#74`）：

  | 時段（UTC，2026-10-12） | 內容 |
  | --- | --- |
  | 08:40:57–08:47:24 | 備份 `config.txt`、裝套件、啟用 SPI 與 TPM overlay；08:42:16 與 08:45:36 兩次重開機；`tpm2_getcap` |
  | 09:02:14–09:05:53 | 建 TPM 金鑰與 CSR、mTLS 與握手量測、TPM 簽章延遲、挑戰簽章 |
  | 09:14:09–10:03:06 | 建 LUKS 容器、TPM 登錄、PCR 15 測試、複製容器、`cryptsetup benchmark`、09:15:54 重開機與開機後驗證、還原設定。實際操作在 09:14–09:17 與 10:02–10:03；中間 agent 因 API 用量上限暫停，鎖多持有約 45 分鐘 |

- **工作站**：Fedora，x86\_64；`.prototools` 的 Rust 1.99.0、zig 0.16.0、cargo-zigbuild 0.23.4、Bun 1.4.2；podman 5.8.7（rootless）；OpenSSL 3.5.9。與 Pi 在同一個區網，Pi 連工作站的 18740（broker）與 18741（驗證端）。
- **broker**：`docker.io/library/eclipse-mosquitto:2.1.2-alpine`（digest `sha256:12436df85d6b2b91e63c71184da35ec89400a98abdc688a78b1403a11ecf44b4`），只開 mTLS 的 listener；設定見可行性文件的「測試環境與重現」。
- **測試程式**：閘道器端 `gw`（tss-esapi 7.7.0、rustls 0.23.45 加 ring 0.17.14、rcgen 0.14.10、p256 0.13.2）以 cargo-zigbuild 交叉編譯成 aarch64（glibc 2.41）；驗證端 `verifier`（axum 0.8.9、p256 0.13.2）在工作站執行。依 2026-10-09 的決定不進 repo，原始碼在工作站的 `~/poc-gw/74/` 與 Pi 的 `~/poc-74/`。
- **另一台電腦**：rootless podman 的 Debian 13 容器（`debian:trixie@sha256:bcf83fd8af414eda0c0a99d25a4e61dedd88e7aac7f7bc519d04555ff0c40ec5` 加 `cryptsetup-bin` 2:2.7.5-2、`systemd-cryptsetup` 257.13-1~deb13u1、`swtpm` 0.7.1-1.5、`tpm2-tools`）。
- 所有金鑰、憑證與 LUKS 測試密碼都在測試時產生，只存在工作站與 Pi 的測試目錄，沒有進 repo；原始資料裡的 LUKS salt、digest 與 TPM 封裝資料已遮蔽，區網位址改成 `<lab-pi>`，Pi 的主機名稱改成 `<pi>`。

## 指令

Pi 上（鎖的取得與釋放）：

```sh
mkdir ~/e2e.lock && echo "Laysi/trustiot-v2#74 $(date -u +%FT%TZ)" > ~/e2e.lock/owner
rm -rf ~/e2e.lock
```

TPM 偵測（`/boot/firmware/config.txt` 先備份成 `config.txt.bak-74`）：

```sh
sudo dmesg | grep -i tpm
sudo ./spiprobe /dev/spidev0.0            # 讀 TIS DID_VID（0xD40F00）
dtc -@ -I dts -O dtb -o tpm-slb9670-ce0.dtbo tpm-slb9670-ce0-overlay.dts
dtmerge /boot/firmware/bcm2711-rpi-4-b.dtb /tmp/merged.dtb tpm-slb9670-ce0.dtbo
sudo install -m 0755 tpm-slb9670-ce0.dtbo /boot/firmware/overlays/
sudo tpm2_getcap properties-fixed; sudo tpm2_getcap properties-variable; sudo tpm2_getcap handles-persistent; sudo tpm2_pcrread
```

mTLS、簽章延遲與挑戰（`<ws>` 是工作站的區網位址）：

```sh
sudo ./gw keygen --parent 0x81000001 --handle 0x81000100
sudo ./gw csr --handle 0x81000100 --cn gw-lab-01 > gw-lab-01.csr      # 工作站：openssl req -verify、openssl x509 -req
sudo ./gw mqtt --handle 0x81000100 --cert gw-lab-01.pem --ca ca.pem --broker <ws>:18740 --server-name <ws> --cn gw-lab-01 --iterations 50
./gw mqtt --key-file gw-soft-01.key --cert gw-soft-01.pem --ca ca.pem --broker <ws>:18740 --server-name <ws> --cn gw-soft-01 --iterations 50
sudo ./gw mqtt ... --iterations 20 --resume yes
# broker 加 set_tcp_nodelay true 後，三種各再跑一次（30、30、20 次）
sudo ./gw bench-sign --handle 0x81000100 --n 300
sudo ./gw challenge --handle 0x81000100 --server http://<ws>:18741 --device gw-lab-01 --ttl-wait 32
```

LUKS（Pi 上以 sudo 執行；密碼由工作站經 ssh 的 stdin 傳入，不寫進 Pi 的檔案或命令列）：

```sh
printf %s "$PASS" | cryptsetup luksFormat --type luks2 --batch-mode --pbkdf argon2id --pbkdf-memory 262144 --label poc74data --key-file=- /var/lib/trustiot-poc74/data.luks
printf %s "$PASS" | systemd-cryptenroll --unlock-key-file=/dev/stdin --tpm2-device=auto --tpm2-pcrs= /var/lib/trustiot-poc74/data.luks
cryptsetup open --test-passphrase --token-only /var/lib/trustiot-poc74/data.luks       # 三次，計時
systemd-cryptenroll --unlock-key-file=<隨機金鑰> --tpm2-device=auto --tpm2-pcrs=15 pcrdemo.luks   # PCR 15 測試用的另一個容器
tpm2_pcrextend "15:sha256=<摘要>"
# 寫入 crypttab、fstab、drop-in（內容見可行性文件），然後
systemctl start systemd-cryptsetup@poc74data.service mnt-poc74data.mount
systemctl reboot
systemctl is-active systemd-cryptsetup@poc74data.service mnt-poc74data.mount
journalctl -b -u systemd-cryptsetup@poc74data.service -u mnt-poc74data.mount -o short-monotonic
systemd-analyze blame; tpm2_pcrread sha256:0,1,2,3,4,5,6,7,10,11,14,15
cryptsetup benchmark -c aes-xts-plain64 -s 512; cryptsetup benchmark -c xchacha12,aes-adiantum-plain64 -s 256
```

另一台電腦（工作站，容器檔以 `sudo cat` 經 ssh 複製，sha256 與 Pi 上相同）：

```sh
podman run --rm -v <複製的容器目錄>:/data:Z,ro -v <腳本目錄>:/scripts:Z,ro localhost/poc74-offdevice:1 bash /scripts/offdevice-test.sh
# 腳本內：cryptsetup luksDump；cryptsetup open --test-passphrase --token-only；
# /usr/lib/systemd/systemd-cryptsetup attach poc74data /data/data.luks none "tpm2-device=auto,headless=true"；
# swtpm socket --tpm2 ...；在 swtpm 的 0x81000001 放同範本的 SRK；
# systemd-cryptsetup attach ... "tpm2-device=swtpm:path=/tmp/swtpm.sock,headless=true"；
# tpm2_load -C 0x81000001 -u <token 封裝資料的 public> -r <private>
podman run --rm -i -v <複製的容器目錄>:/data:Z,ro localhost/poc74-offdevice:1 \
  cryptsetup open --test-passphrase --key-slot 0 --key-file=- /data/data.luks < <工作站上的密碼檔>
```

測試後還原：移除 `/etc/crypttab`（測試前不存在）、以備份還原 `/etc/fstab` 並 `cmp` 確認、刪除 drop-in、`systemctl daemon-reload`、刪除容器檔與掛載點（[luks-revert.log](data/2026-10-12-74-tpm-feasibility/luks-revert.log)）。

## 結果

### TPM 偵測

官方 overlay（CE1）probe 逾時；spidev0.0（CE0）讀到 `DID_VID = 0x001B15D1`；改用 CE0 的 overlay 後 `/dev/tpm0`、`/dev/tpmrm0` 出現（[tpm-detection.txt](data/2026-10-12-74-tpm-feasibility/tpm-detection.txt)、[tpm-getcap-properties-fixed.txt](data/2026-10-12-74-tpm-feasibility/tpm-getcap-properties-fixed.txt)、[tpm-getcap-properties-variable.txt](data/2026-10-12-74-tpm-feasibility/tpm-getcap-properties-variable.txt)、[tpm-getcap-ecc-curves.txt](data/2026-10-12-74-tpm-feasibility/tpm-getcap-ecc-curves.txt)、[tpm-getcap-handles-persistent-before.txt](data/2026-10-12-74-tpm-feasibility/tpm-getcap-handles-persistent-before.txt)、[tpm-pcrread-boot-2026-10-12T0845Z.txt](data/2026-10-12-74-tpm-feasibility/tpm-pcrread-boot-2026-10-12T0845Z.txt)）。

### TPM 金鑰 mTLS

- 金鑰 `0x81000100` 的屬性 `fixedtpm|fixedparent|sensitivedataorigin|userwithauth|sign`（[tpm-readpublic-0x81000100.txt](data/2026-10-12-74-tpm-feasibility/tpm-readpublic-0x81000100.txt)、[gw-lab-01-tpm-public-key.pem](data/2026-10-12-74-tpm-feasibility/gw-lab-01-tpm-public-key.pem)）。
- TPM 簽的 CSR 驗過，公鑰與 TPM 的相同（[csr-and-signature-checks.txt](data/2026-10-12-74-tpm-feasibility/csr-and-signature-checks.txt)）。
- broker 以憑證 CN 認出 `u'gw-lab-01'`，TLS 1.3，訂閱、QoS 1 發布與收回都成功（[mqtt-tpm-run.txt](data/2026-10-12-74-tpm-feasibility/mqtt-tpm-run.txt)、[mosquitto-tpm-run.log](data/2026-10-12-74-tpm-feasibility/mosquitto-tpm-run.log)）。

TPM2\_Sign（n=300，[tpm-sign-latency.csv](data/2026-10-12-74-tpm-feasibility/tpm-sign-latency.csv)）：min 68.78、p50 69.55、p90 70.08、p99 70.99、max 73.06、平均 69.70 ms；含 channel 的來回 p50 69.62 ms。

握手（ms，p50 / p99；CSV 在 data 目錄的 `handshake-*.csv`）：

| broker | 連線方式 | n | TLS 握手 | CONNACK | TPM 簽章 |
| --- | --- | --- | --- | --- | --- |
| 預設 | TPM 金鑰 | 50 | 74.31 / 77.33 | 1.77 / 2.60 | 69.84 / 73.23 |
| 預設 | 軟體金鑰 | 50 | 4.14 / 5.24 | 43.34 / 44.30 | — |
| 預設 | TPM 金鑰 resumption（扣掉第 1 次） | 19 | 2.48 / 2.91 | 44.87 / 45.12 | — |
| `set_tcp_nodelay true` | TPM 金鑰 | 30 | 71.90 / 75.19 | 1.66 / 1.97 | 69.73 / 72.61 |
| `set_tcp_nodelay true` | 軟體金鑰 | 30 | 2.24 / 3.72 | 1.21 / 3.47 | — |
| `set_tcp_nodelay true` | TPM 金鑰 resumption（扣掉第 1 次） | 19 | 1.36 / 1.55 | 0.63 / 0.72 | — |

協商結果都是 TLS 1.3、`TLS13_AES_256_GCM_SHA384`。預設設定下 TPM 金鑰的 73 次連線（第一次試跑 3 次、完整握手 50 次、resumption 20 次）在 broker 的紀錄裡全部是 `u'gw-lab-01'`。

### 換發挑戰簽章

9 個案例全部符合預期（[challenge-suite.jsonl](data/2026-10-12-74-tpm-feasibility/challenge-suite.jsonl)、[challenge-first-exchange.txt](data/2026-10-12-74-tpm-feasibility/challenge-first-exchange.txt)、[challenge-verifier.log](data/2026-10-12-74-tpm-feasibility/challenge-verifier.log)）：正確 200；重放 409；竄改 401，之後同一個 nonce 409；用途不符 403；裝置不符 403；改了到期時間 401；沒發過的 nonce 409；過期 410。正確的那份簽章以 OpenSSL 也驗過（[csr-and-signature-checks.txt](data/2026-10-12-74-tpm-feasibility/csr-and-signature-checks.txt)）。

### LUKS

- 建立與登錄：keyslot 0 是測試密碼（argon2id，256 MiB），keyslot 1 與 token 0 是 `systemd-tpm2`，不綁 PCR、`tpm2-srk: true`（[luks-setup.log](data/2026-10-12-74-tpm-feasibility/luks-setup.log)）。
- 只用 TPM 開啟：1.503、1.382、1.385 秒；token 的封裝資料在 lab Pi 的 TPM 上 `tpm2_load` 成功（[luks-tpm-only-unlock.log](data/2026-10-12-74-tpm-feasibility/luks-tpm-only-unlock.log)；最後的 `tpm2_flushcontext` 錯誤無害，`/dev/tpmrm0` 在連線關閉時就清掉了暫存物件）。
- 開機前以 systemd 啟動與掛載成功（[luks-boot-config.log](data/2026-10-12-74-tpm-feasibility/luks-boot-config.log)）。
- 重開機後：unit 與掛載都是 active；開機 6.44 秒開始解鎖、8.25 秒完成（blame 1.81 秒），8.40 秒掛載；標記檔兩行都在；沒有密碼提示；讀取的 PCR 0–7、10、11、14 為 0，PCR 15 仍是重開機前 extend 的值（[luks-after-reboot.log](data/2026-10-12-74-tpm-feasibility/luks-after-reboot.log)）。這次開機的核心訊息沒有 `starting up the TPM manually`（[tpm-dmesg-after-luks-reboot.txt](data/2026-10-12-74-tpm-feasibility/tpm-dmesg-after-luks-reboot.txt)），斷電後第一次開機則有（[tpm-detection.txt](data/2026-10-12-74-tpm-feasibility/tpm-detection.txt)）：軟體重開機沒有重置 TPM。
- PCR 15 測試容器：PCR 15 為 0 時打得開，extend 後被拒；systemd 登錄時警告 `PCR policy effectively unenforced`（[luks-pcr15-demo.log](data/2026-10-12-74-tpm-feasibility/luks-pcr15-demo.log)）。
- 另一台電腦（[luks-offdevice-test.log](data/2026-10-12-74-tpm-feasibility/luks-offdevice-test.log)）：沒有 TPM 時 `cryptsetup` 與 `systemd-cryptsetup` 都失敗；不同的 TPM（swtpm，同範本的 SRK 名稱 `000b1280…`，lab Pi 的是 `000baf00…`）上 `TPM2_Load` 回 `TPM_RC_INTEGRITY`（0x1DF）、`tpm2_load` 回 `integrity check failed`；對照組（在 swtpm 上登錄的容器）解封成功，只因 rootless 容器沒有 device-mapper 而無法掛上；複製出去的容器以測試密碼開得起來。
- `cryptsetup benchmark`：`aes-xts` 512 位元加密 82.7、解密 83.8 MiB/s；`xchacha12,aes-adiantum` 加密 239.6、解密 251.1 MiB/s（[cryptsetup-benchmark.txt](data/2026-10-12-74-tpm-feasibility/cryptsetup-benchmark.txt)）。
- 還原後 `/etc/crypttab` 不存在、`/etc/fstab` 與備份相同、沒有 `systemd-cryptsetup@` unit、容器與掛載點已刪除；TPM 的持久 handle 為 `0x81000001`、`0x81000100`（[luks-revert.log](data/2026-10-12-74-tpm-feasibility/luks-revert.log)）。

### 測試後 lab Pi 的狀態

- 保留（之後的工作會用到）：`config.txt` 的 `dtparam=spi=on`、`dtoverlay=tpm-slb9670-ce0` 與 `/boot/firmware/overlays/tpm-slb9670-ce0.dtbo`（原檔備份 `/boot/firmware/config.txt.bak-74`、`cmdline.txt.bak-74`，`cmdline.txt` 沒有改）；套件 `tpm2-tools`、`cryptsetup-bin`、`systemd-cryptsetup` 與其相依；TPM 內的持久金鑰 `0x81000100`（mTLS 測試用的閘道器金鑰）；`~/poc-74/` 的測試程式與輸出。
- PCR 15 停在 PCR 15 測試 extend 的值，下次斷電才會歸零；沒有任何東西綁 PCR 15，不影響其他測試。
- 已還原或刪除：crypttab、fstab、drop-in、LUKS 容器與掛載點、`/run` 下的暫存檔。測試程式與驗證端都已結束。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| lab Pi 上的 Rust 程式以 TPM 金鑰與 Mosquitto 完成 mTLS（記錄版本與指令） | 通過：rustls 0.23.45 外部簽章、tss-esapi 7.7.0、Mosquitto 2.1.2，版本與指令見上與可行性文件 |
| 挑戰簽章可被伺服器以公鑰驗證，重放的簽章被拒 | 通過：正確簽章 200、重放 409，另外 7 個負面案例都被拒 |
| 開機自動解鎖 LUKS | 通過：重開機後不輸入密碼即解鎖並掛載，標記檔完好 |
| SD 卡移到別台電腦無法解鎖 | 通過（模擬）：以複製的容器檔在沒有 TPM 與有不同 TPM 的另一台電腦測試，都解不開；這天沒有把 SD 卡實際插到別台電腦。2026-10-14 補做實體驗證並通過，見[實體拔卡驗證報告](2026-10-14-74-sd-card-physical.md) |
| 結果寫成 docs/feasibility/ 下的新檔，並勾選 milestones 的 TPM 項目；記錄採用 rustls 外部簽章或 tpm2-pkcs11 | 通過：[可行性測試結果（2026-10-12）](../feasibility/2026-10-12-tpm-poc.md)，採用 rustls 外部簽章 |
