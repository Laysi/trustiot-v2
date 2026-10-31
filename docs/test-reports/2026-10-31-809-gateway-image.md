# 閘道器映像：從零建置與不開機的映像檢查（2026-10-31）

對應 issue：Laysi/trustiot-v2#809（父 issue Laysi/trustiot-v2#95）。這份報告記錄 `edge/image` 第一次從零建置閘道器映像，以及 `check.ts` 對完成的映像做的不開機檢查。實機燒錄與開機在 Laysi/trustiot-v2#811，不在這裡；lab 的閘道器（展示用閘道器）全程沒有碰。

結果先講：從零建置成功（10 分 21 秒），`check.ts` 的 17 項不開機檢查全部通過；映像裡的 agent 是不帶 feature 的 aarch64 release 建置（SHA-256 `b3df24e87fdd8da9498b0cdb9c4f67e4c37441de9eeeb04a765faab4e21378d5`）；CI 會跑的部分全部通過。實機開機（第一次開機的延伸 root、主機金鑰、網路、TPM 裝置）待 Laysi/trustiot-v2#811。

## 環境

- **映像的建置與檢查**：建置主機（arm64 Linux），rootless podman，Bun 1.4.2（`.prototools`）。工具容器、snapshot 時間點與套件版本都是 `edge/image/src/pins.ts` 的釘選值：
  - 工具容器 `docker.io/library/debian@sha256:2dc419c6d7fe3dad0a2f9fd469c34cc90b0f9ba642d8fde837fe3d7d34a0e7f1`，容器裡的工具從 snapshot.debian.org `20261030T000000Z` 以固定版本安裝：ca-certificates 20250419+deb13u1、dosfstools 4.2-1.2、e2fsprogs 1.47.2-3+b12、fdisk 2.41.5-0+deb13u1、fuse2fs 1.47.2-3+b12、mtools 4.0.48-1、xz-utils 5.8.1-1+deb13u2。
  - 來源映像：Raspberry Pi OS Lite（64 位元）2026-10-06，`2026-10-06-raspios-trixie-arm64-lite.img.xz`，SHA-256 `483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843`。2026-10-31 由 `build.ts` 從官方網站下載到建置主機的快取，每次使用前重新比對 SHA-256。
- **agent 的建置**：建置主機（x86_64 Linux），Rust 1.99.0、zig 0.16.0、cargo-zigbuild 0.23.4（都是 `.prototools` 釘選的版本），不帶任何 feature。原始碼是 main `608766cc` 的 `edge/agent`（tree `253d46e65395f3aa02741f7568e63c83ad0d49a8`）、`crates`（tree `7bac9fc9b70826a333d0699978e3779a981a0ca8`）與 `Cargo.lock`（blob `6c6f29d5e9c468433c2b53ada7a25bb2d25c2b89`）。執行檔複製到 arm64 的建置主機後 SHA-256 相同。
- **程式**：分支 `feat/809-gateway-image` 的 commit `2547c072`（`edge/image` 與 CI 設定）。檢查以這個 commit 執行。建置在提交前幾分鐘開始：當時的工作樹與這個 commit 只差 `check.ts` 的兩處讀取（`debugfs stat` 的權限位元、boot 分割區列表的格式確認）與 `files.ts` 新增的對應解析函式，建置步驟完全相同。那次執行接著也以提交前的 `check.ts` 檢查過一次（17 項通過），下面的檢查結果是以 commit 的 `check.ts` 對同一個映像重跑的。
- **管理者公鑰**：執行時以 `ssh-keygen` 產生的拋棄式 ed25519 金鑰（指紋 `SHA256:8yYv0QR7TwFPHkZVyD0Xlv5IOKlX4DMllltZ1iAQkU0`），放在 repo 之外，建置只拿到公鑰；檢查完成後與映像一起刪除（05:39）。
- **時段**：2026-10-31（+08:00）。agent 建置 03:41 結束；映像建置 05:05 開始、05:16 結束；檢查 05:27 開始、05:38 結束。

