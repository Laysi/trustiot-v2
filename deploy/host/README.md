# SaaS 主機（安裝程式與 runbook）

SaaS 跑在一台單節點 k3s 主機上，主機沒有公網 IP、不開任何產品埠，所有對外流量都經主機上的 cloudflared 主動連到 Cloudflare（決策 27、[部署與可搬移性](../../docs/architecture/deployment.md)）。本目錄是這台主機的安裝程式 `trustiot-host-install` 與維運步驟：安裝、更新 cloudflared、輪替 tunnel token。地端主機不經 Cloudflare，不用本目錄。

```text
瀏覽器、閘道器 ──443──> Cloudflare ──tunnel（主機主動連出）──> cloudflared（systemd 沙箱）
                                                                 ├─ http://<入口 ClusterIP>:80 ──> Istio 入口 gateway ──> 服務
維運人員 ──Cloudflare Access──> Cloudflare ──同一條 tunnel─────> └─ ssh://localhost:22 ──> sshd
```

- cloudflared 到入口 gateway 這一段是**明文 HTTP**，只在主機內（主機的網路命名空間經 k3s 的 cni0 到入口 pod），不經過任何網路線路。入口 gateway 把 `x-forwarded-proto` 改寫為 `https`，後端以「SaaS 一律 https」判斷 Secure cookie 與重導向網址（Laysi/trustiot-v2#62）；入口的 NetworkPolicy 只放行主機來源，叢集內其他 pod 連不到入口。cloudflared 到入口要不要改走 HTTPS 尚未決定（[可行性測試結果（2026-10-10）](../../docs/feasibility/2026-10-10-edge-poc.md)）。
- 維運 SSH 與產品流量共用同一個 cloudflared，k3s 故障時仍進得了主機；SSH 的主機名稱前面一定要有 Cloudflare Access，只放行維運人員。產品主機名稱與閘道器不經 Access。
- tunnel 是遠端管理（remotely managed）：路由（哪個主機名稱送到哪裡）設在 Cloudflare，主機上只有 token 一個祕密。

## 安裝程式做什麼

`trustiot-host-install` 以 root 執行，每一步先讀主機現況，只做還沒做的事；同一台主機再跑一次不會改任何東西，也不會重啟服務。`--dry-run` 只列出要做的變更。不是安裝程式寫的檔案（第一行沒有 `# Managed by trustiot-host-install` 標記）一律不覆寫，遇到時在**改任何東西之前**停下並說明。

