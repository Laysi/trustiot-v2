# 展示機的 Cloudflare Tunnel 與展示主機名稱的 Access（2026-10-21）

Laysi/trustiot-v2#466（父 issue Laysi/trustiot-v2#457）：展示機自己的 Cloudflare Tunnel、以 deploy/host 的安裝程式裝成 `cloudflared-demo` 的 connector，以及三個展示主機名稱前面的 Cloudflare Access。設計與操作見[展示環境 runbook](../runbooks/demo-environment.md)。

時間都是 UTC：安裝與檢查在 2026-10-20 17:21–17:48 UTC（UTC+8 的 2026-10-21 01:21–01:48）。主機名稱、zone、帳號與 tunnel 等物件的 ID、Access 的 AUD 與 `cf-ray` 不寫進報告；需要時查私有的日誌。主機名稱以 app、gateway、ota 主機名稱稱呼，和腳本的佔位符相同。

## 環境

| 項目 | 內容 |
| --- | --- |
| 展示機 | lab 的 Mac mini M1，Fedora Linux Asahi Remix 44（Server Edition），arm64，systemd 259（安裝程式讀到的） |
| connector | `cloudflared-demo.service`，deploy/host 的安裝程式（`--skip-k3s --unit cloudflared-demo --metrics-port 20250 --token-stdin --reinstall-cloudflared`），commit `ae05499` 建置的 aarch64 執行檔，sha256 `dbd2d24a40c39524…`；設定檔 `/etc/credstore/cloudflared-demo.cloudflared.yml`，metrics `127.0.0.1:20250` |
| cloudflared | 安裝前 2026.10.0-1，dnf 的來源 `@commandline`（下載的 RPM）；安裝後 2026.10.2-1，來自 Cloudflare 的套件庫 `cloudflared-stable`（安裝程式以 root 檢查來源與簽章後才寫入 token） |
| 維運 tunnel | `cloudflared.service`，MainPID 1005，自 2026-10-10（展示機的時區）起執行 |
| Cloudflare | 依私有的 Cloudflare 變更清單由協調者套用：一個 self-hosted Access application 涵蓋三個主機名稱（受邀名單的 group，require Google 登入方式，binding cookie）；gateway 主機名稱的 `/mqtt` 與 `/certs/renew` 各一個 Bypass application；tunnel 路由第 1 段（略過路徑的錨定規則回 `http_status:418` 且不要求 JWT，其餘回 `http_status:418` 並設 `originRequest.access`，catch-all 404）；configuration rule 對 gateway 主機名稱的兩條略過路徑關閉 Browser Integrity Check。zone 為 Free 方案，Bot Fight Mode 沒有改（2026-10-20 的決定） |
| 工作站 | curl（以 OpenSSL 建置；TLS 特徵和閘道器的 rustls 不同，見「限制」），Bun 依 `.prototools` |
| 程式碼 | 安裝、`status` 與 `check` 從 `feat/466-demo-tunnel-2` 的 `ae05499` 乾淨 checkout 執行（計畫記下 commit）；之後的修正（`status` 讀 cloudflared 來源的錯誤、執行腳本的 umask）在同一分支 |

## 指令

在 repo 根目錄：

```sh
: "${DEMO_SSH_HOST:?set DEMO_SSH_HOST to the ssh host of the demo machine}"
: "${DEMO_APP_HOST:?set DEMO_APP_HOST to the app host name}"
: "${DEMO_GATEWAY_HOST:?set DEMO_GATEWAY_HOST to the gateway host name}"
: "${DEMO_OTA_HOST:?set DEMO_OTA_HOST to the ota host name}"
bun scripts/demo/tunnel.ts handoff-key --ssh-host "$DEMO_SSH_HOST"
bun install --cwd deploy/host --frozen-lockfile && bun run --cwd deploy/host build
bun scripts/demo/tunnel.ts install --ssh-host "$DEMO_SSH_HOST" --ciphertext <密文檔> --tunnel-id <tunnel ID>
bun scripts/demo/tunnel.ts install --ssh-host "$DEMO_SSH_HOST" --ciphertext <密文檔> --tunnel-id <tunnel ID> --apply --confirm <確認碼>
bun scripts/demo/tunnel.ts status --ssh-host "$DEMO_SSH_HOST"
bun scripts/demo/tunnel.ts check --ssh-host "$DEMO_SSH_HOST"
# 展示機上，check 之後：connector 的請求計數（只記數字；metrics 的其他內容含 Cloudflare 的機房名稱）
curl -s --max-time 5 http://127.0.0.1:20250/metrics | grep -E '^cloudflared_tunnel_(total_requests|request_errors|response_by_code)'
```

