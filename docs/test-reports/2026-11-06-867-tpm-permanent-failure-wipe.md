# 閘道器映像：TPM 永久解不開時清除重建資料分割區並標記回產線（2026-11-06）

對應 issue：Laysi/trustiot-v2#867。這份報告記錄兩件事：

- 判定條件的實驗：在拋棄式容器裡，以 swtpm 看 TPM 與 cryptsetup 在各種情況下回報什麼。
- 開機測試（`edge/image/boot-test.ts`）：在 QEMU 裡以軟體 TPM 開機映像的副本。

判定條件、重試、清除重建、標記、現場與產線的做法寫在 [`edge/image/README.md`](../../edge/image/README.md) 的「TPM 永久解不開時」。實機（Laysi/trustiot-v2#811）與 lab 的閘道器都沒有動。

先講結果：

- **開機測試**：48 次開機，105 項全部通過，2634 秒。
  - **永久解不開**（換一顆 TPM；或同一顆 TPM 在放過閘道器金鑰之後以 `TPM2_Clear` 清除）：連續 3 次看到同一個「另一個 owner hierarchy、沒有金鑰」之後清除重建。標頭留下標記，agent 因 condition 不啟動，沒有密碼提示。
  - **暫時解不開**：沒有 TPM、TPM 晚 150 秒才出現、標頭沒有指紋、owner hierarchy 有密碼、TPM 裡有另一台閘道器的金鑰，都不清除。TPM 晚出現時，出現後的那次就照常解鎖。
  - **其他**：移除標記之後 agent 啟動。清除重建在三個點斷電，下一次開機都把標記留在標頭裡。0x81000001 放了別的 key 的 TPM，第一次開機 setup 拒絕加密。
- **單元測試**：`edge/image` 215 項全部通過。setup 的判斷以替身代替 TPM 與 cryptsetup，在 bash 裡逐項測。
- **其他必要檢查**都通過：型別檢查、secret handling、repo-check、`bun test scripts`、`changes.ts` 的規則。

## 環境