| 步驟 | 內容 |
| --- | --- |
| 前置檢查 | root、x86_64 或 aarch64、Fedora 或 RHEL 相容的發行版（有 dnf）、systemd 252 以上；所有會拒絕的情況（別人寫的 unit、k3s 設定不同、別的 Cloudflare 套件庫定義、不是從釘選套件庫裝的 cloudflared、trusted zone 範圍過大、沒有 token 或 token 格式不對）都在這裡先檢查 |
| SELinux | 必須是 Enforcing，否則停下（不自動切換，切換要重新標記並重開機） |
| firewalld | 沒裝就以 dnf 安裝；沒在執行時先以 `firewall-offline-cmd` 改好永久設定、檢查通過才啟動，所以不會以不安全的設定跑起來；把 k3s 的 Pod 網段 `10.42.0.0/16` 加進 trusted zone（k3s 需要：pod 連 API server 與 kubelet、轉送流量；Service 網段 `10.43.0.0/16` 不會是封包來源，信任它只會放行偽造的封包，所以不加，舊設定加過的會移除。2026-10-09 的可行性測試兩個都加），但先在 trusted zone 加一條 rich rule 拒絕 pod 連主機的 SSH（22/tcp），維運 SSH 只能經 tunnel 與 Access；從 trusted 以外的每個 zone（不只作用中的）移除 `http`、`https`、`http3` 服務與 `80/tcp`、`443/tcp`、`443/udp`（policy 也一樣）；其他會開 80 或 443 的設定（涵蓋它們的埠範圍如 `1-1024/tcp`、含 80 或 443 的自訂服務、接受它們的 rich rule、port forward、target 為 ACCEPT 的 zone（綁了網卡或來源，或是預設 zone；沒有綁定的如 Fedora 內建的 `nm-shared` 不算）或進入主機的 policy、direct rule）不自動改，停下請人處理。已在執行時 runtime 與 permanent 分開設定，不需要 reload。trusted zone 會接受所有流量，所以它是預設 zone（runtime 或 `firewalld.conf`）、掛了網卡，或信任 k3s 網段以外的來源時，安裝程式停下 |
| k3s | 寫 `/etc/rancher/k3s/config.yaml`（0600）：關閉 Traefik 與 servicelb；以官方安裝程式安裝 k3s `v1.36.5+k3s1`。安裝程式從該版本的 git tag 下載，先比對釘選的 SHA-256 才執行；它會下載 k3s 執行檔並以 release 的 sha256sum 驗證，並從 rpm.rancher.io 安裝 k3s-selinux 政策。等 node Ready |
| cloudflared 套件 | 寫入 Cloudflare 的套件簽章金鑰（指紋 `CC94B39C77AE7342A68B89628A682D308D4E5E73`，隨安裝程式一起釘選，不在安裝時下載）與官方套件庫 `/etc/yum.repos.d/cloudflared.repo`（`gpgcheck=1`：每個套件都以釘選的金鑰驗簽。套件庫的 metadata 不驗簽，因為 Cloudflare 用一把已不再公布的舊金鑰 `254B391D8CACCBF8` 簽 `repomd.xml`，開 `repo_gpgcheck` 時 dnf 會拒絕整個套件庫，2026-10-11 在實驗機實測），以 dnf 只從這個套件庫（`--repo=cloudflared-stable`）安裝 cloudflared；之後以 `dnf upgrade --repo=cloudflared-stable` 更新。已裝好的 cloudflared 必須來自這個套件庫且有 Cloudflare 金鑰的簽章（`dnf repoquery` 的 `from_repo` 與 `rpm` 的簽章 Key ID `8a682d308d4e5e73`），否則停下：拿到 token 的執行檔一定要是釘選來源的；安裝程式不替換它，因為主機上可能有別的服務正在用它。主機上有其他定義 Cloudflare 套件庫的檔案（包括依 Cloudflare 官方說明下載、安裝時才抓金鑰的 `cloudflared.repo`）時停下，請先刪除，確保只信任釘選的金鑰 |
| tunnel token | 從 `--token-stdin` 或 `--token-file` 讀入，檢查格式（只印 tunnel ID，不印 token），存成 `/etc/credstore/<unit>.tunnel-token`（root、0600；目錄 0700）。沒給 token 時沿用主機上已有的 |
| cloudflared 服務 | 寫 `/etc/systemd/system/<unit>.service`（預設 `cloudflared`），啟用並啟動；unit 或 token 有變才重啟 |
| 驗收檢查 | 服務是 active；`systemd-analyze security <unit>` 不高於 2.0；`ss -tlnp` 與 `ss -ulnp` 沒有任何 80、443 在監聽；沒有 pod 以 hostPort 發布 80、443（那是 NAT 規則，`ss` 看不到）。任何一項不符就以非零結束 |

安裝程式**不做**的事：Istio（[deploy/istio](../istio/README.md)）、TrustIoT chart（[deploy/chart](../chart/README.md)；SaaS 由 Flux 部署，見 [deploy/flux](../flux/README.md)）、Cloudflare 上的 tunnel、路由與 Access 設定（見下方「安裝」）、k3s 升級。

### cloudflared 的 systemd 沙箱

unit 的內容在 `src/desired.ts` 的 `renderUnit`，重點：

