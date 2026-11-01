# 閘道器映像：資料分割區鎖住時手動啟動 agent、第一次開機加密中途斷電（2026-11-01）

對應 issue：Laysi/trustiot-v2#890（延後自 Laysi/trustiot-v2#810）。這份報告記錄開機測試（`edge/image/boot-test.ts`，在 QEMU 裡以軟體 TPM 開機映像的副本）新加的兩組情境：

- 資料分割區鎖住時，以 root 手動啟動 agent。
- 第一次開機的 setup 在每一步之後斷電。

實機（Laysi/trustiot-v2#811）與 lab 的閘道器都沒有碰。

結果先講：開機測試 26 次開機、51 項全部通過，其中 2 項標為 KNOWN，釘住一個已知問題：

- **手動啟動 agent**：資料分割區鎖住時以 root 啟動 agent，兩種方式都失敗。一般的 `systemctl start` 因相依失敗；略過相依的那次因 `AssertPathIsMountPoint=` 失敗。agent 從未執行，root 上的掛載點仍是空的。
- **issue 列出的每一步之後斷電**：`luksFormat`、TPM 登錄、`mke2fs`、清掉暫時金鑰的 keyslot、改標籤之前，另外加測改標籤之後。下一次開機都重新加密或正常開啟，標頭只剩 TPM 的 keyslot。
- **已知問題**：在重新開始的 setup 裡、把分割區開頭清成零之後斷電，下一次開機會拒絕這個分割區。這是 setup 的 bug，見「已知問題」。它仍是 fail closed：不解鎖、不掛上、agent 不啟動。

## 環境

