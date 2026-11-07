# 閘道器映像：資料分割區 setup 一步之內斷電、寫入遺失或重排的開機測試（2026-11-07）

對應 issue：Laysi/trustiot-v2#918。這份報告記錄兩件事：

- 開機測試（`edge/image/boot-test.ts`）新加的「記錄的 setup」；
- 映像檢查（`edge/image/check.ts`）新加的「開機測試的檔案」。

做法寫在 [`edge/image/README.md`](../../edge/image/README.md) 的「斷電時」、「開機測試」的「記錄的 setup」與「映像檢查」。實機（Laysi/trustiot-v2#811）與 lab 的閘道器都沒有動。

先講結果：

- **開機測試**：95 次開機，289 項有 284 項通過，4224 秒。
  - 不通過的 5 項都出自 setup 的同一個問題：改標籤時斷電（下面「資料遺失」），要另開 Bug。
  - 35-record-first-boot、49-record-start-again：各有 66 個重建的狀態在 TPM 解鎖、agent 執行之後，下一次開機重新開始並格式化，agent 寫的資料不見。
  - 47a、52a、66a 是這一類（66a 是清除重建的）各自的第一個狀態開機：TPM 解鎖、volume 掛上，開機後 `blkid` 卻讀到 `LABEL=trustiot-data-new`。接著的 47b、52b、66b 都重新開始，47a、52a 寫在 volume 上的標記，47b、52b 讀不到。
- **記錄的 setup**：
  - 第一次開機、重新開始、清除重建三次記錄，共 5861 個狀態、27 類，判斷共 116 秒；接著 43 次開機確認，每一次都在 21 到 83 秒內關機。
  - 除了上面那一類，每一類都照預期：重新開始、當第一次開機做、做好的 volume 照常解鎖、清除之前的 volume 再判定 TPM，或拒絕（不解鎖、不掛上、agent 不啟動、沒有密碼提示）。清除重建的標記從沒遺失。
  - 7 類拒絕是永久的（共 977 個狀態）：分割區不再改變，要回產線，另開 Bug。
  - 來源映像的 udisks2 在 setup 拒絕之後修補標頭（4 類、98 個狀態），那幾類下一次開機重新開始或照常判定；另開 Task。
- **寫標頭區的程式**：setup 沒做新標頭的每一次開機，核心追蹤列出的程式都正好是判斷時說的：setup 的 `systemd-cryptenroll` 或 `cryptsetup`、udisks2 的 `udisksd`，或沒有。每一次開機的追蹤有 5746 到 36991 筆區塊請求，沒有丟掉任何事件。
- **映像檢查**：24 項全部通過，含新的「boot test files」（74610 個路徑）。
- **單元測試**：`edge/image` 280 項全部通過（本機與建置主機）。
- **其他必要檢查**都通過：型別檢查、secret handling、repo-check、`changes.ts` 的規則、gitleaks。
- **之前兩次執行**：一次找出上面的問題與 udisks2，另一次因探針與一項檢查的錯誤沒有跑完（下面「之前的執行」）。這一次是兩項修正之後的。

## 環境