| 設定 | 作用 |
| --- | --- |
| `DynamicUser=yes`、`PrivateUsers=yes` | 每次啟動用臨時的非 root 使用者，看不到主機上其他使用者 |
| `CapabilityBoundingSet=`（空）、`AmbientCapabilities=`（空）、`NoNewPrivileges=yes` | 沒有任何 capability，也不能經 setuid 取得 |
| `ProtectSystem=strict`、`ProtectHome=yes`、`PrivateTmp=yes`、`PrivateDevices=yes` | 整個檔案系統唯讀，看不到家目錄、其他程式的暫存檔與裝置 |
| `LoadCredential=tunnel-token:/etc/credstore/<unit>.tunnel-token` | systemd 以 root 讀 token，放進只有這個服務看得到的 credentials 目錄（`%d`），cloudflared 以 `--token-file %d/tunnel-token` 讀；token 不在指令列、環境變數或 unit 檔裡 |
| `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK`、`SystemCallFilter=@system-service` 並排除 `@privileged @resources` | 只有網路服務需要的 socket 與系統呼叫 |
| `--no-autoupdate` | 不自己更新，版本由 dnf 管理 |
| `Restart=always`、`RestartSec=5s`、`StartLimitIntervalSec=0` | 網路長時間中斷後仍持續重試 |
| `TimeoutStopSec=60s` | cloudflared 收到 SIGTERM 會等 30 秒讓進行中的請求結束 |

ICMP proxy 需要 raw socket，在沙箱內停用，產品用不到。不限制可連的 IP（`IPAddressDeny=`）：Cloudflare 的邊緣位址會變，寫死反而容易斷線。

## 支援的主機

- Fedora（實測 Fedora 44）或 RHEL 相容的發行版（RHEL、Rocky、AlmaLinux 9 以上，有 dnf 與 systemd 252 以上；未實測），x86_64 或 aarch64。
- SELinux 為 Enforcing；不需要公網 IP，只要能連出到 Cloudflare（TCP 443 與 UDP 7844）、GitHub、rpm.rancher.io 與 pkg.cloudflare.com。
- 2026-10-11 實測：x86_64 的全新 Fedora Cloud 44 VM（乾淨主機的完整安裝、重跑、重開機與修正手動打開的 443），以及 lab 的 Mac mini M1（Fedora Asahi Remix 44、aarch64、已有 k3s 的既有主機，以 `--skip-k3s` 與另一個 unit 名稱安裝，並經 tunnel 驗證健康檢查與 token 輪替），見[測試報告](../../docs/test-reports/2026-10-11-64-saas-host.md)。

## 建置安裝程式（工作站）

主機上不需要 Bun：安裝程式編譯成單一執行檔，一個架構一個。工作站上的 Bun 由 proto 管理：

```sh
proto install
bun install --cwd deploy/host --frozen-lockfile
bun run --cwd deploy/host test
bun run --cwd deploy/host build      # deploy/host/dist/ 下的兩個執行檔與 SHA256SUMS
```

`bun build --compile` 為另一個架構編譯時，會下載同版本 Bun 的該架構執行環境。

## 安裝

以下 `<app>`、`<gateway>`、`<ota>` 是 chart 的 `hosts.app`、`hosts.gateway`、`hosts.ota`，`<ssh>` 是維運 SSH 的主機名稱（例如 `ssh-saas.example.com`）。

1. **Cloudflare：建立 tunnel。** Zero Trust 的 Networks → Tunnels → Create a tunnel → Cloudflared，名稱例如 `trustiot-saas`；或用 API `POST /accounts/{account_id}/cfd_tunnel`（`config_src: cloudflare`）。複製安裝指令裡的 token（`eyJ…` 那段），不要執行 Cloudflare 給的 `cloudflared service install` 指令：它會建立沒有沙箱的 unit。
2. **Cloudflare：先建 SSH 的 Access 應用程式，再加 SSH 路由。** Access → Applications → Self-hosted，網域 `<ssh>`，政策只放行維運人員（email 或群組），自動化另加 service token 的政策。順序不能反過來：先有路由、後有 Access 的那段時間，SSH 會直接暴露在網際網路上。
3. **把安裝程式放上主機並核對雜湊。** 經雲端主控台或內網的跳板機複製對應架構的執行檔與 `SHA256SUMS`：

   ```sh
   sha256sum --check --ignore-missing SHA256SUMS
   ```