- **建置主機**：建置主機（arm64 Linux VM），沒有 KVM，rootless podman，Bun 1.4.2（`.prototools`）。映像的建置與開機測試都在這台。
- **工具容器與開機測試的套件**：和 [Laysi/trustiot-v2#810 的報告](2026-10-31-810-data-partition-luks.md)相同，都是 `edge/image/src/pins.ts` 的釘選值（snapshot.debian.org `20261030T000000Z`）：
  - Debian 核心 6.12.113+deb13-arm64
  - qemu-system-arm 1:10.0.13+ds-0+deb13u1
  - swtpm 0.7.1-1.5
  - tpm2-tools 5.7-1+b1
  - cryptsetup-bin 2:2.7.5-2

  實際裝上的版本在 `boot-test.txt` 開頭。
- **虛擬機**：`qemu-system-aarch64 -machine virt -cpu max,pauth-impdef=on -smp 4 -m 2048 -accel tcg,thread=multi`，TPM 是 swtpm 經 `tpm-tis-device`，沒有網路。
- **映像**：以 main `6651a296` 的 `edge/image` 與 `edge/agent` 建置。這個分支沒有改映像的內容，只改開機測試。
  - `trustiot-gateway.img` 的 SHA-256：`70ac408169e0916aac33ba591ec45af2e0f03d8cadc98f46e8979a2bdd7fd9a1`
  - agent 在建置主機原生建置（`cargo build --locked --release --target aarch64-unknown-linux-gnu -p agent`），SHA-256：`977ff883fc47091273dd55c788e61af332555dfafddb48e9f136ad851331a759`
- **程式**：分支 `test/890-boot-test-locked-and-power-cut`。開機測試以和 commit `0622a193` 加上 `process.exit` 修正（見「測試時間」）相同的工作樹執行，`edge/image` 的 `git ls-files -s` 雜湊在建置主機與本機相同。這份報告與那項修正在下一個 commit。
- **管理者公鑰**：建置時以 `ssh-keygen` 產生的拋棄式 ed25519 金鑰，放在 repo 之外。
- **時段**：2026-11-01（+08:00）。映像建置在 08:44 結束；最後一次開機測試從 11:09 排入，11:25 結束（990 秒）。

## 指令

在 repo 根目錄、建置主機上執行。`<金鑰目錄>`、`<輸出>`、`<開機報告>` 都是 repo 之外的目錄（這次在 `~/.cache/` 底下）。

```sh
cargo build --locked --release --target aarch64-unknown-linux-gnu -p agent
ssh-keygen -q -t ed25519 -N '' -C '' -f <金鑰目錄>/admin
bun install --cwd edge/image --frozen-lockfile
bun edge/image/build.ts --agent <trustiot-agent> --ssh-key <金鑰目錄>/admin.pub --out <輸出>
bun edge/image/boot-test.ts --image <輸出>/trustiot-gateway.img --report <開機報告>

# CI 會跑的部分
bun run --cwd edge/image typecheck
bun run --cwd edge/image test
```

## 結果

原始資料在 [data/2026-11-01-890-boot-test-power-cut/](data/2026-11-01-890-boot-test-power-cut/)：

| 檔案 | 內容 |
| --- | --- |
| `boot-test.txt` | 開機測試的輸出。每行開頭是開始後的秒數，可以看出每一步花的時間 |
| `boot-test.json` | 每一項的結果；KNOWN 的項目有 `knownBug` |
| `<開機>.probe.txt` | 每次開機的探針輸出 |
| `<開機>.cut.txt` | 斷電的開機寫到結果磁碟的紀錄 |
| `power-cut-serial.txt` | 斷電的開機在序列埠主控台留下的最後兩行 |
| `luks-header.json` | 第一次開機寫出的 LUKS2 標頭 |

探針輸出與 LUKS 標頭裡 token 的 `tpm2-blob` 與 `tpm2_srk` 寫成 `(left out)`。

```text
bun edge/image/boot-test.ts --image <輸出>/trustiot-gateway.img --report <開機報告>
51 of 51 checks passed (2 of them pin a known bug: KNOWN)
exit 0 at 11:25:41 (build host)
```

1 到 12 的情境和 Laysi/trustiot-v2#810 的報告相同，全部通過；各次開機的 QEMU 時間在 `boot-test.txt`。下面只列新加的部分。

### 資料分割區鎖住時手動啟動 agent（8-other-tpm）

TPM B 解不開這個 volume：解鎖失敗、不掛上、agent 沒有啟動。原本的檢查照舊。

開機完成、探針記錄完之後，核心參數 `trustiot_test.start-agent=1` 讓探針以 root 啟動 agent 兩次。`8-other-tpm.probe.txt` 的 `start-agent*` 區段：

```text
=== start-agent
A dependency job for trustiot-agent.service failed. See 'journalctl -xe' for details.
=== exit 1
=== start-agent-ignoring-dependencies
Assertion failed on job for trustiot-agent.service.
=== exit 1
```

**一般的 `systemctl start trustiot-agent.service`**：

- 它再拉起一次解鎖：`systemd-cryptsetup@trustiot_data.service` 在 16.0 秒再執行一次，又以 `exit-code` 失敗。
- agent 的啟動因相依失敗而取消：`Job trustiot-agent.service/start failed with result 'dependency'`。
- 之後 `/run/systemd/ask-password` 仍是空的，沒有密碼提示。

**`systemctl start --job-mode=ignore-dependencies trustiot-agent.service`**：略過所有相依，只剩 agent 自己的 assert 擋著。

- 日誌：`Starting requested but asserts failed.`、`Assertion failed for trustiot-agent.service`。
- agent 的狀態：`AssertResult=no`、`AssertTimestampMonotonic=16175845`（assert 有評估），`ExecMainPID=0`、`ExecMainStartTimestampMonotonic=0`、`InactiveExitTimestampMonotonic=0`、`ActiveState=inactive`。agent 從未離開 inactive。
- assert 在 `ExecStartPre=`（nft）與建立 `StateDirectory=` 之前評估，所以兩者都沒有執行。

**兩次之後**：

- 兩次都在 600 秒的時限內結束。
- 沒有任何程序的執行檔是 `/usr/bin/trustiot-agent`（掃描 `/proc/*/exe`）。
- `/var/lib/trustiot-agent` 沒有掛載，root 上的這個目錄仍是空的。
- 掛載與解鎖都不是 active。

這次開機的 QEMU 時間是 20 秒，Laysi/trustiot-v2#810 是 19 秒。

### 第一次開機的 setup 在每一步之後斷電（13 到 18）

**斷電的做法**：

1. 每組用「斷電副本」的一份新副本。斷電副本是測試副本再加上 `boot-test/power-cut.sh`，以 `/usr/local/sbin/` 的 `cryptsetup`、`systemd-cryptenroll`、`mke2fs`、`dd` 連結指向它。資料分割區全是零，每組有自己的 TPM（P-<步驟>）。
2. 核心參數 `trustiot_test.power-cut=<步驟>` 讓 setup 做完那一步時，把 `power-cut <步驟>` 寫到結果磁碟，接著 sysrq `b` 立刻重置。`before-relabel` 是改標籤之前。
3. 在建置主機以 `blkid -p` 與 `cryptsetup luksDump --dump-json-metadata` 讀分割區，確認停在那一步留下的狀態。
4. 以同一顆 TPM 正常開下一次機。

**怎麼確認斷電發生在那一步**：

- 結果磁碟是 `power-cut <步驟>`，探針沒有執行。
- 序列埠主控台最後是 `trustiot-boot-test: power cut at <步驟>` 與 `sysrq: Resetting`（`power-cut-serial.txt`）。
- 分割區的狀態符合那一步。

| 斷電 | QEMU（秒） | 斷電後的分割區（建置主機讀） | 下一次開機 | QEMU（秒） | 下一次開機看到的 |
| --- | --- | --- | --- | --- | --- |
| 13a `luks-format`（`luksFormat` 之後） | 28 | 標籤 `trustiot-data-new`，只有 keyslot 0，沒有 token | 13b | 20 | `holds a setup that did not finish ... starting again`，重新加密 |
| 14a `tpm-enroll`（TPM 登錄之後） | 27 | `trustiot-data-new`，keyslot 0 與 1，keyslot 1 上的 `systemd-tpm2` token | 14b | 20 | 同 13b |
| 15a `mkfs`（`mke2fs` 之後，解開的 volume 還開著） | 29 | 同 14a | 15b | 22 | 同 13b |
| 16a `kill-slot`（清掉暫時金鑰的 keyslot 之後） | 29 | `trustiot-data-new`，只有 keyslot 1 與它的 token | 16b | 22 | 同 13b |
| 17a `before-relabel`（改標籤之前） | 29 | 同 16a | 17b | 20 | 同 13b |
| 18a `after-relabel`（改標籤之後） | 29 | 標籤 `trustiot-data`，只有 keyslot 1 與它的 token | 18b | 20 | `is encrypted and opens only with the TPM`，不重新加密 |

QEMU 的秒數是 `boot-test.txt` 裡每次開機的 `qemu exited ... after N s`。第一次開機的 setup 先讀完整個 2 GiB 分割區，確認全是零，所以斷電都在開機後 26 到 29 秒（`power-cut-serial.txt`）。

13b 到 18b 的每一次開機都檢查並通過：

- 在虛擬機裡：
  - setup 是 `active/success`。
  - 以 TPM 解鎖，`/dev/mapper/trustiot_data` 以 `rw,nosuid,nodev,noexec,noatime` 掛在 `/var/lib/trustiot-agent`。
  - 根目錄是 `trustiot-agent trustiot-agent 700`，agent 啟動。
  - `blkid` 讀到 `crypto_LUKS`、釘選的 UUID 與標籤 `trustiot-data`。
  - `systemd-cryptenroll` 只列出 `1 tpm2`。
  - 沒有等待中的密碼提示，`/var/log/journal` 是空的，只有 zram 置換空間，沒有 core 檔。
- 在建置主機上：`luksHeaderProblems` 確認標頭只有一個 keyslot，它的 TPM token 沒有 PCR、沒有 PIN。

**沒有任何一次留下密碼 keyslot，也沒有任何一次拒絕。**

15b 的 setup 日誌（`15b-after-mkfs.probe.txt`）是一次完整的重新加密：

```text
[    9.647872] trustiot-data-setup[443]: /dev/vda3 holds a setup that did not finish (no data was ever written to it): starting again
[    9.753552] trustiot-data-setup[443]: encrypting /dev/vda3 (LUKS2, aes-xts-plain64)
[   10.802825] trustiot-data-setup[443]: enrolling this gateway's TPM (no PCRs)
[   11.606852] systemd-cryptenroll[644]: New TPM2 token enrolled as key slot 1.
[   12.649491] trustiot-data-setup[443]: wiping the key's keyslot: from now on only this TPM opens the volume
[   12.994689] trustiot-data-setup[443]: done: /dev/vda3 is encrypted and opens only with this gateway's TPM
```

### 重新開始的 setup 清掉標頭之後斷電（19，已知問題）

19a 用 15a 斷電之後的磁碟與 TPM 的副本。那時已經做過 `mke2fs`，分割區後段有加密過的檔案系統。

19a 開機時，setup 認出 `trustiot-data-new`，重新開始。它先把開頭 16 MiB 清成零，這一步之後斷電（QEMU 11 秒，斷電在開機後 10.4 秒）。

建置主機讀到的分割區：

- 開頭 16 MiB 是零，`blkid` 認不出格式。
- 分割區不是全零：後段還有上一次的密文。

19b 以同一顆 TPM 開機，setup 拒絕（`19b-after-zero-header-again.probe.txt`）：

```text
[   10.250263] trustiot-data-setup[454]: refusing: /dev/vda3 has no known format but is not empty; not formatting it
```

- 仍是 fail closed：不解鎖、不掛上、agent 沒有啟動、root 上的掛載點是空的。
- 沒有密碼提示，ssh 在執行。
- 分割區的 SHA-256 在 19b 前後相同。

開機測試以 KNOWN 釘住這個行為，見下一節。

## 已知問題

**一句話**：重新開始的 setup 在把分割區開頭清成零之後、`luksFormat` 寫好新標頭之前斷電，之後每一次開機都拒絕這個分割區。

**為什麼會發生**：

- `trustiot-data-setup` 的 `format()` 先以 `dd` 把開頭 16 MiB 清成零，再做 `luksFormat`。
- 第一次開機時，整個分割區本來就全是零，這段斷電沒有影響。
- 但上一次的 setup 若已經做過 `mke2fs`，分割區後段就有密文。這時把標頭清掉，分割區就落在「認不出格式、但不是零」，`main()` 會拒絕這種內容（為了不格式化別人的資料）。
- 會出事的時段，從 `dd` 寫第一個區塊（同時清掉主標頭與第二份標頭）開始，到 `luksFormat` 寫好新標頭為止。
- 推論（沒有實測）：第一次開機的 `luksFormat` 若在寫到一半時斷電，留下不完整的標頭與 keyslot 區，也可能落在同一個狀態。

**觸發條件**：同一台閘道器的第一次開機要斷電兩次：

1. 第一次在 `mke2fs` 之後、改標籤之前。
2. 第二次在下一次開機重新開始時，落在上述約一秒的時段。

機率低，但一旦發生不會自己恢復。

**影響**：

- 沒有機密性的影響：volume 從來沒有資料（標籤仍是 `trustiot-data-new`），也不解鎖、不掛上、agent 不啟動，沒有密碼 keyslot 或提示。
- 影響是可用性：資料分割區每次開機都被拒絕，要有人清掉資料分割區，或回到工廠重新登錄（Laysi/trustiot-v2#867）。ssh 仍可登入。

**處理**：依 issue 的範圍，這裡不改 setup 的設計，另開 Bug Laysi/trustiot-v2#917。改法的方向之一：重新開始時，保留 `trustiot-data-new` 標頭，先把標頭之後的資料區清成零並 fsync，再清標頭、`luksFormat`。這樣在任何一點斷電，下一次開機看到的都是「new 的標頭」或「全零」。代價是重新開始時多寫 2 GiB，要在慢的卡上量時間。

**修好之後**：把 `edge/image/src/inside/boot-test.ts` 的 19b 改成和 13b 相同的預期（解鎖、掛上、只剩 TPM 的 keyslot），刪掉 `knownBug`。

**KNOWN 的意思**：那兩項把現在的行為（拒絕、fail closed、分割區不變）當成預期：

- 行為不變時算通過，但印成 KNOWN 並附上這一節的位置。
- 行為一改變（修好或變得更糟）就不通過。

## 測試時間

| 項目 | 增加的時間 |
| --- | --- |
| 準備斷電副本（解開 root 分割區、放進 shim、`e2fsck`、放回） | 7 秒（`boot-test.txt` 的 50 到 57 秒） |
| 手動啟動 agent | 不增加開機次數；8-other-tpm 的 QEMU 時間 20 秒，原本 19 秒 |
| 13a 到 19b 的 14 次開機，加上每次斷電後在建置主機讀分割區 | 334 秒（655 到 989 秒）；QEMU 本身 11 到 29 秒一次 |

新情境合計約 341 秒。整個開機測試這次 990 秒，Laysi/trustiot-v2#810 的報告是 3359 秒。差這麼多是因為下面這項修正，不是新情境變快。

**順手修正的既有問題**：

- **現象**：每次開機都以 `Bun.sleep(BOOT_LIMIT_MS)`（45 分鐘）與 QEMU 競賽，QEMU 先結束後，那個計時器仍在。所有項目都通過時 `main()` 正常返回，不呼叫 `process.exit`，程序要等最後一個計時器到期才結束。
- **影響**：容器多留約 45 分鐘才結束。這次加上 `process.exit` 之前的兩次執行，最後一項在 981 與 992 秒就印出，整個測試卻到 3662 與 3673 秒才結束；Laysi/trustiot-v2#810 的 3359 秒也是同一個原因。
- **修正**：`main()` 最後一律以 `process.exit` 結束。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 開機測試多一個情境：資料分割區鎖住時手動啟動 agent，啟動失敗、agent 沒有執行、root 上的掛載點仍是空的；結果記在 `docs/test-reports/` | **通過**：8-other-tpm 的手動啟動。一般啟動因相依失敗；略過相依的啟動被 `AssertPathIsMountPoint=` 擋下（`AssertResult=no`，assert 有評估）。`ExecMainPID=0`、從未離開 inactive、沒有程序執行 agent，不掛上，root 上的掛載點是空的，沒有密碼提示 |
| 開機測試多一組情境：第一次開機的 setup 在上列每一步之後中斷，下一次開機都重新加密或正常開啟，不拒絕、不留下密碼 keyslot；結果記在 `docs/test-reports/` | **通過**：issue 列出的五個時點各自通過，另加改標籤之後（13 到 18）。斷電發生在那一步，由結果磁碟、序列埠與斷電後分割區的狀態三方確認。下一次開機 1 到 5 重新加密、改標籤之後正常開啟，標頭只剩 TPM 的 keyslot（虛擬機裡與建置主機上各確認一次）。列表以外的 19（重新開始的 setup 清掉標頭之後斷電）發現 setup 的 bug，已釘成 KNOWN，另開 Bug Laysi/trustiot-v2#917（見「已知問題」） |

### 待測與延後

- **斷電模型的限制**：這裡的斷電是在兩步之間重置虛擬機，不是真的斷電。
  - 已經交給虛擬磁碟的寫入都留著，因為磁碟是 `cache=unsafe`，QEMU 不理會 guest 的 flush。
  - 不模擬一步做到一半時斷電，例如 `luksFormat` 寫標頭寫到一半，也不模擬 SD 卡自己的快取遺失、寫一半或重排寫入。
  - 這些屬於進階的故障注入，延後到 Laysi/trustiot-v2#918（同一張也包含映像檢查確認正式映像裡沒有開機測試的 shim）。可能的作法：在一步執行中途觸發 sysrq，或在建置主機以 QEMU 的 blkdebug 在每個 flush 點截斷。
- **實機上的斷電與手動啟動**：Laysi/trustiot-v2#811。