- **建置主機**：arm64 Linux VM，沒有 KVM，rootless podman，Bun 1.4.2（`.prototools`）。建置映像、檢查映像、單元測試與開機測試都在這台跑，透過共用的工作佇列。
- **工具容器與開機測試的套件**：都是 `edge/image/src/pins.ts` 的釘選值（snapshot.debian.org `20261030T000000Z`），與 [Laysi/trustiot-v2#917 的報告](2026-11-02-917-data-setup-power-cut.md)相同。實際裝上的版本在 `boot-test.txt` 開頭。
  - Debian 核心 6.12.113+deb13-arm64
  - qemu-system-arm 1:10.0.13+ds-0+deb13u1
  - swtpm 0.7.1-1.5
  - tpm2-tools 5.7-1+b1
  - cryptsetup-bin 2:2.7.5-2
- **映像裡的版本**：systemd 與 systemd-cryptsetup 是 257.13-1~deb13u1，cryptsetup 2.7.5，tpm2-tools 5.7。
- **虛擬機**：`qemu-system-aarch64 -machine virt -cpu max,pauth-impdef=on -smp 4 -m 2048 -accel tcg,thread=multi`，沒有網路。TPM 是 swtpm 經 `tpm-tis-device`。磁碟是建置主機上的檔案（`cache=unsafe`）；只有 6 的第一次開機限制讀取速度。
- **程式**：分支 `feat/867-tpm-permanent-failure-wipe` 的 `c90feb87`，分支點是 main `9f289c5b`。建置主機上 `git ls-files -s edge/image` 的雜湊是 `18a77978`，不含 README 時是 `9b096a2e`，與這個 commit 相同；之後只加了這份報告。
  - `trustiot-gateway.img` 的 SHA-256：`f788b3a05a665731a254aebe005e70e85a3cda81ab347ea69997e04696ce16f5`
  - agent 在建置主機原生建置（`cargo build --locked --release --target aarch64-unknown-linux-gnu -p agent`），SHA-256：`4155ea88d03a636cc67cc7b2357ca99a27ffc6ff3815abb1a0921949ec4b5d83`
- **管理者公鑰**：建置時以 `ssh-keygen` 產生的拋棄式 ed25519 金鑰，放在 repo 之外。
- **時段**：2026-11-06（+08:00）。
  - 建置、映像檢查、單元測試與開機測試是同一個工作，01:41 開始、02:47 結束：建置 631 秒、映像檢查 667 秒、開機測試 2634 秒。
  - 其他必要檢查是另一個工作，01:41 結束。
- **之前的版本**：`0a8ba639` 以前的版本也在同一台建置主機跑過。那時還沒有 0x81000001 的檢查與 34。這份報告的數字與原始資料都只來自 `c90feb87` 那一次。
- **實驗**：在建置主機的拋棄式容器裡做。容器映像是 `debian:trixie@sha256:2dc419c6d7fe3dad0a2f9fd469c34cc90b0f9ba642d8fde837fe3d7d34a0e7f1`，從同一個 snapshot 裝映像同版的 systemd、systemd-cryptsetup、cryptsetup-bin、tpm2-tools 與 swtpm。實驗只用 swtpm 與檔案，沒有碰真的裝置。

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

其他必要檢查：

```sh
bun scripts/ci/check-secret-handling.ts
cargo test --locked -p repo-check
bun install --cwd scripts --frozen-lockfile
bun test scripts
bun test scripts/ci/changes.test.ts
```

實驗的腳本在 data 目錄（`explore-1.sh`、`explore-2.sh`），開頭寫著做什麼。用 podman 執行（`<腳本>` 是那份腳本存成的檔案）：

```sh
podman run --rm --security-opt=label=disable -v <腳本>:/x/s.sh:ro <上面的 debian 映像> bash /x/s.sh
```

各項的結果：

- 建置 exit 0。
- 映像檢查 `23 of 23 checks passed`（`check.txt`）。
- 型別檢查 exit 0。
- `edge/image` 的單元測試 215 pass、0 fail。
- `check-secret-handling`：`791 file(s) checked, 9 container build file(s), no violations`。
- repo-check：26 與 11 項通過。
- `bun test scripts`：2024 pass、9 skip、0 fail。
- `changes.test.ts`：345 pass。

## 結果

原始資料在 [data/2026-11-06-867-tpm-permanent-failure-wipe/](data/2026-11-06-867-tpm-permanent-failure-wipe/)：

| 檔案 | 內容 |
| --- | --- |
| `explore-1.sh`、`explore-1.txt` | 實驗一的腳本與輸出：指紋的做法、各種 TPM 狀態下解封與指紋的結果、自訂 token |
| `explore-2.sh`、`explore-2.txt` | 實驗二的腳本與輸出：以 setup 的做法重做一次，加上標頭檔裡的 token 與 systemd-cryptenroll 的順序 |
| `check.txt` | 映像檢查（`check.ts`）的輸出 |
| `boot-test.txt` | 開機測試的輸出，每行開頭是開始後的秒數 |
| `boot-test.json` | 每一項的結果 |
| `<開機>.probe.txt` | 每次開機的探針輸出（含 setup 的日誌、`/run/trustiot-back-to-factory`、agent 的 unit 狀態） |
| `<開機>.cut.txt` | 斷電的開機寫到結果磁碟的紀錄 |
| `power-cut-serial.txt` | 斷電的開機在序列埠主控台留下的最後兩行 |
| `luks-header.json` | 第一次開機寫出的 LUKS2 標頭 |
| `luks-header-wiped.json` | 15 清除重建後的 LUKS2 標頭（含標記） |

說明：

- 探針輸出與 LUKS 標頭裡 token 的 `tpm2-blob` 與 `tpm2_srk` 寫成 `(left out)`。
- 指紋與標記都是軟體 TPM 的，照原樣留著。
- 報告目錄的路徑把建置主機的家目錄寫成 `~`。
- 開機測試也存了 `tpm-late-serial.txt`（9 在序列埠主控台上載入 TPM 驅動的那行），這次是空的：那行訊息是 notice 等級，序列埠主控台沒有印出，所以沒有放進來。TPM 出現的時間以 setup 的日誌為準（見下面的 9）。

### 實驗：TPM 與 cryptsetup 回報什麼

以 swtpm 做出 TPM A（密封 volume 的那顆）與 B，對同一個以 TPM A 登錄的 LUKS2 標頭檔，在每種情況下看 setup 的指紋，以及 0x81000001 那把 SRK 的 name（`explore-2.txt`）：

| 情況 | 指紋 | 0x81000001 |
| --- | --- | --- |
| A | `000bab4c…` | 同指紋 |
| A 再做一次 | 同上 | — |
| B（新的） | `000b0fbc…`，不同 | 沒有 |
| B，放了自己的標準 SRK | `000b0fbc…` | 同 B 的指紋 |
| A 以 `TPM2_Clear` 清除 | `000bf06d…`，不同 | 沒有（被清掉） |
| A，SRK 被移除（seed 沒變） | `000bab4c…`，同 A | 沒有 |
| A 在 dictionary attack lockout（`inLockout: 1`） | `000bab4c…`，同 A | 同 A |
| B 在 lockout | `000b0fbc…` | 沒有 |
| B，owner hierarchy 有密碼 | 做不出（`0x9A2`） | 沒有 |
| 沒有 TPM | `tpm2_getcap` exit 1 | — |

由此決定判斷的方式：

- 指紋只在 seed 換了時改變：換 TPM 或 `TPM2_Clear` 會變，移除 SRK、lockout 不會變。lockout 不影響做指紋（primary key 是 `noda`）。
- 做不出指紋時（owner 密碼）分不出來，不清除。
- 容器裡的 swtpm 沒有 `/dev/tpmrm0`，所以每種情況的解封都以 exit 1 結束（`explore-2.txt` 的 `unseal` 行），那一行不是判斷的依據。經 systemd 的 TPM token 解封（`opens`）只在開機測試的虛擬機裡驗證。setup 不以解封的錯誤訊息分暫時或永久，只看解不解得開；解不開時看指紋與 TPM 裡的金鑰。
- cryptsetup 2.7.5：
  - 自訂 type 的 token（沒有 keyslot）匯入 `/run` 上的標頭檔之後，id 不變。
  - 之後的 systemd-cryptenroll 取 token 0。
  - `cryptsetup luksDump` 把它們列成 `  1: trustiot-tpm-owner` 與 `  2: trustiot-back-to-factory`，底下沒有其他行。setup 從這份清單判斷有沒有標記。

### 開機測試

```text
bun edge/image/boot-test.ts --image <輸出>/trustiot-gateway.img --report <開機報告>
105 of 105 checks passed
boot-test exit 0 after 2634 s
```

1 到 7、12、13 與 19 到 29 是之前就有的情境（拒絕別的內容、第一次開機、重複 UUID、第一次開機與重新開始的斷電；這次重新編號），全部通過。30 以 TPM W 加密斷電副本的分割區，是 31 到 33 的起點。這次新加或改了判定的情境：

| 開機 | TPM | setup 每次看 TPM 的回答（第一次之後的秒數） | 結果 |
| --- | --- | --- | --- |
| 8 | 沒有 TPM | 0 到 180 秒每 10 秒一次，19 次都是 `absent` | 不清除；解不開、不掛上、agent 沒有啟動，手動啟動也一樣；標頭不變 |
| 9 | A，驅動在開機 150 秒後才載入 | 0 到 50 秒 `absent`（開機後 98 到 148 秒），60 秒 `opens`（開機後 158.7 秒） | 不清除；setup 在開機 158.9 秒結束，同一次開機解鎖、掛上、讀回標記檔 |
| 10 | B，標頭沒有指紋 | 18 次都是 `unrecorded`，最後一次在 175 秒 | 不清除；分割區逐位元組不變 |
| 11 | O，owner hierarchy 有密碼 | 18 次都是 `unknown-owner`，最後一次在 176 秒 | 不清除；標頭不變 |
| 14 | K，0x81000001 的 SRK 加 0x81000100 的金鑰 | 18 次都是 `keyed 0x81000100` | 不清除；日誌寫 `most likely another gateway's TPM`；標頭不變 |
| 15 | A-cleared：A 放了 0x81000100 的金鑰之後以 `tpm2_clear` 清除，清除後沒有任何持久物件 | 0、11、22 秒 `other-owner`（同一個指紋） | 清除重建（34.2 秒決定，37.5 秒完成）；標記的 `sealed_owner` 是 A、`found_owner` 是 A-cleared；原本的標記檔不在了；agent 因 condition 不啟動 |
| 16 | A-cleared | 第一次就 `opens` | 不再清除；標記還在，agent 不啟動；寫入並讀回新的標記檔；標頭和 15 寫的相同 |
| 17 | B | 0、11、21 秒 `other-owner` | 再清除重建一次，給 B；新標記的 `sealed_owner` 是 15 記下的指紋；16 寫的標記檔不在了；agent 不啟動 |
| 18 | B，在建置主機上移除標記 | 第一次就 `opens` | 沒有 `/run/trustiot-back-to-factory`，agent 啟動（測試映像沒有憑證，所以 agent 隨即以失敗結束，Laysi/trustiot-v2#812 的既有行為） |
| 31 到 33 | W 的 volume，各一顆新的 TPM | 3 次 `other-owner` | 清除重建在 `luksFormat` 之後、寫完 metadata 之後、改標籤之後斷電；下一次開機都以那顆 TPM 解鎖，標記在標頭裡，agent 不啟動 |
| 34 | X，0x81000001 是 RSA 的 storage key（只有它） | —（第一次開機） | setup 拒絕：`0x81000001 holds a key that is not this TPM's standard storage key, or it cannot be read`；分割區仍全是零，不掛上，agent 沒有啟動 |

每次開機都另外檢查這些項目：

- 沒有等待中的密碼提示，`ssh.service` 在執行。
- `/run/trustiot-back-to-factory` 只在標頭有標記時存在。
- 解鎖的開機：標頭記的指紋等於 0x81000001 那把 SRK 的 name。

15 的 setup 日誌（`15-tpm-a-cleared.probe.txt`，指紋縮短）：

```text
[   12.670909] trustiot-data-setup[456]: the TPM (look 1, 0 s): other-owner 000b2e2c…
[   23.234676] trustiot-data-setup[456]: the TPM (look 2, 11 s): other-owner 000b2e2c…
[   34.038870] trustiot-data-setup[456]: the TPM (look 3, 22 s): other-owner 000b2e2c…
[   34.159698] trustiot-data-setup[456]: the TPM's owner hierarchy is 000b2e2c…, the volume was sealed under 000bf3d2…: another TPM, or this one cleared; no TPM can open it again
[   34.161281] trustiot-data-setup[456]: wiping /dev/vda3 and making it anew for this TPM; the gateway goes back to the factory to be registered again
[   37.494168] trustiot-data-setup[456]: done: /dev/vda3 is encrypted and opens only with this gateway's TPM
[   37.737756] trustiot-data-setup[456]: /dev/vda3 was wiped (tpm-owner-changed) and carries the mark to go back to the factory: ...
[   38.541205] systemd[1]: trustiot-agent.service - TrustIoT gateway agent skipped, unmet condition check ConditionPathExists=!/run/trustiot-back-to-factory
```

同一次開機的 `/run/trustiot-back-to-factory`：

```text
reason=tpm-owner-changed
sealed_owner=000bf3d2cab7b37f0d5339daeea832c88ca62018441feb00a30d375d59403c50631a
found_owner=000b2e2cc97aad41a560bd1d11f841836cc5c969f22aaddca3cd4ac390c3cf0357b0
```

### 時間

- 清除重建的決定：第一次看到之後約 22 秒（3 次，間隔 10 秒加上每次看 TPM 的時間）。之後的加密、登錄、做檔案系統約 3 秒（虛擬機裡，TCG 模擬）。
- 不清除的情況，setup 看滿 180 秒才結束。沒有 TPM 時，systemd 先等 `/dev/tpmrm0` 約 90 秒，setup 才開始看（8 的 setup 在開機 95.7 秒開始，第一次看在 96.1 秒），所以那次開機的解鎖約在 277 秒時失敗。開機的其他部分不等它。
- 開機測試總共 2634 秒，[Laysi/trustiot-v2#917 的報告](2026-11-02-917-data-setup-power-cut.md)是 1227 秒（34 次開機）。多出來的主要是 8、10、11、14 各看滿 180 秒，以及多了 14 次開機。

## 結論

逐條對照 Laysi/trustiot-v2#867 的驗收條件：

1. **模擬永久解不開（換一個軟體 TPM，或清掉 TPM 裡的登錄）：資料分割區被清除重建，留下回產線重新登錄的標記，開機不卡在密碼提示**：通過。
   - 15：同一顆 TPM 以 `TPM2_Clear` 清除，清除前放過閘道器金鑰。
   - 17：換一顆軟體 TPM。
   - 兩者都清除重建，標頭有標記（`luks-header-wiped.json`），沒有等待中的密碼提示。
   - 31 到 33：清除重建中途斷電，標記不會丟。
2. **模擬暫時的錯誤（TPM 暫時沒有回應）：不清除，重試的條件照文件執行**：通過。
   - 8：沒有 TPM。
   - 9：TPM 晚 150 秒才出現。
   - 每次看 TPM 的間隔、時間窗與回答，都以 setup 的日誌檢查（`tpmLookProblems`）：間隔至少 10 秒，最後一次不晚於 180 秒。
   - 其他分不清的情況也不清除：10、11、14。
   - 單元測試涵蓋計數、時間窗，以及每一種回答（含 `silent`、`unknown-keys`）。
3. **清除後 agent 不運作，標記看得出來**：通過。
   - 15、16、17、31 到 33：agent 沒有啟動，unit 的 condition 檢查過而且不成立（`ConditionPathExists=!/run/trustiot-back-to-factory`，`ConditionResult=no`）。
   - systemd 每次啟動 unit 都檢查 condition，所以手動 `systemctl start` 也會跳過（README「標記與 agent」）。開機測試沒有在有標記的開機手動啟動 agent，這點沒有實測。
   - 標記看得出來的地方：
     - setup 日誌的 warning。
     - `/run/trustiot-back-to-factory` 的三行。
     - 標頭的 token 2。
     - `systemctl status trustiot-agent` 的 condition。
   - 18：移除標記之後，agent 照常啟動。
4. **`edge/image/README.md` 寫明判定條件、清除的流程，以及現場與產線要做什麼**：通過，見「TPM 永久解不開時」的這幾節：
   - 判定依據（指紋、TPM 裡有沒有金鑰）。
   - 每一次看 TPM 的回答。
   - 重試的條件、清除重建、標記與 agent。
   - 現場要做什麼、產線要做什麼。
   - 濫用與限制。

待測：

- 真實閘道器上的情況：實際的 TPM 模組換掉、清除，以及 TPM 沒有回應時的時間。燒錄 lab 的閘道器與實機開機都在 Laysi/trustiot-v2#811。
- 一步做到一半時斷電，以及卡片遺失或重排寫入：例如清除重建寫那 32 KiB 時只寫進一部分。延後在 Laysi/trustiot-v2#918。setup 在那種情況下的拒絕條件（兩份 metadata 的標籤不同、讀不出標記）只有單元測試。
- 只有單元測試、沒有開機測試的回答，以及沒有實測的部分：
  - `silent`（TPM 在、卻不回應）。
  - `unknown-keys`。
  - `keyed 0x81000001`（已經加密好的 volume 遇到 0x81000001 放了別的 key 的 TPM；開機測試的 34 只測第一次開機的拒絕）。
  - 指紋不同、SRK 卻是記下的那把時的 `same-owner`。
  - 登錄之後 0x81000001 換了的拒絕。
  - 有標記時手動 `systemctl start trustiot-agent` 也被 condition 擋下。
  - systemd-cryptenroll 遇到 0x81000001 已經有別的 key 時怎麼做。setup 在呼叫它之前就拒絕，所以結果不影響安全。