4. **把 token 交給主機並安裝。** token 只經 stdin 或檔案傳入，不放在指令列（會留在 shell 歷史與程序列表）。從密碼管理工具貼上時：

   ```sh
   sudo sh -c 'umask 077; cat > /root/tunnel-token'   # 貼上 token，按 Enter 再按 Ctrl-D
   sudo ./trustiot-host-install-x86_64 --token-file /root/tunnel-token
   sudo shred -u /root/tunnel-token
   ```

   也可以直接用管線：`<輸出 token 的指令> | sudo ./trustiot-host-install-x86_64 --token-stdin`。輸出中 `+` 開頭的是做了的變更，`!` 是警告；最後三行是驗收檢查。想先看會改什麼時加 `--dry-run`；第一次執行時 `--dry-run` 也要給 token，因為所有拒絕條件（包括 token 格式）都在規劃前檢查。

   安裝程式逐一檢查 firewalld 的每個 zone 與 policy（約二十個，runtime 與 permanent 各一次），`firewall-cmd` 每次呼叫在小主機上要 0.5 到 10 秒。2026-10-11 在 2 vCPU、3 GB 的 VM 上，乾淨主機的完整安裝約 18 分鐘（其中 k3s 與 k3s-selinux 約 10 分鐘），沒有變更的重跑約 6 到 9 分鐘；輸出停在同一行數分鐘是正常的。

5. **裝 Istio 與 chart。** 依 [deploy/istio/README.md](../istio/README.md) 安裝 Istio ambient，再依 [deploy/flux/README.md](../flux/README.md) 的「建立叢集」裝 Flux，由 Flux 部署 chart（SaaS 型態、`ingress.exposure: tunnel`；release `trustiot`，namespace `trustiot`）。入口的 NetworkPolicy 只放行 `ingress.tunnel.allowedSources`（預設 `10.42.0.1/32`，k3s 主機經 cni0 連 pod 的位址）連 80 與 15021；安裝程式只接受 k3s 預設的 Pod 網段 `10.42.0.0/16`，單節點時 cni0 就是 `10.42.0.1`，所以預設值不用改，cloudflared 的路由也只用 80（[deploy/chart/README.md](../chart/README.md)）。入口 gateway 的 Service 由 Istio 建立，名稱是 `<release>-ingress-istio`：

   ```sh
   sudo k3s kubectl -n trustiot get service trustiot-ingress-istio -o jsonpath='{.spec.clusterIP}{"\n"}'
   ```

6. **Cloudflare：設定 tunnel 的路由（Public hostnames）。**

   | 主機名稱 | 服務 |
   | --- | --- |
   | `<app>`、`<gateway>`、`<ota>` | `http://<入口 ClusterIP>:80` |
   | `<ssh>` | `ssh://localhost:22` |
   | 其他 | `http_status:404`（catch-all） |

   ClusterIP 在 Service 存在期間不會變；Service 被刪掉重建（例如重裝 chart）後要更新這三條路由，否則 Cloudflare 回 502。

7. **驗證。**

   ```sh
   curl -sS -o /dev/null -w '%{http_code}\n' https://<app>/healthz             # 200
   curl -sS -o /dev/null -w '%{http_code} %{redirect_url}\n' https://<ssh>/    # 302 到 Access 登入頁（沒有通過 Access）
   ssh -o ProxyCommand='cloudflared access ssh --hostname %h' <user>@<ssh>     # 通過 Access 後才連得上
   sudo ss -tlnp                                                              # 沒有 :80、:443
   sudo systemd-analyze security cloudflared                                  # 不高於 2.0
   ```

8. **（建議）關閉 sshd 的直連入口。** 確認經 Access 的 SSH 可用、雲端主控台也能登入後，把 `ssh` 從主機網卡所在的 firewalld zone 移除，SSH 就只剩經 tunnel 一條路：`sudo firewall-cmd --zone=<zone> --remove-service=ssh` 加上同樣帶 `--permanent` 的一次。安裝程式不自動做這一步，以免安裝途中切斷正在用的連線。

## 重跑與既有主機

