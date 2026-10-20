# 展示環境 runbook

展示切片（[里程碑](../roadmap/milestones.md)「展示切片」，epic Laysi/trustiot-v2#456）的展示環境（Laysi/trustiot-v2#457）跑在展示機（lab 的 Mac mini M1，Fedora Asahi Remix、k3s、aarch64）。本文目前寫對外的入口：展示用的 Cloudflare Tunnel、展示機上的 connector 與 app 主機名稱前的 Cloudflare Access（Laysi/trustiot-v2#466），以及外寄郵件與 Anthropic 金鑰（Laysi/trustiot-v2#468）。部署、更新、重設資料與共用叢集的注意事項由 Laysi/trustiot-v2#469 補上。

三個主機名稱（app、gateway、ota）都是同一個 zone 的一層子網域（Free 方案的 Universal SSL 只涵蓋一層）。主機名稱與 zone 都不寫進公開的 repo、issue 與 PR（[密鑰與權限](../architecture/secrets.md)「公開 repo 的資訊規則」；`scripts/demo/public-text.test.ts` 檢查本文與 `scripts/demo/` 只出現允許的網域）。文中稱「app 主機名稱」「gateway 主機名稱」「ota 主機名稱」；指令用 `DEMO_APP_HOST`、`DEMO_GATEWAY_HOST`、`DEMO_OTA_HOST`，展示機的 ssh 主機用 `DEMO_SSH_HOST`，由執行的人在自己的終端機設定。

## 架構

```text
瀏覽器 ──> Cloudflare（app、gateway、ota 主機名稱：Access，只放行受邀名單）──┐
閘道器 ──> Cloudflare（gateway 主機名稱的 /mqtt、/certs/renew：略過 Access）──┴─> 展示用 tunnel ─> 展示機的 cloudflared-demo.service ─> 入口
維運人員 ─> Cloudflare Access ─> 維運用 tunnel ─> 展示機原有的 cloudflared.service ─> sshd
```

