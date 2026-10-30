# 產線工作站在展示環境以 lab 的閘道器實機登錄（2026-10-30）

對應 issue：Laysi/trustiot-v2#618（父 issue Laysi/trustiot-v2#80）。照 issue「範圍」的 A 到 D 執行，執行者是專案的協調者，平台管理員的 email 驗證碼由負責人在自己的終端機輸入（驗證碼與 cookie 都沒有經過執行者的指令或日誌）。

## 環境

- **工作站**：x86_64 Linux，一般帳號，有 `XDG_RUNTIME_DIR`。`trustiot-workstation` 從 main `6c97de88685fa7419d15f6363ffbd8d0e0c6bc80` 以 `cargo build --release --locked -p workstation` 建出，SHA-256 `2b54bc931457f0c4b62d948438daefde017162be3a68df201814ba7f597c7108`。工作站名稱 `lab-coordinator-1`，工作站憑證序號 `76f711de6164e7b02a81df742309af14`，到期 2027-04-28T09:41:04Z。
- **上傳的 agent**：同一個 commit，以 `cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent` 建出（不帶 feature），SHA-256 `4c3154cd4d17fc3799c663511cdaad3b4dc61b0531ee498ef5a6185aec15ed91`。
- **閘道器**：lab 的閘道器（也是 Laysi/trustiot-v2#458 的展示用閘道器），2026-10-30 重灌為 Raspberry Pi OS Lite arm64（Debian 13 trixie），kernel `6.18.50+rpt-rpi-v8`。依 `docs/feasibility/2026-10-12-tpm-poc.md` 裝上自編的 `tpm-slb9670-ce0` overlay（`config.txt` 的 `[all]` 加 `dtparam=spi=on` 與 `dtoverlay=tpm-slb9670-ce0`），另裝 `tpm2-tools` 5.7-1+b1 與 `device-tree-compiler`，重開機後 `/dev/tpm0`、`/dev/tpmrm0` 出現。閘道器上沒有安裝 agent（`/etc/trustiot/agent.toml` 不存在、`trustiot-agent` 不在 PATH）。
- **TPM**：Infineon SLB9670（`TPM2_PT_MANUFACTURER` `IFX`，`TPM2_PT_VENDOR_STRING_1` `SLB9`），韌體 `FIRMWARE_VERSION_1` `0x7003F`、`FIRMWARE_VERSION_2` `0xD1900`（7.63.3353.0）。持久 handle `0x81000001`、`0x81000100`，沿用 `0x81000100`。
- **平台**：展示環境（Laysi/trustiot-v2#457、Laysi/trustiot-v2#619），經 Cloudflare Access，所以每個指令帶 `--headers-file`。根憑證 `TrustIoT Demo Gateway Root CA 1`，SHA-256 指紋見下方指令的 `ROOT`（記在 Laysi/trustiot-v2#619 的留言）。
- **硬體 ID**：`tg-lab-0001`（閘道器沒有標籤，2026-10-30 由負責人決定）。
- 執行時段：2026-10-30 09:39–09:43 UTC。

## 指令

主機名稱、ssh 的 Host 與帳號不寫進報告：`<app 主機名稱>` 是展示環境的 app 主機名稱，`<GW>` 是工作站 `~/.ssh/config` 裡這台閘道器的 Host。

```sh
H=~/.config/trustiot-workstation/access-headers   # 0600，只有 cf-access-client-id 與 cf-access-client-secret 兩行
ROOT=C7:65:CF:FA:3F:5A:DE:1B:DC:88:2D:19:DB:75:67:B5:FE:E0:7F:D2:C8:C5:1F:DD:11:1B:9B:D7:D6:6D:F9:4B
AGENT=target/aarch64-unknown-linux-gnu/release/trustiot-agent

# B. 工作站的準備
trustiot-workstation init
trustiot-workstation login --url "https://<app 主機名稱>" --email <平台管理員的 email> --headers-file "$H"   # 負責人在自己的終端機執行
trustiot-workstation enroll --name lab-coordinator-1 --headers-file "$H"

# C.1（閘道器上）
date -u; touch /tmp/trustiot-618-mark
sudo tpm2_getcap handles-persistent; sudo tpm2_readpublic -c 0x81000100; sudo tpm2_getcap properties-fixed

# C.2、C.3（同一個指令跑兩次）
trustiot-workstation register --hardware-id tg-lab-0001 --gateway <GW> --root-sha256 "$ROOT" \
  --upload-agent "$AGENT" --remote-sudo --headers-file "$H"

# C.4
trustiot-workstation verify --hardware-id tg-lab-0001 --headers-file "$H"

# C.5（閘道器上）
sudo find / -xdev -newer /tmp/trustiot-618-mark -type f 2>/dev/null
ls -la "${TMPDIR:-/tmp}" | grep -i tmp.
sudo journalctl --since "2026-10-30 09:41:13 UTC" --no-pager
sudo tpm2_getcap handles-persistent
ls -la /var/lib/trustiot-agent /etc/trustiot

# C.6（工作站上）
W=${XDG_DATA_HOME:-$HOME/.local/share}/trustiot-workstation
c=$(sed -n 's/^cookie=//p' "$XDG_RUNTIME_DIR/trustiot-workstation/session")
grep -rlF -- "$c" "$W" && echo FOUND-COOKIE || echo no-cookie; unset c
grep -rl 'PRIVATE KEY' "$W" --exclude=workstation-key.pem && echo FOUND-KEY || echo no-key
grep -lE '\b[0-9]{8}\b' "$W/registrations.jsonl" || echo no-8-digit

# C.7
trustiot-workstation logout --headers-file "$H"
```