- **建置主機**：arm64 Linux VM，沒有 KVM，rootless podman，Bun 1.4.2（`.prototools`）。建置映像、檢查映像、單元測試與開機測試都在這台跑，透過共用的工作佇列。
- **工具容器與開機測試的套件**：都是 `edge/image/src/pins.ts` 的釘選值（snapshot.debian.org `20261030T000000Z`），與 [Laysi/trustiot-v2#867 的報告](2026-11-06-867-tpm-permanent-failure-wipe.md)相同。實際裝上的版本在 `boot-test.txt` 開頭：
  - Debian 核心 6.12.113+deb13-arm64
  - qemu-system-arm 1:10.0.13+ds-0+deb13u1
  - swtpm 0.7.1-1.5
  - tpm2-tools 5.7-1+b1
  - cryptsetup-bin 2:2.7.5-2
- **映像裡的版本**：
  - systemd 257.13-1~deb13u1，cryptsetup 2.7.5。
  - util-linux（`blkid`）、bash 與 udisks2 是來源映像的。
  - setup 對每一個重建的狀態的判斷，都以映像裡的這些程式執行（斷電副本 root 的 chroot）。
- **虛擬機**：與 Laysi/trustiot-v2#867 的報告相同（`qemu-system-aarch64 -machine virt`，TCG，swtpm 經 `tpm-tis-device`）。
  - 記錄的三次開機，閘道器磁碟改經 `blklogwrites`：底下的檔案節點 `cache.no-flush=on`，virtio-blk `write-cache=on`。其他開機照舊是 `cache=unsafe`。
  - 重建的狀態接著開機時，核心參數加上 `trace_event=block:block_bio_queue trace_buf_size=16M`。
- **程式**：分支 `test/918-data-setup-torn-writes` 的 `ab197838`，分支點是 main `4b2dc3e8`。
  - 建置主機上 `git ls-files -s edge/image` 的雜湊是 `c47575be`，不含 README 時是 `e609fc7b`。之後的 commit 只改了 README 與這份報告，不含 README 的雜湊相同。
  - `trustiot-gateway.img` 的 SHA-256：`ef517266472ab5fa8def21d3f40a7c3d9acc9ec69f55758935e746c45656570c`
  - agent 在建置主機原生建置（`cargo build --locked --release --target aarch64-unknown-linux-gnu -p agent`），SHA-256：`62365696619db96bf47cb884c129b87b9c7fc4514f7cdffdf57f96241e475da8`
- **管理者公鑰**：建置時以 `ssh-keygen` 產生的拋棄式 ed25519 金鑰，放在 repo 之外。
- **時段**：2026-11-07 18:13 到 19:49（+08:00）。建置 630 秒，映像檢查 676 秒，開機測試 4224 秒。
- **本機**：單元測試、型別檢查與其他必要檢查。
  - 開發時另在本機做過實驗，找出下面「之前的執行」那幾項失敗的原因：拋棄式容器，以及 KVM 虛擬機（Debian trixie amd64，與映像相同版本的 cryptsetup、systemd 與 udisks2，跑映像的 setup）。實驗用的是不在 repo 裡的暫時腳本。
  - 這份報告的數字，除了寫明是本機的，都來自建置主機上的最後一次執行。

## 指令

在 repo 根目錄、建置主機上執行。`<金鑰目錄>`、`<輸出>`、`<檢查報告>`、`<開機報告>` 都是 repo 之外的目錄（這次在 `~/.cache/` 底下）。

```sh
cargo build --locked --release --target aarch64-unknown-linux-gnu -p agent
ssh-keygen -q -t ed25519 -N '' -C '' -f <金鑰目錄>/admin
bun install --cwd edge/image --frozen-lockfile
bun run --cwd edge/image typecheck
bun run --cwd edge/image test
bun edge/image/build.ts --agent <trustiot-agent> --ssh-key <金鑰目錄>/admin.pub --out <輸出>
bun edge/image/check.ts --image <輸出>/trustiot-gateway.img --agent <trustiot-agent> --ssh-key <金鑰目錄>/admin.pub --report <檢查報告>
bun edge/image/boot-test.ts --image <輸出>/trustiot-gateway.img --report <開機報告>
```

本機的其他必要檢查：

```sh
bun run --cwd edge/image typecheck
bun run --cwd edge/image test
bun scripts/ci/check-secret-handling.ts
cargo test --locked -p repo-check
bun install --cwd scripts --frozen-lockfile
bun test scripts/ci/changes.test.ts
gitleaks git --redact --log-opts="origin/main..HEAD" .
git archive HEAD | tar -x -C <空目錄> && gitleaks dir --redact <空目錄>
```

各項的結果：

| 檢查 | 在哪 | 結果 |
| --- | --- | --- |
| `edge/image` 型別檢查 | 本機、建置主機 | 通過 |
| `edge/image` 單元測試 | 本機、建置主機 | 280 項全部通過 |
| 建置映像 | 建置主機 | 通過，630 秒 |
| 映像檢查 | 建置主機 | 24 項全部通過，676 秒 |
| 開機測試 | 建置主機 | 289 項有 284 項通過（下面「結果」），4224 秒 |
| `check-secret-handling.ts` | 本機 | 通過 |
| `cargo test --locked -p repo-check` | 本機 | 通過 |
| `bun test scripts/ci/changes.test.ts` | 本機 | 345 項全部通過 |
| gitleaks（分支的 commit、`git archive` 的樹） | 本機 | 都沒有發現 |

## 結果

原始資料在 [data/2026-11-07-918-data-setup-torn-writes/](data/2026-11-07-918-data-setup-torn-writes/)：

| 檔案 | 內容 |
| --- | --- |
| `check.txt` | 映像檢查（`check.ts`）的輸出 |
| `boot-test.txt` | 開機測試的輸出，每行開頭是開始後的秒數 |
| `boot-test.json` | 每一項的結果 |
| `<編號>-record-<路徑>.states.json` | 記錄的 setup：每一步的寫入範圍與結束碼、flush 的位置、每一筆寫入（位置、大小、屬於哪一步）、狀態的數目，以及每一類的結果、狀態數、來自哪些模型、是否永久 fail closed、第一個狀態 |
| `<開機>.probe.txt` | 每次開機的探針輸出 |
| `<開機>.cut.txt` | 兩步之間斷電的開機寫到結果磁碟的紀錄 |
| `power-cut-serial.txt` | 兩步之間斷電的開機在序列埠主控台留下的最後兩行 |
| `luks-header.json`、`luks-header-wiped.json` | 第一次開機與 15 清除重建後的 LUKS2 標頭 |

說明：

- 探針輸出與 LUKS 標頭裡 token 的 `tpm2-blob` 與 `tpm2_srk` 寫成 `(left out)`。指紋與標記都是軟體 TPM 的，照原樣留著。
- 寫入的紀錄（blklogwrites 的檔案，每次約 65 MB）與重建的狀態不放進來：`states.json` 列出每一筆寫入的位置與大小，狀態可以從它與模型重建。

### 1 到 34

情境與 Laysi/trustiot-v2#867 的報告相同，全部通過，到第 2611 秒。8-tpm-removed 的 setup 在第一次之後 0、10、…、170 秒各看一次 TPM，最後一次記在 181 秒。setup 在 170 秒看完時判斷 10 秒後的下一次仍在 180 秒內，所以再看一次；那一次的暫停拖長，記下的時間過了 180 秒。檢查改成 setup 的規則之後通過（下面「之前的執行」）。

### 記錄的 setup

| 記錄 | 寫入（flush） | 狀態 | 不同的分割區 | 類別 | 判斷 | 接著的開機 | 時段（秒） |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 35-record-first-boot | 300（52） | 2371 | 1155 | 9 | 54 秒 | 16 | 2611 到 3280 |
| 49-record-start-again | 316（34） | 1611 | 386 | 6 | 24 秒 | 9 | 3280 到 3588 |
| 56-record-wipe | 355（35） | 1879 | 558 | 12 | 38 秒 | 18 | 3588 到 4223 |

每一步的寫入數（到改標籤結束為止）：

| 步驟 | 第一次開機 | 重新開始 | 清除重建 |
| --- | ---: | ---: | ---: |
| luks-format | 0 | 0 | 0 |
| write-header | 1 | 1 | 1 |
| write-keyslots | 24 | 27 | 37 |
| tpm-enroll | 13 | 13 | 13 |
| mkfs | 209 | 222 | 251 |
| kill-slot | 46 | 46 | 46 |
| relabel | 6 | 6 | 6 |

三次的每一步都依序、結束碼都是 0。從紀錄算出的標頭區等於開機後磁碟上的。setup 對記錄前後的分割區的決定，與開機時相同。

每一類（模型欄是產生那一類狀態的模型與狀態數；一個不同的分割區可能來自兩種以上的模型）：

**第一次開機（35）**

| 結果 | 狀態 | 模型 | 開機確認 |
| --- | ---: | --- | --- |
| 重新開始 | 1377 | cut 294、torn-cut 551、lost 238、torn 294 | 36a 到 40a（write-header 到 kill-slot 做到一半） |
| 當第一次開機做 | 4 | cut 1、lost 3 | 44a |
| 拒絕：`blkid` 讀到 `trustiot-data-new`，cryptsetup 不是（標記檔欄位 `unknown`），永久 | 227 | torn-cut 9、lost 20、torn 198 | 42a |
| 拒絕：認不出的格式而不是零，永久 | 356 | lost 12、torn 344 | 43a |
| 拒絕：`blkid` 讀到別的標籤，永久 | 154 | torn 154 | 45a |
| 拒絕，udisks2 修補標頭，下一次重新開始 | 62 | torn 62 | 46a、46b |
| **TPM 解鎖，setup 修補標頭，下一次重新開始（資料遺失）** | 66 | cut 2、torn-cut 30、lost 6、torn 28 | **47a 不通過**、47b |
| TPM 解鎖，setup 修補標頭，下一次照常解鎖 | 112 | cut 3、torn-cut 37、lost 16、torn 56 | 41a（relabel 做到一半）、41b |
| TPM 解鎖 | 13 | cut 1、torn-cut 7、lost 5 | 48a |

**重新開始（49）**

| 結果 | 狀態 | 模型 | 開機確認 |
| --- | ---: | --- | --- |
| 重新開始 | 1328 | cut 311、torn-cut 576、lost 265、torn 176 | 50a |
| 拒絕：`blkid` 讀到 `trustiot-data-new`，cryptsetup 不是，永久 | 80 | lost 16、torn 64 | 51a |
| **TPM 解鎖，setup 修補標頭，下一次重新開始（資料遺失）** | 66 | cut 2、torn-cut 30、lost 6、torn 28 | **52a 不通過**、52b |
| TPM 解鎖，setup 修補標頭，下一次照常解鎖 | 112 | cut 3、torn-cut 37、lost 16、torn 56 | 53a、53b |
| 拒絕：標籤只寫進一部分，udisks2 修補標頭，下一次重新開始 | 12 | torn 12 | 54a、54b |
| TPM 解鎖 | 13 | cut 1、torn-cut 7、lost 5 | 55a |

**清除重建（56）**

| 結果 | 狀態 | 模型 | 開機確認 |
| --- | ---: | --- | --- |
| 帶著標記重新開始 | 1356 | cut 349、torn-cut 583、lost 264、torn 160 | 61a |
| 清除之前的 volume（再判定 TPM、再清除） | 74 | cut 1、lost 19、torn 54 | 58a |
| 拒絕：`blkid` 讀到 `trustiot-data-new`，cryptsetup 不是；setup 修補標頭，下一次是清除之前的 volume | 52 | torn-cut 32、lost 8、torn 12 | 57a、57b |
| 拒絕：`blkid` 讀到 `trustiot-data-new`，cryptsetup 不是，永久 | 68 | lost 8、torn 60 | 59a |
| 拒絕：有 TPM 以外的 keyslot（兩份 metadata 都讀不出），永久 | 80 | lost 8、torn 72 | 60a |
| 拒絕：有 TPM 以外的 keyslot；setup 修補標頭，下一次帶著標記重新開始 | 22 | lost 4、torn 18 | 62a、62b |
| 拒絕：`blkid` 讀到別的標籤；udisks2 修補標頭，下一次是清除之前的 volume | 6 | torn 6 | 63a、63b |
| 拒絕：`blkid` 讀到別的標籤，永久 | 12 | torn 12 | 64a |
| 拒絕：`blkid` 讀到別的標籤；udisks2 修補標頭，下一次帶著標記重新開始 | 18 | torn 18 | 65a、65b |
| **TPM 解鎖（帶標記），setup 修補標頭，下一次帶著標記重新開始** | 66 | cut 2、torn-cut 30、lost 6、torn 28 | **66a 不通過**、66b |
| TPM 解鎖（帶標記），setup 修補標頭，下一次照常解鎖 | 112 | cut 3、torn-cut 37、lost 16、torn 56 | 67a、67b |
| TPM 解鎖（帶標記） | 13 | cut 1、torn-cut 7、lost 5 | 68a |

接著的 43 次開機都在 21 到 83 秒內關機（每一次的 `qemu exited` 在 `boot-test.txt`）。探針每一段開始前把段名寫到序列埠：例如 47a 的序列埠紀錄，最後一段 `header-writes` 在核心時間 26.4 秒開始；QEMU 在啟動後 29 秒結束。

### 資料遺失：改標籤時斷電

三次記錄各有 66 個狀態是同一種：改標籤寫第一份 metadata 時斷電，第一份的二進位標頭已經寫成 `trustiot-data`、序號較大，checksum 還是舊的。四種模型都會留下這種狀態，包含卡片照順序寫的 cut（各 2 個）。以 47a 為例：

1. 開機前 `blkid` 讀主標頭（不檢查 checksum），讀到 `trustiot-data`，setup 走「做好了」的路，不問 cryptsetup 的標籤。
2. setup 記「the TPM (look 1, 0 s): opens」與「is encrypted and opens only with the TPM」。核心追蹤顯示 `systemd-crypten`（`systemd-cryptenroll`，setup 的 `tpm_only`）寫了標頭區的 sector 0（8 個）、8（24 個）、0（8 個），也就是 libcryptsetup 以第二份（`trustiot-data-new`）蓋掉 checksum 不對的第一份。
3. volume 解鎖、掛上，探針寫進標記檔 `5c9fd9ee…`（代表 agent 的資料）並讀回。開機後 `blkid` 讀到 `LABEL=trustiot-data-new`：這一項不通過。
4. 47b：setup 記「holds a setup that did not finish (no data was ever written to it): starting again」，格式化，標記檔不見了。

52a、52b（重新開始的記錄）相同。66a、66b 是清除重建：標頭帶著標記，agent 被 `/run/trustiot-back-to-factory` 擋著，沒有 agent 的資料遺失；但 TPM 解鎖時標頭說「沒做完」的不變式同樣不成立，下一次開機帶著標記重新開始。

這是 setup 的問題（改標籤的規則來自 Laysi/trustiot-v2#810：「重新開始」要 cryptsetup 同意，「做好了」不必），不在這個 issue 的範圍；開機測試的這幾項維持不通過，不改成預期的失敗。

### udisks2

來源映像的 udisks2 在 multi-user.target 啟動，以 libblockdev（libcryptsetup 的 `crypt_load`）讀每一個 LUKS 分割區。一份 metadata 的 checksum 不對、另一份是好的時，它以好的那份蓋掉壞的，setup 拒絕的分割區也是：

- 46a、54a、63a、65a：setup 拒絕，核心追蹤只列出 `udisksd` 寫標頭區（3 筆），接著的開機照判斷重新開始，或是清除之前的 volume。
- setup 格式化的開機（例如 44a、46b、47b、52b、54b、57b、58a），追蹤裡另有數千筆標頭區的寫入記在 `udisksd` 名下，與 setup 的 `dd` 交錯。setup 的 `dd` 以 `conv=fsync` 寫完才結束，所以這些是 setup 寫進 page cache 的資料，在 setup 還在寫的時候，由 udisksd 對裝置的存取送出去的（推論：直接 I/O 的讀取，或關閉裝置時的 flush，會先寫出重疊的髒頁）。也就是說，udisks2 在 setup 進行中就讀資料分割區。這些開機的結果與判斷相同，都通過。

### 永久拒絕

7 類、977 個狀態被拒絕之後，分割區不再改變，之後每次開機都同樣拒絕，要回產線或現場重新設定（fail closed，沒有機密性影響）：

- 第一次開機 227 + 356 + 154 個；
- 重新開始 80 個；
- 清除重建 68 + 80 + 12 個。

都發生在寫兩份 metadata 的時候；最實際的一種不需要卡片亂序：第一次開機寫那 32 KiB 時照順序只寫進前面幾個 sector（torn-cut）。另有 4 類、98 個狀態原本也會永久拒絕，只因 udisks2 修補了標頭，下一次才重新開始或照常判定：udisks2 若被拿掉，它們也是永久的。

## 之前的執行

1. **2026-11-06，`8a263b5b`**（rebase 到 main 之前，分支點是 Laysi/trustiot-v2#867 的分支）：建置 622 秒，映像檢查 24 項全部通過；開機測試 3547 秒，208 項有 4 項不通過。
   - `49a-start-again-class-3`、`59a-wipe-class-8`：setup 說 TPM 解得開、volume 掛上，開機後 `blkid` 卻讀到 `LABEL=trustiot-data-new`。原因是上面的「資料遺失」。本機的 KVM 虛擬機以同一個狀態重現：核心追蹤看到 `systemd-cryptenroll` 在 3.188 秒改寫第一份，下一次開機重新開始，標記檔不見。
   - `50a-start-again-class-4`、`58a-wipe-class-7`：setup 拒絕，分割區卻變了。原因是 udisks2：本機的虛擬機看到 `udisksd` 在 4.108 秒寫標頭區；同一個狀態以 `systemd.mask=udisks2.service` 開機時，分割區不變。
   - 之後判斷加上 udisks2 的模擬（`classify.sh` 以 `cryptsetup isLuks` 代表）。標頭有變的狀態也改成接著開到下一次開機，在 TPM 解鎖的那次寫標記，並以核心追蹤檢查寫標頭區的程式。
2. **2026-11-07，`8b75e999`**（rebase 到 main 之後）：建置 621 秒，映像檢查 24 項全部通過；開機測試沒有跑完。
   - `36a-first-boot-mid-write-header` 與 `38a-start-again-class-1`：虛擬機 45 分鐘內都沒有關機，第一次開機的記錄剩下的開機因此沒有開。序列埠紀錄停在 `Reached target timers.target`：setup 做完、volume 解鎖並掛上，探針沒有結束，一顆 vCPU 一直在忙。
   - 只有這些接著開的開機打開區塊追蹤。原因在探針的 `header_writes`：它以 bash 的 `while read` 逐行讀 `/sys/kernel/tracing/trace`。`trace` 是可以 seek 的 seq_file，bash 每讀一行就 seek 回那一行的結尾，核心為了每一次 seek 從頭再走一遍追蹤，整個迴圈是平方的。
   - 本機 KVM 虛擬機（Debian 核心 6.12.113+deb13-amd64，同樣的追蹤參數）量到的：
     - 一次開機有 25529 筆 `block_bio_queue`。
     - `grep -c`、awk、`cat` 讀完整個追蹤各 0.03 到 0.04 秒。
     - bash 逐行讀前 250、500、1000、2000 行各 0.04、0.13、0.50、1.96 秒，行數加倍時間約變四倍；逐行讀 `/run` 裡的副本不超過 0.03 秒。
   - 修正（`a683aa5b`）：
     - awk 一次讀完追蹤。本機同一台虛擬機以修正後的函式（mawk）讀 25516 筆的追蹤，0.07 秒。
     - 探針每一段開始前把段名寫到 console；本身沒有時限的段最多 300 秒，超過時那一段失敗、在輸出寫明，探針繼續做下一段並關機。開機測試對任何一段超過時限的開機判為不通過。
     - `boot-test/` 的其他腳本（`power-cut.sh`、`classify.sh`、`tpm-late.sh`）沒有逐行讀 /proc、/sys 或追蹤的迴圈。
   - `8-tpm-removed`：setup 記下的看法在 0、10、…、170 秒，接著 181 秒；檢查要求最後一次不晚於 180 秒而不通過。setup 是在看完之後、以開機後的整數秒判斷 10 秒後的下一次是否仍在 180 秒內（`check_tpm`），暫停可能拖長，所以那一次記下的時間可能過了 180 秒：檢查比 setup 嚴格。修正（`ab197838`）：後面還有下一次的每一次，都要在第一次之後 170 秒內開始（`look.at + intervalS <= windowS`）。0 到 170 秒、接著 181 秒的看法通過，在 180 秒之後還再看一次的（例如 180 秒、190 秒）不通過。
   - 那一次已經跑完的兩次記錄（第一次開機、重新開始），各有 66 個資料遺失的狀態，與這一次相同。

## 結論

對照 Laysi/trustiot-v2#918 的驗收條件：

- **一步做到一半時斷電的情境加進開機測試，結果記在 `docs/test-reports/`**：通過。cut、torn-cut 模型涵蓋每一筆寫入之後的斷電與最後一筆寫一半。第一次開機每一步做到一半的狀態都實際開機（36a 到 41a）。三次記錄的每一類都開機確認。
- **寫入遺失或重排的情境加進開機測試**：通過。lost 與 torn 模型。模型做不到的（卡片弄壞沒有寫的位置、sector 內除了 metadata 第一個 sector 之外的寫一半、實際卡片的行為）寫在 README「斷電時」。
- **`check.ts` 確認正式映像不含開機測試的 shim 或探針，並有對應的單元測試**：通過。映像檢查的「boot test files」通過，`tests/boot-test-files.test.ts` 在 280 項單元測試裡。
- **測試找到的問題**：開機測試不全部通過。資料遺失的那一類（每次記錄 66 個狀態，47a、52a、66a）是 setup 的 Bug，要另開 issue 修正，修好之前這幾項維持不通過。永久拒絕與 udisks2 也要另開 issue。