Cloudflare 端，唯讀，由協調者執行：

- `GET /accounts/<帳號>/cfd_tunnel/<tunnel>`：展示用與維運用兩條 tunnel 的狀態、connector 與連線。
- `GET /accounts/<帳號>/teamnet/routes?tunnel_id=<tunnel>&is_deleted=false`：展示用 tunnel 沒有私有網路路由。
- GraphQL `httpRequestsAdaptiveGroups`（zone，2026-10-20 17:47:30–17:48:30 UTC，篩三個展示主機名稱）：每項探測的 `edgeResponseStatus` 與 `originResponseStatus`。
- GraphQL `firewallEventsAdaptive`（zone，17:41–17:52 UTC）：安全事件。

## 結果

原始資料（已遮蔽：tunnel ID 與 `cf-ray` 換成說明文字，主機名稱與 Access 的 team 網域在腳本寫入時就已換成佔位符）：

- [install-plan.json](data/2026-10-21-466-demo-tunnel-access/install-plan.json)：`--apply` 確認的計畫
- [host-before.json](data/2026-10-21-466-demo-tunnel-access/host-before.json)、[host-after.json](data/2026-10-21-466-demo-tunnel-access/host-after.json)：安裝前後的主機狀態
- [check-stage1.json](data/2026-10-21-466-demo-tunnel-access/check-stage1.json)：`check` 第 1 段的 20 項結果

### 安裝

- **dry run**（17:21 UTC）：計畫的 commit 是 `ae05499`；token 所屬的 tunnel 等於 `--tunnel-id`；安裝程式的 13 項變更都在允許清單上，沒有 `--allow-change`：trusted zone 的兩項變更（runtime 與 permanent 各兩行，也就是 deploy/host 的主機基線，2026-10-20 接受）、Cloudflare 的套件庫與金鑰、換掉 cloudflared 套件、`cloudflared-demo` 的 token、設定檔與 unit、`daemon-reload`、啟用與啟動。cloudflared 的報告行：

  ```
  cloudflared: the installer replaces /usr/bin/cloudflared 2026.10.0-1 (from @commandline) with 2026.10.2-1 from Cloudflare's signed repository (cloudflared-stable)
  cloudflared: this is a version change, 2026.10.0-1 -> 2026.10.2-1
  cloudflared: the ops tunnel cloudflared.service runs the same /usr/bin/cloudflared; its running process (PID 1005) keeps the binary it started with and runs 2026.10.2-1 from its next restart (reboot); this script never restarts it
  cloudflared: the old package's %postun deletes /usr/local/bin/cloudflared and /usr/local/etc/cloudflared/.installedFromPackageManager; the run's ExecStopPost puts them back as root, however the installer ends
  cloudflared update units: cloudflared-update.timer not-found, cloudflared-update.service not-found
  ```

  版本變化（2026.10.0 換成 2026.10.2）事先接受：2026-10-20 接受的是換到套件庫的版本，dry run 指名了確切的版本，確認碼也涵蓋它。
