# 閘道器映像：重新開始的資料分割區 setup 在斷電後不再被拒絕（2026-11-02）

對應 issue：Laysi/trustiot-v2#917（在 [Laysi/trustiot-v2#890 的報告](2026-11-01-890-boot-test-power-cut.md)裡以 KNOWN 釘住的問題）。這份報告記錄修正前的兩個實驗，以及修正後在 QEMU 裡以軟體 TPM 開機映像副本的開機測試（`edge/image/boot-test.ts`）。

實機（Laysi/trustiot-v2#811）與 lab 的閘道器都沒有碰。

先講結果：

- **實驗**：cryptsetup 2.7.5 在分割區上 `luksFormat` 時，會先把 16 MiB 清成零，再以亂數填 keyslot 區，最後才寫標頭。所以除了 setup 自己的 `dd` 清零，`luksFormat` 本身也有「標頭不在、後面卻不是零」的時段。
- **改法**：`luksFormat --header` 把標頭做在 `/run` 的檔案裡，不寫分割區。確認那個檔案之後，再以 `dd` 把它寫到分割區：先寫兩份 metadata，再寫 keyslot 區。在容器裡實測過：寫出的標頭和直接格式化的版面相同，資料區一個 byte 都沒動，寫完 metadata 時 `blkid` 已讀得到新標頭。做法比較在 [`edge/image/README.md`](../../edge/image/README.md) 的「斷電時」。
- **開機測試**：34 次開機、70 項全部通過，沒有 KNOWN。新加的斷電點都在下一次開機重新加密、解鎖、掛上，標頭只剩 TPM 的 keyslot。新加的斷電點是：第一次開機寫標頭的兩步之後；在上一次已經做出檔案系統的分割區上，重新開始的 setup 的 `luksFormat`、寫 metadata、寫 keyslot 區之後。原本被拒絕的那次開機（890 的 19b）對應這次的 22b。
- **重新開始多花的時間**：在虛擬機裡，setup 從「starting again」到「done」是 3.48 到 4.03 秒，890 那次是 3.36 到 3.47 秒，多 0.1 到 0.6 秒。寫進分割區的資料從約 48 MiB 減為 16 MiB。實際卡片上的時間待測，見「結論」。

## 環境