## 指令

在 repo 根目錄執行。`<快取>`、`<金鑰目錄>`、`<輸出>`、`<報告>` 都是 repo 之外的目錄。

```sh
# 1. agent（x86_64 Linux 建置主機）
proto install zig && proto install cargo-zigbuild
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
  PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
  cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
sha256sum target/aarch64-unknown-linux-gnu/release/trustiot-agent

# 2. 拋棄式管理者金鑰（arm64 Linux 建置主機，以下同）
ssh-keygen -q -t ed25519 -N '' -C throwaway-test-key -f <金鑰目錄>/admin

# 3. 建置與檢查
bun install --cwd edge/image --frozen-lockfile
bun edge/image/build.ts --agent <trustiot-agent> --ssh-key <金鑰目錄>/admin.pub --out <輸出> --cache <快取>
bun edge/image/check.ts --image <輸出>/trustiot-gateway.img --agent <trustiot-agent> \
  --ssh-key <金鑰目錄>/admin.pub --report <報告> --cache <快取>

# 4. CI 會跑的部分（不下載映像）
bun run --cwd edge/image typecheck
CI=true bun run --cwd edge/image test
bun install --cwd scripts --frozen-lockfile
bun test scripts/ci/changes.test.ts scripts/ci/workflow-conditions.test.ts
bun run --cwd scripts typecheck
bun scripts/ci/check-workflows.ts
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/lint-workflows.ts
```

## 結果

原始資料在 [data/2026-10-31-809-gateway-image/](data/2026-10-31-809-gateway-image/)：`build-info.json`（建置記錄）、`trustiot-gateway.img.sha256`、`checks.json`（每一項檢查的結果）、`delta.tsv` 與 `delta-boot.tsv`（建置改了來源映像 root 與 boot 分割區的哪些路徑）。

### agent

```text
cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
    Finished `release` profile [optimized] target(s) in 29.50s
size 4540728
sha256 b3df24e87fdd8da9498b0cdb9c4f67e4c37441de9eeeb04a765faab4e21378d5
exit 0 at 03:41:51 (build host)
```

建置印出 agent 有 1 個編譯警告（與映像無關，不在本 issue 處理）。映像裡執行 `trustiot-agent --version` 得到 `trustiot-agent 0.1.0`，表示這個以 zig 連結的執行檔能在映像自己的 glibc 上執行。

### 建置

```text
== tool packages (snapshot.debian.org 20261030T000000Z)
ca-certificates	20250419+deb13u1
dosfstools	4.2-1.2
e2fsprogs	1.47.2-3+b12
fdisk	2.41.5-0+deb13u1
fuse2fs	1.47.2-3+b12
mtools	4.0.48-1
xz-utils	5.8.1-1+deb13u2
source 2026-10-06-raspios-trixie-arm64-lite.img.xz SHA-256 483db18a48da399b5b7022ffae9a07bc6d99daab30e593aedac09b69ff422843
0 upgraded, 14 newly installed, 0 to remove and 12 not upgraded.
tpm-udev	0.6+nmu1
tpm2-tools	5.7-1+b1
packages with a newer version in the snapshot (not installed): 12
Creating user 'trustiot-agent' (TrustIoT gateway agent) with UID 985 and GID 985.
trustiot-agent --version: trustiot-agent 0.1.0
NTP=time.cloudflare.com, no fallback
rootfs: 74043/154736 files (0.1% non-contiguous), 510088/618496 blocks
/work/boot.vfat: 434 files, 153188/1032408 clusters
trustiot-data: 12/131072 files (0.0% non-contiguous), 26284/524288 blocks
Disk /work/out/trustiot-gateway.img: 4.87 GiB, 5226102784 bytes, 10207232 sectors
Disk identifier: 0x27b94958
Device                          Boot   Start      End Sectors  Size Id Type
/work/out/trustiot-gateway.img1        16384  1064959 1048576  512M  c W95 FAT32 (LBA)
/work/out/trustiot-gateway.img2      5259264 10207231 4947968  2.4G 83 Linux
/work/out/trustiot-gateway.img3      1064960  5259263 4194304    2G 83 Linux
Partition table entries are not in disk order.
image trustiot-gateway.img: 5226102784 bytes, SHA-256 8b21db1a07ebc351786f98bfa1d76a5bfb4a30187080341f9b56cec24717a187
real	10m20.852s
build exit 0
```