- **安裝**（17:39:54–17:42:01 UTC，結束碼 0）：取得 lab 鎖；在鎖內重新放置檔案並再跑一次 dry run，計畫與確認碼相符；把 trusted zone 存到 `/var/lib/trustiot-demo/firewalld/`；安裝程式在暫時 unit 裡執行，13 項變更全部完成。換套件時 `dnf reinstall` 如預期失敗（套件庫沒有 2026.10.0-1），接著 `dnf distro-sync` 從 `cloudflared-stable` 升到 2026.10.2-1，並匯入 Cloudflare 的套件簽章金鑰（指紋 `CC94B39C77AE7342A68B89628A682D308D4E5E73`）；安裝程式以 root 檢查新套件的來源與簽章後才寫入 token。`systemd-analyze security cloudflared-demo.service` = 1.1（上限 2）；沒有 80、443 的監聽。`ExecStopPost` 的結果：`service_result=success exit_code=exited exit_status=0 link=restored marker=restored`。之後刪掉交付私鑰與密文、釋放 lab 鎖；維運 tunnel 仍是同一個行程。
- **主機狀態前後**：`cloudflared-demo.service` 從 `not-found` 變成 `loaded`、`active`，`NRestarts=0`；trusted zone 的 sources 從 `10.42.0.0/16 10.43.0.0/16` 變成 `10.42.0.0/16`，並多了拒絕 pod 連 22 埠的 rich rule，runtime 與 permanent 相同；`/usr/local/bin/cloudflared` 仍是指向 `/usr/bin/cloudflared` 的連結，標記檔仍在。維運 tunnel 的 MainPID 前後都是 1005，`NRestarts=0`。
- **安裝最後的報告行**是 `cloudflared now: 2026.10.2-1 from ; OK /usr/local/bin/cloudflared -> /usr/bin/cloudflared; …`：「from」後面是空的。原因見下一節；已修正。

### cloudflared 的來源讀不到（已修正）

`status`（17:46 UTC）印出 `cloudflared: 2026.10.2-1 from unknown`。2026-10-21 在展示機上查到：dnf5 5.4.6 把套件的 `from_repo` 存在 `/usr/lib/sysimage/libdnf5/packages.toml`，安裝的 dnf 交易（17:41:47 UTC）之後這個檔是 root:root 0600（目錄 0755）。一般使用者的 `dnf -q repoquery --installed --qf '%{from_repo}\n' cloudflared` 只在 stderr 寫 `Loading system state TOML file … failed`，stdout 是空的，結束碼 0；腳本把 stderr 丟掉，空字串被當成值。同一個查詢以 root 加上 `--disablerepo='*'` 印出 `cloudflared-stable`。這個檔為什麼變成 0600，見下一節。

修正後腳本以 root 讀 `from_repo`（`--disablerepo='*'`：已安裝的套件不需要套件庫的 metadata），保留 dnf 的 stderr；空的或不像套件庫 ID 的回答、dnf 失敗，都是讀取失敗，附上 dnf 最後一行 stderr，不再當成值。`status` 與安裝最後的報告行改為一行 `OK cloudflared <版本> from cloudflared-stable`；來自別處或讀不到時是 `FAIL`，附上原因（`status` 照樣以 0 結束，和它其他的 `FAIL` 行一樣）。dry run 的計畫在來源的位置寫出讀取失敗與原因。單元測試涵蓋 root 的指令列（含 shell 傳給 dnf 的參數）、空的輸出、dnf 失敗、正常的輸出，以及三處使用者。

修正後以 `0906740` 的程式碼在 2026-10-20 18:24:32 UTC 跑 `status`，來源那一行是：

```text
OK cloudflared 2026.10.2-1 from cloudflared-stable
```

這次 `status` 的其他各行與 17:46 UTC 那次相同：`127.0.0.1:20250/ready` 4 條連線、`NRestarts=0`，token 與設定檔 `root:root 600`、目錄 `/etc/credstore` `root:root 700`，`/run/trustiot-demo` 是空的，沒有留下的安裝程式執行，token 的稽核三項都是 0（journal、展示機的兩個目錄、工作站的日誌目錄），維運 tunnel 仍是同一個行程。

### 安裝留下的 dnf 權限（已修正；展示機上待恢復）

2026-10-21 在展示機上以唯讀指令查到：

