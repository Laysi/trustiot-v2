# 密鑰輪替與外洩處理

[密鑰清冊](../architecture/secrets.md) 每一項的輪替步驟，以及密鑰外洩時的處理順序（2026-10-10，Laysi/trustiot-v2#72）。docs/roadmap/risks.md 要求安全關鍵設施一開始就寫 runbook，不能只有一個人懂。

還沒實作的元件只寫原則與順序，負責的 issue 實作時把確切指令補進對應小節（小節裡寫著是哪一張）。

## 何時輪替

- **外洩或疑似外洩**：立即，依「外洩處理」。
- **能讀到它的人離開**：維運人員離職或不再負責時，輪替他能讀到的每一項（SOPS 檔案的所有值、他保管的離線備份）。只把他從 SOPS 的收件者移除不夠：他之前解密過的值仍然有效。
- **到期**：有到期日的 token（GHCR 拉取 token）在到期前 14 天輪替；憑證依 CA 的換發流程（Laysi/trustiot-v2#78）。
- **定期**：其餘長期金鑰（第三方 API key、內部身分簽章金鑰、SOPS 叢集私鑰）每年至少輪替一次，順便確認本 runbook 仍然可用；每年檢查一次清冊是否與實際一致。

每次輪替在對應的 issue 或維運紀錄留下：日期、哪一項、誰做的、舊值何時撤銷。不寫任何值。

## 準備

- 工具：`sops` 與 `age`（版本由 Laysi/trustiot-v2#70 鎖進 `.prototools`，以 `proto install` 安裝）。
- 執行者要是 SOPS 的收件者之一：自己的 age 私鑰在自己的裝置上，以 `SOPS_AGE_KEY_FILE` 指向它。
- 在 SaaS 主機上操作 Kubernetes 時，經 Cloudflare Access 的維運 SSH 登入，以 `sudo k3s kubectl` 執行。GitHub 不持有叢集憑證，所以這些步驟不能交給 CI。
- 新的值只經由檔案、stdin 或編輯器輸入，不寫在指令列（[規則 3](../architecture/secrets.md)）。複製到剪貼簿的值用完即清除。

## 通用步驟：SaaS 以 SOPS 管理的 Secret

適用於第三方發的金鑰：LLM、embedding、rerank 的 API key，Qdrant 的兩把 key，物件儲存與備份用的存取金鑰，SMTP 帳密，Sentry DSN，GHCR 拉取 token，以及之後同樣放在 `deploy/flux/` 的 Secret。

1. **先產生新值，舊值保留**：在供應商的主控台建立新的 key。供應商允許同時有兩把有效 key 時，舊的先不刪，切換期間服務不中斷。
2. **改 SOPS 檔案**：`sops edit deploy/flux/<路徑>.sops.yaml`，在編輯器裡換掉值後存檔，sops 自動重新加密。不要用 `sops set`，它把值放在指令列。
3. **讓使用它的 pod 重啟**：Secret 以環境變數注入，Kubernetes 不會因 Secret 改變而重啟 pod。在同一個 PR 裡，把 SaaS 安裝實例覆寫檔中該服務的 `services.<name>.podAnnotations` 加上或更新 `trustiot.io/secret-rotated: "<YYYY-MM-DD>"`（每個服務都用到的，例如 Sentry DSN，改 `serviceDefaults.podAnnotations`），Flux 套用時 pod 會滾動更新。
4. **開 PR 並合併**：diff 只會看到 `ENC[...]`、`sops.lastmodified`、`sops.mac` 與 annotation 的變化，看不到值。CI 的 `check-secret-handling.ts` 會擋下沒有加密的 Secret。
5. **確認套用**：Flux 套用後（`flux get kustomizations` 的 READY 為 True），在主機上 `sudo k3s kubectl -n trustiot rollout status deploy/<release>-<服務>`，再確認服務健康、實際呼叫外部服務成功（例如 AI 閘道的一次呼叫、寄一封測試信）。
6. **撤銷舊值**：在供應商的主控台刪除或停用舊 key。
7. **記錄**：依上方「何時輪替」最後一段。

切換失敗時 revert 該 PR，舊 key 還沒撤銷，服務回到舊值。

## 通用步驟：地端

地端的 Secret 由客戶建立與保管，chart 只認名稱與 key。客戶輪替時：