- 再跑一次（不帶 token）只檢查並修正與預期不同的地方，例如有人在 firewalld 打開了 443，或 token 檔的權限被放寬。沒有差異時輸出「nothing changed」，服務不重啟。
- 在已經有其他設定的主機上，先用 `--dry-run` 看會改什麼。
- `/etc/rancher/k3s/config.yaml` 與預期不同時安裝程式會停下：`--reconfigure-k3s` 會把舊檔留成 `config.yaml.bak-<時間>`、寫入新設定並重啟 k3s（已在跑的 pod 不受影響，API server 會中斷數十秒）；`--skip-k3s` 則完全不碰 k3s。
- 主機上的 cloudflared 是從別處裝的（例如 GitHub release 的 RPM、其他套件庫）時，安裝程式在改任何東西之前停下。加上 `--reinstall-cloudflared` 再跑一次，安裝程式會寫好套件庫與金鑰，再以 `dnf reinstall --repo=cloudflared-stable cloudflared`（套件庫沒有同一版時改用 `dnf distro-sync`）換成 Cloudflare 簽章的套件，並再檢查一次來源。dnf 只換掉執行檔，正在跑的服務（包括別的 cloudflared unit）不受影響，下次重啟才用新的執行檔。
- 主機上已有不是安裝程式寫的 `cloudflared.service`（例如 `cloudflared service install` 建立的）時，安裝程式不覆寫它。如果維運 SSH 正經由它連線，按這個順序換掉，才不會把自己鎖在外面：
  1. 以另一個名稱安裝沙箱版：`… --unit cloudflared-trustiot`（同一條 tunnel 的 token，兩個 connector 同時在線）。
  2. 從另一個工作階段確認經 Access 的 SSH 與產品主機名稱都正常。
  3. `sudo systemctl disable --now cloudflared`，刪除舊的 unit 與它的 token 檔，`sudo systemctl daemon-reload`。
  4. 之後都以 `--unit cloudflared-trustiot` 執行安裝程式（或再以預設名稱重裝一次，確認後停掉 `cloudflared-trustiot`）。

## 更新 cloudflared

cloudflared 來自 Cloudflare 的官方套件庫，以 dnf 更新；不使用 cloudflared 自己的自動更新（unit 帶 `--no-autoupdate`）。

```sh
sudo dnf upgrade --repo=cloudflared-stable cloudflared
sudo systemctl restart cloudflared
journalctl -u cloudflared -n 50 --no-pager      # 看到 "Registered tunnel connection" 共 4 條
cloudflared --version
```

重啟時 cloudflared 先等進行中的請求結束（最多 30 秒），然後斷線、由新程序重新連上。2026-10-11 在實驗機量到：沒有進行中的請求時，從斷線到新連線註冊約 0.5 秒，每 0.5 秒一次的健康檢查只有一次回 503；可行性測試量到約 3.5 秒的 530（[可行性測試結果（2026-10-10）](../../docs/feasibility/2026-10-10-edge-poc.md)）。閘道器靠本地緩衝補送，資料不丟，但 WebSocket、即時圖與 SSH 連線會斷，請在維護時段做。更新後有問題時以 `sudo dnf downgrade --repo=cloudflared-stable cloudflared-<版本>` 退回，再重啟。dnf 換掉執行檔不會自動重啟服務，沒重啟前仍跑舊版本。

cloudflared 套件的 `%postun` 不分升級或移除，一律刪掉 `/usr/local/bin/cloudflared`（指向 `/usr/bin/cloudflared` 的連結）與 `/usr/local/etc/cloudflared/.installedFromPackageManager`，所以每次 `dnf upgrade` 或 `dnf reinstall` 後這兩個都會消失（2026-10-11 在實驗機確認）。安裝程式寫的 unit 用 `/usr/bin/cloudflared`、帶 `--no-autoupdate`，不受影響；自己的腳本若用 `/usr/local/bin/cloudflared`，請改用 `/usr/bin/cloudflared`。

## 輪替 tunnel token

token 外洩、人員異動，或定期輪替時：