| 位置 | 權限 |
| --- | --- |
| `/usr/lib/sysimage/libdnf5/` 的 `environments.toml`、`groups.toml`、`modules.toml`、`nevras.toml`、`packages.toml`、`system.toml`（dnf5 的系統狀態） | `root:root 600`，修改時間都是 2026-10-20 17:41:47 UTC，也就是安裝的 dnf 交易 |
| `/var/cache/libdnf5/cloudflared-stable-<hash>/`（Cloudflare 套件庫的 dnf 快取） | 這個目錄與其中的 `repodata`、`solv`、`packages` 是 700，`repomd.xml` 與 primary 檔是 600；同一處其他套件庫的快取是 755 與 644 |
| `/var/cache/PackageKit/44/metadata/cloudflared-stable-<hash>/repodata/` 的 `repomd.xml` 與 `<sha256>-primary.xml.zst`（PackageKit 的 metadata 快取，dnf 把剛下載的 metadata 複製到這裡） | `root:root 600`，修改時間 17:41:47 與 17:41:50 UTC，在安裝的 dnf 交易內；這個目錄、`repodata/`、`solv/` 是 755，`solv/cloudflared-stable.solv` 是 644，`/var/cache/PackageKit/44/metadata/` 下其他套件庫的目錄都是 755 |
| 安裝程式自己寫的檔案 | 照預期：`/etc/credstore` 700 與其中的檔案 600，unit、`.repo` 檔、套件簽章金鑰、firewalld 的 zone 檔、標記檔 644 |
| `rpmdb.sqlite` 與 dnf5 的交易紀錄 | 644 |

原因：執行腳本開頭的 `umask 077` 一路傳給安裝程式和它呼叫的 dnf，dnf 依 umask 建立狀態檔與快取。SaaS 主機上安裝程式由 sudo 執行；sudo 的預設 umask 是 022，但 sudo 會把它和呼叫者的 umask 合併（sudoers 設了 `umask_override` 時除外），所以從 umask 較嚴（例如 027、077）的 shell 執行時，較嚴的 umask 一樣會傳給安裝程式與 dnf，同樣的問題也會在 SaaS 主機上發生；deploy/host/README.md 因此要求從 `umask` 印出 `0022` 的 shell 執行，安裝程式端的修正在 Laysi/trustiot-v2#476。影響是一般使用者的 dnf 讀不到系統狀態（上一節的 `from_repo` 就是如此），套件與服務本身不受影響。

完整性檢查（2026-10-21，協調者在展示機上以唯讀指令執行）：

- 以 `sudo find` 搜尋 `/etc`、`/usr`、`/var`、`/opt`（`-xdev`），找修改時間或狀態變更時間在 2026-10-20 17:15:00–17:50:00 UTC（兩次 dry run 與安裝的時段）、而其他使用者讀不到的檔案與目錄。排除 k3s 與 journald 寫入的 `/var/lib/rancher`、`/var/log/pods`、`/var/log/containers`、`/var/lib/kubelet`、`/var/log/journal`、`/var/lib/containerd`、`/var/lib/cni`：它們不是執行腳本的子程序寫的。
- 找到的只有以下兩類：
  - 預期的：`/etc/credstore` 700 與其中的 token、設定檔 600；`/var/lib/trustiot-demo` 700，其中三次執行的 `.log`、`.exit`、`.post` 600，放置好的執行腳本與安裝程式 700，firewalld 備份目錄 700 與其中的 `runtime.txt`、`permanent.txt`、`complete` 600；`cloudflared-demo.service` 的 systemd PrivateTmp 目錄 `/var/tmp/systemd-private-<id>-cloudflared-demo.service-<id>` 700（systemd 本來就這樣建立）。
  - dnf 的：上表的六個 TOML、libdnf5 的 `cloudflared-stable-<hash>` 快取，以及 PackageKit 的兩個檔。
- 另以 `sudo find` 不限時間搜尋套件管理的路徑 `/usr/lib/sysimage`、`/var/cache/libdnf5`、`/var/lib/dnf`、`/etc/yum.repos.d`、`/etc/pki/rpm-gpg`、`/usr/local`、`/etc/dnf`、`/var/cache/PackageKit`，除了上表的項目，沒有其他讀不到的檔案或目錄。

