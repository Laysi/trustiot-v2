# 展示環境 runbook

展示切片（[里程碑](../roadmap/milestones.md)「展示切片」，epic Laysi/trustiot-v2#456）的展示環境（Laysi/trustiot-v2#457）跑在展示機（lab 的 Mac mini M1，Fedora Asahi Remix、k3s、aarch64）。本文目前寫對外的入口：展示用的 Cloudflare Tunnel、展示機上的 connector 與 app 主機名稱前的 Cloudflare Access（Laysi/trustiot-v2#466），以及外寄郵件與 Anthropic 金鑰（Laysi/trustiot-v2#468）。本文也寫展示環境的部署、打開切片的服務、換 chart 版本、重設展示資料、金鑰輪替，以及與 lab e2e 共用叢集的注意事項（Laysi/trustiot-v2#469）。

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
- **路由**：分兩段。入口還沒部署時，三個主機名稱都回 `http_status:418`：這是 connector 自己的回應，用來證明請求穿過 Cloudflare 到了展示機，而且沒有被 Access 或機器人防護攔下。展示環境的入口 Service 出現後，三條路由改為 `http://<入口 ClusterIP>:80`（主機解析不了叢集 DNS）。最後一條 catch-all 回 404。這個 ClusterIP 是展示叢集 values 固定的位址（`deploy/flux/clusters/demo/trustiot/values-demo.yaml` 的 `ingress.tunnel.clusterIP`，在 Service 網段 `10.43.0.0/16` 的靜態區段，Laysi/trustiot-v2#478），不是 Service 當下剛好拿到的位址。但路由不照 values-demo.yaml 手寫：origin 只取自 `route-origin` 的輸出。`route-origin` 是切換路由前的關卡（Laysi/trustiot-v2#483），只讀叢集（`kubectl get`），不取 lab 鎖：

  ```sh
  bun scripts/demo/tunnel.ts route-origin --ssh-host "$DEMO_SSH_HOST"
  ```

  它比對三個位址：工作站 checkout 的 commit 裡 `values-demo.yaml` 的 `ingress.tunnel.clusterIP`（這個檔有未 commit 的變更時不算數）、叢集裡 ConfigMap `trustiot-ingress-options` 的 `service` overlay 的 `clusterIP`，以及 Istio 為 Gateway `trustiot-ingress` 產生的 Service 的 `spec.clusterIP`。這個 Service 以 label `gateway.networking.k8s.io/gateway-name=trustiot-ingress` 找，與 nightly 的 smoke 相同，必須恰好一個，而且確實是 Istio 為這個 Gateway 產生的：名稱是 `trustiot-ingress-istio`、ownerReference 是 Gateway `trustiot-ingress`、類型是 `ClusterIP`、有 80 埠，`clusterIPs` 的第一個就是它的 IPv4 ClusterIP。三者相同之外，位址還必須在叢集 Service 網段的靜態區段內（讀 `kubectl get servicecidr kubernetes`，必須恰好一個 IPv4 網段；讀不到也算不通過）。全部通過時以 0 結束，最後一行印出 `the stage-2 routes' origin: http://<位址>:80`；否則印出 `FAIL` 與原因，不印 origin，以非零結束。原因只寫哪個指令失敗與它的結束碼，不轉貼 ssh、kubectl 或 git 的輸出，所以比對的那幾行與 `route-origin` 的 JSON 紀錄可以直接放進測試報告；要看錯誤細節，在展示機上自己執行訊息裡的那個指令。其他的 `ERROR` 與 `WARNING` 行（例如讀維運 tunnel 的狀態時 ssh 失敗）可能帶 ssh 自己的錯誤訊息，其中有展示機的位址或 ssh 的主機別名，貼出之前先檢查並遮掉。切換的步驟：

  1. 在審查過的 commit 的乾淨 checkout，緊接在切換之前執行 `route-origin`。
  2. 只有它以 0 結束才切換。指向入口的每一條路由，origin 都照它最後一行印出的值設定；不要從 values-demo.yaml 或 `status` 抄。
  3. 以 Cloudflare API 切換之後，馬上讀回 tunnel 的設定，確認每一條指向入口的路由，`service` 都與 `route-origin` 最後一行印出的 origin 逐字相同（Laysi/trustiot-v2#469）。`check --stage 2` 不讀路由設定；第 2 段裡，兩條略過 Access 的路徑得到 Cloudflare 的 502（connector 連不上 origin）也算通過（WebSocket 的佐證在 Laysi/trustiot-v2#473），所以它不能證明路由用的是這個 origin。
  4. 接著馬上執行 `check --stage 2`，中間不做其他步驟。`check --stage 2` 在送出任何請求之前會再跑一次同一個關卡（同一段程式），不通過時不探測就以非零結束，紀錄寫 `probed: false`。

  讀回的設定不符，或 `check --stage 2` 失敗（關卡不通過或探測失敗）時，把指向入口的路由改回 `http_status:418`，查清楚再重來。

  第 1 段的 `check` 不跑這個關卡，那時路由還是 connector 自己的 418。`status` 也印出同樣的比對（`OK` 或 `FAIL`），但它只是報告：照樣以 0 結束，也不印 origin，不能取代 `route-origin`。關卡不通過時不切換路由，已切換的先改回 `http_status:418`，再依原因處理：
  - values 沒有位址：工作站的 checkout 早於設定位址的 Laysi/trustiot-v2#480，或不是審查過的 commit。
  - ConfigMap 沒有位址，或與 values 不同：叢集部署的不是這個 commit（Flux 還沒套用新的 values，或工作站的 checkout 不是部署的那個 commit）。等 Flux 套用，或改用部署的 commit 的 checkout，再比對一次。
  - ConfigMap 與 Service 不同：Service 在設定位址之前就建立了，或改位址只做到一半。這時 Istio 不再更新這個 Service，Gateway 的狀態也看不出來（`deploy/chart/README.md` 的「固定的入口 ClusterIP」），照該節「改位址」的第 3 到 5 步重建 Service。
  - 沒有 Service、不只一個，或那個 Service 不是 Istio 為 Gateway 產生的（名稱、ownerReference、類型或埠不對）：入口還沒部署好（Gateway 不是 `Programmed`），或有別的 Service 帶著同一個 label。查清楚之前，路由不指向任何一個。
  - 位址不在靜態區段，或讀不到靜態區段：入口 Service 重建的空檔，靜態區段外的位址可能被任何先建立的 Service 拿走。照 README 的「位址怎麼選」在靜態區段裡選新位址，再照「改位址」換；讀不到 ServiceCIDR 時，先在展示機上確認 `kubectl get servicecidr kubernetes`。

  Service 被刪掉重建後仍拿回同一個位址，路由不用改，舊位址也不會被別的 Service 拿走而經 gateway 主機名稱那兩條略過 Access 的路徑（下面「只有閘道器的固定路徑略過 Access」）公開。要換位址時，路由不能指向已釋放的位址：先把指向入口的路由改回 `http_status:418`，再照 `deploy/chart/README.md` 的「固定的入口 ClusterIP」的「改位址」改 values 並重建 Service，等新的 Service 拿到新位址、Gateway 為 `Programmed` 之後，照上面的步驟以 `route-origin` 的輸出把路由改到新位址（README 的步驟裡，改回 418 排在 values 部署之後、刪除 Service 之前，縮短斷線時間）。
- **三個主機名稱都在 Access 後面**：切片延後了限流、停權、AI 額度、分析護欄與注入題庫，展示期間以 Access 補上（[里程碑](../roadmap/milestones.md)的延後項目表）。chart 依主機名稱掛路由（`deploy/chart/templates/_ingress.tpl`），主機名稱來自 CI 讀不到的 SOPS 加密 values；把 chart values 裡 app 與 gateway 的主機名稱對調的變更在 CI 仍會通過，而 tunnel 把三個主機名稱都送到同一個入口，對調後 app（含登入頁）就會出現在沒有 Access 的主機名稱上。所以 Access 不能只放在 app 主機名稱：一個 self-hosted application 涵蓋三個主機名稱，只允許 Google 登入，policy 只放行一個 Access group（受邀名單），並另以 `require` 要求登入方式是那個 Google identity provider。application 開啟 binding cookie（`enable_binding_cookie`，`CF_Binding`）：session cookie 被複製到別的瀏覽器時不能用。依 Cloudflare 的文件，這個設定不適合非瀏覽器的工具；需要 session 的請求都來自瀏覽器（app 本身的 API 與 WebSocket 都由同一個瀏覽器帶著 cookie 發出），閘道器只用下面略過 Access 的路徑，所以開啟。唯一不是瀏覽器的 client 是產線工作站工具：它以 service token 通過（「Access 名單」的「工作站的 service token」，Laysi/trustiot-v2#619），每個請求都帶 service token 的標頭，不依賴 `CF_Authorization` cookie。
- **只有閘道器的固定路徑略過 Access**：閘道器過不了 Access，靠內層 mTLS（[部署與可搬移性](../architecture/deployment.md)）。略過的路徑是 chart 不需要 cookie 的 gateway 路徑中的 `certRenewal` 與 `mqtt`（`ingress.publicPaths`，都是完整路徑比對）：gateway 主機名稱的 `/certs/renew` 與 `/mqtt`，各以一個 self-hosted application 加上 Bypass policy。chart 的 `rootCrl`（`/certs/root-crl`，閘道器取根 CRL，Laysi/trustiot-v2#831）暫時還不略過，留在 Access 後面。閘道器端已會取用（Laysi/trustiot-v2#563，2026-11-01；請求被 Access 擋下時 agent 記一筆 WARN、以退避重試，保留手上的根 CRL），開放時照下面的方式加上 Bypass application、tunnel 規則與 `BYPASS_PATHS`。開放的前提和換發相同（「換發端點上線」第 1 步）：先把 `/certs/root-crl` 加進「`/mqtt` 的速率限制」那條規則的路徑集合（同一條規則以 IP 合併計數；閘道器很少取根 CRL，不影響門檻），讀回 ruleset，再從工作站在 10 秒內連續送 41 次 `GET /certs/root-crl`，第 41 次起要得到 Cloudflare 的 429（`server: cloudflare`），確認後才加 Bypass。chart 目前沒有給 ota 主機名稱任何不需要 cookie 的路徑，所以 ota 主機名稱整個在 Access 後面；OTA client 的路徑加進 chart 的表時，同時加上它的 Bypass application 與下面的 tunnel 規則，並更新 `scripts/demo/tunnel.ts` 的 `BYPASS_PATHS`（測試核對它與 chart 的表一致）。Cloudflare 的 Access application 帶路徑時也涵蓋它下面的子路徑，同一個請求符合多個 application 時以路徑較具體的為準（[Application paths](https://developers.cloudflare.com/cloudflare-one/access-controls/policies/app-paths/)），所以 `/mqtt/x` 也會略過 Access；真正把略過限縮成完整路徑的是 tunnel：路由以錨定的路徑規則（`^/mqtt$`、`^/certs/renew$`，cloudflared 以 Go 的 regexp 比對路徑，不含 query string）把這兩個路徑送到入口而不要求 JWT，同一主機名稱的其他路徑與 app、ota 主機名稱一律設 `originRequest.access`：connector 也驗證 Access 的 JWT，沒有 JWT 的請求回 403。Access application 被誤刪或改壞時，這些主機名稱也是回 403 而不是變成公開。略過的路徑都不是 app 的路徑，也不是 app 路徑的前綴。閘道器的 WebSocket client 不送 User-Agent，所以以 configuration rule 對 gateway 主機名稱關掉 Browser Integrity Check。zone 的 Bot Fight Mode 在 Free 方案不能只對單一主機名稱略過；2026-10-20 決定先不動，以下方的 `check` 實測，被 challenge 或擋下時再由使用者決定是否關閉整個 zone 的設定。

## 部署

2026-10-22 的狀態：平台元件已與 repo 對齊（`scripts/demo/platform.ts`，Laysi/trustiot-v2#465）；Flux 已裝在展示叢集（Laysi/trustiot-v2#467，[測試報告](../test-reports/2026-10-22-467-demo-flux-install.md)），HelmRelease 釘在 chart `0.0.0-rc.10`，打開的服務是 `identity` 與 `assets`；tunnel 的路由已照「架構」的「路由」切到入口的固定 ClusterIP（第 2 段，Laysi/trustiot-v2#469）。2026-10-28 起 main 把展示叢集釘在 chart `0.0.0-rc.11`（第一個含 certs 的版本），換版前先對齊了 istiod（Laysi/trustiot-v2#619，見「換 chart 版本」）。2026-10-30 起釘在 `0.0.0-rc.12`（含 certs 的換發端點，以及 ingest、gateway-ops 的映像），換版前 istiod 的 values 與 rc.11 相同、不必對齊（Laysi/trustiot-v2#795）。之後展示環境的內容都由 main 決定：開 PR 改 `deploy/flux/clusters/demo/`，合併後 Flux 套用（GitRepository 每分鐘讀一次 main）。不要在叢集上直接改 Flux 管理的物件，下一次同步會改回去（[deploy/flux/README.md](../../deploy/flux/README.md)「日常操作」）。

會寫入展示機或叢集的步驟，都由有展示機 SSH 權限的維運人員在持有 lab 鎖（展示機的 `~/e2e.lock`）時執行。腳本照 [scripts/README.md](../../scripts/README.md)「展示機（`demo/`）」的規則先印出計畫與確認碼，加上 `--apply --confirm <確認碼>` 才動手；本文寫成手動指令的步驟（重設展示資料、輪替 deploy key 與 age 金鑰）也要先取得 lab 鎖，鎖的擁有者寫成 `Laysi/trustiot-v2#<issue> <UTC 時間> demo <步驟>`，`<issue>` 是這次工作的 issue。

從頭部署時（例如以 `flux.ts remove` 移除之後）的順序：

1. connector 與主機基線：本文「安裝 connector」，路由先維持 `http_status:418`。
2. 清除 PoC 與對齊平台元件：[scripts/README.md](../../scripts/README.md)「清除 PoC 與平台對齊的順序」。
3. 叢集的 age 金鑰、pin PR、GHCR 拉取 token 與私有 values：[deploy/flux/README.md](../../deploy/flux/README.md)「展示叢集的 age 金鑰」與「在展示機安裝 Flux」的「順序」；移除後再裝的差異在同一節「移除」的最後兩段。
4. deploy key 與 Flux：同一節的「步驟」（`flux.ts deploy-key`、`install`）。
5. 路由切到入口：「架構」的「路由」的切換步驟（`route-origin`、讀回路由設定、`check --stage 2`）。

隨時可以看的狀態（只讀，不取 lab 鎖）：

```sh
bun scripts/demo/flux.ts status --ssh-host "$DEMO_SSH_HOST"         # Flux 物件的 revision、部署中的 chart、Deployment 與 pod
bun scripts/demo/tunnel.ts status --ssh-host "$DEMO_SSH_HOST"       # connector、維運 tunnel 與入口位址的比對
bun scripts/demo/tunnel.ts route-origin --ssh-host "$DEMO_SSH_HOST" # 路由該指向的 origin
```

## 打開切片的服務

切片的服務做完、進了某一版 chart 之後，照以下順序在展示環境打開它：