（`/work` 是工具容器裡的工作目錄。）映像在快取裡是稀疏檔，實際占用約 2.2 GB。安裝套件時 man-db 的觸發程序在 fuse2fs 上無法執行 `mandb`（`Permission denied`），只少了新套件 man page 的索引，`dpkg --audit` 為空。snapshot 裡有更新版本、建置刻意沒裝的 12 個套件（`build-info.json` 的 `upgradable`）多數是 Debian 的安全更新，見 `edge/image/README.md`「套件更新」。

### 檢查

```text
PASS  partition table: 0x27b94958: p1 16384+1048576 type c, p2 5259264+4947968 type 83, p3 1064960+4194304 type 83
PASS  MBR boot code is the source image's (first boot keeps the disk identifier): first byte 0xfa
PASS  file systems are clean: e2fsck root and data, fsck.vfat boot
PASS  data partition is formatted for the agent: trustiot-data, 6f3c1b2e-5a4d-4c1e-9b7a-2d8e0f4a9c31, owner trustiot-agent, mode 0700, empty
PASS  data partition is mounted at the agent's state directory: PARTUUID=27b94958-03 /var/lib/trustiot-agent ext4 defaults,noatime,nodev,nosuid,noexec,nofail
PASS  first boot grows root and leaves boot and data in place: parted resizepart 2: root 4947968 -> 28295168 sectors, p1 and p3 unchanged
PASS  first boot state is kept (machine ID, no SSH host keys): machine-id uninitialized, no host keys, cmdline.txt resize
PASS  agent executable is the given build: SHA-256 b3df24e87fdd8da9498b0cdb9c4f67e4c37441de9eeeb04a765faab4e21378d5, aarch64 ELF, runs on the image's C library (trustiot-agent 0.1.0)
PASS  agent unit and system user are installed and enabled: enabled, user trustiot-agent:x:985:985, tss group 105, tpmrm0 udev rule
PASS  TPM packages at the pinned versions: tpm-udev 0.6+nmu1; tpm2-tools 5.7-1+b1
PASS  TPM overlay is installed and loaded for every model: dtparam=spi=on, dtoverlay=tpm-slb9670-ce0; dtmerge onto bcm2711-rpi-4-b.dtb gives slb9670@0 on spi0, spidev0 disabled
PASS  NTP servers: NTP=time.cloudflare.com, FallbackNTP empty, systemd-timesyncd enabled
PASS  sshd accepts public keys only, for the administrator only: sshd -G and sshd -T for 3 connections (trustiot-admin, root, pi): permitrootlogin no, passwordauthentication no, kbdinteractiveauthentication no, authenticationmethods publickey, allowusers trustiot-admin; no Match block
PASS  administrator has the given public keys and no password: 1 key(s), each read by the image's ssh-keygen, password field "*", sudo without password (/etc/sudoers.d/trustiot-admin, the only sudo rule added; the sudo group is empty)
PASS  no account can log in with a password: 29 accounts, none with a password; login shells: root (/bin/bash), trustiot-admin (/bin/bash)
PASS  no account setup from the boot partition or the console (cloud-init, userconfig): masked: cloud-config.service, cloud-config.target, cloud-final.service, cloud-init-hotplugd.service, cloud-init-hotplugd.socket, cloud-init-local.service, cloud-init-main.service, cloud-init-network.service, cloud-init.target, userconfig.service; /etc/cloud/cloud-init.disabled; no meta-data, network-config, user-data on the boot partition
PASS  no private keys, secrets or LAN names in what the build changed: 349 changed or added files (root and boot) scanned: no private key blocks, no LAN names in configuration, gitleaks: only the reviewed package files of KNOWN_FINDINGS (generic-api-key in /etc/tpm2-tss/fapi-profiles/P_ECCP384SHA384.json:87, generic-api-key in /etc/tpm2-tss/fapi-profiles/P_RSA3072SHA384.json:94)

17 of 17 checks passed
real	11m0.533s
check exit 0
exit 0 at 05:38:30 (build host (arm64 Linux VM))
```