修正：commit `12db555`（`fix(scripts): run the demo installer under umask 022, not the runner's 077`）只讓安裝程式在 umask 022 下執行；執行腳本自己的 `.log`、`.exit`、`.post` 仍以 077 建立（0600），複本仍以 `install -m` 設為 0600 與 0700。022 不會放寬任何 secret：deploy/host 的 `writeFile` 以 `O_EXCL` 和最終權限建立檔案後再 `chmod`，`makeDirectory` 以指定權限 `mkdir` 後再 `chmod`，都不依賴呼叫者的 umask。單元測試以替身安裝程式確認它看到的 umask 是 `0022`、它建立的檔案是 644，執行腳本的 `.log`、`.exit`、`.post` 是 600，複本是 700 與 600；拿掉 022，或把 022 放到腳本開頭，測試都會失敗。

展示機上已經變成 600 與 700 的權限由人以一次性的指令改回（[runbook](../runbooks/demo-environment.md)「2026-10-21 安裝留下的 dnf 權限」）：那六個檔改成 0644，libdnf5 與 PackageKit 的 `cloudflared-stable-<hash>` 目錄以 `chmod -R u=rwX,go=rX` 改回。

> **待補**：協調者在恢復之後讀到的權限（六個檔與快取目錄）補在這裡。

### 連線狀態（驗收 1）

| 檢查 | 結果 |
| --- | --- |
| Cloudflare：展示用 tunnel | `healthy`，1 個 connector，4 條連線，版本 2026.10.2 |
| 展示機：`cloudflared-demo.service`（`status`，17:46 UTC） | `LoadState=loaded ActiveState=active`，`NRestarts=0`；`127.0.0.1:20250/ready` 有 4 條連線 |
| Cloudflare：維運 tunnel | `healthy`，4 條連線，版本 2026.10.0，2026-10-16 以來沒有變 |
| 展示機：維運 tunnel | MainPID 1005，安裝前後不變；`/proc/1005/exe` 是 `/usr/bin/cloudflared (deleted)`：它繼續跑啟動時的 2026.10.0，下次重啟才用 2026.10.2 |
| Cloudflare：展示用 tunnel 的私有網路路由 | 0 條 |

`status` 在 `127.0.0.1:20250/ready` 讀到 4 條連線，是 deploy/host 的 `--config` 變更第一次在真的主機上被證實：每個 unit 以 `LoadCredential` 交給 cloudflared 自己的設定檔，metrics 埠由設定檔指定，connector 讀了它，而不是 cloudflared 自己在 20241–20245 挑的埠（維運 tunnel 佔用 20241）。這項證據（2026-10-20 17:46 UTC）出現之前，新版的安裝程式不在任何 SaaS 主機上執行。

### 對外請求（驗收 2、3）

`check` 第 1 段（17:47:56 UTC），每一項從工作站與展示機（lab 網路）各送一次，兩處的結果相同：

| 請求 | 預期 | 結果（兩處） | 回應 |
| --- | --- | --- | --- |
| app 主機名稱 `/`，瀏覽器的 User-Agent，沒有 session | `access` | PASS，`access` | 302 到 `<team>.cloudflareaccess.com` 的登入頁，`server: cloudflare` |
| gateway 主機名稱 `/mqtt`，WebSocket 升級，沒有 User-Agent 與 Accept | `origin`（418 或 101） | **FAIL**，`unexpected` | 502，`server: cloudflare`，`text/html`，148 bytes |
| gateway 主機名稱 `POST /certs/renew`，沒有 User-Agent | `origin`（418） | PASS，`origin` | 418，body 0 bytes |
| gateway 主機名稱的 `/auth/login`、`/healthz`（app 的路徑），沒有 User-Agent | `access` | PASS，`access` | 302 到 Access 的登入頁 |
| ota 主機名稱的 `/auth/login`、`/healthz`，沒有 User-Agent | `access` | PASS，`access` | 302 到 Access 的登入頁 |
| gateway 與 ota 主機名稱的 `/`，沒有 User-Agent | `access` | PASS，`access` | 302 到 Access 的登入頁 |
| gateway 主機名稱 `/mqtt/x`（略過路徑下的子路徑），沒有 User-Agent | `refused` | PASS，`refused` | 403，沒有 `content-type`，body 0 bytes |