1. **chart 有這個服務**：部署中的版本（`deploy/flux/clusters/demo/trustiot/chart.yaml` 的 `ref.tag`）的 git tag 裡，`deploy/chart/values.yaml` 有 `services.<服務>`、`deploy/images/` 有這個服務的目錄（`git show v<版本>:deploy/chart/values.yaml`）。沒有時先照下一節「換 chart 版本」。
2. **服務自己的前提**：讀服務的 README 與 issue，找出啟用前要做的事（例如 certs 的 CA 儀式，本文「簽發 CA 的儀式」）、它要的 Secret（例如寄信的 `trustiot-smtp`、AI 閘道的 `trustiot-anthropic`，產生方式在 [deploy/flux/README.md](../../deploy/flux/README.md)「展示叢集」的 SOPS 表），以及它新增的不需要 cookie 的公開路徑（照「Access 名單」的「新的公開路徑」，Cloudflare 與 `scripts/demo/tunnel.ts` 一起改）。
3. **記憶體**：在展示機上 `free -h` 確認放得下，也看「與 lab e2e 共用叢集」的記憶體一項。
4. **PR**：照 [deploy/flux/README.md](../../deploy/flux/README.md)「日常變更」的「打開切片的服務」改 `values-demo.yaml`，Secret 的 SOPS 檔一起加進來並列進 `kustomization.yaml`。`bun test deploy/flux` 以 HEAD 的 chart 渲染，不是部署中的版本；兩者的 values 欄位不同時，另以部署中的版本 `helm template` 確認。審查後合併。
5. **確認**：用 Postgres 的服務會改變 migration Job 的 spec，Job 重跑，每個服務跟著滾動重啟一次（[deploy/chart/README.md](../../deploy/chart/README.md)「migration Job」的「Job 何時再跑」），展示環境短暫中斷，所以不要排在展示前。以 `flux.ts status` 確認 HelmRelease 最新一筆是 `deployed`、每個 Deployment available，再跑 `tunnel.ts check --stage 2`，以受邀的帳號在瀏覽器走一次這個服務的功能。
6. **退回**：revert 這個 PR。服務關掉後下一次 Job 讓它的登入角色 `NOLOGIN`，資料與它的密碼 Secret 留著（deploy/chart/README.md「migration Job」的「產生的密碼」）。

### 打開閘道器的資料路徑

Laysi/trustiot-v2#795：MQTT broker、mqtt-bridge、ingest 與 gateway-ops 一起打開（值與理由在 [deploy/flux/README.md](../../deploy/flux/README.md)「打開閘道器的資料路徑」）。照上面的順序，另外：

1. **前提**：
   - 部署中的 chart 比 `0.0.0-rc.11` 晚（`bun test deploy/flux` 也檢查）。換到那一版是另一個 PR，先合併、確認穩定（「換 chart 版本」）；那一版含換發端點，所以先完成「換發端點上線」。
   - Cloudflare 的 `/mqtt` 速率限制已套用（「`/mqtt` 的速率限制」），讀回 ruleset 確認：mqtt-bridge 一部署，`/mqtt` 就經 Bypass 對網際網路開放。
   - 記憶體：這一步新增 broker（requests 64Mi，加上 trust sidecar）、mqtt-bridge、ingest 與 gateway-ops（各 requests 32Mi、limits 256Mi）。打開前在展示機上 `free -m`，量測結果記在 issue。
   - 本機的信任：入口的 NetworkPolicy 放行節點自己（`ingress.tunnel.allowedSources`），所以展示機上任何本機行程都能直接連入口、自帶 `Host` 與 `CF-Connecting-IP`，`/mqtt` 與 `/certs/renew` 在本機繞得過 Cloudflare 的速率限制與 mqtt-bridge 的每來源上限。展示機的本機使用者等同受信任（與 SaaS 主機相同的假設，deploy/chart/README.md「入口 gateway」的「入口 pod 的埠」）。
2. **合併後確認**（只讀，不取 lab 鎖）：
   - `flux.ts status`：HelmRelease 最新一筆是 `deployed`；`trustiot-mqtt-broker-0`、`trustiot-mqtt-bridge-*`、`trustiot-ingest-*`、`trustiot-gateway-ops-*` 與重新滾動的 `trustiot-certs-*` 都 Running 且 ready，migration Job 是 `Succeeded`。新的 ingest、gateway-ops 與 certs pod 先停在 `ContainerCreating`（等 broker 的 init container `accounts` 寫入密碼）是正常的，broker 起來後自己繼續；停超過幾分鐘時照 deploy/chart/README.md「服務的帳號」看 `accounts` 的日誌。
   - broker 的伺服器憑證：憑證 CronJob 每 15 分鐘執行一次，第一次執行填入 Secret（Job 的日誌 `wrote a new server certificate and key for the broker`），trust sidecar 讓 broker 重啟一次帶上 8883。確認 `kubectl -n trustiot logs trustiot-mqtt-broker-0 -c mosquitto` 有 8883 的 listen socket，`-c trust` 沒有錯誤。CRL：`-c trust` 的日誌沒有 `does not reload the trust files` 或 CRL 的錯誤；以 port-forward 讀 certs 的 `/internal/certs/v1/crl.pem`（「簽發 CA 的儀式」第 5 步的做法）也可以。
   - 閘道器：在 lab 的閘道器上 `sudo journalctl -u trustiot-agent --since <合併時間>`（只讀）。agent 在自己的退避間隔內（最多 60 秒）重連，依序出現 `connected to the broker`（內層 mTLS 完成、CONNACK 0）；閘道器還沒綁定時接著是 `the broker refused the online status: 0x87 (not authorized)`，這是預期的：還沒綁定的閘道器連得上 broker，但不能發布或訂閱（services/certs/README.md「broker 權限」）。mqtt-bridge 的日誌同時有 `relaying a gateway connection`。
   - 未登錄的憑證連不上：在展示機上以 port-forward 連 broker 的 8883（不經 mesh，只到 broker 自己的 TLS 檢查），用一張當場產生、自簽的用戶端憑證，以及不帶憑證，各握手一次，兩次都要被 broker 以 TLS alert 拒絕；做完關掉 port-forward、刪掉暫存檔：

     ```sh
     ssh "$DEMO_SSH_HOST" 'set -eu
       d=$(mktemp -d "${XDG_RUNTIME_DIR:?}/broker-check.XXXXXX")
       pf=""
       stop() { if [ -n "$pf" ]; then sudo -n kill "$pf" 2>/dev/null || true; wait "$pf" 2>/dev/null || true; pf=""; fi; }
       trap "stop; rm -rf \"\$d\"" EXIT
       closed() { if ss -ltnH "sport = :18883" | grep -q .; then echo "FAIL: something still listens on 18883" >&2; exit 1; fi; }
       # handshake <name> [s_client options]: one handshake through its own port-forward, closed afterwards.
       handshake() {
         name=$1; shift
         closed
         # sudo runs it as root, so only root can stop it; timeout ends it even if stop never runs.
         sudo -n timeout 120 k3s kubectl -n trustiot port-forward pod/trustiot-mqtt-broker-0 18883:8883 >"$d/pf-$name.log" 2>&1 &
         pf=$!
         for i in $(seq 1 60); do grep -q "Forwarding from" "$d/pf-$name.log" && break; sleep 1; done
         grep -q "Forwarding from" "$d/pf-$name.log" || { echo "the port-forward did not start" >&2; exit 1; }
         timeout 20 openssl s_client -connect 127.0.0.1:18883 -tls1_3 -ign_eof -servername mqtt-broker.trustiot.internal "$@" </dev/null >"$d/$name.txt" 2>&1 || true
         grep -E "alert|^CONNECTED" "$d/$name.txt" || true
         stop
         closed
       }
       openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=tg-unregistered-0001" -days 1 -keyout "$d/k.pem" -out "$d/c.pem" 2>/dev/null
       echo "--- a self-signed client certificate"
       handshake self-signed -cert "$d/c.pem" -key "$d/k.pem"
       echo "--- no client certificate"
       handshake none
       grep -q "alert unknown ca" "$d/self-signed.txt" || { echo "FAIL: the broker did not refuse the self-signed certificate" >&2; exit 1; }
       grep -q "alert certificate required" "$d/none.txt" || { echo "FAIL: the broker did not refuse a connection without a certificate" >&2; exit 1; }
       echo "PASS: both refused; the port-forward is closed"'
     ```

     最後一行是 `PASS` 才算通過：兩次都先 `CONNECTED`（TCP 連上），接著是 broker 回的 TLS alert，自簽的憑證是 `alert unknown ca`（OpenSSL 的 alert 48），沒有憑證是 `alert certificate required`（116），而且 port-forward 已經結束、18883 沒有 listener。每次握手各開一個 port-forward：broker 拒絕握手後重設連線，`kubectl port-forward` 接著以 `error: lost connection to pod` 結束，同一個 port-forward 上的第二次握手只會得到 `Connection refused`（Laysi/trustiot-v2#795 第一次實測時遇到）。port-forward 以 root 執行（sudo），一般使用者的 `kill` 停不了它，所以以 `sudo -n kill` 停，並以 `timeout 120` 保證它自己結束：它把展示機的 loopback 直接接到 broker 的 8883，繞過 mesh 與 mqtt-bridge 的限制（broker 的 mTLS 仍然把關），不能留著。TLS 1.3 的用戶端憑證在用戶端送完握手之後才被檢查，所以要 `-ign_eof` 讓 `s_client` 等 broker 的回應。這裡不給根憑證，`s_client` 不檢查 broker 的憑證，結果只看 broker 是否拒絕。登錄過的憑證連得上，由上一項閘道器的日誌證明。
   - 記憶體：`free -m` 與 `sudo k3s kubectl top pod -n trustiot`，結果記在 issue，放得下才算完成。
   - `tunnel.ts check --stage 2`：`/mqtt` 的升級現在由 mqtt-bridge 回 101（curl 等到 `--max-time` 才結束，算通過）。
3. **退回**：revert 打開的 PR。要把 chart 退回 `0.0.0-rc.11` 或更早時，先 revert 打開的 PR、確認套用，再 revert 換版的 PR：順序反過來時 `bun test deploy/flux` 失敗，而 GitHub 不會擋下紅燈的合併，合併下去 Helm 會為沒有映像的 ingest 與 gateway-ops 等到逾時後 rollback 並重試，展示環境中斷好幾次。打開這個 PR 本身也一樣：合併前確認它最新 commit 的 CI 是在含新版 pin 的 main 上通過的。mqtt-bridge 關掉後 `/mqtt` 不再路由，閘道器回到 404 退避；broker 的 PVC、帳號與伺服器憑證的 Secret 留著（`keep`），之後再打開時接回。

### 打開 SPA（services.web）

Laysi/trustiot-v2#961：`services.web` 打開後，app 主機名稱上 `/api`、`/auth`（與它們底下的路徑）以外的路徑都由 `web` 回應，不經 cookie 檢查，這條路由也移除 `Cookie` 與身分標頭（[deploy/chart/README.md](../../deploy/chart/README.md)「SPA 的靜態檔」）。這是第一次在任何環境打開 `web`。Laysi/trustiot-v2#498 移到 Laysi/trustiot-v2#961 的安全關卡要求：告知展示對象之前，經展示環境的真實入口（Cloudflare、Access、tunnel、入口 gateway、istiod 的 ext_authz provider 與身分服務的 cookie 檢查）確認兩件事：路由，以及帶 cookie 的跨站請求被擋下。跨站檢查要 istiod 把 `Origin` 與 `Sec-Fetch-Site` 交給 cookie 檢查（[deploy/istio/README.md](../../deploy/istio/README.md)「ext_authz provider」）；沒有對齊時跨站請求一律放行，不報錯也不留紀錄，所以第 2 步排在合併之前。values 的寫法在 [deploy/flux/README.md](../../deploy/flux/README.md)「日常變更」的「打開 SPA」。