1. **在 Cloudflare 產生新 token。** Networks → Tunnels → 選 tunnel → Rotate token（或 Refresh token）。API：`PATCH /accounts/{account_id}/cfd_tunnel/{tunnel_id}`，body `{"tunnel_secret": "<至少 32 bytes 隨機值的 base64>"}`，再以 `GET /accounts/{account_id}/cfd_tunnel/{tunnel_id}/token` 取得新 token。輪替後舊 token 不能再建立新連線，已在線的 connector 維持到重啟為止，所以這一步之後要**馬上**做第 2 步：期間只要有一條連線斷掉重連就會失敗。
2. **把新 token 交給主機。** 同安裝第 4 步，例如：

   ```sh
   sudo sh -c 'umask 077; cat > /root/tunnel-token'
   sudo ./trustiot-host-install-x86_64 --token-file /root/tunnel-token
   sudo shred -u /root/tunnel-token
   ```

   安裝程式換掉 `/etc/credstore/cloudflared.tunnel-token` 並重啟 cloudflared（中斷與更新時相同），等新連線建立才結束；新 token 被拒絕時以非零結束並指向 `journalctl -u cloudflared`。2026-10-11 在實驗機以 API 輪替後交給安裝程式：輪替到重啟之間既有連線照常服務，新 token 約 0.5 秒內建立新連線，之後健康檢查回 200。
3. **token 外洩時**另外中斷所有既有連線，讓拿到舊 token 的人已建立的 connector 也斷線：`DELETE /accounts/{account_id}/cfd_tunnel/{tunnel_id}/connections`，之後主機上的 cloudflared 會以新 token 自動重連。
4. 確認：`curl https://<app>/healthz` 回 200，經 Access 的 SSH 可連。

## 疑難排解

| 現象 | 原因與處理 |
| --- | --- |
| 安裝程式停在 `systemctl start` 後失敗 | cloudflared 連不上 Cloudflare 或 token 被拒；`journalctl -u cloudflared` |
| 產品主機名稱回 502 | 路由的 ClusterIP 不對（Service 重建過）或入口 pod 沒起來；重新查 ClusterIP 並更新路由 |
| 回 530 或 503 | 沒有 connector 在線：cloudflared 停了或正在重啟 |
| 安裝程式好幾分鐘沒有新的輸出 | 正在逐一檢查 firewalld 的 zone 與 policy，或在裝 k3s、k3s-selinux；見「安裝」第 4 步的時間 |
| 安裝程式說 `has no "<欄位>" field` | `firewall-cmd --info-zone` 的輸出格式與預期不同（新版 firewalld 改了格式）；安裝程式不猜，停下。回報並附上 `firewall-cmd --info-zone=public` 的輸出 |
| 日誌有 ICMP proxy 或 receive buffer 的警告 | 沙箱拿掉了 raw socket 與調整 socket 緩衝區的權限，預期中，不影響運作 |
| 安裝程式說 `differs from the config this installer writes` | 見「重跑與既有主機」 |

## 檔案

| 路徑 | 內容 |
| --- | --- |
| `trustiot-host-install.ts` | 指令列進入點 |
| `src/desired.ts` | 主機的目標狀態：版本、釘選的雜湊與金鑰、k3s 設定、套件庫、cloudflared unit |
| `src/install.ts` | 各步驟與驗收檢查 |
| `src/system.ts` | 主機操作（指令一律以陣列執行，不經 shell）；測試以 `tests/fake-host.ts` 模擬 |
| `src/token.ts` | token 格式檢查，錯誤訊息不引用 token |
| `assets/cloudflare-packaging.asc` | Cloudflare 的套件簽章公鑰（取自 `https://pkg.cloudflare.com/cloudflare-ascii-pubkey.gpg`，測試核對指紋） |
| `build.ts` | 編譯兩個架構的執行檔 |

升級 k3s 的釘選版本時，同時改 `K3S_VERSION`、`K3S_INSTALLER_URL` 與 `K3S_INSTALLER_SHA256`（`curl -fsSL <url> | sha256sum`），並與 chart 測試的 Kubernetes 版本一致。Cloudflare 更換套件簽章金鑰時，換掉 `assets/` 的公鑰與 `CLOUDFLARE_KEY_FINGERPRINT`，否則 dnf 會拒絕新套件（安全的失敗方式）。