20 項中 18 項 PASS；兩項 FAIL 都是 WebSocket 升級。`cf-ray` 每一項都記在私有的日誌裡。

同一時段 Cloudflare 的 HTTP 請求分析（`httpRequestsAdaptiveGroups`，三個展示主機名稱）：

| 請求 | 次數 | edge 的狀態碼 | origin 的狀態碼 |
| --- | --- | --- | --- |
| gateway 主機名稱 `GET /mqtt`（WebSocket 升級） | 2 | 418 | 418 |
| gateway 主機名稱 `POST /certs/renew` | 2 | 418 | 418 |
| gateway 主機名稱 `GET /mqtt/x` | 2 | 403 | 403（connector 的拒絕） |
| 每一項 Access 後面的探測（app `/`；gateway 與 ota 的 `/`、`/auth/login`、`/healthz`） | 各 2 | 302 | 0（沒有到 origin） |

connector 的計數（`check` 剛結束後，`127.0.0.1:20250/metrics`）：`cloudflared_tunnel_total_requests` 6，`cloudflared_tunnel_response_by_code{status_code="418"}` 4，`cloudflared_tunnel_request_errors` 2。6 個請求正是兩次 WebSocket 升級、兩次 `POST /certs/renew` 與兩次 `/mqtt/x`；418 的 4 次是前兩者；2 個錯誤是 `/mqtt/x`，展示機的 journal 對那兩個請求記了 Access JWT 檢查的錯誤（`AccessJWTValidator`，`ingressRule=3`）。Access 後面的探測沒有一個到 connector。

Cloudflare 的安全事件（`firewallEventsAdaptive`，整個 zone，17:41–17:52 UTC）：0 筆。

**WebSocket 升級的 502**：client 收到 Cloudflare 的 502 頁，而 Cloudflare 的 HTTP 請求分析與 connector 的計數都記錄 origin 回了 418（2026-10-21 確認；Cloudflare 的文件沒有描述這個行為）。路由以 418 而不是 101 回應升級，client 就收到這個 502；請求本身穿過了 Access 與機器人防護、到了 connector。`check` 照舊把這一項記為 FAIL：它只看 client 收到的回應，分不出這個 502。runbook 寫了人工確認的方法（探測前後讀 connector 的兩個計數）；讓 `check` 自己讀計數並判斷是 Laysi/trustiot-v2#473。在那之前 `check` 的第 2 段會把同樣的 502 當成入口的回應而通過，沒有計數佐證。

**connector 拒絕的形狀**：`/mqtt/x` 的 403 沒有 `content-type`、body 0 bytes。第 2 段只把「沒有 body 也沒有 `content-type` 的 403」算成 connector 的拒絕，這一項確認這個判斷適用實際跑的 cloudflared 2026.10.2。

**限制**：curl 的 TLS 特徵（OpenSSL）和閘道器的 rustls 不同。展示用閘道器（Laysi/trustiot-v2#458）第一次連線後，再查該時段的安全事件。

**瀏覽器的互動檢查移到 Laysi/trustiot-v2#469**（2026-10-21 決定）：受邀的帳號登入後看到 418（放行方向：帶有效 Access JWT 的請求通過 connector 自己的 JWT 檢查，證明 tunnel 路由裡的 team 名稱與 AUD 正確）、沒有受邀的 Google 帳號被拒、gateway 的 `/mqtt` 在瀏覽器不需登入。原因：tunnel 後面目前只有回 418 的替身，互動檢查在路由改到展示環境入口之前、在 Laysi/trustiot-v2#469 一起做。

### tunnel token 的位置（驗收 4）