- 與來源映像相比，root 檔案系統新增 545 個、改了 15 個、刪了 34 個路徑（`delta.tsv`）；boot 分割區新增 TPM overlay、改了 `config.txt`（與 `overlays/` 目錄本身），刪了 cloud-init 的 `meta-data`、`network-config`、`user-data`（`delta-boot.tsv`）。
- gitleaks 在 libtss2-fapi1t64 4.1.3-1.2 的兩個 TPM FAPI profile 各有一個 `generic-api-key` 發現：比對到的是 `authPolicy`（NV index 授權策略的公開摘要），不是祕密。這兩個檔案依路徑、規則、套件與整個檔案的 SHA-256 列在 `edge/image/src/pins.ts` 的 `KNOWN_FINDINGS`，其他發現仍讓檢查失敗（單元測試涵蓋）。
- 「第一次開機」那一項以映像自己的 parted 對 16 GiB 的檔案重做官方 initramfs 的 `resize_early`：root 從 4947968 延伸到 28295168 個 sector，boot 與資料分割區不動、磁碟識別碼不變。資料分割區在建置時建立，第一次開機沒有自己寫的分割步驟。

### CI 會跑的部分

```text
bun test scripts/ci/changes.test.ts scripts/ci/workflow-conditions.test.ts
 368 pass
 0 fail
bun run --cwd scripts typecheck            -- exit 0
workflow policy: 4 file(s) checked, no violations
secret handling: 574 file(s) checked, 7 container build file(s), no violations
actionlint: 3 workflow file(s), no errors
bun run --cwd edge/image typecheck         -- exit 0
CI=true bun run --cwd edge/image test
 74 pass
 0 fail
exit 0 at 05:40:44 (build host (arm64 Linux VM))
```

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 依 README 在建置主機上從零建置；來源映像以版本與 SHA-256 釘選 | 通過：上面的建置，來源 2026-10-06，SHA-256 `483db18a…2843`（`build.ts` 比對後才使用） |
| 映像自動檢查通過：資料分割區存在並掛在 `/var/lib/trustiot-agent`、agent 執行檔與 unit 已安裝並啟用、TPM overlay、NTP 伺服器、sshd 只收公鑰、沒有可用密碼的帳號 | 通過：17 項全部 PASS（`checks.json`） |
| 資料分割區若在第一次開機建立，該步驟不靠閘道器測過一次 | 不適用：資料分割區在建置時建立。第一次開機唯一動到分割表的官方 `resize_early`，以映像的 parted 在檔案上重做並通過（「第一次開機」那一項） |
| 映像裡的 agent 是不帶 feature 的 aarch64 release 建置；建置指令與 SHA-256 在報告裡 | 通過：見「agent」 |
| repo 與映像沒有私鑰、密碼、內網主機名稱或 LAN 位址；建置時的 SSH 公鑰檔不在 repo | 映像：通過（「祕密與內網名稱」那一項，gitleaks 只有兩個檢視過的套件檔）；公鑰檔在 repo 之外（`build.ts` 拒絕 repo 內的路徑，單元測試涵蓋）。repo：分支的 gitleaks 掃描結果寫在 PR |
| CI 能跑的部分接進既有 workflow；`bun test scripts/ci/changes.test.ts` 通過 | 通過：新的 `edge-image` job，見「CI 會跑的部分」 |

待測：實機上的第一次開機（root 延伸、磁碟識別碼、主機金鑰、資料分割區掛載、網路、`/dev/tpmrm0`、NTP 校時）在 Laysi/trustiot-v2#811 驗證。