## 結果

原始輸出（主機名稱、位址、帳號與本機路徑已遮蔽）在 [data/2026-10-30-618-lab-registration/](data/2026-10-30-618-lab-registration/)，檔名對應步驟（`c1.txt` 到 `c7.txt`）。

| 步驟 | 結果 | 結束碼 |
| --- | --- | --- |
| B `init`、`enroll` | 建立工作站金鑰與 CSR；工作站憑證序號 `76f711de6164e7b02a81df742309af14` | 0 |
| C.1 | 2026-10-30 09:41:13 UTC；持久 handle `0x81000001`、`0x81000100`；`0x81000100` 是 ECC、`fixedtpm|fixedparent|sensitivedataorigin|userwithauth|sign` | 0 |
| C.2 登錄 | `key_created=false`，`ek_certificate=true`，公鑰 SHA-256 `2914ee8b4921c5da755b61f4b01486d3feb2427d45455a397721c437361184e2`；平台 **201**，序號 `5756b62458c56803416c03789c6c22ae`，到期 2027-01-28T09:41:47Z；安裝憑證停在 33（閘道器上沒有 agent，`sudo: trustiot-agent: command not found`） | 33 |
| C.3 重跑 | 金鑰仍沿用（`key_created=false`），平台 **200**、同一個序號；安裝仍是 33 | 33 |
| C.4 `verify` | 只有一筆 `certs.gateway_registered`（2026-10-30T09:41:47.165Z，event `01a18c2f-d41d-74be-a3db-bbd7ce9483d6`），`it matches this workstation's record (serial and public key)` | 0 |
| C.5 閘道器 | `find` 沒有任何新檔案；`/tmp` 沒有 `tmp.*` 目錄（上傳 agent 的兩個 `mktemp -d` 目錄都已刪除）；journal 只有 ssh 登入、sudo 執行上傳的 agent `provision --hardware-id tg-lab-0001 --handle 0x81000100 --inspect` 兩次，以及本步驟自己的 `tpm2_*`、`find`、`journalctl`；持久 handle 與 C.1 相同；`/var/lib/trustiot-agent`、`/etc/trustiot` 不存在（結束碼 33 時預期如此） | — |
| C.6 工作站 | `no-cookie`、`no-key`、`no-8-digit`；執行者保存的 C.1 到 C.7 輸出也搜尋過，沒有 cookie | 0 |
| C.7 `logout` | `logged out: the session is revoked and its file removed` | 0 |
| C.8 `trustiot-agent --check` | 待測：C.2 是 33，閘道器上還沒有 agent | — |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 以 lab 的閘道器實際登錄一台，平台端看得到該設備 | 通過：C.2 回 201，C.4 的 `verify` 找到 `certs.gateway_registered` |
| 重跑不產生重複紀錄：第二次是 200 與同一個序號，`verify` 仍只有一筆事件 | 通過：C.3 回 200、序號 `5756b62458c56803416c03789c6c22ae` 相同；C.4 只有一筆事件 |
| 工作站的日誌與結果檔不含私鑰、平台管理員的 cookie 與登入驗證碼 | 通過：C.6 三項都是 `no-…` |
| 登錄完成後閘道器上沒有工作站憑證、私鑰或平台管理員的任何憑證 | 通過：C.5 沒有新檔案、上傳目錄已刪除、journal 只有預期的紀錄、持久 handle 不變 |
| 金鑰沿用 `0x81000100`，持久 handle 的清單在前後相同 | 通過：C.2、C.3 都是 `key_created=false`；C.1 與 C.5 的 handle 清單相同 |
| 測試報告已合併進 `docs/test-reports/`，沒有主機名稱、位址或機密 | 本報告的 PR 合併時通過（gitleaks 與 `cargo test -p repo-check`） |

待測（不在本 issue 的驗收條件內，由 Laysi/trustiot-v2#458 補做）：

- 憑證鏈與根憑證裝到閘道器上：Laysi/trustiot-v2#458 安裝 agent，並加上 sudoers 規則，讓 ssh 帳號能以 `sudo -n` 執行 `trustiot-agent provision install --hardware-id tg-lab-0001`，而且 `trustiot-agent` 在 sudo 的 `secure_path` 上。之後重跑同一個 `register`，預期 200、同一個序號，兩行 `written`。
- C.8：agent 接受裝好的憑證（`sudo trustiot-agent --check`）。