1. 以檔案更新 Secret，不用 `--from-literal`：
   `kubectl -n <namespace> create secret generic <名稱> --from-file=<key>=<檔案> --dry-run=client -o yaml | kubectl apply -f -`
2. `kubectl -n <namespace> rollout restart deploy/<release>-<服務>`，再 `rollout status` 確認完成。
3. 撤銷舊值。

客戶使用 External Secrets Operator 等工具時，依該工具的方式更新後同樣要重啟使用它的服務。

## 各項密鑰

### 第三方 API key、SMTP、Sentry DSN、物件儲存

依「通用步驟：SaaS」。補充：

- **物件儲存**：服務用的與備份用的是不同金鑰，分開輪替；備份用的金鑰換完後，確認下一次備份成功（Laysi/trustiot-v2#134、Laysi/trustiot-v2#135 補上確認指令）。
- **Qdrant**：唯讀與寫入兩把 key 分開輪替，換完以知識庫的一次檢索與一次背景處理確認（Laysi/trustiot-v2#194）。
- **Sentry DSN**：所有服務都用到，以 `serviceDefaults.podAnnotations` 讓全部 pod 滾動。

### GHCR 拉取 token

GHCR 只接受 classic personal access token，所以以專用機器帳號建立，只勾 `read:packages`，並設定到期日。

1. 以機器帳號建立新 token（到期日最長一年），在團隊行事曆記下到期前 14 天的提醒。
2. 依「通用步驟：SaaS」更新 `deploy/flux/` 內的 `kubernetes.io/dockerconfigjson` Secret（Laysi/trustiot-v2#70 補上檔名）。
3. 確認 Flux 拉得到：`flux reconcile source oci <名稱>`（chart 以 HelmRepository 取得時用 `flux reconcile source helm <名稱>`；名稱由 Laysi/trustiot-v2#70 補上），READY 為 True。每次 reconcile 都以這個 Secret 向 GHCR 重新認證，不靠快取。
4. 確認 kubelet 拉得到：節點上已有快取的映像不會重新拉取，所以另起一個 `imagePullPolicy: Always` 的一次性 Pod，強迫 kubelet 以新 token 向 GHCR 認證。把下列內容存成 `ghcr-pull-check.yaml`（填入 Secret 名稱與目前版本的任一個服務映像）：

   ```yaml
   apiVersion: v1
   kind: Pod
   metadata:
     name: ghcr-pull-check
     namespace: trustiot
   spec:
     restartPolicy: Never
     imagePullSecrets:
       - name: <拉取 token 的 Secret>
     securityContext:
       runAsNonRoot: true
       runAsUser: 65532
       seccompProfile:
         type: RuntimeDefault
     containers:
       - name: check
         image: ghcr.io/laysi/trustiot-v2/<服務>:<目前版本>
         imagePullPolicy: Always
         securityContext:
           allowPrivilegeEscalation: false
           readOnlyRootFilesystem: true
           capabilities:
             drop: [ALL]
   ```

   `sudo k3s kubectl apply -f ghcr-pull-check.yaml`，再以 `sudo k3s kubectl -n trustiot get events --field-selector involvedObject.name=ghcr-pull-check` 確認有 `Pulled`（Successfully pulled image），沒有 `ErrImagePull` 或 `401`。容器啟動後因為沒有設定而結束是正常的，這裡只確認拉取。最後 `sudo k3s kubectl -n trustiot delete pod ghcr-pull-check`。
5. 兩項都確認後，刪除舊 token。

### 內部身分簽章金鑰

chart 的欄位（Laysi/trustiot-v2#61，說明在 [deploy/chart/README.md](../../deploy/chart/README.md) 的「內部身分」）：