| 檢查 | 結果 |
| --- | --- |
| token 檔與設定檔（`status`） | `root:root 600 /etc/credstore/cloudflared-demo.tunnel-token`、`root:root 600 /etc/credstore/cloudflared-demo.cloudflared.yml`，目錄 `/etc/credstore` 是 `root:root 700` |
| 交付用的私鑰與密文 | 只在 tmpfs 的 `/run/trustiot-demo/`（root 0700），安裝成功後刪除；`status` 顯示 `/run/trustiot-demo: (absent or empty)` |
| `/var/lib/trustiot-demo/` | 三次執行（兩次 dry run、一次 apply）的 `.log`、`.exit`、`.post`，trusted zone 的備份 `firewalld`，執行腳本 `run-installer` 與安裝程式 `trustiot-host-install`；沒有 token 或密文 |
| 展示機：整個 journal（`journalctl -o verbose`，含指令列等所有欄位）與 `/run/trustiot-demo/`、`/var/lib/trustiot-demo/` 含 token 或其中 secret 的次數 | journal 0、檔案 0（root 以 `/run` 下 0600 的 pattern 檔 `grep -f` 比對 2 個 pattern，只回傳次數） |
| 工作站：日誌目錄含 tunnel token 開頭（`eyJhIjoi`）的檔案數 | 0 |
| repo | 分支的 commit 與 tree 以 gitleaks 掃描沒有發現；`git grep -E 'eyJhIjoi[A-Za-z0-9+/=_-]{12,}'` 沒有結果。`eyJhIjoi` 本身（`{"a":"` 的 base64，不含 token 的內容）只出現在 `deploy/host/tests/unit.test.ts`、`docs/runbooks/demo-environment.md`、`scripts/demo/tunnel.ts`、`scripts/demo/tunnel.test.ts` |
| 交付過程 | token 只在 Cloudflare 的 API 執行環境內讀取，以 RSA-OAEP 加密給展示機上產生的金鑰；離開那個環境的只有 tunnel ID、公鑰指紋、密文與位元組數。展示機上解密後直接進安裝程式的 stdin |

### 文件（驗收 5）

- `docs/runbooks/demo-environment.md`：架構與取捨、Cloudflare 設定的順序、安裝與中斷後的處理、cloudflared 的更新與退回、檢查（含第 1 段 WebSocket 升級的 502 與人工確認）、輪替、移除 connector；Access 名單的管理與拿掉 Access。
- `deploy/host/README.md`：同一台主機多條 tunnel 時各自的設定檔與 metrics 埠。
- `docs/architecture/secrets.md` 的清冊與 `docs/runbooks/secrets.md` 的輪替入口。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 展示用的 tunnel 在 Cloudflare 與展示機上都顯示連線正常，維運用的 tunnel 不受影響 | 通過：Cloudflare 上 `healthy`、4 條連線；展示機上 `active`、`/ready` 4 條連線。維運 tunnel 仍 `healthy`、4 條連線，行程與版本不變 |
| 沒有受邀的瀏覽器連三個主機名稱時被 Cloudflare Access 擋下 | 通過：`check` 的瀏覽器探測與其他 Access 後面的探測，從兩處都是 302 到 Access；Cloudflare 的請求分析中這些請求都是 edge 302、origin 0。以真的瀏覽器登入的互動檢查移到 Laysi/trustiot-v2#469（原因見上） |
| 閘道器的固定路徑（gateway 主機名稱的 `/mqtt`、`/certs/renew`）不經 Access，也沒有被機器人防護擋下，回應來自 tunnel 後面；它們下面的子路徑被 connector 拒絕 | 通過：`POST /certs/renew` 從兩處都是 418；`/mqtt` 的 WebSocket 升級在 client 是 Cloudflare 的 502，但 Cloudflare 的請求分析記錄 origin 418，connector 的計數也記了這兩個請求與它們的 418；兩條路徑都沒有轉到 Access；安全事件 0 筆；`/mqtt/x` 從兩處都被 connector 以 403 拒絕（請求分析 origin 403，connector 記為 JWT 檢查的錯誤）。`check` 自己對 `/mqtt` 仍記 FAIL，自動判斷是 Laysi/trustiot-v2#473 |
| tunnel token 只在 Cloudflare 與展示機上只有 root 能讀的位置 | 通過 |
| cloudflared 的安裝方式與 Access 名單的管理方式寫在 repo 的文件 | 通過 |

待測：路由的第 2 段（三個主機名稱改到展示環境入口的 ClusterIP，`check --stage 2`），要等展示環境的入口 Service 部署後才能套用。