- **建置主機**：建置主機（arm64 Linux VM），沒有 KVM，rootless podman，Bun 1.4.2（`.prototools`）。建置映像、檢查映像、開機測試都在這台。
- **工具容器與開機測試的套件**：與 [Laysi/trustiot-v2#890 的報告](2026-11-01-890-boot-test-power-cut.md)相同，都是 `edge/image/src/pins.ts` 的釘選值（snapshot.debian.org `20261030T000000Z`）：
  - Debian 核心 6.12.113+deb13-arm64
  - qemu-system-arm 1:10.0.13+ds-0+deb13u1
  - swtpm 0.7.1-1.5
  - tpm2-tools 5.7-1+b1
  - cryptsetup-bin 2:2.7.5-2

  實際裝上的版本在 `boot-test.txt` 開頭。
- **虛擬機**：與 890 相同，`qemu-system-aarch64 -machine virt -cpu max,pauth-impdef=on -smp 4 -m 2048 -accel tcg,thread=multi`。TPM 是 swtpm 經 `tpm-tis-device`，沒有網路。磁碟是建置主機上的檔案（`cache=unsafe`），所以寫入幾乎不花時間；只有 6 的第一次開機限制讀取速度。
- **程式**：分支 `fix/917-data-setup-restart-power-cut` 的 `4b5d55f8`，分支點是 main `228693bc`。建置主機上 `git ls-files -s edge/image` 的雜湊（`e98b059b`）與這個 commit 相同。
  - `trustiot-gateway.img` 的 SHA-256：`8c2f4bd3a570dffdf2fd7f1e10952d6e9fd35556def1414dd3038a58a195b9d2`
  - agent 在建置主機原生建置（`cargo build --locked --release --target aarch64-unknown-linux-gnu -p agent`），SHA-256：`eccb38b95aab1c61dd9f194b8be49cf03de19bb49afdd5051d3ea4fd4482191a`
- **管理者公鑰**：建置時以 `ssh-keygen` 產生的拋棄式 ed25519 金鑰，放在 repo 之外。
- **時段**：2026-11-02（+08:00）。建置、映像檢查、型別檢查、單元測試與開機測試是同一個工作，02:12 排入，02:54 結束；開機測試 1227 秒。
- **前一次**：在這個 commit 之前，以 `28bcf9ad`（第二個 `dd` 還沒有 `count`，見「結果」）建置的映像（SHA-256 `d7e3bf41...`）跑過一次同樣的工作，結果相同：70 項通過，1281 秒。這份報告的數字與原始資料都來自 `4b5d55f8` 那一次。
- **實驗**：在本機的拋棄式容器裡做。映像是 `debian:trixie@sha256:2dc419c6d7fe3dad0a2f9fd469c34cc90b0f9ba642d8fde837fe3d7d34a0e7f1`，從同一個 snapshot 裝 cryptsetup-bin 2:2.7.5-2（與映像的版本相同）。實驗只在磁碟映像檔上做，沒有碰真的裝置。

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

實驗的腳本與輸出都在 data 目錄，每個檔案開頭寫著容器映像。用 podman 執行（`<腳本>` 是那份腳本存成的檔案）：

```sh
podman run --rm -v <腳本>:/x/s.sh:ro,Z <上面的 debian 映像> bash /x/s.sh
# detached-header.txt 的腳本需要一個 64 MiB 的 tmpfs，代替閘道器的 /run
podman run --rm --tmpfs /r:size=64m,mode=0755 -v <腳本>:/x/s.sh:ro,Z <上面的 debian 映像> bash /x/s.sh
```

結果：

- 建置 exit 0，626 秒。
- 映像檢查 `23 of 23 checks passed`（`check.txt`）。
- 型別檢查 exit 0。
- `edge/image` 的單元測試 149 pass、0 fail。

## 結果

原始資料在 [data/2026-11-02-917-data-setup-power-cut/](data/2026-11-02-917-data-setup-power-cut/)：

| 檔案 | 內容 |
| --- | --- |
| `luksformat-write-order.txt` | 實驗一：`luksFormat` 直接寫分割區時寫了哪裡、依什麼順序（`--debug` 與 strace） |
| `detached-header.txt` | 實驗二：新的做法在 tmpfs 上做標頭，再寫到裝著上一次密文的分割區 |
| `check.txt` | 映像檢查（`check.ts`）的輸出 |
| `boot-test.txt` | 開機測試的輸出。每行開頭是開始後的秒數 |
| `boot-test.json` | 每一項的結果 |
| `<開機>.probe.txt` | 每次開機的探針輸出 |
| `<開機>.cut.txt` | 斷電的開機寫到結果磁碟的紀錄 |
| `power-cut-serial.txt` | 斷電的開機在序列埠主控台留下的最後兩行 |
| `luks-header.json` | 第一次開機寫出的 LUKS2 標頭 |

探針輸出與 LUKS 標頭裡 token 的 `tpm2-blob` 與 `tpm2_srk` 寫成 `(left out)`。報告目錄的路徑把建置主機的家目錄寫成 `~`。

### 實驗一：`luksFormat` 直接寫分割區的順序

`--debug` 的輸出：

```text
Wiping LUKS areas (0x000000 - 0x1000000) with zeroes.
Wiping keyslots area (0x008000 - 0x1000000) with random data.
Updating keyslot area [0x8000].
```

strace 看到的寫入順序：

1. 16 次 1 MiB 的零（0 到 16 MiB），fsync。
2. 從 32 KiB 起約 16 MiB 的亂數，fsync。
3. 主標頭（0 與 4096），fsync。
4. 第二份標頭（16384 與 20480），fsync。
5. keyslot 0 的金鑰材料，接著兩份標頭再更新一次。

所以 1 之後、3 之前斷電時：

- 分割區開頭是零或亂數，沒有標頭。
- 上一次的 setup 已經做過 `mke2fs` 時，16 MiB 以後是它的密文。

`main()` 看到的就是「認不出格式但不是零」，會拒絕這個分割區。這和 setup 自己用 `dd` 清零造成的問題相同。原本的 `dd` 清零只是讓這個時段更長，拿掉它也不夠。

### 實驗二：先在 `/run` 做好標頭，再寫到分割區

`format()` 裡的同一行 `luksFormat --header`，跑在一個 64 MiB 的 tmpfs 上；stand-in 是 2 GiB 的 sparse 檔案。之後把標頭寫到一個前 64 MiB 是亂數（代表上一次的標頭與密文）、其餘是零的分割區映像檔：

```text
stand-in allocated: 0 KiB; tmpfs used: 16388 KiB
16777216 bytes, mode 600
UUID=9da7f680-5912-4d5c-bca6-d46c83e064f8
LABEL=trustiot-data-new
TYPE=crypto_LUKS
after the metadata copies: LABEL=trustiot-data-new TYPE=crypto_LUKS keyslots ["0"]
the partition's first 16 MiB equal the header file
bytes 16 MiB to 64 MiB are still the old ones
and the rest is still zeros
the key opens keyslot 0 through the header on the partition
{"segment":"16777216","config":{"json_size":"12288","keyslots_size":"16744448"}}
```

- `luksFormat` 沒有寫 stand-in（佔 0 KiB），`/run` 上只多了 16 MiB 的標頭檔，權限 0600。
- 標頭的版面與直接格式化的相同：資料從 16 MiB 開始，metadata 每份 16 KiB，keyslot 區 16744448 bytes。開機測試的 `luksHeaderProblems` 也檢查這三個值。
- 只寫了兩份 metadata（前 32 KiB）時，`blkid` 已讀得到 `trustiot-data-new` 與 keyslot 0。下一次開機的 setup 會把它當成「上次沒做完」重新開始。
- 兩個 `dd` 都寫完之後，分割區的前 16 MiB 就是標頭檔，後面一個 byte 都沒變；暫時金鑰經分割區上的標頭打得開 keyslot 0。

### 開機測試

```text
bun edge/image/boot-test.ts --image <輸出>/trustiot-gateway.img --report <開機報告>
70 of 70 checks passed
boot-test exit 0 after 1227 s
```

1 到 12（拒絕別的內容、第一次開機、換 TPM、重複 UUID 等）與 890 時相同，全部通過。其中 1-foreign-fs、2-foreign-luks、3-not-empty 是「不是 setup 留下的內容」，setup 都拒絕，分割區的 SHA-256 前後相同。

斷電的部分。「a」是斷電的那次開機，結果磁碟只有 `power-cut <步驟>`，之後在建置主機上讀分割區；「b」是同一份磁碟與 TPM 的下一次開機：

| 開機 | 斷電在 | 斷電後的分割區（建置主機上讀） | 下一次開機 |
| --- | --- | --- | --- |
| 13 | 第一次開機 `luksFormat` 之後 | 全是零 | `is empty: first boot`，加密、解鎖、掛上 |
| 14 | 寫完兩份 metadata 之後 | `trustiot-data-new`、keyslot 0 | `starting again`，重新加密、解鎖、掛上 |
| 15 | 寫完 keyslot 區之後 | `trustiot-data-new`、keyslot 0 | 同 14 |
| 16 | TPM 登錄之後 | `trustiot-data-new`、keyslot 0 與 1、TPM token | 同 14 |
| 17 | `mke2fs` 之後 | 同 16 | 同 14 |
| 18 | 清掉 keyslot 0 之後 | `trustiot-data-new`、keyslot 1、TPM token | 同 14 |
| 19 | 改標籤之前 | 同 18 | 同 14 |
| 20 | 改標籤之後 | `trustiot-data`、keyslot 1、TPM token | 不重新加密，解鎖、掛上 |
| 21 | 17a 之後的磁碟：重新開始的 setup 在 `luksFormat` 之後 | 沒變：同 16 | 同 14 |
| 22 | 同上：寫完兩份 metadata 之後（原本的 setup 在這裡已清掉標頭） | `trustiot-data-new`、只有 keyslot 0 | 同 14 |
| 23 | 同上：寫完 keyslot 區之後 | 同 22 | 同 14 |

- 每個「b」都確認標頭只剩 TPM 的 keyslot。13 到 19、21 到 23 在虛擬機裡與建置主機上各讀一次。
- 21 到 23 斷電之前，先確認那份磁碟標頭後面不是零（`before the setup starts again, the data partition holds the earlier attempt's file system behind the header`）。所以這三次測的是 890 的 19 那種分割區，不是空的分割區。
- 22b 以同一顆 TPM 開機，setup 重新開始，沒有拒絕（`22b-after-again-write-header.probe.txt`）：

```text
[    9.913832] trustiot-data-setup[456]: /dev/vda3 holds a setup that did not finish (no data was ever written to it): starting again
[   10.057474] trustiot-data-setup[456]: encrypting /dev/vda3 (LUKS2, aes-xts-plain64)
[   11.540719] trustiot-data-setup[456]: enrolling this gateway's TPM (no PCRs)
[   13.910213] trustiot-data-setup[456]: done: /dev/vda3 is encrypted and opens only with this gateway's TPM
```

### 重新開始的 setup 多花的時間

取探針日誌裡 setup 的時間戳：「總計」從 `starting again`（或 `is empty: first boot`）到 `done`，「加密」從 `encrypting` 到 `enrolling`。新的做法取這次（`4b5d55f8`），原本的做法取 890 那次（main `c6e464a9` 的映像，同一台建置主機、同樣的虛擬機設定）。單位是秒：

| 情況 | 新：總計 | 新：加密 | 原本：總計 | 原本：加密 |
| --- | --- | --- | --- | --- |
| 重新開始（新：4、14b 到 19b、21b 到 23b，共 10 次；原本：4、13b 到 17b，共 6 次） | 3.48 到 4.03 | 1.07 到 1.57 | 3.36 到 3.47 | 0.93 到 1.19 |
| 22b（原本這次被拒絕） | 4.00 | 1.48 | 拒絕 | — |
| 第一次開機（6，讀取限速） | 2.43 | 0.67 | 2.10 | 0.46 |

- 在虛擬機裡，新的做法多 0.1 到 0.6 秒，差別都在「加密」這一步。這台虛擬機是 TCG 模擬，每個程序都慢；新的這一步多了 `truncate`、`stat`、三次 `blkid` 與一個 `dd`。虛擬機的磁碟寫入不花時間（`cache=unsafe`），所以寫入量的差別在這裡看不出來。
- 寫進分割區的量，依實驗一數出來：
  - 原本約 48 MiB：`dd` 清零 16 MiB，`luksFormat` 再清零 16 MiB、填約 16 MiB 亂數，加上標頭。
  - 新的做法 16 MiB：`luksFormat` 的清零與亂數只寫在 `/run`（記憶體）。

  以每秒 10 MB 寫入的卡片推算，原本約 5 秒，新的約 1.7 秒。這是推算，不是實測。
- 第一次開機也一樣，少寫約 32 MiB。

## 結論

逐條對照 Laysi/trustiot-v2#917 的驗收條件：

1. **重新開始的 setup 在原本清掉標頭、寫新標頭之前斷電，下一次開機重新加密，不拒絕、不留密碼 keyslot**：通過。
   - 新的 setup 不再清零。原本的那個時段，現在是 22（寫完 metadata 之後），前後兩點是 21（`luksFormat` 之後）與 23（寫完 keyslot 區之後）。
   - 三次的下一次開機都重新加密、解鎖、掛上，標頭只有 TPM 的 keyslot。22b 就是原本 19b 的情境。
2. **開機測試原本的 13 到 18 仍通過，拿掉 `knownBug`，沒有 KNOWN**：通過。
   - 原本的 13 到 18 是這次的 13、16 到 20，另外加了 14、15。
   - `knownBug` 與 KNOWN 的機制已從 `boot-test.ts` 拿掉，70 項全部 PASS。
3. **不是 setup 留下的非零內容仍拒絕**：通過。
   - 1-foreign-fs、2-foreign-luks、3-not-empty 都拒絕，分割區不變。
   - `main()` 的判斷沒有改。
4. **量測重新開始多花的時間**：虛擬機裡通過，見上一節（多 0.1 到 0.6 秒，寫入量從約 48 MiB 減為 16 MiB）。實際卡片上的時間待測：要在閘道器上量，這次沒有碰實機；燒錄 lab 的閘道器並實機開機在 Laysi/trustiot-v2#811。
5. **README 更新**：通過。[`edge/image/README.md`](../../edge/image/README.md) 改了這幾處：
   - 「何時加密」第 3 步。
   - 新的「斷電時」：需求、原本的問題、三種做法的比較、採用的做法、沒有涵蓋的風險。
   - 「開機測試」的步驟與 13 到 23 的表。

沒有涵蓋、延後在 Laysi/trustiot-v2#918：

- 一步做到一半時斷電（例如第一次開機寫那 32 KiB 時只寫進一部分）。
- 卡片不理會 fsync 或重排寫入。
- 斷電 shim 本身的檢查。

遇到時仍是 fail closed，清除重建在 Laysi/trustiot-v2#867。