- **公鑰**：`internalIdentity.publicKeys`，每把 `{kid, x}`，`x` 是 32 bytes 公鑰的 base64url。chart 組成 JWK Set 放進平台設定 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`（ConfigMap）；ConfigMap 改變時每個服務都會滾動更新。公鑰不是祕密，SaaS 寫在安裝實例覆寫檔，不必加密。
- **私鑰**：`internalIdentity.signingKeySecret` 指向的 Secret（`name`；`key` 預設 `signing-key.pem`），內容是 PKCS#8 PEM，只以唯讀檔案掛進身分服務的 `/etc/trustiot/identity-signing-key/signing-key.pem`，檔案權限 0440，只有 root 與 pod 的 `fsGroup`（`services.identity.podSecurityContext.fsGroup`，預設 65532；OpenShift 設成 `null` 由 SCC 指派）讀得到。SaaS 的這個 Secret 以 SOPS 加密。
- **kid 不另外設定**：身分服務啟動時用私鑰對 `publicKeys` 的每個 kid 簽一個測試 token 再驗證，找出自己的 kid。私鑰對應的公鑰不在 `publicKeys` 裡時，新的 pod 拒絕啟動，舊的 pod 繼續服務。身分服務只在啟動時讀私鑰，Secret 改變不會讓它重啟。

為什麼要依這個順序、等多久，見 [crates/service-kit/README.md](../../crates/service-kit/README.md) 的「公鑰與輪替」與 [services/identity/README.md](../../services/identity/README.md) 的「金鑰輪替」。SaaS 的三個步驟各是一個 PR：

1. **加新公鑰**：產生新的金鑰對，把新公鑰以新的 kid（例如 `2027-04-a`）加進覆寫檔的 `internalIdentity.publicKeys`，新舊並存。合併後確認每個服務都滾動完成：每個 Deployment 都跑 `sudo k3s kubectl -n trustiot rollout status`。
2. **換簽章私鑰**：以 `sops edit` 把 `signingKeySecret` 指向的 Secret 換成新私鑰。同一個 PR 更新 `services.identity.podAnnotations` 的 `trustiot.io/secret-rotated`，讓身分服務重啟。確認 `deploy/<release>-identity` 的 rollout 完成，而且身分服務的啟動日誌 `signing internal identity tokens` 記的 kid 是新的。
3. **移除舊公鑰**：身分服務滾動完成後至少等 305 秒（驗證端接受的最長效期 300 秒加 5 秒時鐘誤差），再從 `publicKeys` 移除舊公鑰。合併並滾動完成後，刪除舊私鑰的檔案與備份。

地端由客戶操作，順序相同：

- 公鑰：改 values 後 `helm upgrade`。
- 私鑰：照「通用步驟：地端」換掉 Secret 的值，再 `kubectl -n <namespace> rollout restart deployment/<release>-identity`。

私鑰外洩時，三個步驟在同一次變更裡完成，不等舊 token 過期。滾動期間有一部分請求會被拒絕（401），換取外洩的金鑰馬上失效。

公鑰的 `x` 由私鑰檔算出：`openssl pkey -in <檔案> -pubout -outform DER | tail -c 32 | basenc --base64url | tr -d "="`。

產生金鑰對時私鑰直接寫進檔案（例如 `openssl genpkey -algorithm ed25519 -out <檔案>`），由檔案貼進 `sops edit`，用完刪除檔案。

### Cloudflare Tunnel token

在 `deploy/host/README.md`（Laysi/trustiot-v2#64）。原則：token 檔案只有 root 能讀，以 systemd `LoadCredential` 交給 cloudflared；換 token 後重啟 cloudflared，確認網站與維運 SSH 都恢復，再於 Cloudflare 撤銷舊 token。維運 SSH 也經這條 tunnel，所以先確認有其他進入主機的方式（例如實體主控台）再操作。

### CA 私鑰

Laysi/trustiot-v2#78 補上確切指令。原則：

- **根 CA 私鑰**：只在簽發或撤銷簽發 CA 時取出，在離線的機器上操作，完成後確認兩份離線備份都還能解密。根 CA 換新是重大變更：所有閘道器的信任設定都要更新，另開 issue 規劃。
- **簽發 CA 私鑰**：以根 CA 簽一張新的簽發 CA 憑證，新舊兩張同時列在信任清單；certs 服務改用新的簽，舊的簽發 CA 簽過的憑證全部到期或換發後，才移除舊的簽發 CA 並撤銷它。
- **外洩**：立即撤銷該簽發 CA、發新 CRL，所有它簽過的憑證要求換發（決策 12 的換發端點，閘道器不必寄回）。

### 叢集內 ClickHouse 的管理帳號密碼

Secret `trustiot-clickhouse-admin`（key `password`）。ClickHouse 只在啟動時讀這個密碼，所以換值之後要讓它重啟：

1. 產生新值：`openssl rand -hex 32`。SaaS 以 `sops edit deploy/flux/<檔名>.sops.yaml` 換掉 `password`；地端由客戶換值，值經管線傳入、不出現在命令列：`openssl rand -hex 32 | tr -d "\n" | kubectl -n trustiot create secret generic trustiot-clickhouse-admin --from-file=password=/dev/stdin --dry-run=client -o yaml | kubectl apply -f -`。
2. 同一個變更把 values 的 `clickhouse.bundled.adminSecretRevision` 加一。升級時 ClickHouse 的 pod 重啟並換上新密碼，migration Job 也拿新密碼連線。
3. 確認 ClickHouse 的 `/ping` 探針恢復、migration Job 完成；舊密碼不再能登入（以 `clickhouse-client --ask-password` 輸入舊值，確認被拒）。

### 叢集內產生的帳號

Postgres（CloudNativePG）、ClickHouse、MQTT dynsec、Grafana 的帳號由叢集內產生，值不經過人。各自的 issue 補上指令（Laysi/trustiot-v2#58、Laysi/trustiot-v2#59、Laysi/trustiot-v2#84、Laysi/trustiot-v2#183）。原則：在叢集內重新產生值、更新資料庫裡的帳號、重啟使用它的服務，三步在一次維護中完成；只有懷疑外洩或人員異動時才需要輪替。

### OTA 簽章私鑰

Laysi/trustiot-v2#164 補上。原則：閘道器的 updater 內建公鑰，所以輪替要先發一版同時信任新舊公鑰的 updater，確認全部閘道器都已更新，才改用新私鑰簽章。

### SOPS 叢集私鑰

1. 在可信的機器上 `age-keygen -o <新檔案>`，記下輸出的公鑰。
2. 在 `.sops.yaml` 的收件者加入新公鑰（舊的先保留），對 `deploy/flux/` 下每個 SOPS 檔案執行 `sops updatekeys -y <檔案>`，開 PR 合併。
3. 在主機上更新叢集的 Secret，私鑰從檔案經 stdin 傳入：
   `sudo k3s kubectl -n flux-system create secret generic sops-age --from-file=age.agekey=/dev/stdin --dry-run=client -o yaml < <新檔案> | sudo k3s kubectl apply -f -`
4. 確認 Flux 仍能解密（`flux reconcile kustomization <名稱> --with-source` 後 READY 為 True）。
5. 從 `.sops.yaml` 移除舊公鑰，再對每個檔案 `sops updatekeys -y`，並以 `sops rotate -i <檔案>` 換掉資料金鑰，開 PR 合併。
6. 更新離線備份，銷毀舊私鑰與 `<新檔案>`。

### 維運人員加入與離開

- **加入**：新成員以 `age-keygen` 產生自己的金鑰對，只交出公鑰。把公鑰加進 `.sops.yaml`，對每個 SOPS 檔案 `sops updatekeys -y`，開 PR 合併。
- **離開**：從 `.sops.yaml` 移除他的公鑰，對每個檔案 `sops updatekeys -y` 與 `sops rotate -i`；接著依各小節輪替他能解密的每一項密鑰、他保管的離線備份與 SOPS 叢集私鑰的備份；撤銷他的 Cloudflare Access 權限、主機上的 SSH 公鑰與 GitHub 權限。

## 外洩處理

1. **先止血**：能撤銷的立即撤銷或停用（供應商主控台、CA 撤銷、GitHub token 刪除），接受短暫中斷；無法立即撤銷的，立即依上方步驟輪替。
2. **找出範圍**：確認外洩管道（git 歷史、日誌、映像層、CI 紀錄、聊天紀錄）。進了 git：gitleaks 會在 CI 標出檔案與 commit；不論是否改寫歷史，都要輪替，因為 clone 與 fork 已經有副本。進了映像：刪除該版映像並輪替。
3. **檢查是否被使用**：供應商的用量與存取紀錄、服務日誌、資料庫的操作紀錄。
4. **補上防線**：找出規則或檢查為什麼沒有擋下，修正 `scripts/ci/check-secret-handling.ts`、chart 測試或本文件，與事件處理同一批變更。
5. **記錄**：開一張帶 `security` label 的 issue，寫時間線、影響與處理，不寫密鑰的值。

## GitHub 的 Actions 設定

每年與人員異動時，依 [CI 與發佈](../architecture/ci-release.md#github-的-actions-設定) 列出的指令重新確認 repo 的 Actions 設定沒有被改動。