- **兩條 tunnel**：展示用 tunnel（名稱 `trustiot-demo`）與展示機原有的維運 tunnel（經 Access 的維運 SSH）分開，token、路由與故障互不影響。維運 tunnel 的 `cloudflared.service` 不是安裝程式寫的 unit，展示環境的腳本只讀它的狀態（ActiveState、MainPID），從不改它、重啟它，也不讀它的 ExecStart；`scripts/demo/tunnel.test.ts` 檢查腳本與它在展示機上執行的指令都沒有停止、重啟、重新載入、kill、停用或 mask 它。
- **connector**：以 [deploy/host](../../deploy/host/README.md) 的安裝程式（Laysi/trustiot-v2#64）加 `--skip-k3s --unit cloudflared-demo --metrics-port 20250` 裝成另一個 systemd unit，沙箱、token 的保存方式（root 0600 的檔案，經 `LoadCredential` 交給服務）與 SaaS 主機相同。它以 `--config` 只讀自己的設定檔，metrics 固定在 `127.0.0.1:20250`：維運 tunnel 沒有指定時佔用 20241（cloudflared 自己會從 20241 到 20245 挑），固定的埠讓 `status` 讀到的是展示用 connector 的 `/ready`。
- **主機基線**：安裝程式同時套用 SaaS 主機的主機設定，展示期間一直留著。2026-10-20 使用者接受這份基線：firewalld 的 trusted zone 移除 Service 網段 `10.43.0.0/16`、加上拒絕 pod 連主機 SSH（22/tcp）的 rich rule，runtime 與 permanent 都改；寫入 Cloudflare 的套件庫與釘選的簽章金鑰，把主機的 cloudflared 換成該套件庫的簽章套件。安裝程式還會在 dry run 與安裝時以 root 多次執行 `firewall-cmd` 讀取每個 zone 與 policy。不用 Cloudflare 的 `cloudflared service install` 或在 k3s 裡跑 connector，因為那樣沒有沙箱與驗簽，也不是 SaaS 主機實際的裝法；同一套做法 2026-10-11 已在這台主機實測（[測試報告](../test-reports/2026-10-11-64-saas-host.md)）。
- **共用的 cloudflared 執行檔**：維運 tunnel 與 connector 都跑 `/usr/bin/cloudflared`。展示機原本的 cloudflared 是從 GitHub release 的 RPM 裝的，安裝程式的 `--reinstall-cloudflared` 會以 Cloudflare 套件庫的版本取代它；2026-10-20 套件庫只有 2026.10.1，所以版本會從 2026.10.0 升到 2026.10.1。2026-10-20 決定接受：維運 tunnel 正在跑的行程不受影響，下次重啟（例如重開機）後才用新版。要不要換套件以安裝程式自己的 dry run 為準：它沒有要換時，`tunnel.ts` 不查 Cloudflare 的套件庫，所以套件庫連不上時仍能輪替 token。
- **連結與標記**：cloudflared 套件的 `%postun` 不分升級或移除，一律刪掉 `/usr/local/bin/cloudflared`（指向 `/usr/bin/cloudflared` 的連結）與 `/usr/local/etc/cloudflared/.installedFromPackageManager`。標記不在時，cloudflared 自己的更新程式（`cloudflared update`，由 `cloudflared service install` 沒加 `--no-update-service` 時建立的 `cloudflared-update.timer` 執行）會換掉兩條 tunnel 共用的執行檔。所以：安裝時發現 `cloudflared-update.timer` 或 `cloudflared-update.service` 存在就停下；換套件的那次安裝，由執行安裝程式的 systemd 暫時 unit 在結束後（成功、失敗、被停止或被 kill 都一樣）以 root 依原樣補回兩者；之後每次手動 `dnf upgrade` 或 `dnf downgrade` cloudflared，也要照「cloudflared 的更新與退回」補回。
- **路由**：分兩段。入口還沒部署時，三個主機名稱都回 `http_status:418`：這是 connector 自己的回應，用來證明請求穿過 Cloudflare 到了展示機，而且沒有被 Access 或機器人防護攔下。展示環境的入口 Service 出現後，三條路由改為 `http://<入口 ClusterIP>:80`（主機解析不了叢集 DNS）。最後一條 catch-all 回 404。入口 Service 被刪掉重建後 ClusterIP 會變，路由要跟著改。
- **三個主機名稱都在 Access 後面**：切片延後了限流、停權、AI 額度、分析護欄與注入題庫，展示期間以 Access 補上（[里程碑](../roadmap/milestones.md)的延後項目表）。chart 依主機名稱掛路由（`deploy/chart/templates/_ingress.tpl`），主機名稱來自 CI 讀不到的 SOPS 加密 values；把 chart values 裡 app 與 gateway 的主機名稱對調的變更在 CI 仍會通過，而 tunnel 把三個主機名稱都送到同一個入口，對調後 app（含登入頁）就會出現在沒有 Access 的主機名稱上。所以 Access 不能只放在 app 主機名稱：一個 self-hosted application 涵蓋三個主機名稱，只允許 Google 登入，policy 只放行一個 Access group（受邀名單），並另以 `require` 要求登入方式是那個 Google identity provider。application 開啟 binding cookie（`enable_binding_cookie`，`CF_Binding`）：session cookie 被複製到別的瀏覽器時不能用。依 Cloudflare 的文件，這個設定不適合非瀏覽器的工具；需要 session 的請求都來自瀏覽器（app 本身的 API 與 WebSocket 都由同一個瀏覽器帶著 cookie 發出），閘道器只用下面略過 Access 的路徑，所以開啟。日後若有 service token 的 client，每個請求都要帶 service token 的標頭，不能依賴 `CF_Authorization` cookie。
- **只有閘道器的固定路徑略過 Access**：閘道器過不了 Access，靠內層 mTLS（[部署與可搬移性](../architecture/deployment.md)）。略過的路徑正是 chart 不需要 cookie 的 gateway 路徑（`ingress.publicPaths` 的 `certRenewal` 與 `mqtt`，都是完整路徑比對）：gateway 主機名稱的 `/certs/renew` 與 `/mqtt`，各以一個 self-hosted application 加上 Bypass policy。chart 目前沒有給 ota 主機名稱任何不需要 cookie 的路徑，所以 ota 主機名稱整個在 Access 後面；OTA client 的路徑加進 chart 的表時，同時加上它的 Bypass application 與下面的 tunnel 規則，並更新 `scripts/demo/tunnel.ts` 的 `BYPASS_PATHS`（測試核對它與 chart 的表一致）。Cloudflare 的 Access application 帶路徑時也涵蓋它下面的子路徑，同一個請求符合多個 application 時以路徑較具體的為準（[Application paths](https://developers.cloudflare.com/cloudflare-one/access-controls/policies/app-paths/)），所以 `/mqtt/x` 也會略過 Access；真正把略過限縮成完整路徑的是 tunnel：路由以錨定的路徑規則（`^/mqtt$`、`^/certs/renew$`，cloudflared 以 Go 的 regexp 比對路徑，不含 query string）把這兩個路徑送到入口而不要求 JWT，同一主機名稱的其他路徑與 app、ota 主機名稱一律設 `originRequest.access`：connector 也驗證 Access 的 JWT，沒有 JWT 的請求回 403。Access application 被誤刪或改壞時，這些主機名稱也是回 403 而不是變成公開。略過的路徑都不是 app 的路徑，也不是 app 路徑的前綴。閘道器的 WebSocket client 不送 User-Agent，所以以 configuration rule 對 gateway 主機名稱關掉 Browser Integrity Check。zone 的 Bot Fight Mode 在 Free 方案不能只對單一主機名稱略過；2026-10-20 決定先不動，以下方的 `check` 實測，被 challenge 或擋下時再由使用者決定是否關閉整個 zone 的設定。

## Cloudflare 上的設定

由有 Cloudflare 權限的維運人員以 API 建立。主機名稱只在 Cloudflare 與一份不進 repo 的變更清單裡；清單逐一寫出每個 API 呼叫、需要的 API 權限、確認方式與還原方式，經使用者同意後才套用。順序：

1. remotely-managed tunnel `trustiot-demo`（`config_src: cloudflare`）。建立後只記下 `id`、`name`、`config_src` 與 `status`。
2. Access group（受邀名單）、引用它的 reusable policy（include 這個 group，require Google 的登入方式）、涵蓋三個主機名稱的 self-hosted application（只允許 Google 登入、session 24 小時、binding cookie），以及 gateway 主機名稱 `/certs/renew` 與 `/mqtt` 的 Bypass application。
3. tunnel 的路由（先回 418，入口部署後改成入口的 ClusterIP）：兩條略過路徑的錨定規則在前、不要求 JWT，其餘每條都設 `originRequest.access`（team 名稱與三個主機名稱那個 application 的 AUD），最後 catch-all 回 404。
4. gateway 主機名稱的 configuration rule（Browser Integrity Check 關閉）。
5. 三筆 proxied CNAME 指向 `<tunnel ID>.cfargotunnel.com`。DNS 紀錄一定在 Access application 之後建立，主機名稱不會有一段沒有保護的時間。

token 由同一個人在 Cloudflare 的 API 執行環境裡取得並加密（見下方「安裝 connector」第 2 步），不經過其他地方。在那個執行環境裡依序：以 `GET /accounts/<帳號>/cfd_tunnel/<tunnel>` 確認名稱是 `trustiot-demo`、`config_src` 是 `cloudflare`；以 `GET …/cfd_tunnel/<tunnel>/token` 取得 token，確認它是以 `eyJhIjoi` 開頭的字串、解開後的 `t` 等於 tunnel ID；以 `crypto.subtle.exportKey("spki")` 算公鑰的 SHA-256，與 `handoff-key` 印出的相同才加密；只回傳 tunnel ID、指紋、密文與它的位元組數。

## 安裝 connector

在有 repo 的工作站上執行，`tunnel.ts` 經 ssh 操作展示機。工作目錄必須是審查過的 commit 的乾淨 checkout：`scripts/demo` 或 `deploy/host` 有未 commit 的變更時，計畫直接停下；計畫記下 commit，確認碼也涵蓋它。日誌與記錄放在 `$XDG_STATE_HOME/trustiot-v2/demo/`（0700，檔案 0600）或 `--log-dir`；指令失敗時最後印出 `ERROR <原因>` 與 `log: <日誌路徑>`，成功時不印日誌路徑。

```sh
: "${DEMO_SSH_HOST:?set DEMO_SSH_HOST to the ssh host of the demo machine}"
bun scripts/demo/tunnel.ts handoff-key --ssh-host "$DEMO_SSH_HOST"
```

1. **交付金鑰**：`handoff-key` 在展示機產生 RSA-4096 金鑰對，私鑰在 `/run/trustiot-demo/handoff.pem`（tmpfs，目錄 root 0700、檔案 0600），印出公鑰與它 SPKI DER 的 SHA-256。私鑰只在記憶體裡：重開機就消失，之後要重跑 `handoff-key` 並重新加密；記憶體不足時它可能被換到磁碟上的 swap 檔。已有金鑰時沿用同一把。公鑰交給持有 Cloudflare 權限的人。
2. **加密 token**：持有權限的人照「Cloudflare 上的設定」最後一段，在 API 的執行環境內確認 tunnel 與 token，再以 RSA-OAEP（SHA-256，MGF1 SHA-256；WebCrypto 的 `{name: "RSA-OAEP", hash: "SHA-256"}` 加上 SPKI 公鑰）加密，只交出 base64 密文與 tunnel ID，密文存成工作站上的檔案。token 不經過聊天、終端機輸出、工作站的明文或任何紀錄。
3. **建置安裝程式**：`bun install --cwd deploy/host --frozen-lockfile`，再 `bun run --cwd deploy/host build`。`tunnel.ts` 以 `deploy/host/dist/SHA256SUMS` 核對 aarch64 執行檔，複製到展示機後再由 root 核對一次。
4. **dry run**：

   ```sh
   bun scripts/demo/tunnel.ts install --ssh-host "$DEMO_SSH_HOST" --ciphertext <密文檔> --tunnel-id <tunnel ID>
   ```

   lab 鎖（展示機的 `~/e2e.lock`）被別的工作持有，或鎖的目錄在但讀不到擁有者（別的工作正在取得鎖，或取得到一半就停了）時停下；dry run 不取得鎖。它先檢查：沒有 `cloudflared-update.timer` 或 `.service`、`/usr/local/bin/cloudflared` 不存在或是指向 `/usr/bin/cloudflared` 的連結（一般檔案或指向別處就停下）、`127.0.0.1:20250` 沒被別的程式佔用、沒有其他安裝程式的執行還在。接著把安裝程式與它的執行腳本放進 `/var/lib/trustiot-demo/`、密文放進 `/run/trustiot-demo/`（都是 root 0700），三者的 sha256 寫進計畫；每次執行先把它們複製成這次執行自己的檔案（以執行 ID 命名），核對複本與計畫相同後只用複本，與計畫不同（例如另一次執行在這期間放了別的密文）就什麼都不跑。

   安裝程式在展示機的 systemd 暫時 unit `trustiot-demo-install-<執行 ID>` 裡執行（`systemd-run --collect`），不靠工作站或 ssh 連線；token 在展示機上解密後直接進安裝程式的 stdin，不出現在指令列、檔案或輸出。輸出寫到 `/var/lib/trustiot-demo/<執行 ID>.log`，結束狀態寫到 `<執行 ID>.exit`，unit 的 `ExecStopPost` 最後寫 `<執行 ID>.post`。執行腳本自己的這些檔案以 umask 077 建立（0600）；只有安裝程式在 umask 022 下執行。022 是 sudo 的預設值：sudo 把它和呼叫者的 umask 合併（sudoers 設了 `umask_override` 時除外），所以 SaaS 主機上從 umask 較嚴（例如 027、077）的 shell 執行 sudo 時，較嚴的 umask 一樣會傳給安裝程式與它呼叫的 dnf（deploy/host/README.md「安裝」第 4 步；安裝程式端的修正在 Laysi/trustiot-v2#476）。在 022 下，它呼叫的 dnf 建立的狀態檔與快取是一般的 644 與 755，而 token 等 secret 的權限由安裝程式自己明確設定，不依賴 umask；`tunnel.ts` 每 10 秒讀一次並印出新的行。安裝程式要逐一讀 firewalld 的 zone 與 policy，期間沒有新輸出是正常的。

   最後印出計畫並以確認碼涵蓋：commit、token 所屬的 tunnel（安裝程式從 token 讀到的 ID 必須等於 `--tunnel-id`，否則停下）、安裝程式的每一項變更、cloudflared 套件的版本變化（目前版本與來源、換成的版本、維運 tunnel 何時改用新版）、要補回的連結與標記、`cloudflared-update` 的狀態。安裝程式的變更只能是允許清單上的項目：trusted zone 的兩項變更（runtime 與 permanent）、Cloudflare 的套件庫與金鑰、換掉 cloudflared 套件、`cloudflared-demo` 的 token、設定檔與 unit、`daemon-reload`、啟用、啟動與重啟 `cloudflared-demo`。其他任何變更都會停下；確認過的例外以 `--allow-change "<計畫印出的那一行>"` 逐行指名（可重複），也算進確認碼。
5. **安裝**：同一個指令加 `--apply --confirm <確認碼>`。它取得 lab 鎖（自己已持有時加 `--lock-held`，鎖的擁有者必須正是 Laysi/trustiot-v2#466），在鎖內重新放置檔案、再跑一次 dry run，計畫與確認碼不符就停下；接著在第一次安裝前把 trusted zone 存到 `/var/lib/trustiot-demo/firewalld/`（`trusted.xml`，沒有這個檔時記下它不存在；以及 `firewall-cmd --info-zone=trusted` 的 runtime 與 permanent 輸出；已有完整備份時不覆寫，所以之後的輪替不會蓋掉安裝前的狀態），記錄主機狀態、安裝、再記錄一次。成功後刪掉 `/run/trustiot-demo/` 的交付私鑰與密文；失敗時兩者留著，修好後可以重跑。最後印出 `run <執行 ID>: service_result=… exit_code=… exit_status=… link=… marker=…`（`ExecStopPost` 的結果）、`cloudflared now: <來源那一行>; <連結、標記與 cloudflared-update 的狀態>`（來源那一行與 `status` 相同，見下一步），以及維運 tunnel 是否仍是同一個行程（`cloudflared.service kept running (same process)`）；`cloudflared-demo.service` 自己的狀態不在這裡，由下一步的 `status` 讀。
6. **確認**：

   ```sh
   bun scripts/demo/tunnel.ts status --ssh-host "$DEMO_SSH_HOST"
   ```

   印出 `cloudflared-demo.service` 的狀態與它對 Cloudflare 的連線數（`127.0.0.1:20250/ready`）、維運 tunnel 的狀態與它正在跑的執行檔（換過套件後顯示 `(deleted)`，表示下次重啟才用新版）、cloudflared 的版本與來源（一行：dnf 的 `from_repo` 是 `cloudflared-stable` 時 `OK cloudflared <版本> from cloudflared-stable`；來自別處，或讀不到時 `FAIL`，讀不到時附上 dnf 最後一行的錯誤。`from_repo` 以 root 讀：dnf5 把它存在 `/usr/lib/sysimage/libdnf5/packages.toml`，這個檔是 root 0600 時（2026-10-21 的安裝留下的，見「2026-10-21 安裝留下的 dnf 權限」），一般使用者的 `dnf repoquery` 讀不到也不報錯）、連結與標記和 `cloudflared-update` 的狀態（標記不在而 timer 啟用時 `FAIL`，連結或標記不在時 `WARN`）、token 與設定檔的擁有者與權限、`/run/trustiot-demo/` 與 `/var/lib/trustiot-demo/` 剩下的檔案、還在的安裝程式執行、token 或其中的 secret 是否出現在整個 journal（含指令列等所有欄位）或那兩個目錄（只回傳次數；由 root 在展示機上把兩者寫進 `/run` 下 0600 的 pattern 檔，只以 `grep -f` 讀，不進指令列）、工作站日誌目錄裡含 tunnel token 開頭（`eyJhIjoi`）的檔案數，以及展示環境入口 Service 的 ClusterIP。`FAIL` 與 `WARN` 只標出問題，`status` 照樣以 0 結束，所以每一行都要讀過。Cloudflare 端以 `GET /accounts/<帳號>/cfd_tunnel/<tunnel>` 確認 `status` 為 `healthy`。

### 中斷之後

安裝程式在展示機的暫時 unit 裡跑，工作站按了 Ctrl-C、睡眠或 ssh 斷線都不會中斷它；`--apply` 的工作站行程中斷時 lab 鎖留著。等候超過 60 分鐘，或 unit 消失卻沒有 `.post` 檔時，`tunnel.ts` 也不釋放鎖，而是印出以下步驟：

1. 在展示機上 `systemctl status trustiot-demo-install-<執行 ID>`：還在跑就等；查不到表示已結束。`sudo tail /var/lib/trustiot-demo/<執行 ID>.log` 看進度，`sudo cat /var/lib/trustiot-demo/<執行 ID>.post` 看結果（`link=`、`marker=` 是補回的結果，`conflict` 或 `failed` 時照「cloudflared 的更新與退回」手動補回）。
2. 非停不可時 `sudo systemctl stop trustiot-demo-install-<執行 ID>`：這會中斷正在跑的 dnf 或 firewall-cmd，`ExecStopPost` 仍會補回連結與標記。之後跑 `status` 看主機狀態，必要時再跑一次 dry run。
3. 確認 unit 已結束、`cat ~/e2e.lock/owner` 是這次執行寫的那一行（`Laysi/trustiot-v2#466 <時間> demo tunnel-install`）之後，才以 `rm -rf ~/e2e.lock` 釋放鎖。

### 2026-10-21 安裝留下的 dnf 權限

2026-10-21（UTC+8；dnf 交易在 2026-10-20 17:41:47 UTC）的安裝時，執行腳本的 `umask 077` 傳給了安裝程式呼叫的 dnf。之後展示機上：

- `/usr/lib/sysimage/libdnf5/` 的 `environments.toml`、`groups.toml`、`modules.toml`、`nevras.toml`、`packages.toml`、`system.toml` 是 `root:root 600`（應為 644）；一般使用者的 dnf 讀不到系統狀態，`dnf repoquery` 只在 stderr 報錯、結束碼 0。
- Cloudflare 套件庫的 dnf 快取 `/var/cache/libdnf5/cloudflared-stable-<hash>/`：這個目錄與其中的 `repodata`、`solv`、`packages` 是 700，`repomd.xml` 與 primary 檔是 600（其他套件庫的快取是 755 與 644）。
- PackageKit 的 metadata 快取 `/var/cache/PackageKit/<releasever>/metadata/cloudflared-stable-<hash>/repodata/` 的 `repomd.xml` 與 `<sha256>-primary.xml.zst` 是 `root:root 600`（dnf 把剛下載的 metadata 複製到這裡；同一處的目錄是 755，其他檔案是 644）。

安裝程式自己寫的檔案（`/etc/credstore` 700 與 600，unit、`.repo` 檔、套件簽章金鑰、firewalld 的 zone 檔、標記檔 644）、`rpmdb.sqlite` 與 dnf5 的交易紀錄（644）都不受影響。修正是 commit `12db555`（`fix(scripts): run the demo installer under umask 022, not the runner's 077`，Laysi/trustiot-v2#466）：之後的執行只讓安裝程式在 umask 022 下跑。已經改變的權限由人改回一次；展示機已在 2026-10-21 改回，改回前後的權限記在[測試報告](../test-reports/2026-10-21-466-demo-tunnel-access.md)「安裝留下的 dnf 權限」。同樣的情形再發生時，以下在工作站上執行，經 ssh 在展示機上先印出改回前的權限，把六個檔改成 0644、兩個 `cloudflared-stable-<hash>` 目錄以 `chmod -R u=rwX,go=rX` 改回，再印出改回後的權限，最後不用 root 讀一次 `from_repo`（要印出 `cloudflared-stable`）：

```sh
ssh "$DEMO_SSH_HOST" 'set -e; f=/usr/lib/sysimage/libdnf5; set -- /var/cache/libdnf5/cloudflared-stable-* /var/cache/PackageKit/*/metadata/cloudflared-stable-*; echo before:; stat -c "%a %U:%G %n" $f/*.toml; sudo -n find "$@" -printf "%m %u:%g %p\n"; sudo -n chmod 0644 $f/environments.toml $f/groups.toml $f/modules.toml $f/nevras.toml $f/packages.toml $f/system.toml; sudo -n chmod -R u=rwX,go=rX "$@"; echo after:; stat -c "%a %U:%G %n" $f/*.toml; sudo -n find "$@" -printf "%m %u:%g %p\n"; dnf -q repoquery --installed --disablerepo="*" --qf "%{from_repo}\n" cloudflared'
```

`set --` 讓每個符合的快取目錄各自成為一個參數（PackageKit 可能有好幾個 `<releasever>`）；有一個樣式沒有符合時，`find` 拿到的是樣式本身而失敗，`set -e` 就在改任何權限之前停下。

要確認沒有漏掉的檔案時，以唯讀的 `find` 找某段時間內變更、而其他使用者讀不到的檔案與目錄（排除 k3s 與 journald 寫入的路徑，它們不是執行腳本的子程序寫的），以及套件管理路徑下所有讀不到的項目。2026-10-21 的結果記在 [測試報告](../test-reports/2026-10-21-466-demo-tunnel-access.md)「安裝留下的 dnf 權限」：

```sh
# 唯讀：<開始>、<結束> 是要查的時段，寫成 "YYYY-MM-DD HH:MM:SS UTC"（2026-10-21 查的是兩次 dry run 與安裝的 2026-10-20 17:15:00–17:50:00 UTC）
ssh "$DEMO_SSH_HOST" 'sudo -n find /etc /usr /var /opt -xdev \( -path /var/lib/rancher -o -path /var/log/pods -o -path /var/log/containers -o -path /var/lib/kubelet -o -path /var/log/journal -o -path /var/lib/containerd -o -path /var/lib/cni \) -prune -o \( \( -newermt "<開始>" ! -newermt "<結束>" \) -o \( -newerct "<開始>" ! -newerct "<結束>" \) \) ! -perm -o=r -printf "%m %u:%g %p\n"'
# 唯讀：套件管理的路徑，不限時間
ssh "$DEMO_SSH_HOST" 'sudo -n find /usr/lib/sysimage /var/cache/libdnf5 /var/lib/dnf /etc/yum.repos.d /etc/pki/rpm-gpg /usr/local /etc/dnf /var/cache/PackageKit ! -perm -o=r -printf "%m %u:%g %p\n"'
```

## cloudflared 的更新與退回

cloudflared 來自 Cloudflare 的套件庫，以 dnf 更新或退回，只重啟展示用的 connector；維運 tunnel 下次重啟才用新的執行檔：

```sh
sudo dnf upgrade --repo=cloudflared-stable cloudflared                     # 更新
sudo dnf downgrade --repo=cloudflared-stable cloudflared-<版本>            # 退回套件庫裡的舊版
# %postun 刪掉的連結與標記，每次 dnf 換過 cloudflared 後都要補回：
[ -e /usr/local/bin/cloudflared ] || sudo ln -s /usr/bin/cloudflared /usr/local/bin/cloudflared
sudo install -d -m 0755 /usr/local/etc/cloudflared
[ -e /usr/local/etc/cloudflared/.installedFromPackageManager ] || sudo install -m 0644 /dev/null /usr/local/etc/cloudflared/.installedFromPackageManager
sudo systemctl restart cloudflared-demo.service
```

補回連結是為了維持套件原本的狀態；補回標記是為了讓 cloudflared 的自動更新不動這個由套件管理的執行檔。補回後以 `status` 確認連結與標記是 `OK`。

## 檢查對外的請求

```sh
: "${DEMO_APP_HOST:?set DEMO_APP_HOST to the app host name}"
: "${DEMO_GATEWAY_HOST:?set DEMO_GATEWAY_HOST to the gateway host name}"
: "${DEMO_OTA_HOST:?set DEMO_OTA_HOST to the ota host name}"
bun scripts/demo/tunnel.ts check --ssh-host "$DEMO_SSH_HOST"              # 路由還是 418 的時候
bun scripts/demo/tunnel.ts check --ssh-host "$DEMO_SSH_HOST" --stage 2    # 路由已到入口之後
```

同一組請求從工作站與展示機（lab 的網路，展示用閘道器也在這裡）各送一次，以 curl 的 HTTP/1.1：

| 請求 | 預期 |
| --- | --- |
| app 主機名稱 `/`，瀏覽器的 User-Agent，沒有 Access 的 session | `access`：302 到 `<team>.cloudflareaccess.com` 的登入頁 |
| gateway 主機名稱 `/mqtt`，WebSocket 升級，和閘道器的 client 一樣不帶 User-Agent 與 Accept，`Sec-WebSocket-Protocol: mqtt` | 到達 tunnel：第 1 段是 `origin`（418 或 101）；第 2 段是入口的任何回應，只要不是 `access`、`challenge`、`block`、`refused` 或 `tunnel-down`。第 1 段目前一律 `FAIL`（`unexpected`，502），見下方「第 1 段的 WebSocket 升級」 |
| gateway 主機名稱 `/certs/renew`，POST，不帶 User-Agent | 同上 |
| gateway 與 ota 主機名稱的 `/auth/login`、`/healthz`（app 的路徑），不帶 User-Agent | `access`：Access 擋下，不會到 app（紀錄狀態碼與 `cf-ray`） |
| gateway 與 ota 主機名稱的 `/`，不帶 User-Agent | `access` |
| gateway 主機名稱 `/mqtt/x`（略過路徑下面的子路徑），不帶 User-Agent | `refused`：Access 依子路徑規則略過它，connector 因為沒有 JWT 而回 403 |

結果的判讀：

| 結果 | 依據 | 意思 |
| --- | --- | --- |
| `origin` | 第 1 段路由的回應 418，或 101（origin 接受了 WebSocket）；第 2 段另外包含入口自己的 403（帶 body 或 `content-type`，例如 Envoy 的 `RBAC: access denied`） | 請求穿過 Cloudflare 到了 tunnel 後面，沒有被 Access 或機器人防護攔下 |
| `access` | 3xx 轉到 `<team>.cloudflareaccess.com`（沒有 session 的請求，瀏覽器或不是，Access 都回 302；managed OAuth 沒開，[Managed OAuth](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/managed-oauth/)） | Access 擋下沒有 session 的請求；出現在略過路徑上表示 Bypass application 不見了或沒有比對到 |
| `refused` | 403，不是 Cloudflare 的錯誤碼頁；第 2 段還要沒有 body 也沒有 `content-type`（connector 拒絕時只寫狀態碼，入口的 403 都帶內容） | connector 的 JWT 檢查擋下沒有經過 Access 的請求；出現在略過路徑上表示 tunnel 的路徑規則錯了。第 1 段 tunnel 後面只有 connector，所以任何這種 403 都算；第 1 段的紀錄留下它的 `content-type` 與 body 大小，可以確認第 2 段的判斷適用實際跑的 cloudflared |
| `challenge` | 回應帶 `cf-mitigated: challenge`，或是 challenge 頁 | Cloudflare 要求瀏覽器驗證；閘道器過不了。在 gateway 或 ota 主機名稱出現時，多半是 Bot Fight Mode：記下時間、來源與 `cf-ray`（紀錄裡有），由使用者決定是否關閉整個 zone 的 Bot Fight Mode |
| `block` | 403 加上 Cloudflare 的錯誤碼頁（例如 1010 瀏覽器特徵、1020 規則） | 被規則或 Browser Integrity Check 擋下；檢查 configuration rule 是否涵蓋該主機名稱（紀錄裡有 `cf-ray`） |
| `tunnel-down` | 530 或錯誤碼 1033 | 沒有 connector 在線 |
| `unexpected` | 其他（例如 502：路由的 origin 連不上；WebSocket 升級的 502 見下方） | 看日誌與路由設定 |

**第 1 段的 WebSocket 升級**：第 1 段的 `check` 中，gateway `/mqtt` 的 WebSocket 升級從兩處都是 `FAIL`：`unexpected`，502，`server: cloudflare`，148 bytes 的 HTML。原因是路由以 418 回應升級而不是 101：client 收到 Cloudflare 的 502 頁，而 Cloudflare 的 HTTP 請求分析與 connector 的計數都記錄 origin 回了 418（2026-10-21 實測；Cloudflare 的文件沒有描述這個行為）。所以這一項失敗不代表請求沒有到 connector，但 `check` 自己分不出來。人工確認請求到了 connector：在展示機上讀兩個計數，從工作站單獨送一次同樣的升級，再讀一次；`cloudflared_tunnel_total_requests` 與 `cloudflared_tunnel_response_by_code{status_code="418"}` 各多 1，就是這個請求到了 connector、由路由回了 418。期間有別的請求（例如已登入的瀏覽器）時差值會更大，那就重做。

```sh
# 展示機上（前後各一次）：只取兩個計數；metrics 的其他內容含 Cloudflare 的機房名稱，不貼進紀錄或報告
curl -s --max-time 5 http://127.0.0.1:20250/metrics | grep -E '^cloudflared_tunnel_(total_requests|response_by_code\{status_code="418"\}) '
# 工作站上：和 check 相同的升級請求
curl -sS -i --http1.1 --max-time 15 -H 'User-Agent:' -H 'Accept:' -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
  -H 'Sec-WebSocket-Version: 13' -H "Sec-WebSocket-Key: $(head -c 16 /dev/urandom | base64)" -H 'Sec-WebSocket-Protocol: mqtt' \
  "https://$DEMO_GATEWAY_HOST/mqtt" | head -n 1
```

讓 `check` 自動讀這兩個計數並據以判斷是 Laysi/trustiot-v2#473。在那之前第 2 段把這個 502 當成入口的回應而通過（它不是 `access`、`challenge`、`block`、`refused` 或 `tunnel-down`），沒有計數佐證。也因為這一項，第 1 段的 `check` 一律以非零結束，其餘各項要逐行看是否 `PASS`。

`check` 只印出與保存狀態碼、body 的位元組數（不含 body）、`server`、`cf-mitigated`、`content-type`、去掉 query 的 `location`、錯誤頁的標記與 `cf-ray`；主機名稱、Access 的 team 網域與 curl 錯誤訊息裡的家目錄都換成佔位符，可以直接放進測試報告。任何一項不符預期就以非零結束。curl 的 TLS 特徵和閘道器的 rustls 不同，所以展示用閘道器（Laysi/trustiot-v2#458）第一次連線後，再以 Cloudflare 的 Security Events（`firewallEventsAdaptive`，篩 gateway 主機名稱）確認沒有被 challenge 或擋下的紀錄。

## 輪替 tunnel token

1. 在 Cloudflare 的 API 執行環境裡，一次呼叫內完成：`GET /accounts/<帳號>/cfd_tunnel/<tunnel>` 確認名稱是 `trustiot-demo`；在同一個環境以 `crypto.getRandomValues` 產生 32 bytes 的新 `tunnel_secret`，`PATCH /accounts/<帳號>/cfd_tunnel/<tunnel>`，body `{"tunnel_secret": "<base64>"}`；再取得新 token 並照「安裝 connector」第 2 步檢查與加密。`tunnel_secret` 與新 token 都不離開那個環境，只回傳 tunnel ID、指紋、密文與位元組數。舊 token 不能再建立新連線，已在線的 connector 維持到重啟為止，所以接著馬上做下一步。加密要用展示機上現有的交付公鑰：先跑 `handoff-key`（安裝成功後私鑰已刪掉，會產生新的一把），把公鑰交給持有權限的人再做這一步。
2. `install` 的 dry run（計畫只有寫入新 token 與重啟 `cloudflared-demo.service`；不需要換套件時不查 Cloudflare 的套件庫）、`--apply --confirm`。重啟時展示環境的連線會中斷，直到新的連線建立；維運 tunnel 不受影響。
3. token 外洩時另外以 `DELETE /accounts/<帳號>/cfd_tunnel/<tunnel>/connections` 中斷所有既有連線。

## 移除 connector

```sh
bun scripts/demo/tunnel.ts remove --ssh-host "$DEMO_SSH_HOST"                            # 印出計畫與確認碼
bun scripts/demo/tunnel.ts remove --ssh-host "$DEMO_SSH_HOST" --apply --confirm <確認碼>
```

取得 lab 鎖後停用並刪除 `cloudflared-demo.service`、它的 token 與設定檔，刪掉 `/run/trustiot-demo/`。`/etc` 是 btrfs（寫入時複製），刪掉的 token 內容可能仍留在磁碟上，所以 token 真正失效靠的是在 Cloudflare 輪替或刪除 tunnel，不是刪檔。trusted zone 依 `/var/lib/trustiot-demo/firewalld/` 的備份，只還原安裝程式改的兩項（pod SSH 規則與 Service 網段），runtime 與 permanent 分開判斷、還原後逐一確認；接著比對整個 zone（runtime 與 permanent 的每個欄位）與備份，全部相同才刪掉 `/var/lib/trustiot-demo/`（含備份與執行紀錄），不同時留下並列出不同的欄位。安裝前沒有 `/etc/firewalld/zones/trusted.xml` 時，還原後它仍在，內容與 firewalld 的預設相同。保留：Cloudflare 的套件庫、簽章金鑰與簽章的 cloudflared 套件（維運 tunnel 也用它），以及其他 zone 的設定（安裝程式只會關掉 80、443，不重新打開）。之後在 Cloudflare 刪掉 DNS 紀錄、configuration rule、Access application、policy 與 group，最後刪除 tunnel。

## Access 名單

名單只存在 Cloudflare：展示環境的 Access group，以 email 列出受邀的人，只能以 Google 登入。名單、email 與帳號不寫進 repo、issue 或 PR。

- **加人或移除**：Zero Trust → Access → Groups → 展示環境的 group → 編輯 Include 的 email；或 `PUT /accounts/<帳號>/access/groups/<group>`，body 帶完整的 `name` 與 `include`（PUT 會取代整份名單，先 `GET` 再改）。改完以 `GET` 確認。
- **立刻撤銷**：移除的人在 session 到期（application 的 session duration，24 小時）前仍能使用。要立刻撤銷時，在 Zero Trust → Users 撤銷他的 session，或 `POST /accounts/<帳號>/access/apps/<application>/revoke_tokens` 撤銷這個 application 所有人的 session（所有人要重新登入）。
- **確認**：沒有受邀的 Google 帳號登入時，Access 顯示沒有權限的頁面，不會到 app。
- **新的公開路徑**：展示切片沒有 OTA（OTA 的派送在 [MS4](../roadmap/milestones.md)），所以 ota 主機名稱目前整個在 Access 後面。chart 為閘道器或 OTA client 加上不需要 cookie 的公開路徑時，三樣一起加：該路徑的 Bypass application、錨定該路徑的 tunnel 規則（不要求 JWT，排在同一主機名稱設 `originRequest.access` 的規則之前），以及對該路徑關閉 Browser Integrity Check 的 configuration rule；並更新 `scripts/demo/tunnel.ts` 的 `BYPASS_PATHS`。

## 拿掉 Access

Laysi/trustiot-v2#113（限流）、Laysi/trustiot-v2#107（停權）、Laysi/trustiot-v2#140（AI 額度）、Laysi/trustiot-v2#148（分析護欄）與 Laysi/trustiot-v2#445（注入題庫）都在展示環境上線後：

1. tunnel 路由的 app 主機名稱拿掉 `originRequest.access`（先拿掉這一項，Access application 還在，主機名稱仍受保護）。
2. Access application 改成只涵蓋 gateway 與 ota 主機名稱（不拿掉這兩個：主機名稱對調的風險與 app 的防護無關，見「架構」），刪掉只有 app 主機名稱用到的設定。
3. 確認 app 主機名稱不再轉到 Access，登入頁由身分服務回應。tunnel、其他路由與 DNS 不變。

## 外寄郵件與 Anthropic 金鑰

展示環境的服務只經 SMTP 寄信（chart 的 `smtp.*`；[里程碑](../roadmap/milestones.md)「展示切片」），2026-10-19 起暫用 Resend 的 relay：`values-demo.yaml` 設 `smtp.resend.com`、587、`starttls`，帳密在 Secret `trustiot-smtp`（`username` 是 `resend`，`password` 是 Resend 的 API key），寄件人（`smtp.from`）在私有 values `trustiot-demo-values` 的 `smtp-from`。AI 閘道（Laysi/trustiot-v2#139）的 Anthropic 金鑰在 Secret `trustiot-anthropic` 的 `api-key`，閘道加入前沒有服務使用。兩個 Secret 都以 SOPS 加密放在 `deploy/flux/clusters/demo/trustiot/`（[deploy/flux/README.md](../../deploy/flux/README.md)「展示叢集」的表），第一次建立後列進同目錄的 `kustomization.yaml`。金鑰只經 stdin 交給 `scripts/flux/sops-secret.ts`，不貼到 issue、PR、聊天或指令列；主控台只顯示一次的金鑰，複製後以輸出剪貼簿的指令直接接上管線，用完清除剪貼簿。

寄件網域是上層網域（展示主機名稱所在的 zone）下專門寄信的子網域（Resend 建議以子網域寄信，寄件信譽與上層網域分開）。它和展示的主機名稱一樣不寫進公開的 repo、issue 與 PR（[密鑰與權限](../architecture/secrets.md)「公開 repo 的資訊規則」），文中稱「寄件網域」。

### Resend 的寄件網域與金鑰

由有 Resend 權限的維運人員在 Resend 主控台操作；DNS 紀錄照「Cloudflare 上的設定」的做法以 Cloudflare API 建立，變更清單先經使用者同意。

1. **加入網域**：Domains → Add Domain，填寄件網域，區域選 Tokyo（`ap-northeast-1`），離展示的收件者最近。區域之後不能直接改：要刪掉網域、以新區域重新加入並換 DNS 紀錄。不論選哪個區域，帳號的資料（含寄出的信件與紀錄）都存在美國（[Regions](https://resend.com/docs/dashboard/domains/regions)）。
2. **DNS 紀錄**：照主控台列出的值，名稱都相對於寄件網域（[Cloudflare 的設定方式](https://resend.com/docs/dashboard/domains/cloudflare)）：
   - `send` 的 MX（主控台列出的退信主機，優先序 10）與 `send` 的 TXT（`v=spf1 include:<主控台列出的網域> ~all`）：Resend 寄信時信封寄件人（MAIL FROM）在 `send.<寄件網域>`，收件端的 SPF 驗的是這個名稱，兩筆都要在。
   - `resend._domainkey` 的 TXT（`p=` 加上公鑰）：DKIM，簽章的 `d=` 是寄件網域本身。
   - 建議加 `_dmarc` 的 TXT，從 `v=DMARC1; p=none;` 開始（[DMARC](https://resend.com/docs/dashboard/domains/dmarc)）；沒有這筆時，收件端改用上層網域（展示主機名稱所在的 zone）的 DMARC 紀錄（它的 `sp`，沒有 `sp` 時是 `p`）。DKIM 的 `d=` 與 From 相同，嚴格與寬鬆對齊都成立；SPF 的 `send.<寄件網域>` 只在寬鬆對齊下對齊，所以 `aspf` 不設成 `s`。
   - 收信用的 MX（`inbound`）不加：展示環境不收信。MX 與 TXT 都是 DNS only。

   主控台顯示 Verified 後才寄得出去；手動加的紀錄可能要等到 72 小時。
3. **金鑰**：API Keys → Create API Key，權限選 Sending access（`sending_access`，只能寄信），Domain 選寄件網域，只能以這個網域寄（[API keys](https://resend.com/docs/dashboard/api-keys/introduction)、[Create API key](https://resend.com/docs/api-reference/api-keys/create-api-key)）。金鑰只顯示這一次，之後看不到也改不了：

   ```sh
   <輸出 API key 的指令> | bun scripts/flux/sops-secret.ts --cluster demo --name trustiot-smtp --key password --set username=resend
   ```

   產生 `deploy/flux/clusters/demo/trustiot/trustiot-smtp.sops.yaml`；開 PR（內文只寫改了哪個 Secret 與 key），合併後 Flux 套用。
4. **主控台成員**：Resend 主控台的 Emails 頁看得到每封寄出的信，含收件地址與內容（[Managing emails](https://resend.com/docs/dashboard/emails/manage-emails)），登入驗證碼（Laysi/trustiot-v2#103）上線後也包括驗證碼。所以主控台的成員只限維運人員；有人不再負責時移除他，並依 [secrets runbook](secrets.md)「何時輪替」輪替金鑰。
5. **紀錄**：網域的狀態（Verified）與區域、金鑰的權限與限定網域記進測試報告，不寫網域名稱與金鑰。

### Anthropic 的工作區與金鑰

由 Anthropic 組織的管理員在 Claude Console 操作（[Workspaces](https://platform.claude.com/docs/en/manage-claude/workspaces)、[Authentication](https://platform.claude.com/docs/en/manage-claude/authentication)）：

1. **工作區**：Settings → Workspaces → Create workspace，建一個展示專用的工作區。只有組織管理員能建立。
2. **用量上限**：在 Workspaces 頁選這個工作區 → Spend limits，設每月上限與到達門檻時的通知。Default Workspace 不能設上限，所以金鑰不放在 Default Workspace。需要時在 Rate limits 頁選這個工作區另設每分鐘的限制。
3. **金鑰**：連結一個 service account（Settings → Service accounts 建立，再加入這個工作區），不用個人金鑰：個人金鑰代表那個人，他離開組織或工作區時金鑰就失效。在 API keys 頁 Create key：Linked account 選這個 service account，限定在展示的工作區，設到期日（Anthropic 在到期前寄信給建立的人；到期日建立後不能改）。

   ```sh
   <輸出 API key 的指令> | bun scripts/flux/sops-secret.ts --cluster demo --name trustiot-anthropic --key api-key
   ```

   產生 `deploy/flux/clusters/demo/trustiot/trustiot-anthropic.sops.yaml`，PR、合併、Flux 套用，同上。
4. **紀錄**：每月上限與通知門檻、金鑰連結的是 service account、限定的工作區與到期日記進測試報告，不寫金鑰。

### 輪替 Resend 與 Anthropic 金鑰

依 [secrets runbook](secrets.md)「通用步驟：SaaS 以 SOPS 管理的 Secret」，展示環境的差別如下：

1. 在主控台建立新金鑰（權限、限定的網域或工作區同上），舊的先留著。
2. 以上面的管線指令重新產生整個 SOPS 檔（不用 `sops edit`），開 PR。服務以環境變數讀金鑰，Secret 換了 pod 不會自己重啟：已有服務使用時（寄信在 Laysi/trustiot-v2#103 之後，Anthropic 在 Laysi/trustiot-v2#139 之後），同一個 PR 在 `values-demo.yaml` 為該服務的 `services.<name>.podAnnotations` 加上或更新 `trustiot.io/secret-rotated: "<YYYY-MM-DD>"`。
3. 合併、Flux 套用後確認：SMTP 照下方「寄測試信」寄一封（測試 pod 每次都從 Secret 讀，直接驗證新金鑰）；Anthropic 金鑰以 AI 閘道的一次呼叫確認（Laysi/trustiot-v2#139 補上指令）。
4. 在主控台刪掉舊金鑰（Anthropic 的 Disable 可以還原，Delete 不行）。外洩時不保留舊金鑰：先刪掉，再做第 1 步。

### 寄測試信

`scripts/demo/test-mail.ts` 在展示叢集裡以展示環境的 SMTP 設定與 Secret 寄一封信，用來確認收件端的 SPF 與 DKIM 通過。在工作站上執行，經 ssh 操作展示機；工作目錄要是審查過的 commit 的乾淨 checkout（`scripts/demo`、`scripts/release`、`deploy/flux/clusters/demo/trustiot` 或 `deploy/chart/values.yaml` 有未 commit 的變更時停下），`--apply` 還要這個 commit 已在 `origin/main` 裡。

```sh
bun scripts/demo/test-mail.ts send --ssh-host "$DEMO_SSH_HOST"                              # 只讀：印出計畫與確認碼
bun scripts/demo/test-mail.ts send --ssh-host "$DEMO_SSH_HOST" --apply --confirm <確認碼>    # 問收件地址，寄出
```

- **計畫**（只讀）：以 checkout 的 chart 預設值、`values-saas.yaml` 與 `values-demo.yaml` 合出 SMTP 主機、埠與加密方式（只接受 `starttls` 與 `tls`，從不寄明文 SMTP），從 HelmRelease 找出寄件人來自哪個 Secret 的哪個 key。在展示機上確認：namespace `trustiot` 強制 Pod Security `restricted`；叢集的 ConfigMap `trustiot-values` 與 checkout 的 SMTP 設定相同（不同表示 Flux 還沒套用這個 commit）；`trustiot-smtp` 有 `username` 與 `password`，`trustiot-demo-values` 有 `smtp-from`（只讀 key 的名稱，值不經過 ssh）。也印出 commit 是否已在 `origin/main` 裡：以本機的 ref 判斷，不會 fetch；不在或判斷不了時 `--apply` 拒絕，先 `git fetch origin main`，並且只從已合併的 commit 寄。也列出 namespace 裡的 NetworkPolicy（哪些限制 egress、是否選到測試 pod）、AuthorizationPolicy 的數量、ztunnel 是否在跑，以及之前留下的測試 pod。這些都算進確認碼，另外印出 lab 鎖目前的持有者。
- **收件地址**：`--apply` 時在終端機提示輸入（輸入的字不顯示，不留在畫面與錄下的終端機紀錄；Ctrl-C 取消；不是終端機時從 stdin 讀一行），不放在指令列，所以不進 shell history 與行程列表。只接受一般的地址（`@` 前是英數與 `. _ % + -`）。地址只經 ssh 的 stdin 交給 `kubectl attach`，進到 pod 的 stdin，不在 pod 的定義、Secret、日誌或任何輸出裡。收件信箱用維運人員自己能看原始標頭的信箱，例如 Gmail。
- **執行**：取得 lab 鎖（Laysi/trustiot-v2#468；自己已持有時加 `--lock-held`。它只在 `trustiot` 裡建一個短暫的 pod，不改叢集層級的東西；取得鎖是 `scripts/demo` 改變叢集時的共同規則，也避免和正在換 Secret 或部署的工作同時進行），重新計畫並比對確認碼，刪掉之前留下的測試 pod，再建立 pod `trustiot-test-mail-<執行 ID>`：curl 的官方映像（以 multi-arch index 的 digest 釘選，含 linux/arm64），非 root、拿掉所有 capability、唯讀的根檔案系統、RuntimeDefault seccomp、沒有 ServiceAccount token；帳密與寄件人是 `secretKeyRef` 的環境變數，curl 以 `--variable` 讀，不出現在任何行程的參數裡。pod 和 chart 的服務一樣帶 `istio.io/dataplane-mode: ambient`，經 ztunnel 出去；chart 的 NetworkPolicy 都只限制 ingress，所以這條連線走的路和服務寄信相同，腳本不改任何 policy。pod 最多存在 10 分鐘（`activeDeadlineSeconds`），結束後不論成敗都刪掉；刪不掉時印出刪除的指令。取得鎖之後收到 SIGINT（Ctrl-C）、SIGTERM 或 SIGHUP 時，先刪掉 pod（已建立的話）、再釋放 lab 鎖，然後以 128 加訊號編號結束（Ctrl-C 是 130）；第二個訊號立刻結束。只有 SIGKILL、第二個訊號與 ssh 連線中斷會留下它們：kubelet 在 `activeDeadlineSeconds` 到時停掉 pod，下次執行時當作之前留下的 pod 刪掉；lab 鎖照腳本印出的說明（`scripts/demo/lib.ts` 的 `LOCK_HELP`）手動移除。它不像 `scripts/demo` 的其他腳本記錄執行前後的叢集狀態：它只建立並刪掉自己的一個短暫 pod，不改其他東西。
- **輸出**：pod 裡 curl 的對話直接進 awk，只留下每一步的回應碼（與增強狀態碼）、指令名稱、TLS 版本與 cipher、憑證是否驗證通過；回應的文字（含 Resend 的信件 ID）、地址、AUTH 的參數、主機與 IP 都不會離開 pod。工作站再過濾一次，印出 `RCPT TO: 250 2.1.5`、`message: 250 2.0.0` 這樣的各段結果，可以直接放進測試報告。算是寄出要：TLS 憑證驗證通過、登入 235、MAIL FROM 與 RCPT TO 250、DATA 354、信件 250、pod 裡的 curl 與容器本身都以 0 結束；否則以非零結束並印出 curl 的錯誤碼與原因。curl 7（連不上）多半是 587 被擋：照 `values-demo.yaml` 的註解改成 2587（`starttls`）或 465（`tls`），開 PR，Flux 套用後再寄（計畫會比對叢集的設定）。

pod 的腳本可以先在工作站上試：`TRUSTIOT_DEMO_MAIL_E2E=1 bun test scripts/demo/test-mail.container.test.ts` 以 podman 跑同一個映像與腳本（權限設定同 pod），對著假的 SMTP relay 驗證 STARTTLS 與 TLS 的寄送、登入失敗、不提供 STARTTLS 的 relay 與不合格的地址，並確認輸出沒有地址、帳密與信件 ID。

### 讀收件端的標頭

在收件信箱下載原始信件（Gmail：信件的選單 →「顯示原始郵件」→「下載原始郵件」），存成工作站上的檔案：

```sh
bun scripts/demo/test-mail.ts headers < <原始信件檔>
```

它只讀標頭，印出：每個 `Authentication-Results`（最上面那個是收件端自己加的）與 `ARC-Authentication-Results` 的 `spf`、`dkim`、`dmarc` 結果；每個 DKIM `d=` 與 SPF 的 MAIL FROM 網域和 From 網域是否對齊（嚴格與寬鬆、只有寬鬆、不對齊）；`Received-SPF`；`DKIM-Signature` 的演算法與 selector（只照印 32 字以內的小寫英數，例如 Resend 的 `resend`；其他的可能含網域，例如 Microsoft 365 的，印成 `<selector>`）；每個 `Received` 記下的協定、TLS 版本與 cipher。網域只印成 `<from-domain>`、`<subdomain of from-domain>`、`<other-domain-1>` 這類佔位符，收件端印成 `<receiver-1>`，不印地址、IP、簽章或信件 ID，可以直接貼進測試報告。判定只看最上面的 `Authentication-Results`：`spf=pass` 且 MAIL FROM 網域和 From 網域有關（相同、子網域、上層或同一個組織網域；SPF 的對齊另印一行），有一個 `d=` 和 From 對齊的 `dkim=pass`，而且收件端有寫 `dmarc` 結果時要是 `dmarc=pass`（Gmail 會寫）。都成立時最後一行是 `verdict: PASS`；否則是 `FAIL`，列出缺了哪一項，並以非零結束。收件端沒寫 `dmarc` 結果時印一行說明，只以 SPF 與 DKIM 判定。除了寄件網域的簽章，信上可能還有 Resend 所用寄信服務的 DKIM 簽章，印成 `<other-domain-N>`，不對齊是正常的；SPF 的網域是 `send.<寄件網域>`，印成 `<subdomain of from-domain>`，只在寬鬆對齊下對齊。對齊以網域的最後兩段近似組織網域，收件端的 `dmarc=` 結果才是準的。看完刪掉原始信件檔：它含收件地址與寄件網域。