1. **前提**（合併前）：
   - chart：部署中的版本要比 `0.0.0-rc.10` 晚：`web` 的映像從 `0.0.0-rc.11` 起才有，身分服務也從那一版起擋跨站請求（`bun test deploy/flux` 檢查）。2026-11-02 展示叢集釘在 `0.0.0-rc.13`，它的 chart（`templates/`、`values.yaml`、schema）與當時的 main 相同。
   - 記憶體：`web` 用 chart 預設的 requests 32Mi、limits 256Mi，arm64 實測的峰值約 15 MB（[web 映像的量測](../test-reports/2026-11-01-944-web-image-arm64.md)）。打開前在展示機上 `free -m`，並以 `sudo k3s kubectl describe node` 的「Allocated resources」確認 requests 還有餘裕（2026-11-02 的數字在 [Laysi/trustiot-v2#961 的測試報告](../test-reports/2026-11-02-961-demo-web.md)）。
   - 影響：多一個 Deployment `trustiot-web` 與它的 Service、NetworkPolicy、AuthorizationPolicy；入口的 HTTPRoute 與 cookie 檢查的 AuthorizationPolicy 改變，collector 的 NetworkPolicy 與 AuthorizationPolicy 放行 `web` 的遙測。migration Job 的 spec 不變（Job 不重跑），其他服務不重啟；不用 Secret；Cloudflare 與 tunnel 不必改（app 主機名稱的每個路徑本來就經 Access 送到入口），`tunnel.ts check --stage 2` 的預期也不變（沒有 Access session 的請求仍被 Access 擋下）。
   - 檢查用的身分：持有工作站的操作員有平台管理員的 session（「平台管理員的登入確認」第 1、2 步）與 Access 的 service token 標頭檔（「Access 名單」的「工作站的 service token」）。第 5 步的腳本以它們通過 Access 與 cookie 檢查；它讀的設備池只有平台管理員讀得到。
2. **確認 istiod**（只讀，不取 lab 鎖；合併前，在要合併的 commit 的乾淨 checkout）：

   ```sh
   ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n istio-system get configmap istio -o jsonpath="{.data.mesh}"' \
     | bun scripts/demo/web-check.ts istiod
   bun scripts/demo/platform.ts --ssh-host "$DEMO_SSH_HOST"    # 只讀的 dry run
   ```

   第一個指令讀 istiod 正在用的 mesh 設定，與 checkout 的 `deploy/istio/istiod-values.yaml` 比對路徑正規化，以及 provider `trustiot-identity` 的 service、port、pathPrefix、failOpen、timeout、statusOnError 與兩個標頭清單（以名稱的集合比對）。最後一行是 `PASS` 才繼續；輸出只有設定名稱、叢集內的服務名稱、埠與標頭名稱，可以直接放進測試報告。`FAIL` 時列出不同的設定，例如 repo 的 `includeRequestHeadersInCheck` 多了 istiod 還沒有的標頭：照「換 chart 版本」第 2 步以 `platform.ts` 對齊 istiod（會寫入整個叢集共用的 istiod，由維運人員持有 lab 鎖執行），再從這一步重來。第二個指令是同一節的 dry run，計畫裡沒有 istiod values 的差異。
3. **基準**（可選，合併前）：照第 5 步執行一次檢查腳本。`web` 還沒打開，所以 SPA 的四項（`/`、深連結、帶 cookie 的 `/`、`/static/*`）`FAIL`，其餘九項應該 `PASS`。這一次先證明 service token、session 與展示環境現有的跨站檢查都正常；跨站的任何一項 `FAIL` 時不合併，先查原因。
4. **合併**：PR 審查後合併。GitRepository 每分鐘讀一次 main；`bun scripts/demo/flux.ts status --ssh-host "$DEMO_SSH_HOST"` 的根 Kustomization 停在合併的 commit、HelmRelease 最新一筆是 `deployed`、`trustiot-web` 1/1 available。
5. **經真實入口檢查**（合併並部署之後、告知展示對象之前；在持有工作站的操作員的 Linux 帳號、以 bash 執行）：

   ```sh
   : "${DEMO_APP_HOST:?set DEMO_APP_HOST to the app host name, in lower case}"
   O="$XDG_RUNTIME_DIR/demo-app-origin"
   printf 'https://%s\n' "$DEMO_APP_HOST" > "$O"
   bun scripts/demo/web-check.ts --expected-origin-file "$O"
   rm -f "$O"
   ```

   腳本只對 `$O` 寫的 origin 送請求。session 與 service token 從檔案讀，不從命令列：`--session-file` 預設是工作站工具的 `$XDG_RUNTIME_DIR/trustiot-workstation/session`，`--access-headers-file` 預設是 `~/.config/trustiot-workstation/access-headers`；兩個檔都要是執行者自己的一般檔案、權限 0600。session 檔的 `origin=` 不是 `$O` 的 origin 時拒絕（結束碼 2），一個請求都不送。每個請求都帶 service token；寫入只送到 GET 限定的設備池（通過 cookie 檢查後資產服務回 405，什麼都不改），登入路徑只送一個沒有本文的 POST（身分服務在讀任何東西之前就以 400 拒絕）。資產服務自己不看 `Origin` 與 `Sec-Fetch-Site`，所以那裡的 403 `security.cross_site_request` 只會來自 cookie 檢查，也就證明 istiod 把兩個標頭交給了它。共 13 項：

   | 項目 | 預期 |
   | --- | --- |
   | `GET /`，不帶 cookie | 200，`web` 的 `index.html`（只有 `web` 送的 CSP 與 `X-Frame-Options: DENY`） |
   | 深連結 `GET /devices/<ID>/settings`，不帶 cookie | 200，與 `/` 相同的 `index.html` |
   | `GET /`，帶 session cookie | 200，相同的 `index.html` |
   | `index.html` 載入的一個 `/static/*` 檔案，不帶 cookie | 200，`web` 回應，`text/javascript` 或 `text/css`，`immutable` |
   | `GET /api/assets/v1/platform/gateways?limit=1`，不帶 cookie | 401 `unauthenticated`（cookie 檢查） |
   | `GET /auth/login`，不帶 cookie（只有 POST 不需要 cookie） | 401 `unauthenticated`（cookie 檢查） |
   | `POST /auth/login`，不帶 cookie 與本文 | 400 `invalid_request`（身分服務的登入，原本的例外路由） |
   | 設備池 `GET`，cookie、`Sec-Fetch-Site: same-origin` | 200 與設備池（對照組：沒有它，下面的拒絕證明不了什麼） |
   | 設備池 `GET`，cookie、`Sec-Fetch-Site: cross-site` | 403 `permission_denied` `security.cross_site_request` |
   | 設備池 `POST`，cookie、`Sec-Fetch-Site: cross-site` | 403 `permission_denied` `security.cross_site_request` |
   | 設備池 `POST`，cookie、外站的 `Origin`，不帶 `Sec-Fetch-Site` | 403 `permission_denied` `security.cross_site_request` |
   | 設備池 `POST`，cookie、app 的 `Origin`、`Sec-Fetch-Site: same-origin` | 405（資產服務，通過了 cookie 檢查） |
   | 設備池 `POST`，cookie、app 的 `Origin`，不帶 `Sec-Fetch-Site` | 405（資產服務） |

   每項一行 `PASS` 或 `FAIL`，寫路徑、狀態碼、M1 的 `code` 與 message key 和固定的字，不印網址、主機名稱、標頭的值、cookie 或本文，可以直接放進測試報告；全部通過時最後一行是 `PASS all 13 checks`、結束碼 0，任何一項不通過時結束碼 1。`302 to the Cloudflare Access login` 表示 Access 沒有接受 service token（標頭檔或 policy 的問題，與 `web` 無關）；`a Cloudflare challenge` 或 `from Cloudflare` 是 Cloudflare 在 Access 之前擋下；設備池的對照組回 401 `unauthenticated` 是 session 已結束（以工作站工具重新 `login`），回 403 `permission_denied` 是 session 不是平台管理員的。
6. **告知展示對象**：第 5 步 13 項都通過，結果存進測試報告之後，才通知展示對象可以用 SPA，並以受邀的帳號在瀏覽器打開 app 確認登入畫面出現。
7. **不通過時與退回**：revert 打開的 PR，Flux 套用後 `web` 與它的路由都移除，app 主機名稱的 `/` 回到 cookie 檢查（沒有 cookie 時 401）；`web` 沒有資料，也沒有留下的 Secret。路由的項目不通過時先 revert 再查。跨站的項目不通過時立刻 revert：cookie 檢查沒有擋下跨站請求，而 SPA 讓瀏覽器在 app 主機名稱上持有 session；查清楚 istiod（第 2 步）之後從第 1 步重來。Access 或 Cloudflare 擋下的情況與 `web` 無關，修好之後重跑第 5 步即可。

## 簽發 CA 的儀式

展示環境的 certs 服務（Laysi/trustiot-v2#619）有自己的根 CA。儀式照 [services/certs/README.md](../../services/certs/README.md)「發簽發 CA 憑證」的 1 到 5 步，有兩處不同：根的保管方式（下一段），以及順序。展示環境先啟用 certs、還沒有根憑證（[deploy/flux/README.md](../../deploy/flux/README.md)「日常變更」的「打開 certs」第 1 步），保管人一次建立根 CA 並簽發簽發 CA，之後一個 PR 同時設定根憑證與簽發 CA 憑證。順序不同的代價有兩點：

- 等待中的金鑰 id 不從 certs 主容器的 `keys wait for a certificate` 那一行讀：沒有根憑證時服務載入不了 CA，只記一次 ERROR `cannot load the issuing CA`、之後每 `crl.check_interval` 一行 WARN `no issuing CA is loaded, so the service cannot sign gateway certificates`，不列等待中的金鑰。改讀 init container `ca-keys` 的 INFO `the issuing CA keys are in place`，欄位 `keys` 是金鑰 id，`generations` 是對應的代。寫入 Secret 與 CSR 的就是這個 init container，所以它的 id 與主容器讀到的相同。
- 這段期間 `ca.pem` 與 `crl.pem` 回 503，工作站憑證與出廠登錄的 API 回 503（`ca_not_ready`）；readiness 不受影響，升級不會因此失敗或回滾。展示環境還沒有 broker（Laysi/trustiot-v2#84），沒有東西在讀這兩個檔。

**展示環境的保管方式**（2026-10-28 專案擁有者決定）：展示環境的根不做離線儀式。根的私鑰在保管人自己的電腦上產生，以權限 0600 的明文檔案存在那台電腦上，只有一位保管人。理由有三：

- 沒有專用的離線機器，也沒有第二位保管人。
- USB 隨身碟這類快閃媒體長期不通電會流失資料，不適合長期保存私鑰。
- 展示資料可以遺失；根的私鑰遺失或外洩時，代價是重做儀式、展示用閘道器重新登錄。

不變的規則：根的私鑰不進 repo（加密的也不放）、不進叢集，不出現在 issue、PR、留言或日誌；不印到終端機，也不當指令參數（`certs-ca` 從 stdin 讀）。這個例外只給展示環境：正式環境的保管方式與儀式在 Laysi/trustiot-v2#716 重新決定，在那之前以 services/certs/README.md 為準。

**金鑰 id** 是公鑰的 DER SubjectPublicKeyInfo 的 SHA-256，小寫十六進位 64 字元（`crates/ca-keys` 的 `key_id`；init container、certs 服務與 `certs-ca` 都用它）。任何人都能以 openssl 從 CSR 或憑證自己算出來，不必相信別人抄來的值：

```sh
openssl req -in issuing-ca-generation-1.csr.pem -noout -pubkey | openssl pkey -pubin -outform DER | sha256sum | cut -d' ' -f1   # CSR
openssl x509 -in issuing-ca-generation-1.pem -noout -pubkey | openssl pkey -pubin -outform DER | sha256sum | cut -d' ' -f1      # 憑證
```

（2026-10-28 以 `certs-ca root sign` 實測：它印出的 `for key` 與這兩行的結果相同。）

展示環境的根只給展示環境：不拿來簽 SaaS 或地端的簽發 CA，正式環境的閘道器也不信任它；展示用閘道器（Laysi/trustiot-v2#458）登錄時拿到的信任錨就是它。展示資料可以遺失（「重設展示資料」不動簽發 CA 的金鑰 Secret，CRL 由服務自己重發）。根的私鑰遺失或外洩時，以新的根重做整個儀式，每台展示用閘道器重新登錄、換上新的根（「保管紀錄」）。

角色：

- 有展示機 SSH 權限的維運人員做第 1、2、5 步（只讀，不取 lab 鎖）。
- 根的保管人在自己的電腦上做第 3 步。
- 第 4 步的 PR 由保管人核准；不是保管人的審查人，從保管人那裡經 PR 以外的管道拿到根憑證的指紋。審查人要能以 SSH 讀展示機的叢集（第 4 步自己讀金鑰 id）。

第 2 步的金鑰 id 不必在簽發前由第二個人再讀一次：簽錯的 CSR 會在第 4 步（審查人自己從叢集讀金鑰 id）或第 5 步被發現，展示環境的處理是換根（第 5 步），代價只有展示用閘道器重新登錄。

在 issue 或 PR 貼出的輸出，只能是指令刻意挑出的欄位（狀態碼、金鑰 id、固定的文字），不貼整行日誌、網址或錯誤訊息：它們可能帶著主機名稱。

### 1. 確認 certs 已啟用

前提：展示叢集部署的 chart 含 certs，`values-demo.yaml` 已打開 `services.certs`，Flux 已套用（「打開切片的服務」與 deploy/flux/README.md「打開 certs」）。

```sh
bun scripts/demo/flux.ts status --ssh-host "$DEMO_SSH_HOST"
```

HelmRelease 最新一筆是 `deployed`，`trustiot-certs` available。這份輸出存進 Laysi/trustiot-v2#619 的留言（`flux.ts` 不印 ssh 主機與密鑰）。

### 2. 取出 CSR 與金鑰 id

在工作站上，以 ssh 讀展示機的叢集：

```sh
ssh "$DEMO_SSH_HOST" "sudo k3s kubectl -n trustiot get configmap trustiot-certs-ca-requests -o jsonpath='{.data.issuing-ca-generation-1\.csr\.pem}'" > issuing-ca-generation-1.csr.pem
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot logs deployment/trustiot-certs -c ca-keys' \
  | grep 'the issuing CA keys are in place' | grep -oE '[0-9a-f]{64}' | sort -u
openssl req -in issuing-ca-generation-1.csr.pem -noout -pubkey | openssl pkey -pubin -outform DER | sha256sum | cut -d' ' -f1
```

第一次啟用只有第 1 代，第二行只印出一個金鑰 id，第三行從 CSR 算出的要與它相同；不同時停下，照 services/certs/README.md 第 4 步的說明調查。金鑰 id 是公開資料，可以寫進 issue 留言。CSR 也是公開資料，複製到保管人的電腦。

### 3. 在保管人的電腦上建立根 CA 並簽發

**建置工具**（儀式之前，在可信的建置機或保管人的電腦上）：

1. 確認 tag 指向發佈時的 commit：`git fetch origin --tags`，`git rev-parse v<部署中的版本>^{commit}`（部署中的版本是 `chart.yaml` 的 `ref.tag`）要等於那一版 GitHub Release 頁面寫的「來源 commit」，也等於建出它的 release.yml 執行紀錄（Release 頁面連到的那次執行）的 commit。不同就停下。
2. `git checkout v<部署中的版本>`，再 `cargo build --release --locked -p certs --bin certs-ca`，記下 SHA-256。展示環境的私鑰不加密，不需要 `age`。
3. 在別台機器建出時，複製到保管人的電腦之後再核對一次 SHA-256。

**保管人的電腦**：

- 根的私鑰放在只有保管人讀得到的目錄（權限 0700），檔案權限 0600。這個目錄不在任何 git 工作目錄裡，也不在會同步到雲端的資料夾裡。私鑰只存在這台電腦上，不另做備份：遺失時重做儀式（「保管紀錄」）。
- 私鑰只經過重新導向（`>`、`<`）：不 `cat`、不複製貼上、不當指令參數。

**步驟**（照 services/certs/README.md「發簽發 CA 憑證」的第 1、4 步）。下面的指令都在保管目錄裡執行，`$T` 是放 `certs-ca` 的目錄：

1. **續做規則**：目錄裡已經有 `root-key.pem` 與 `root.pem`（例如上次在建立之後被中斷），就跳過第 2 步，沿用這兩個檔。要放棄一個已經建立的根（例如決定重來），刪掉它的 `root-key.pem`，並寫進不進 repo 的維運紀錄；不留下沒人管的根。
2. 建立根 CA，CN 寫 `TrustIoT Demo Gateway Root CA 1`。私鑰先寫進同一個目錄的暫存檔（`mktemp` 建的檔案權限是 0600），成功才改名：

   ```sh
   umask 077
   t=$(mktemp root-key.XXXXXX)
   "$T/certs-ca" root create --common-name "TrustIoT Demo Gateway Root CA 1" --certificate root.pem > "$t" && mv "$t" root-key.pem
   "$T/certs-ca" root check --root-certificate root.pem < root-key.pem
   ```

   `root create` 不把私鑰寫到終端機，`root.pem` 已經存在時也拒絕執行。失敗時刪掉暫存檔與留下的 `root.pem`，從這一步重來。`root check` 印出 `the root private key belongs to the root certificate in root.pem (key <金鑰 id>)`。
3. 計算根憑證的指紋：`openssl x509 -in root.pem -noout -fingerprint -sha256`，寫進不進 repo 的維運紀錄。第 4 步核准的人以這個值為準。
4. 簽發前再以第 2 步的 openssl 指令，從複製過來的 CSR 算一次金鑰 id，要等於第 2 步讀到的值。
5. 簽發簽發 CA，CN 寫 `TrustIoT Demo Gateway Issuing CA G1 <儀式日期>`：

   ```sh
   "$T/certs-ca" root sign --root-certificate root.pem --request issuing-ca-generation-1.csr.pem \
     --common-name "TrustIoT Demo Gateway Issuing CA G1 <儀式日期>" --certificate issuing-ca-generation-1.pem < root-key.pem
   openssl verify -CAfile root.pem issuing-ca-generation-1.pem
   ```

   `certs-ca root sign` 在 stderr 印出 `for key <金鑰 id>`，**和第 2 步的 id 一致才繼續**；不一致時刪掉這張憑證、停下，照 services/certs/README.md 第 4 步的說明調查。`openssl verify` 印出 `issuing-ca-generation-1.pem: OK`。
6. 帶出這個目錄的只有 `root.pem` 與 `issuing-ca-generation-1.pem`（公開資料）。

兩張憑證的 CN 不放主機名稱、人名或客戶名稱：它們都會進公開的 repo。

### 4. PR：設定根憑證與簽發 CA 憑證

把兩張憑證各轉成一行 base64（`sed '/-----/d' <檔案> | tr -d '\n'`），照 deploy/flux/README.md「打開 certs」第 2 步寫進 `values-demo.yaml` 的 `certificateAuthority`。PR 的說明寫儀式日期與金鑰 id，不寫保管人是誰、私鑰在哪台電腦。PR 的說明只是方便閱讀，審查不以它為準：寫 PR 的人也能改 values 與說明。

審查的人從 PR 的 values 取回憑證，再自己核對三件事：

```sh
printf '%s' '<rootCertificate 的值>' | base64 -d > root.der
printf '%s' '<issuingCertificates 的值>' | base64 -d > issuing.der
openssl x509 -inform DER -in root.der -out root.pem && openssl x509 -inform DER -in issuing.der -out issuing.pem
openssl x509 -in root.pem -noout -fingerprint -sha256                                                      # 1
openssl verify -CAfile root.pem issuing.pem                                                                 # 2：issuing.pem: OK
openssl x509 -in issuing.pem -noout -pubkey | openssl pkey -pubin -outform DER | sha256sum | cut -d' ' -f1  # 3
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot logs deployment/trustiot-certs -c ca-keys' \
  | grep 'the issuing CA keys are in place' | grep -oE '[0-9a-f]{64}' | sort -u                             # 3
```

1. 根的指紋等於保管人記在維運紀錄裡的值。核准的人是保管人，或經 PR 以外的管道（當面或電話）從保管人拿到這個值；不從 PR 的說明或留言拿。
2. 簽發 CA 鏈得到這個根。
3. 從簽發 CA 憑證算出的金鑰 id，等於審查人自己從叢集讀到的等待中的金鑰 id。

三項都成立才核准。合併後 certs 的 pod 不重啟：kubelet 更新掛載的 ConfigMap，服務最多 30 秒後重讀。

### 5. 確認

```sh
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot logs deployment/trustiot-certs -c certs --since=30m' \
  | grep -oE '"message":"(loaded the issuing CA|issued a new CRL)"|"(current|ca_key_id)":"([0-9a-f]{64}|none)"|"crl_number":[0-9]+'
```

- `loaded the issuing CA` 之後的 `current` 是第 2 步的金鑰 id（不是 `none`）。`certs.ca.signing` 就是依同一個判斷變成 1：服務有一張這一刻有效的簽發 CA 憑證。展示環境讀不到這個指標本身：collector 的指標管線在 Laysi/trustiot-v2#183 之前不送到任何地方，所以以這一行為證據。
- 下一輪 CRL 工作（最多 `crl.check_interval`，預設 5 分鐘）記 `issued a new CRL`，`ca_key_id` 是同一個 id，`crl_number` 是 1。在那之前工作站憑證的 API 仍回 503（`ca_not_ready`）。

**`current` 不是第 2 步的 id 時**（包括第 2 步之後、合併之前金鑰 Secret 被重新產生，這時 `current` 是 `none`）：叢集裡沒有這張簽發 CA 憑證的私鑰，持有私鑰的可能是別人。把這張憑證當成外洩：

1. 開 PR 把它從 `certificateAuthority.issuingCertificates` 拿掉，合併。
2. 在維運紀錄與 Laysi/trustiot-v2#619 記下金鑰 id、日期與經過（不寫人名）。
3. 換根：雲端這一側的根 CRL 已經實作：儀式簽出（Laysi/trustiot-v2#829）、certs 採用（Laysi/trustiot-v2#830）、出廠登錄時交給閘道器（Laysi/trustiot-v2#832）、經 HTTPS 提供（Laysi/trustiot-v2#831）。agent 也會取用新的根 CRL（Laysi/trustiot-v2#563），但展示環境的 `/certs/root-crl` 還在 Access 後面（見上方「架構」），已登錄的閘道器手上只有登錄時拿到的那一張，撤銷這個簽發 CA 的新根 CRL 到不了它們，信任這個根的閘道器會一直接受那張憑證。所以在這條路徑開放、展示用閘道器的 agent 更新到含 Laysi/trustiot-v2#563 的版本之前，仍以新的根（CN `TrustIoT Demo Gateway Root CA 2`）重做整個儀式，刪掉舊根的私鑰檔並記錄；已經以舊根登錄的展示用閘道器重新登錄。

再以 `GET` 讀 CRL。`crl.pem` 是內部端點，入口沒有它的路由，所以以 port-forward 直接連 certs 的 pod（不經 mesh，需要展示機上的叢集管理權限）。開兩個終端機：

```sh
# 終端機 1：保持開著
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot port-forward deploy/trustiot-certs 18080:8080'
# 終端機 2
ssh "$DEMO_SSH_HOST" 'curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:18080/internal/certs/v1/crl.pem'
ssh "$DEMO_SSH_HOST" 'curl -s http://127.0.0.1:18080/internal/certs/v1/crl.pem | openssl crl -noout -issuer -lastupdate -nextupdate -crlnumber'
```

兩個 curl 跑完就在終端機 1 按 Ctrl-C 關掉 port-forward，不要留著。第一個是 `200`；第二個的 issuer 是第 3 步簽發 CA 的 CN（展示機沒有 `openssl` 時，改以 `grep -c 'BEGIN X509 CRL'` 確認是 1）。還沒有 CRL 時是 503（`an issuing CA has no CRL yet`），等一輪再試。這些輸出都只有公開資料，存進 Laysi/trustiot-v2#619 的留言。

### 保管紀錄

- **根憑證**：`values-demo.yaml` 的 `certificateAuthority.rootCertificate`（git 保留每一版），保管人的電腦上也有 `root.pem`。它的指紋另記在不進 repo 的維運紀錄（第 3 步），第 4 步以那個值核對。
- **根的私鑰**：一個權限 0600 的明文檔，在保管人的電腦上，沒有其他副本。保管人是誰、在哪台電腦，只記在不進 repo 的維運紀錄（與「Cloudflare 上的設定」的變更清單同一處），不寫進 repo、issue、PR 或留言。公開的紀錄（issue 留言）只寫：儀式日期、根憑證的 SHA-256 指紋、簽發 CA 的金鑰 id，以及「一位保管人」這個角色。放棄或刪除的根也記在維運紀錄。
- **之後再用到私鑰**：簽發 CA 輪替（services/certs/README.md「輪替」）要以根簽新一代的 CSR，同樣以 `< root-key.pem` 交給 `certs-ca root sign`；用之前先以 `certs-ca root check` 確認這個檔還是這張根憑證的私鑰。
- **遺失或外洩**（電腦壞了、檔案被刪，或電腦、檔案可能被別人取得）：以新的根（CN 的編號加一）重做整個儀式，`values-demo.yaml` 換成新的根憑證與簽發 CA 憑證，每台展示用閘道器重新登錄；外洩時舊根的私鑰檔若還在也刪掉，並記進維運紀錄。保管人異動時，依 [docs/runbooks/secrets.md](secrets.md) 的「維運人員加入與離開」判斷要不要換根。
- 展示環境的根在 2026-10-28 建立，根憑證的指紋與簽發 CA 的金鑰 id 記在 Laysi/trustiot-v2#619 的留言。

## 平台管理員的登入確認

產線工作站登錄閘道器（Laysi/trustiot-v2#80、Laysi/trustiot-v2#618）要平台管理員在工作站上的 session。工作站經 Access 的 service token 通過 Cloudflare（「Access 名單」的「工作站的 service token」），再以 email 驗證碼登入平台。以下在持有工作站的操作員的 Linux 帳號、以 bash 執行，`H` 是操作員的標頭檔。`DEMO_APP_HOST` 用小寫，與 session 檔記的網址比對：

```sh
: "${DEMO_APP_HOST:?set DEMO_APP_HOST to the app host name, in lower case}"
H=~/.config/trustiot-workstation/access-headers    # 「工作站的 service token」的交付步驟建立的檔案
S="$XDG_RUNTIME_DIR/trustiot-workstation/session"  # 工作站工具的 session 檔：origin= 與 cookie= 各一行
# curl 的設定（標頭）經 stdin 交給 curl（-K -），不出現在命令列與 shell 的歷史紀錄
token_headers() { grep -iE '^cf-access-client-(id|secret):' "$H" | sed 's/^/header = "/; s/$/"/'; }
session_headers() {
  token_headers
  sed -n 's/^cookie=\(.*\)$/header = "Cookie: __Host-trustiot-session=\1"/p' "$S"
}
# 平台網址只取自 session 檔的 origin=，而且必須是 DEMO_APP_HOST 的；不是就拒絕
session_origin() {
  local origin
  origin=$(sed -n 's/^origin=//p' "$S" 2>/dev/null)
  if [ -z "$origin" ] || [ "$origin" != "https://$DEMO_APP_HOST" ]; then
    echo 'refused: no session, or the session belongs to another platform' >&2
    return 1
  fi
  printf '%s\n' "$origin"
}
# 只印狀態碼，以及回應裡的 M1 code 與 message key；不印網址、標頭或本文的其他內容
status_and_keys() {
  local out
  out=$(curl -s -K - -w '\n%{http_code}' "$1")
  printf '%s %s\n' "${out##*$'\n'}" \
    "$(printf '%s' "${out%$'\n'*}" | grep -oE '"(code|message_key)":"[a-z0-9_.]+"' | sort -u | tr '\n' ' ')"
}
```

1. **平台管理員存在**：照 [platform-admin.md](platform-admin.md)（展示環境在展示機上以 `sudo k3s kubectl`，namespace 與 release 都是 `trustiot`）。不確定是否已建立時照樣執行一次：結束碼 3 表示已經建立過，日誌的 `user_id` 是那一位。地址只在 attach 之後輸入，不寫進任何紀錄。建立用的 CronJob `trustiot-platform-admin-bootstrap`，以及身分服務讀 `trustiot-smtp` 帳密寄出驗證碼的設定，都是 chart `0.0.0-rc.11` 才有：展示叢集還釘在 rc.10 時兩者都沒有，先換版。
2. **登入**（[factory/workstation](../../factory/workstation/README.md)「`login`」）：工具只從命令列收 email，所以先以 `read` 讀進變數，地址不留在 shell 的歷史紀錄：

   ```sh
   read -r -p 'email: ' e
   trustiot-workstation login --url "https://$DEMO_APP_HOST" --email "$e" --headers-file "$H"; unset e
   ```

   驗證碼寄到那個地址（Resend 的 SMTP，「外寄郵件與 Anthropic 金鑰」），從提示輸入。
3. **讀平台的操作紀錄**：只印狀態碼：

   ```sh
   o=$(session_origin) && session_headers | curl -s -o /dev/null -w '%{http_code}\n' -K - "$o/api/identity/v1/platform/event-log?limit=1"
   ```

   平台管理員是 `200`。
4. **`/api/certs` 到得了 certs**：送一個空的簽發請求，certs 檢查完呼叫者是平台管理員、讀了本文之後以缺欄位拒絕，什麼都不簽：

   ```sh
   o=$(session_origin) && { session_headers; echo 'header = "Content-Type: application/json"'; echo 'data = "{}"'; } \
     | status_and_keys "$o/api/certs/v1/workstation-certificates"
   ```

   `422`，`"code":"validation_failed"`，message key 有 `workstation_certificate.field_required`。這是 certs 自己的回應；沒有 `/api/certs` 的路由時是入口的 404。
5. **一般成員讀不到**：以一個不是平台管理員的使用者登入（先 `trustiot-workstation logout --headers-file "$H"`，工作站一次只存一個 session），重做第 3 步與第 4 步，兩個都是 `403`（`permission_denied`）。展示環境目前只有 `bootstrap-platform-admin` 能建立使用者，它建立的都是平台管理員；建立使用者的 API 在 Laysi/trustiot-v2#105。專案擁有者在 2026-10-24 決定這一步等 Laysi/trustiot-v2#105 完成、展示環境能建立一般成員之後再做，不直接改資料庫造一個。這個行為已有自動化測試涵蓋：身分服務的 `tests/event_log.rs`（租戶的 `admin` 讀平台的操作紀錄是 403）與 certs 的 `tests/workstation.rs`（只有平台管理員能簽發與撤銷工作站憑證）。
6. **登出**：`trustiot-workstation logout --headers-file "$H"`。第 3 到 5 步印出的那一行（狀態碼與 key，不含 cookie、標頭、網址與本文）存進 Laysi/trustiot-v2#619 的留言。

## 綁定展示用閘道器與建立設備

出廠登錄過的閘道器（[factory/workstation](../../factory/workstation/README.md)）在資產服務的設備池裡、沒有綁定：agent 連得上 broker，但 `online` 被拒（PUBACK 0x87），也收不到設定（「打開閘道器的資料路徑」）。綁定、型號與設備都經 app 主機的 API 建立，不改資料庫（Laysi/trustiot-v2#793）。前提：`values-demo.yaml` 有 `/api/assets` 與 `/api/gateway-ops` 的 cookie 路由（[deploy/flux/README.md](../../deploy/flux/README.md)「閘道器的綁定、型號與設備的 API」），平台管理員已在工作站登入（上一節的第 1、2 步）。請求以上一節的方式帶 Access 的 service token 與 session cookie，不帶 `Origin` 與 `Sec-Fetch-Site`（不是瀏覽器，身分服務的跨站檢查放行），寫入一律 `Content-Type: application/json`。依序：

| 步驟 | 請求 | 誰 | 重跑時 |
| --- | --- | --- | --- |
| 1. 讀 session | `GET /api/identity/v1/sessions/current` | 平台管理員本人 | 只讀；`tenants` 列出他所屬的租戶 |
| 2. 建立展示租戶 | `POST /api/identity/v1/tenants`，body `{"name"}` | 平台管理員本人 | **不是冪等的**：每次 201 都是一個新租戶，而且平台管理員不會自動成為成員、第 1 步看不到它。重跑前先確認沒有建過（第 1 步的 `tenants`，或以 superuser 唯讀查 `identity.tenants`）；回應遺失時不要重送，先查 |
| 3. 自己成為成員 | `PUT /api/identity/v1/platform/tenants/<租戶 ID>/members`，body `{"email", "role": "admin"}`（email 是第 1 步回的自己的地址） | 平台管理員本人 | 冪等：角色相同時 200、什麼都不寫 |
| 4. 切到展示租戶 | `PUT /api/identity/v1/sessions/current/tenant`，body `{"tenant_id"}` | 成員 | 冪等；之後這個 session 的每個請求都帶展示租戶與在那裡的角色 |
| 5. 綁定閘道器 | `PUT /api/assets/v1/platform/gateways/<硬體 ID>/binding`，body `{"tenant_id"}` | 平台管理員本人 | 冪等：已綁在同一個租戶時 200、什麼都不寫；別的租戶握有時 409。先以 `GET /api/assets/v1/platform/gateways/<硬體 ID>` 確認它在設備池（404 是還沒出廠登錄）。剛建立的租戶回 422 `gateway_binding.tenant_unknown`：資產服務還沒收到 `identity.tenant_created`，等幾秒以同一個 `tenant_id` 重送，**不要**重做第 2 步（`GET /api/assets/v1/tenant` 回 200 之後就收到了） |
| 6. 建立型號 | `POST /api/assets/v1/device-models`，body `{"name", "points"}` | 租戶 editor 以上 | 名稱重複 409 `device_model.name_taken`：先以 `GET /api/assets/v1/device-models` 找同名的型號。503 `device_model.tenant_not_ready` 同第 5 步的 422：等幾秒重送，不要重做第 2 步 |
| 7. 建立設備 | `POST /api/assets/v1/devices`，body `{"name", "hardware_id", "model_id", "transport", "host", "port", "unit_id", "poll_interval_ms"}` | 租戶 editor 以上 | 名稱重複 409 `device.name_taken`：先以 `GET /api/assets/v1/devices` 找同名的設備 |
| 8. 確認 | `GET /api/gateway-ops/v1/gateways` | 租戶 viewer 以上 | 只讀 |

- **RS485 轉換器只給一個連線**：轉換器把設備的回應送給每一條 TCP 連線（[現場限制](../requirements/field-constraints.md)），第 7 步一建立，agent 就收到設定開始輪詢，之後其他程式不能再連那個轉換器。lab 資料的錄製（Laysi/trustiot-v2#680）改從展示環境匯出。
- **轉換器的位址不公開**：`host` 是 lab 的區網位址，只出現在請求與資產服務的資料裡，不寫進 issue、PR、測試報告或日誌的摘錄；貼進紀錄前遮掉。
- **第 8 步的預期**：綁定之後 `presence` 先是 `unknown`，agent 下一次重連（退避最多 60 秒）後是 `online`；`desired_config_version` 在第 5 步之後就有（空的設定也有版本），第 7 步之後再往上一版，`applied_config_version` 在 agent 套用後的下一次心跳跟上。agent 的日誌（`sudo journalctl -u trustiot-agent`）有 `applied the configuration`，之後 ingest 把遙測寫進 ClickHouse。
- **重設展示資料之後**（「重設展示資料」）：租戶、綁定、型號與設備都不在了，從第 2 步重做；certs 的登錄也不在了，閘道器要先重新出廠登錄才會回到設備池。

## 換 chart 版本

改 `deploy/flux/clusters/demo/trustiot/chart.yaml` 的 PR 有兩種：release.yml 開的 deploy PR（可能同時移動 SaaS 與展示叢集），以及 `bun scripts/flux/pin-chart.ts --cluster demo --version <版本> --digest <chart digest>` 只換展示叢集的 pin PR（[deploy/flux/README.md](../../deploy/flux/README.md)「發版與回滾」與「日常變更」）。

Laysi/trustiot-v2#500（commit `26b454d`）起，入口的跨站請求檢查要 istiod 依 `deploy/istio/istiod-values.yaml` 的 `includeRequestHeadersInCheck` 把 `origin` 與 `sec-fetch-site` 交給 ext_authz。istiod 沒有對齊時身分服務看不到這兩個標頭，跨站請求一律放行，不報錯也不留紀錄（[deploy/istio/README.md](../../deploy/istio/README.md)「ext_authz provider」）。所以 2026-10-22 決定：每次換版都先比對新舊版本的 istiod values，有改就先對齊 istiod，再合併 pin PR。展示叢集的 `0.0.0-rc.10` 不含 `26b454d`，第一次換到 rc.10 之後的版本一定要做第 2 步。合併任何一種 PR 之前：

1. **比對 istiod 的 values**：`git fetch origin --tags`，比對部署中的版本（`chart.yaml` 的 `ref.tag`）與新版本：

   ```sh
   git diff v<部署中的版本> v<新版本> -- deploy/istio/istiod-values.yaml
   ```

   沒有輸出就跳到第 3 步。
2. **對齊 istiod**：在 main 的乾淨 checkout 先確認 main 的這個檔與新版本的相同（`git diff v<新版本> origin/main -- deploy/istio/istiod-values.yaml` 沒有輸出；不同時停下，在換版的 issue 寫明差異再決定），再照 [scripts/README.md](../../scripts/README.md)「`demo/platform.ts`：平台元件與 repo 的釘選對齊」執行：

   ```sh
   bun scripts/demo/platform.ts --ssh-host "$DEMO_SSH_HOST"                            # 檢查、計畫與確認碼，只讀
   bun scripts/demo/platform.ts --ssh-host "$DEMO_SSH_HOST" --apply --confirm <確認碼>
   bun scripts/demo/platform.ts --ssh-host "$DEMO_SSH_HOST"                            # 確認：nothing to change
   ```

   計畫印出 istiod values 改前與改後的文字，確認改後的包含第 1 步看到的變更。lab 的入口 e2e 正疊著 `HelmChartConfig istiod` 時 `--apply` 拒絕，等它測完再做。計畫若還列出其他差異（Istio、Gateway API、CloudNativePG 的版本），照該元件的 README 處理，並先看「與 lab e2e 共用叢集」。最後一次 dry run 沒有差異之前，不合併 pin PR。
3. **合併並確認**：合併 PR 後，`flux.ts status` 印出 `deployed: the same (<新版本>+<digest 前 12 位> (deployed))`、每個 Deployment available；`tunnel.ts route-origin` 仍然通過（入口的 ClusterIP 由 values 固定，Service 重建後也拿回同一個位址），再跑 `check --stage 2`。

換版時 migration Job 重跑，每個服務與 ClickHouse（init container 用 migration 映像）都重啟一次（[deploy/chart/README.md](../../deploy/chart/README.md)「叢集內的 ClickHouse」），展示環境短暫中斷，不要排在展示前。

**換發端點上線**：certs 打開之後，chart 為 gateway 主機名稱的 `/certs/renew` 掛著指向 certs 的路由，tunnel 也讓這個路徑略過 Access（「架構」）。換發端點由 Laysi/trustiot-v2#81 實作（services/certs/README.md 的「換發」）；釘的 chart 還不含它時，certs 對這個路徑回 404。一旦釘的 chart 含 Laysi/trustiot-v2#81 的換發端點，這個路徑就經既有的 Bypass 對網際網路開放，展示環境這邊不必再改任何東西。換發端點在 2026-10-29 進了 main（PR Laysi/trustiot-v2#734），`0.0.0-rc.11` 沒有它，之後的第一個候選版就有；這次換版與暴露的審查在 Laysi/trustiot-v2#795。所以換到第一個含 Laysi/trustiot-v2#81 的版本之前，下面兩件都完成才合併 pin PR：

1. **前提條件：入口的速率限制涵蓋 `/certs/renew`**。照「`/mqtt` 的速率限制」最後一段改那條規則，讀回 ruleset 確認，再從工作站在 10 秒內連續送 41 次 `POST /certs/renew`（本文空的即可，certs 回 400 或 404，不碰資料庫），第 41 次起得到 Cloudflare 的 429（`server: cloudflare`），等 10 秒後再送一次，不再是 Cloudflare 的 429。理由：換發端點自己只有每個副本的限流，任何人都能以捏造的硬體 ID 用完它的全域限流，讓每台閘道器取挑戰都得到 429，包括憑證已過期、只能經這個端點恢復的閘道器；以來源 IP 計數只能在入口做（services/certs/README.md「換發」的「規則與取捨」）。
2. 在換版的 issue 審查這個暴露（端點自己的驗證與限流、`check --stage 2` 對它的預期），確認後才合併。

退回是 revert 那個 PR。對齊 istiod 本來就排在升級 chart 之前，新的 istiod values 與舊版 chart 會同時存在一段時間，所以 revert 不必同時退回 istiod；確實要退回時用 `platform.ts --rollback`（條件見 scripts/README.md 同一節）。

退回到 `0.0.0-rc.10` 或更早、沒有 certs 的版本時，先合併一個關掉 certs 的 PR（`services.certs.enabled: false`，拿掉 `/api/certs` 的路由），再 revert 換版的 PR：順序反過來時 `bun test deploy/flux` 失敗（deploy/flux/README.md「打開 certs」），那一版也沒有 certs 的映像。關掉 certs 時，chart 標了 `helm.sh/resource-policy: keep` 的 Secret `trustiot-certs-password`、`trustiot-certs-ca-keys`（簽發 CA 的私鑰）與 ConfigMap `trustiot-certs-ca-requests` 留在叢集裡；放憑證的 ConfigMap `trustiot-certs-ca-certificates` 沒有標，會被刪掉，它的內容在 values 裡。之後再打開 certs 時 chart 接回留下的物件（`deploy/chart/templates/passwords.yaml` 與 `certs-ca.yaml` 開頭的說明），init container 沿用原來的金鑰，values 裡的簽發 CA 憑證仍然對得上，不必重做儀式；以「簽發 CA 的儀式」第 5 步確認 `current` 仍是原來的金鑰 id。

## 重設展示資料

里程碑「展示切片」接受展示資料遺失（沒有備份），但展示要靠累積一兩週的真實資料（同一節的「資料面先通」），重設會把累積的資料一起清掉。所以只在必要時重設（例如資料被測試弄亂），先在 issue 寫明原因並經使用者同意。

資料在 namespace `trustiot` 的兩個磁碟區（local-path，PV 的 reclaim policy 是 `Delete`，PVC 刪掉時資料目錄一起刪除）。兩個一起重設：只清一個時，留下的那邊可能指向已經不存在的租戶或設備。

| 資料 | 物件 |
| --- | --- |
| Postgres：每個服務的資料（帳號、租戶、session，以及之後各服務的資料） | CloudNativePG 的 Cluster `trustiot-postgres` 與它的 PVC（label `cnpg.io/cluster=trustiot-postgres`） |
| ClickHouse：遙測與日誌 | StatefulSet `trustiot-clickhouse` 的 PVC `data-trustiot-clickhouse-0` |

不刪的東西：

- namespace `trustiot` 本身與其他物件：Flux 的 OCIRepository 與 HelmRelease、Flux 從 git 解密的 Secret（`ghcr-pull`、`trustiot-demo-values` 等）、Helm 的 release 紀錄、入口的 Gateway 與 Istio 為它產生的 Service（tunnel 的路由指向它的固定 ClusterIP）。刪 namespace 會連這些一起刪掉。
- chart 在叢集內產生、帶 `helm.sh/resource-policy: keep` 的 Secret 與 ConfigMap：服務與 collector 的密碼、ClickHouse 管理帳號的密碼、內部身分簽章金鑰與公鑰（之後啟用 certs 時還有簽發 CA 的金鑰）。它們不是展示資料，新的資料庫照樣用它們：migration Job 以 Secret 裡的密碼建立角色（[deploy/chart/README.md](../../deploy/chart/README.md)「migration Job」的「產生的密碼」）。ImageCatalog `trustiot-postgres` 與 Database `trustiot-postgres-trustiot` 也不另外刪。
- 其他 namespace（`flux-system`、`istio-system`、`cnpg-system`、`kube-system`、lab e2e 的），以及主機上的 connector 與維運 tunnel。

刪掉資料之後要讓 migration Job 再跑一次：新的 Postgres 與 ClickHouse 是空的，而 Job 名稱沒變時已完成的 Job 不會重跑，服務的 init container 看到它已完成就啟動，服務就在沒有 schema 的資料庫上跑。所以調大 `migrate.revision`：Job 換名重跑，建立角色、schema、ClickHouse 的表與 collector 帳號，服務跟著滾動重啟（deploy/chart/README.md「migration Job」的「Job 何時再跑」）；同一次升級裡 Helm 也重建被刪掉的 Cluster 與 StatefulSet。HelmRelease 沒有開 drift detection，刪掉的物件要等下一次升級才會重建，所以順序是：暫停 Flux、刪資料、合併改 `migrate.revision` 的 PR、恢復 Flux。

這個程序尚未在展示機實測：Helm 升級時重建被刪掉的 Cluster 與 StatefulSet、CloudNativePG 對新的 Cluster 重新套用 Database，都還沒在叢集上驗證。第一次照做時把每一步的輸出記進測試報告。

由有展示機 SSH 權限的維運人員執行，PR 由另一位審查。以下 `kubectl` 都在展示機上以 `sudo k3s kubectl` 執行：

1. **準備 PR**：在 `values-demo.yaml` 加上 `migrate.revision`，或把已有的值加一（目前沒有設，是 chart 預設的 `"1"`，所以第一次是 `"2"`）。CI 通過、審查完，先不合併。
2. **取得 lab 鎖**：

   ```sh
   ssh "$DEMO_SSH_HOST" 'mkdir ~/e2e.lock && echo "Laysi/trustiot-v2#<issue> $(date -u +%FT%TZ) demo data-reset" > ~/e2e.lock/owner && echo ACQUIRED || cat ~/e2e.lock/owner'
   ```

3. **暫停 Flux**，先根 Kustomization、再 HelmRelease（根 Kustomization 沒暫停時，kustomize-controller 下一次套用會清掉以 kubectl 寫的欄位，把 HelmRelease 的 `suspend` 改回 git 的樣子；[deploy/flux/README.md](../../deploy/flux/README.md)「在展示機安裝 Flux」）：

   ```sh
   kubectl -n flux-system patch kustomizations.kustomize.toolkit.fluxcd.io flux-system --type=merge -p '{"spec":{"suspend":true}}'
   kubectl -n trustiot patch helmreleases.helm.toolkit.fluxcd.io trustiot --type=merge -p '{"spec":{"suspend":true}}'
   kubectl -n trustiot get helmreleases.helm.toolkit.fluxcd.io trustiot    # READY 為 True：沒有進行中的升級
   ```

4. **刪 Postgres**：CloudNativePG 刪 Cluster 時連同 pod、PVC 與它產生的 Secret（`trustiot-postgres-app` 等）一起刪；PVC 在 pod 停止之後才在背景刪掉（deploy/chart/README.md「叢集內的 Postgres」的「刪除」）：

   ```sh
   kubectl -n trustiot delete clusters.postgresql.cnpg.io trustiot-postgres
   kubectl -n trustiot get pod,pvc -l cnpg.io/cluster=trustiot-postgres    # 重複到兩者都沒有為止
   ```

   Postgres 的 pod 已經不在而 PVC 還在、也沒有標示刪除中時（與 chart README 寫的不同），以 `kubectl -n trustiot delete pvc -l cnpg.io/cluster=trustiot-postgres` 明確刪除，記進測試報告。

5. **刪 ClickHouse**：StatefulSet 刪除時保留 PVC（`whenDeleted: Retain`），pod 還在時 PVC 也刪不掉，所以先刪 StatefulSet、等 pod 消失，再刪 PVC：

   ```sh
   kubectl -n trustiot delete statefulset trustiot-clickhouse
   kubectl -n trustiot get pod trustiot-clickhouse-0                  # 重複到 NotFound
   kubectl -n trustiot delete pvc data-trustiot-clickhouse-0
   kubectl get pv | grep 'trustiot/'                                   # 這兩個 PVC 的 PV 都不在
   ```

6. **合併 PR，恢復 Flux**，先 HelmRelease（values 還沒變，不升級）、再根 Kustomization，並立刻同步。根 Kustomization 套用新的 `values-demo.yaml` 後，helm-controller 隨即升級：

   ```sh
   kubectl -n trustiot patch helmreleases.helm.toolkit.fluxcd.io trustiot --type=merge -p '{"spec":{"suspend":false}}'
   kubectl -n flux-system patch kustomizations.kustomize.toolkit.fluxcd.io flux-system --type=merge -p '{"spec":{"suspend":false}}'
   kubectl -n flux-system annotate --overwrite gitrepository/flux-system reconcile.fluxcd.io/requestedAt="$(date +%s)"
   ```

7. **確認**：`flux.ts status` 的根 Kustomization 停在合併的 commit、HelmRelease 與 Kustomization 都沒有暫停、每個 Deployment available；`kubectl -n trustiot get helmreleases.helm.toolkit.fluxcd.io trustiot` 的 READY 為 True，訊息是這次的升級；`kubectl -n trustiot get cluster trustiot-postgres` 的 STATUS 是 `Cluster in healthy state`，`kubectl -n trustiot get database trustiot-postgres-trustiot` 的 APPLIED 是 `true`；`kubectl -n trustiot get jobs` 有完成的 `trustiot-migrate-<新的 hash>`；`kubectl -n trustiot get pvc` 有新的兩個 PVC。最後跑 `tunnel.ts check --stage 2`。升級失敗時看 `describe helmrelease` 與 Job 的日誌（deploy/chart/README.md「migration Job」的「失敗時」）。
8. **重建帳號與租戶**：repo 沒有展示環境的種子資料（`bun scripts/dev.ts seed` 只給本機的開發環境），使用者、租戶與成員資格照它們第一次建立的方式重建。所有 session 都已失效，受邀的人要重新登入 app（Access 的 session 不受影響）。
   - 第一位平台管理員照[第一位平台管理員 runbook](platform-admin.md) 的「步驟」建立（資料重設後標記也不在了，所以指令以 0 結束），再以 email 驗證碼登入 app。其他使用者照它們第一次建立的方式重建（身分服務還沒有管理使用者的 API，見 [services/identity](../../services/identity/README.md)）。
   - 租戶：平台管理者以 `POST /api/identity/v1/tenants` 建立（services/identity 的「租戶」）。
   - 成員資格與角色：平台管理者以 `PUT /api/identity/v1/platform/tenants/<租戶 ID>/members` 設定，每個成員一次，使用者要先存在（services/identity 的「成員」，Laysi/trustiot-v2#583）。在 app 的頁面以平台管理者登入，在瀏覽器開發者工具的 Console 執行（同源請求，瀏覽器帶上 session cookie 與 `Sec-Fetch-Site: same-origin`，ext_authz 的跨站檢查才放行）：

     ```js
     await fetch("/api/identity/v1/platform/tenants/<租戶 ID>/members", {
       method: "PUT",
       headers: { "Content-Type": "application/json" },
       body: JSON.stringify({ email: "<成員的 email>", role: "admin" }),
     }).then(async (r) => [r.status, await r.json()]);
     ```

     新成員回 201、改角色回 200，`previous_role` 是原本的角色；404 是沒有這個租戶，422 `member.email_unknown` 是這個 email 還沒有使用者。`role` 是 `viewer`、`editor` 或 `admin`；展示者自己要進入展示租戶時，也以這個方式把自己設為成員。每次變更都記在那個租戶的操作紀錄（`identity.member_role_changed`），成員下一個請求就帶新角色；已經在用的 app 頁面要切到這個租戶（或重新登入）才會以它為目前的租戶。
   - 展示用閘道器的綁定、型號與設備：照「綁定展示用閘道器與建立設備」。
9. **釋放 lab 鎖**，在重建完成之後：

   ```sh
   ssh "$DEMO_SSH_HOST" 'grep -q "^Laysi/trustiot-v2#<issue> " ~/e2e.lock/owner && rm -rf ~/e2e.lock'
   ```

第 4 步之後就只能往前做完：資料刪掉之後，只有改 `migrate.revision` 的升級會重建資料庫並重跑 Job。中途停下時展示環境中斷（服務連不上資料庫），但不會再壞下去，從停下的那一步接著做。第 4 步之前放棄時，照第 6 步恢復 Flux（不合併 PR）。

## 金鑰輪替

| 金鑰 | 怎麼輪替 |
| --- | --- |
| Resend 與 Anthropic 的 API key | 本文「外寄郵件與 Anthropic 金鑰」的「輪替 Resend 與 Anthropic 金鑰」 |
| GHCR 拉取 token | 下方「輪替 GHCR 拉取 token」 |
| Flux 讀 repo 的憑證（deploy key） | 下方「輪替 Flux 的 deploy key」 |
| 叢集的 age 金鑰 | 下方「輪替叢集的 age 金鑰」 |
| tunnel token | 本文「輪替 tunnel token」 |
| 工作站的 Access service token | 本文「Access 名單」的「工作站的 service token」 |
| certs 的簽發 CA 金鑰 | 照 [services/certs/README.md](../../services/certs/README.md)「輪替」調大 `certificateAuthority.keyGeneration`，由根的保管人以展示環境的根簽新的一代（本文「簽發 CA 的儀式」，根已經存在，不再建立）；根 CA 換新還沒有做法 |
| chart 在叢集內產生的密碼與金鑰（服務與 collector 的密碼、ClickHouse 管理帳號的密碼、內部身分簽章金鑰） | 與 SaaS 相同，在 `values-demo.yaml` 調大對應的代（`migrate.passwordGeneration`、`clickhouse.bundled.adminPasswordGeneration`、`internalIdentity.keyGeneration`），步驟與影響在[密鑰輪替 runbook](secrets.md) 的各節 |

外洩時先照密鑰輪替 runbook 的「外洩處理」止血。repo 是公開的：SOPS 檔的舊版本永遠留在 git 歷史，用當時的收件者私鑰就解得開，所以 age 金鑰外洩時，以它加密過的每一個值都要輪替。

### 輪替 GHCR 拉取 token

token 一年到期，在到期前 14 天輪替（建立的人在團隊行事曆記下提醒）。步驟照[密鑰輪替 runbook](secrets.md)「GHCR 拉取 token」：以 `ghcr-pull-secret.ts --cluster demo` 重寫 `ghcr-pull.sops.yaml`、開 PR 合併，確認 OCIRepository 與 kubelet 都拉得到之後，才在 GitHub 刪除舊 token。

- 新 token 同樣是一位組織成員的 classic token，只勾 `read:packages`，以 [deploy/flux/README.md](../../deploy/flux/README.md)「GHCR 拉取 token」第 2 步確認只有這個 scope。帶 `repo` 或任何寫入 scope 的 token 絕不放進叢集或 SOPS 檔。換人建立時，挑看得到的 repo 最少的成員。
- runbook 第 4 步的一次性 Pod 會寫入叢集，期間持有 lab 鎖。
- 個人的 token 是暫時的做法：正式的 SaaS 叢集上線（MS4）前改用有唯讀機器帳號的 registry（Laysi/trustiot-v2#502，[CI 與發佈](../architecture/ci-release.md)「GHCR 套件的可見性」）。

### 輪替 Flux 的 deploy key

展示叢集的 deploy key 由 `flux.ts deploy-key` 在展示機上產生、不離開展示機；它不取代已經存在的 Secret，所以先刪舊的再產生新的（[deploy/flux/README.md](../../deploy/flux/README.md)「在展示機安裝 Flux」的「deploy key 換新」）。從刪除到 GitHub 加上新的公鑰之間，Flux 讀不到 main、不套用新的變更，正在跑的服務不受影響。`deploy-key` 只接受空著或屬於 Laysi/trustiot-v2#467 的 lab 鎖，所以這次的鎖以 #467 為擁有者。刪掉再產生的做法尚未在展示機實測。

1. 取得 lab 鎖：`ssh "$DEMO_SSH_HOST" 'mkdir ~/e2e.lock && echo "Laysi/trustiot-v2#467 $(date -u +%FT%TZ) demo deploy-key-rotation" > ~/e2e.lock/owner && echo ACQUIRED || cat ~/e2e.lock/owner'`。
2. 外洩時，先在 GitHub（Laysi/trustiot-v2 → Settings → Deploy keys）刪除舊的 key。
3. 刪除舊的 Secret：`ssh "$DEMO_SSH_HOST" 'grep -q "^Laysi/trustiot-v2#467 " ~/e2e.lock/owner && sudo k3s kubectl -n flux-system delete secret flux-system'`。
4. 產生新的：`bun scripts/demo/flux.ts deploy-key --ssh-host "$DEMO_SSH_HOST" --lock-held`，再加 `--apply --confirm <確認碼>`。在 GitHub 加上印出的公鑰（不勾 Allow write access），確認指紋相同；有 repo 管理權限的人以 GitHub API 確認它唯讀（`read_only` 是 `true`）。
5. 立刻同步並確認：在展示機上 `sudo k3s kubectl -n flux-system annotate --overwrite gitrepository/flux-system reconcile.fluxcd.io/requestedAt="$(date +%s)"`，`flux.ts status` 的 GitRepository 是 `Ready=True`、停在 main 的 commit。
6. 在 GitHub 刪除舊的 key（第 2 步還沒刪的話），釋放 lab 鎖：`ssh "$DEMO_SSH_HOST" 'grep -q "^Laysi/trustiot-v2#467 " ~/e2e.lock/owner && rm -rf ~/e2e.lock'`。

### 輪替叢集的 age 金鑰

展示叢集的 age 私鑰在展示機上產生，經管線直接寫進 `flux-system` 的 Secret `sops-age`，不落地、不離開展示機，也沒有備份（[deploy/flux/README.md](../../deploy/flux/README.md)「展示叢集的 age 金鑰」）。新的公鑰要等私鑰產生後才知道，所以順序是：在展示機上換掉 Secret 的內容，再開 PR 把 `.sops.yaml` 的 demo 收件者換成新的公鑰。從換掉 Secret 到 PR 合併之間，Flux 解不開 SOPS 檔、不套用新的變更，正在跑的服務不受影響（與遺失時的做法相同）。換掉 Secret 的指令尚未在展示機實測。

1. 照「展示叢集的 age 金鑰」的前三段指令：在工作站建置 linux/arm64 的 age-keygen 並算出雜湊、取得 lab 鎖、把 age-keygen 送到展示機。
2. 在展示機上比對雜湊，產生新的私鑰並以 `kubectl patch` 換掉現有 Secret `sops-age` 的 `age.agekey`（不建立 namespace，其他欄位不動），再從 Secret 算一次公鑰：

   ```sh
   ssh "$DEMO_SSH_HOST" 'set -eu
     set -o pipefail
     [ -d "${XDG_RUNTIME_DIR:?}" ]
     grep -q "^Laysi/trustiot-v2#<issue> " ~/e2e.lock/owner || { echo "the lab lock is not held for this issue" >&2; exit 1; }
     K="$XDG_RUNTIME_DIR/age-keygen"
     P="$XDG_RUNTIME_DIR/age.pub"
     cleanup() { rm -f "$K" "$P"; }
     trap cleanup EXIT
     echo "<雜湊>  $K" | sha256sum -c --quiet -
     sudo k3s kubectl -n flux-system get secret sops-age -o name
     key=$("$K" 2>"$P")
     case "$key" in *AGE-SECRET-KEY-1*) ;; *) echo "age-keygen printed no key" >&2; exit 1;; esac
     b64=$(printf "%s\n" "$key" | base64 -w0)
     unset key
     printf "{\"data\":{\"age.agekey\":\"%s\"}}" "$b64" | sudo k3s kubectl -n flux-system patch secret sops-age --type=merge --patch-file=/dev/stdin
     unset b64
     printed=$(sed -n "s/^Public key: //p" "$P")
     derived=$(sudo k3s kubectl -n flux-system get secret sops-age -o "jsonpath={.data.age\.agekey}" | base64 -d | "$K" -y)
     [ -n "$derived" ]
     [ "$derived" = "$printed" ]
     echo "Public key: $derived"'
   ```

   私鑰只在 shell 變數與管線裡：`printf` 是 shell 的內建指令，不出現在行程的指令列；patch 經 stdin 交給 `kubectl patch`，不印出也不寫檔。age-keygen 失敗或沒有印出私鑰時，在換掉 Secret 之前就停下。最後兩個比對失敗時，Secret 可能已經換成壞的，Flux 同樣解不開，修好原因後重跑這一段。之後釋放 lab 鎖（「展示叢集的 age 金鑰」的最後一段指令）。
3. 開 PR：把 `.sops.yaml` demo 規則裡 `# cluster: demo` 那一行換成印出的公鑰，再由一位維運人員以自己的私鑰對每個 `deploy/flux/clusters/demo/trustiot/*.sops.yaml` 執行 `sops updatekeys -y`；外洩時再對每個檔 `sops rotate -i`，並照上面輪替其中的每一個值。合併。
4. 確認：在展示機上 `sudo k3s kubectl -n flux-system annotate --overwrite kustomization/flux-system reconcile.fluxcd.io/requestedAt="$(date +%s)"`，`flux.ts status` 的根 Kustomization 是 `Ready=True`、停在合併的 commit。

## 與 lab e2e 共用叢集

展示機的 k3s 也是 lab e2e 唯一的叢集（[deploy/flux/README.md](../../deploy/flux/README.md)「展示叢集」）。

- **同一把 lab 鎖**：lab e2e、展示機的腳本與本文的手動步驟都用展示機的 `~/e2e.lock`，以同一個 ssh 使用者登入（換一個使用者就看不到另一邊的鎖）。沒有持有鎖時，不寫叢集或主機。
- **叢集層級的元件**：會動到 Istio（istiod 的 values、`HelmChartConfig istiod`、mesh 層級的設定、Istio 的升級）、CloudNativePG（operator 與 CRD）、Gateway API（CRD 與 GatewayClass）或 Flux（CRD 與 `flux-system`）的工作，開始前先確認不影響展示環境，並在 issue 寫明怎麼確認：這些是整個叢集共用的，展示環境的入口、資料庫與部署都在上面。已知的情形：
  - 入口 e2e 以 `HelmChartConfig istiod` 加自己的 provider：名稱要用測試自己的，`extensionProviders` 要把 `trustiot-identity` 原樣複製進去，否則測試期間展示環境的入口對需要 cookie 的請求一律回 403；測完刪掉並比對 mesh（[deploy/chart/tests/e2e/README.md](../../deploy/chart/tests/e2e/README.md) 入口測試的前置條件 2）。它存在時 `platform.ts --apply` 拒絕執行。
  - 展示機上 2026-10-10 PoC 的 Istio 檔案不能再以 `kubectl apply` 套用，否則 istiod 換回 PoC 的 provider（[deploy/istio/README.md](../../deploy/istio/README.md)「安裝（k3s）」）。
  - Flux 的 lab e2e 在叢集有 Flux 時拒絕執行；要跑就得先以 `flux.ts remove` 移除展示環境的 Flux，展示環境在這段期間中斷，所以只在必要時這樣做（deploy/flux/README.md「在展示機安裝 Flux」的「移除」最後一段）。
  - CloudNativePG 的 operator 與 CRD 的變更也作用在展示環境的 Postgres；Postgres e2e 用自己的 namespace 與 Cluster，不動 operator。
  - 平台元件與 repo 釘選的版本以 `platform.ts` 的 dry run 比對；不一致時照該元件的 README 處理。
- **namespace**：測試用 `e2e-<issue>`，不用 `trustiot` 或 `flux-system`，也不刪別人的 namespace。
- **入口位址**：展示環境的入口固定在 `10.43.0.80`（Service 網段的靜態區段）；測試要指定 ClusterIP 時不用這個位址。
- **tunnel**：維運 tunnel（`cloudflared.service`，ssh 經過它）與展示用的 connector（`cloudflared-demo.service`）不停也不重啟。
- **閘道器**：lab 的閘道器兼作展示用閘道器，會中斷資料的 lab 測試要避開資料累積期與展示（[里程碑](../roadmap/milestones.md)「展示切片」的「資料面先通」）。
- **記憶體**：lab e2e 與展示環境共用展示機的記憶體。跑 e2e 或打開新服務之前，在展示機上以 `free -h` 確認放得下；各 lab e2e 需要多少記憶體由 Laysi/trustiot-v2#493 補上。

## Cloudflare 上的設定

由有 Cloudflare 權限的維運人員以 API 建立。主機名稱只在 Cloudflare 與一份不進 repo 的變更清單裡；清單逐一寫出每個 API 呼叫、需要的 API 權限、確認方式與還原方式，經使用者同意後才套用。順序：

1. remotely-managed tunnel `trustiot-demo`（`config_src: cloudflare`）。建立後只記下 `id`、`name`、`config_src` 與 `status`。
2. Access group（受邀名單）、引用它的 reusable policy（include 這個 group，require Google 的登入方式）、涵蓋三個主機名稱的 self-hosted application（只允許 Google 登入、session 24 小時、binding cookie），以及 gateway 主機名稱 `/certs/renew` 與 `/mqtt` 的 Bypass application。
3. tunnel 的路由（先回 418；入口部署後，照「架構」的「路由」的步驟，改成 `route-origin` 通過時印出的 origin，也就是展示叢集 values 固定的入口 ClusterIP；切換後讀回路由設定，再馬上執行 `check --stage 2`）：兩條略過路徑的錨定規則在前、不要求 JWT，其餘每條都設 `originRequest.access`（team 名稱與三個主機名稱那個 application 的 AUD），最後 catch-all 回 404。
4. gateway 主機名稱的 configuration rule（Browser Integrity Check 關閉）。
5. 三筆 proxied CNAME 指向 `<tunnel ID>.cfargotunnel.com`。DNS 紀錄一定在 Access application 之後建立，主機名稱不會有一段沒有保護的時間。
6. `/mqtt` 升級的速率限制（下方「`/mqtt` 的速率限制」）。展示環境部署 mqtt-bridge 之前就要套用；釘的 chart 含換發端點（Laysi/trustiot-v2#81）之前，規則也要涵蓋 `/certs/renew`（「換發端點上線」）。

token 由同一個人在 Cloudflare 的 API 執行環境裡取得並加密（見下方「安裝 connector」第 2 步），不經過其他地方。在那個執行環境裡依序：以 `GET /accounts/<帳號>/cfd_tunnel/<tunnel>` 確認名稱是 `trustiot-demo`、`config_src` 是 `cloudflare`；以 `GET …/cfd_tunnel/<tunnel>/token` 取得 token，確認它是以 `eyJhIjoi` 開頭的字串、解開後的 `t` 等於 tunnel ID；以 `crypto.subtle.exportKey("spki")` 算公鑰的 SHA-256，與 `handoff-key` 印出的相同才加密；只回傳 tunnel ID、指紋、密文與它的位元組數。

### `/mqtt` 的速率限制

2026-10-23 決定設（Laysi/trustiot-v2#600）：gateway 主機名稱的 `/mqtt` 不經 Access，任何人都能升級。mqtt-bridge 自己限制每個來源 IP 同時的連線數，並結束內層 TLS 沒有進展的連線（[services/mqtt-bridge/README.md](../../services/mqtt-bridge/README.md)「連線上限」），但擋不住同一個 IP 不斷重連：每次升級都讓 broker 做一次 TLS 交握。Cloudflare 在最前面把每個 IP 的升級次數壓住。

規則（zone 是 Free 方案，[rate limiting rules 的方案差異](https://developers.cloudflare.com/waf/rate-limiting-rules/#availability)：Free 只有一條規則、運算式只能用 Path、只能以 IP 計數、計數期間與封鎖時間都只有 10 秒）：

| 欄位 | 值 |
| --- | --- |
| 運算式 | `http.request.uri.path eq "/mqtt"`（Free 不能用主機名稱；app 與 ota 主機名稱沒有這個路徑，而且在 Access 後面） |
| 計數 | 每個 IP（API 的 `characteristics` 是 `["cf.colo.id", "ip.src"]`，`cf.colo.id` 是 API 規定必帶的，[參數說明](https://developers.cloudflare.com/waf/rate-limiting-rules/parameters/)） |
| 門檻 | 每 10 秒 40 次（`period` 10、`requests_per_period` 40） |
| 動作 | 封鎖 10 秒（`action` 為 `block`、`mitigation_timeout` 10），回 429（預設） |

門檻為什麼是 40：同一個場域的閘道器可能共用一個對外 IP，mqtt-bridge 每個副本讓一個 IP 同時有 32 條連線；滾動更新時這些閘道器一起收到 close、一起重連，40 讓它們一次連上。攻擊者每個 IP 每秒最多 4 次升級，每條連線最多佔約 11 秒（mqtt-bridge 的進展期限加上送 close 的 1 秒），要一直佔滿一個副本的 1,536 個名額每秒要約 140 次升級，約 35 個 IPv4 位址。這不是高門檻：IPv6 換位址就換了計數，擁有很多位址（例如幾個 IPv6 /48）的攻擊者做得到，那時靠 mqtt-bridge 每個桶的上限（IPv6 每個 /48 最多 128 條）與擴充副本。被封鎖的閘道器收到 429，照自己的重連間隔再試。

這條規則依賴兩個前提，改 Cloudflare 或 tunnel 的設定時要確認仍成立：

- zone 的「Normalize incoming URLs」（URL normalization）保持開啟：`/%6Dqtt` 這類編碼過的路徑先正規化成 `/mqtt` 再比對，才會被計數。
- tunnel 的錨定路由 `^/mqtt$` 比對的是解碼後的路徑：`/%2Fmqtt`、`//mqtt` 這類不等於 `/mqtt` 的路徑比對不到，走同一主機名稱其他路徑的 `originRequest.access`（沒有 JWT 回 403），到不了 mqtt-bridge，所以不會繞過計數連上它。

這條規則用掉 Free 方案唯一的一條，`/certs/renew` 也用它：釘的 chart 含換發端點（Laysi/trustiot-v2#81）之前，一定要以「Update a zone ruleset rule」把運算式改成 `(http.request.uri.path in {"/mqtt" "/certs/renew"})`、`description` 改成 `mqtt-bridge and certs renewal: /mqtt and /certs/renew per IP (Laysi/trustiot-v2#600, Laysi/trustiot-v2#81)`，其他欄位不變，共用同一個門檻（「換發端點上線」）。同一個 IP 後面的閘道器一起重連時，換發也可能被擋 10 秒，閘道器退避後重試。還原時改回上面的運算式與 `description`。

套用（有 Cloudflare 權限的維運人員，在 Cloudflare 的 API 執行環境裡，照本節開頭的規則先寫進變更清單、經使用者同意）：

1. 以「Get a zone entry point ruleset」讀 `http_ratelimit` phase 的 entry point ruleset（[以 API 建立規則](https://developers.cloudflare.com/waf/rate-limiting-rules/create-api/)）。回 404 時以「Create a zone ruleset」建立它（`kind` 為 `zone`、`phase` 為 `http_ratelimit`），規則一起放進去；已經有規則時，先確認它是不是這一條，不是就停下來問。
2. 以「Create a zone ruleset rule」把規則加進那個 ruleset，內容：

   ```json
   {
     "description": "mqtt-bridge: /mqtt upgrades per IP (Laysi/trustiot-v2#600)",
     "expression": "(http.request.uri.path eq \"/mqtt\")",
     "action": "block",
     "ratelimit": {
       "characteristics": ["cf.colo.id", "ip.src"],
       "period": 10,
       "requests_per_period": 40,
       "mitigation_timeout": 10
     }
   }
   ```

3. 讀回 ruleset，確認只有這一條規則、`enabled` 為 true、四個值與上表相同。記下規則的 `id`（還原用），只把 `id`、`version` 與四個值寫進紀錄，不寫 zone ID。
4. 確認規則有效、閘道器仍連得上：從工作站在 10 秒內連續送 41 次與「檢查對外的請求」相同的 `/mqtt` 升級請求，第 41 次起得到 Cloudflare 的 429（`server: cloudflare`；mqtt-bridge 自己的 429 body 是 `over_source_limit`，那是另一回事），等 10 秒後再送一次，不再是 Cloudflare 的 429。mqtt-bridge 部署之後，展示用閘道器的連線出現在 mqtt-bridge 的 `relaying a gateway connection` 日誌。

還原：以「Delete a zone ruleset rule」刪除記下的規則 `id`，再讀回 ruleset 確認它不在了。

### 入口的限流與 CF-Connecting-IP

入口 gateway 以 `CF-Connecting-IP` 為每個用戶端限流（Laysi/trustiot-v2#113，[chart 的 README](../../deploy/chart/README.md)「入口的限流」）。這個標頭一旦沒有送到源站，所有外部請求都算成同一個用戶端 `-`，整個展示環境很快就一直回 429，而且沒有任何地方會報錯（監看在 Laysi/trustiot-v2#955）。

1. 保持關閉：zone 的 Managed Transform「Remove visitor IP headers」會移除送往源站的 `CF-Connecting-IP`、`X-Forwarded-For` 與 `True-Client-IP`（[Managed Transforms 的說明](https://developers.cloudflare.com/rules/transform/managed-transforms/reference/#remove-visitor-ip-headers)），所以它必須維持關閉。改 Managed Transforms、或新增會改請求標頭的 Transform Rule 之前，先確認不會移除或改寫這個標頭；改完照第 2 步確認。
2. 確認源站收得到這個標頭：釘的 chart 含限流的第一次部署之後，以及第 1 步提到的任何變更之後，經真正的 tunnel 確認一次。展示環境開著 mqtt-bridge，它只信 `CF-Connecting-IP`，所以從工作站送一次「檢查對外的請求」裡同樣的 `/mqtt` 升級請求（不帶 client 憑證，到 broker 的內層 TLS 不會成功，沒有影響），然後讀 mqtt-bridge 的日誌：

   ```sh
   # 展示機上：只看最近的一行轉送或拒絕紀錄
   sudo k3s kubectl -n trustiot logs deploy/trustiot-mqtt-bridge --since=5m | grep -E 'relaying a gateway connection|refused a gateway WebSocket' | tail -n 1
   ```

   標頭有送到時，是一行 `relaying a gateway connection`，`client.address` 是工作站的對外位址；標頭沒送到時，是 `refused a gateway WebSocket`，`reason` 為 `client_ip_missing`（mqtt-bridge 的 README「來源 IP」）。紀錄與報告不要寫出位址本身，只寫「是工作站的對外位址」。後者出現時照第 3 步關閉限流，再找出移除標頭的設定。
3. 緊急關閉限流：限流的設定在 chart 裡，叢集上沒有開關可以直接改（不要在叢集上改 EnvoyFilter，Flux 下一次同步會改回來）。在 `deploy/flux/clusters/demo/trustiot/values-demo.yaml` 加上 `ingress: {rateLimit: {client: ""}}`（`ingress` 底下已有的值要保留），開 PR 合併，Flux 套用後 EnvoyFilter 被移除；或以 `scripts/flux/pin-chart.ts` 把展示叢集退回不含限流的前一版 chart。兩者都要經 PR，不是立即生效（[deploy/flux/README.md](../../deploy/flux/README.md)「入口的限流」）。

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

   印出 `cloudflared-demo.service` 的狀態與它對 Cloudflare 的連線數（`127.0.0.1:20250/ready`）、維運 tunnel 的狀態與它正在跑的執行檔（換過套件後顯示 `(deleted)`，表示下次重啟才用新版）、cloudflared 的版本與來源（一行：dnf 的 `from_repo` 是 `cloudflared-stable` 時 `OK cloudflared <版本> from cloudflared-stable`；來自別處，或讀不到時 `FAIL`，讀不到時附上 dnf 最後一行的錯誤。`from_repo` 以 root 讀：dnf5 把它存在 `/usr/lib/sysimage/libdnf5/packages.toml`，這個檔是 root 0600 時（2026-10-21 的安裝留下的，見「2026-10-21 安裝留下的 dnf 權限」），一般使用者的 `dnf repoquery` 讀不到也不報錯）、連結與標記和 `cloudflared-update` 的狀態（標記不在而 timer 啟用時 `FAIL`，連結或標記不在時 `WARN`）、token 與設定檔的擁有者與權限、`/run/trustiot-demo/` 與 `/var/lib/trustiot-demo/` 剩下的檔案、還在的安裝程式執行、token 或其中的 secret 是否出現在整個 journal（含指令列等所有欄位）或那兩個目錄（只回傳次數；由 root 在展示機上把兩者寫進 `/run` 下 0600 的 pattern 檔，只以 `grep -f` 讀，不進指令列）、工作站日誌目錄裡含 tunnel token 開頭（`eyJhIjoi`）的檔案數，以及入口的三個位址、靜態區段與比對結果（values、ConfigMap 的 overlay 與 Service，見「架構」的「路由」；入口還沒部署時是 `FAIL`）。這一項只是報告：`status` 不印 origin，切換路由前的關卡是 `route-origin`。`FAIL` 與 `WARN` 只標出問題，`status` 照樣以 0 結束，所以每一行都要讀過。Cloudflare 端以 `GET /accounts/<帳號>/cfd_tunnel/<tunnel>` 確認 `status` 為 `healthy`。

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

第 2 段在送出任何請求之前先跑 `route-origin` 的關卡（「架構」的「路由」），不通過就不探測並以非零結束。切換路由、讀回路由設定之後，馬上執行第 2 段；它失敗時，把指向入口的路由改回 `http_status:418`。第 2 段不讀路由設定，不能證明路由用的是 `route-origin` 印出的 origin，這要靠讀回的設定確認。

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

### 工作站的 service token

產線工作站工具（[factory/workstation](../../factory/workstation/README.md)）不是瀏覽器，過不了 Google 登入；Laysi/trustiot-v2#80 的實機登錄在展示環境執行，所以工作站以 Access 的 service token 通過（2026-10-23 決定，Laysi/trustiot-v2#619）。token 只讓請求過 Access，不是平台的憑證：過了 Access 之後，工作站仍要平台管理員的 session（「平台管理員的登入確認」）。Laysi/trustiot-v2#550 拿掉 Access 時一併刪除 token 與它的 policy（「拿掉 Access」）。

**建立**：由有 Cloudflare 權限的人在 Cloudflare 的管理介面做，不用 API，也不經 agent 的工具（client secret 只在建立時出現一次，經 API 建立會留在 API 的回應與執行環境裡）。[Service tokens](https://developers.cloudflare.com/cloudflare-one/access-controls/service-credentials/service-tokens/)：

1. Zero Trust → Access controls → Service credentials → Service Tokens → Create Service Token。名稱寫 `trustiot-demo-workstation`（不放主機名稱或人名）；期限選有期限的（一年），不選不會過期的。
2. 建立後頁面顯示 Client ID 與 Client Secret，secret 只顯示這一次：照下方「交付」直接寫進工作站的標頭檔。不貼進聊天、email、issue、PR、留言、工單、Cloudflare 的任何備註或欄位。離開這一頁就讀不回 secret；遺失時刪掉這個 token 重建。
3. 在涵蓋三個主機名稱的那個 self-hosted application 加一個 policy：Action 選 **Service Auth**，Include 選 **Service Token** 並只選上面這一個 token。不要用 Any Access Service Token：之後在這個帳號建立的任何 token 也會通過。Service Auth 與 Bypass 的 policy 先於 Allow 評估（[Access policies](https://developers.cloudflare.com/cloudflare-one/access-controls/policies/)）；受邀名單的 policy、binding cookie 與 session 的設定都不動。
4. Notifications 加一個 **Expiring Access Service Token** 的通知（到期前一週通知），收件人是維運人員。
5. 在不進 repo 的變更清單記下 token 的名稱、到期日、policy 的名稱與持有工作站的操作員；Client ID 與 secret 都不記。Laysi/trustiot-v2#619 的留言只寫「已建立、到期日」。

policy 掛在涵蓋三個主機名稱的 application 上，所以 token 在 gateway 與 ota 主機名稱也過得了 Access；那兩個主機名稱沒有 cookie 的路由（「架構」），持有 token 的人到得了的地方與受邀的人相同。Access 對 service token 的請求一樣在送往 tunnel 的請求加上 JWT，所以 tunnel 的 `originRequest.access` 不必改。2026-10-05 以後建立的 Zero Trust 組織預設是嚴格的 service token 模式，Access 不回 `CF_Authorization` cookie；工作站工具每個請求都帶這兩個標頭，兩種模式都能用，也不受 binding cookie 影響。

**交付**：工作站上從不登入 Cloudflare 的管理介面。建立 token 的人在自己的裝置上建立，Client ID 與 Client Secret 只顯示在那台裝置上：最直接的做法是建立的人在工作站旁，操作員看著那個畫面，在下面的提示輸入；不能同處時，用端到端加密、取用後就刪除的管道送到操作員自己的裝置，操作員看著那台裝置輸入，交付後雙方確認訊息已刪除。不經雲端剪貼簿或跨裝置同步的剪貼簿，也不存成檔案再搬。

以操作員自己的 Linux 帳號、在 bash 執行。不用編輯器，所以不留下 swap、復原或備份檔；secret 以 `read -rs` 讀，不顯示在畫面上，也不進 shell 的歷史紀錄。檔案路徑自選，不在任何 git checkout 裡：

```sh
(
  set -e
  umask 077
  d=~/.config/trustiot-workstation
  install -d -m 700 "$d"; chmod 700 "$d"
  read -r -p 'Client ID: ' id
  read -rs -p 'Client Secret: ' secret; echo
  t=$(mktemp "$d/.access-headers.XXXXXX")
  trap 'rm -f "$t"' EXIT; trap 'exit 1' INT TERM HUP   # no copy is left behind, whatever stops it
  printf 'cf-access-client-id: %s\ncf-access-client-secret: %s\n' "$id" "$secret" > "$t"
  mv -f "$t" "$d/access-headers"
)
stat -c '%a %U' ~/.config/trustiot-workstation/access-headers   # 600 與操作員自己
```

`printf` 是 bash 的內建指令，secret 不出現在任何行程的命令列；變數只在括號裡的 subshell，結束就消失。secret 先寫進 `mktemp` 建立的新檔（0600），再以 `mv` 整個換掉舊檔：直接重導向到已經存在的檔案會沿用它原來的權限，`umask` 管不到。中途中斷或 `mv` 失敗時，`trap` 刪掉這個暫存檔。內容照 factory/workstation 的「經 Cloudflare Access（`--headers-file`）」：`cf-access-client-id:` 與 `cf-access-client-secret:` 各一行。工具要求檔案是 0600、屬於執行的使用者。

這個目錄不進任何備份與同步：工作站的備份工具與雲端同步（家目錄同步、`~/.config` 的 dotfiles 同步）都把 `~/.config/trustiot-workstation` 排除，確認排除之後才寫入。懷疑檔案被備份、同步或複製出去時，當成外洩，照下方「到期、外洩與異動」刪掉重建。

**確認**：

1. 沒帶 token 的請求照樣被擋：`bun scripts/demo/tunnel.ts check --ssh-host "$DEMO_SSH_HOST" --stage 2` 通過（預期不變，見「檢查對外的請求」）。
2. 從工作站以 curl 各送一次。輸出只有狀態碼、固定的 yes/no 與 M1 錯誤的 code，從不印網址，所以不會帶出 Access 的 team 網域或 app 主機名稱。`token_headers` 與 `status_and_keys` 是「平台管理員的登入確認」開頭定義的函式，先在同一個 shell 執行那一段：

   ```sh
   u="https://$DEMO_APP_HOST/api/certs/v1/workstation-certificates"
   # 沒帶 token：302 到 Access 的登入頁
   curl -s -o /dev/null -w '%{http_code} %{redirect_url}\n' "$u" \
     | awk '{ print $1, ($2 ~ /^https:\/\/[a-z0-9-]+\.cloudflareaccess\.com\// ? "redirects-to-access=yes" : "redirects-to-access=no") }'
   # 帶 token、沒有 session：過了 Access 與 connector，入口的 cookie 檢查回 401 unauthenticated
   token_headers | status_and_keys "$u"
   ```

   第一個是 `302 redirects-to-access=yes`。第二個是 `401 "code":"unauthenticated"`（後面可能還有 message key）：請求過了 Access 與 connector 的 JWT 檢查、對到 `/api/certs` 的 cookie 路由（沒有這條路由時是入口的 404），由身分服務的 cookie 檢查拒絕。certs 自己的回應在「平台管理員的登入確認」第 4 步確認。token 經 stdin 交給 curl（`-K -`），不出現在命令列；兩個輸出都不含 token、網址與主機名稱，可以存進 Laysi/trustiot-v2#619 的留言。

**到期、外洩與異動**：

- 到期前（通知）還需要時，在 Service Tokens 選 **Refresh** 延長一年，ID 與 secret 不變。
- 外洩、或持有的操作員異動：刪掉這個 token（刪除才會讓它失效，撤銷 session 不夠），照上面重建並把 policy 改成只選新的 token。預定的交接也可以用 **Rotate**：ID 不變、產生新的 secret，舊的 secret 選立即失效。
- 不再需要時（Laysi/trustiot-v2#550，或工作站改用別的方式通過）：先刪 policy，再刪 token。

## 拿掉 Access

Laysi/trustiot-v2#113（限流）、Laysi/trustiot-v2#107（停權）、Laysi/trustiot-v2#140（AI 額度）、Laysi/trustiot-v2#148（分析護欄）與 Laysi/trustiot-v2#445（注入題庫）都在展示環境上線後：

1. tunnel 路由的 app 主機名稱拿掉 `originRequest.access`（先拿掉這一項，Access application 還在，主機名稱仍受保護）。
2. Access application 改成只涵蓋 gateway 與 ota 主機名稱（不拿掉這兩個：主機名稱對調的風險與 app 的防護無關，見「架構」），刪掉只有 app 主機名稱用到的設定。
3. 確認 app 主機名稱不再轉到 Access，登入頁由身分服務回應。tunnel、其他路由與 DNS 不變。
4. 同一批變更把 `scripts/demo/tunnel.ts` 的 `check` 對 app 主機名稱 `/` 的預期從 `access` 改成到達入口，否則 `check --stage 2` 在這一項失敗；gateway 與 ota 主機名稱的預期不變。
5. 刪掉工作站的 service token 與只給它的 Service Auth policy（「Access 名單」的「工作站的 service token」，先刪 policy 再刪 token），通知持有工作站的操作員刪掉標頭檔，工作站工具之後不再帶 `--headers-file`。

## 外寄郵件與 Anthropic 金鑰

展示環境的服務只經 SMTP 寄信（chart 的 `smtp.*`；[里程碑](../roadmap/milestones.md)「展示切片」），2026-10-19 起暫用 Resend 的 relay：`values-demo.yaml` 設 `smtp.resend.com`、587、`starttls`，帳密在 Secret `trustiot-smtp`（`username` 是 `resend`，`password` 是 Resend 的 API key），寄件人（`smtp.from`）在私有 values `trustiot-demo-values` 的 `smtp-from`。AI 閘道（Laysi/trustiot-v2#139）的 Anthropic 金鑰在 Secret `trustiot-anthropic` 的 `api-key`，閘道加入前沒有服務使用。兩個 Secret 都以 SOPS 加密放在 `deploy/flux/clusters/demo/trustiot/`（[deploy/flux/README.md](../../deploy/flux/README.md)「展示叢集」的表），第一次建立後列進同目錄的 `kustomization.yaml`。金鑰只經 stdin 交給 `scripts/flux/sops-secret.ts`，不貼到 issue、PR、聊天或指令列；主控台只顯示一次的金鑰，複製後以輸出剪貼簿的指令直接接上管線，用完清除剪貼簿。

寄件網域是上層網域本身（展示主機名稱所在的 zone）。2026-10-20 原本選了 zone 下專門寄信的子網域（Resend 建議以子網域寄信，寄件信譽與上層網域分開），2026-10-22 決定改以上層網域本身寄信。上層網域也收發公司的郵件：它的 MX、SPF 與 DMARC（`p=reject; adkim=r; aspf=r`）屬於另一個郵件服務，Resend 的紀錄不動它們（下面第 2 步）。寄件網域和展示的主機名稱一樣不寫進公開的 repo、issue 與 PR（[密鑰與權限](../architecture/secrets.md)「公開 repo 的資訊規則」），文中稱「寄件網域」。

### Resend 的寄件網域與金鑰

由有 Resend 權限的維運人員在 Resend 主控台操作；DNS 紀錄照「Cloudflare 上的設定」的做法以 Cloudflare API 建立，變更清單先經使用者同意。

1. **加入網域**：Domains → Add Domain，填寄件網域，區域選 Tokyo（`ap-northeast-1`），離展示的收件者最近。區域之後不能直接改：要刪掉網域、以新區域重新加入並換 DNS 紀錄。不論選哪個區域，帳號的資料（含寄出的信件與紀錄）都存在美國（[Regions](https://resend.com/docs/dashboard/domains/regions)）。
2. **DNS 紀錄**：照主控台列出的值，名稱都相對於寄件網域（[Cloudflare 的設定方式](https://resend.com/docs/dashboard/domains/cloudflare)）：
   - `send` 的 MX（主控台列出的退信主機，優先序 10）與 `send` 的 TXT（`v=spf1 include:<主控台列出的網域> ~all`）：Resend 寄信時信封寄件人（MAIL FROM）在 `send.<寄件網域>`，收件端的 SPF 驗的是這個名稱，兩筆都要在。上層網域自己的 MX 與 SPF 不改。
   - `resend._domainkey` 的 TXT（`p=` 加上公鑰）：DKIM，簽章的 `d=` 是寄件網域本身。
   - `_dmarc` 不加也不改：上層網域已有 DMARC 紀錄（`p=reject`），由收發公司郵件的一方管理（Resend 的說明見 [DMARC](https://resend.com/docs/dashboard/domains/dmarc)）。DKIM 的 `d=` 與 From 相同，嚴格與寬鬆對齊都成立，所以 Resend 寄出的信在 `p=reject` 下照常通過 DMARC；SPF 的 `send.<寄件網域>` 只在寬鬆對齊下對齊，所以這筆紀錄的 `aspf` 不能改成 `s`。
   - 主控台另外列出 `rsend` 的 CNAME：那是 Resend Forge（Resend 自己的寄信引擎，只在北維吉尼亞 `us-east-1` 運作）的退信網域，在主控台按 Activate Resend Forge 才會啟用（[Resend Forge](https://resend.com/docs/forge)）。展示環境不加，寄信留在 Tokyo 區域；沒有這筆時主控台的網域狀態是 `partially_failed`，Forge 的說明是驗證完成前網域照常寄信。
   - 收信用的 MX（`inbound`）不加：展示環境不收信。MX 與 TXT 都是 DNS only。

   DKIM 與 `send` 的 MX、TXT 都顯示 verified 後才寄得出去；手動加的紀錄可能要等到 72 小時。
3. **金鑰**：API Keys → Create API Key，權限選 Sending access（`sending_access`，只能寄信），Domain 選寄件網域，只能以這個網域寄（[API keys](https://resend.com/docs/dashboard/api-keys/introduction)、[Create API key](https://resend.com/docs/api-reference/api-keys/create-api-key)）。金鑰只顯示這一次，之後看不到也改不了：

   ```sh
   <輸出 API key 的指令> | bun scripts/flux/sops-secret.ts --cluster demo --name trustiot-smtp --key password --set username=resend
   ```

   產生 `deploy/flux/clusters/demo/trustiot/trustiot-smtp.sops.yaml`；開 PR（內文只寫改了哪個 Secret 與 key），合併後 Flux 套用。
4. **主控台成員**：Resend 主控台的 Emails 頁看得到每封寄出的信，含收件地址與內容（[Managing emails](https://resend.com/docs/dashboard/emails/manage-emails)），登入驗證碼（Laysi/trustiot-v2#103）上線後也包括驗證碼。所以主控台的成員只限維運人員；有人不再負責時移除他，並依 [secrets runbook](secrets.md)「何時輪替」輪替金鑰。
5. **紀錄**：網域每筆紀錄的狀態（DKIM 與 `send` 的 MX、TXT）與區域、金鑰的權限與限定網域記進測試報告，不寫網域名稱與金鑰。

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
3. 合併、Flux 套用後確認：SMTP 照下方「寄測試信」寄一封（測試 pod 每次都從 Secret 讀，直接驗證新金鑰）；Anthropic 金鑰以 AI 閘道的一次呼叫確認：閘道的 pod 滾動更新完成後，以 `kubectl -n trustiot port-forward deploy/trustiot-ai-gateway 18080:8080` 連進閘道的 pod，送 [services/ai-gateway/README.md](../../services/ai-gateway/README.md)「範例：一次最小的呼叫」的請求到 `http://127.0.0.1:18080`，角色換成 `values-demo.yaml` 裡用 Anthropic 的角色（port-forward 不經 mesh，需要展示機上的叢集管理權限；說明見 [chart README](../../deploy/chart/README.md)「AI 閘道」）。
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
