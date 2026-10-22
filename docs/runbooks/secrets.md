# 密鑰輪替與外洩處理

[密鑰清冊](../architecture/secrets.md) 每一項的輪替步驟，以及密鑰外洩時的處理順序（2026-10-10，Laysi/trustiot-v2#72）。docs/roadmap/risks.md 要求安全關鍵設施一開始就寫 runbook，不能只有一個人懂。

還沒實作的元件只寫原則與順序，負責的 issue 實作時把確切指令補進對應小節（小節裡寫著是哪一張）。

## 何時輪替

- **外洩或疑似外洩**：立即，依「外洩處理」。
- **能讀到它的人離開**：維運人員離職或不再負責時，輪替他能讀到的每一項（SOPS 檔案的所有值、他保管的離線備份）。只把他從 SOPS 的收件者移除不夠：他之前解密過的值仍然有效。
- **到期**：有到期日的 token（GHCR 拉取 token）在到期前 14 天輪替；憑證依 CA 的換發流程（Laysi/trustiot-v2#78）。
- **定期**：其餘長期金鑰（第三方 API key、內部身分簽章金鑰、SOPS 叢集私鑰）每年至少輪替一次，順便確認本 runbook 仍然可用；每年檢查一次清冊是否與實際一致。內部身分簽章金鑰由叢集依金鑰年齡自動輪替（預設 90 天，Laysi/trustiot-v2#429），不必排進人的定期輪替；每年檢查時確認輪替的 CronJob 最近有成功執行（下方「內部身分簽章金鑰」的「看現況」）。

每次輪替在對應的 issue 或維運紀錄留下：日期、哪一項、誰做的、舊值何時撤銷。不寫任何值。

## 準備

- 工具：`sops`（`.prototools` 釘選，`proto install sops`）與 age（`bun scripts/flux/age.ts age-keygen ...` 或 `bun scripts/flux/age.ts age ...`：以 `.prototools` 釘選的 Go 從原始碼建置，由 Go checksum database 驗證，見 [deploy/flux/README.md](../../deploy/flux/README.md) 的「祕密」）。下文的 `age-keygen` 指的就是 `bun scripts/flux/age.ts age-keygen`。
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
- **SMTP（暫用 Resend）**：新 key 的權限選只能寄信（`sending_access`）並限定寄件網域，不選完整權限（`full_access`）；帳號固定是 `resend`，只換密碼。
- **展示環境的 SMTP 與 Anthropic 金鑰**：與 SaaS 分開，步驟（含 Anthropic 工作區的用量上限與換完後的測試信）見 [展示環境 runbook](demo-environment.md)「輪替 Resend 與 Anthropic 金鑰」。

### GHCR 拉取 token

GHCR 只接受 classic personal access token。2026-10-21 決定不另建機器帳號：只有展示叢集用這把 token，由一位組織成員建立，只勾 `read:packages`，一年到期（為什麼與外洩範圍見 [deploy/flux/README.md](../../deploy/flux/README.md)「GHCR 拉取 token」）。SaaS 叢集不放個人的 token，拉取憑證等 Laysi/trustiot-v2#502。

1. 由一位組織成員建立新 token（只勾 `read:packages`，到期日一年），在團隊行事曆記下到期前 14 天的提醒；以 README 第 2 步確認 token 只有 `read:packages` 一個 scope。
2. 重新產生 `deploy/flux/clusters/demo/trustiot/ghcr-pull.sops.yaml`（Secret `ghcr-pull`，namespace `trustiot`），token 只經 stdin：`<輸出 token 的指令> | bun scripts/flux/ghcr-pull-secret.ts --username <GitHub 帳號> --cluster demo`。整份檔案換新，所以不必 `sops edit`；開 PR 合併。這把 token 只在展示叢集，以下第 3、4 步都在展示機上執行（存取方式見[展示環境 runbook](demo-environment.md)），不是 SaaS 主機。
3. 確認 Flux 拉得到：合併後在展示機上以 `sudo k3s kubectl -n trustiot annotate --overwrite ocirepository/trustiot reconcile.fluxcd.io/requestedAt="$(date +%s)"` 立刻觸發，再 `sudo k3s kubectl -n trustiot get ocirepository trustiot`，READY 為 True。每次 reconcile 都以這個 Secret 向 GHCR 重新認證，不靠快取。
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

   Secret 名稱是 `ghcr-pull`。`sudo k3s kubectl apply -f ghcr-pull-check.yaml`，再以 `sudo k3s kubectl -n trustiot get events --field-selector involvedObject.name=ghcr-pull-check` 確認有 `Pulled`（Successfully pulled image），沒有 `ErrImagePull` 或 `401`。容器啟動後因為沒有設定而結束是正常的，這裡只確認拉取。最後 `sudo k3s kubectl -n trustiot delete pod ghcr-pull-check`。
5. 兩項都確認後，刪除舊 token。

### 內部身分簽章金鑰

身分服務簽內部身分 token 的 Ed25519 金鑰在叢集內產生、公開與輪替，沒有人建立、看到或複製它（Laysi/trustiot-v2#404；設計、每個失敗情況與為什麼這樣做見 [docs/architecture/secrets.md](../architecture/secrets.md) 的「內部身分簽章金鑰」，欄位見 [deploy/chart/README.md](../../deploy/chart/README.md) 的「內部身分」）：

- 身分服務 pod 的 init container `signing-keys` 在每次身分服務的 pod 啟動時執行，把私鑰放在 Secret `<release>-identity-signing-keys`，公鑰的 JWK Set 放在 ConfigMap `<release>-internal-identity-keys`。只有身分服務的 ServiceAccount 能讀寫這兩個物件，token 只掛進 init container 與下面的 CronJob。
- CronJob `<release>-identity-key-rotation`（Laysi/trustiot-v2#429）依 `internalIdentity.keyRotationSchedule`（預設每天 03:17 UTC）以同一個 ServiceAccount 執行同一個指令：金鑰公開超過 `internalIdentity.maxKeyAgeSeconds`（預設 90 天）時自動輪替，也移除 token 都已過期的舊金鑰。`maxKeyAgeSeconds` 是 0 時沒有這個 CronJob。
- 每個服務掛這個 ConfigMap，每 5 秒重讀，換公鑰不重啟；身分服務也每 5 秒重讀掛載的 Secret。
- 下面的 `<namespace>` 與 `<release>` 在 SaaS 是 `trustiot`，指令在主機上以 `sudo k3s kubectl` 執行。值只有 kid（公鑰的 thumbprint）與代，不是祕密；不要讀 Secret 的 `data`。

**看現況**：

- 公開的公鑰：`kubectl -n <namespace> get configmap <release>-internal-identity-keys -o jsonpath='{.data.jwks\.json}'`，每把的 `kid`。
- init container 最近一次做了什麼：`kubectl -n <namespace> logs deploy/<release>-identity -c signing-keys`，最後一行 `the internal identity signing keys are in place` 列出每把的 kid、正在簽章的 kid（`signing`）與最早可以移除舊金鑰的時間（`retire_at`，Unix 時間）。
- 輪替的 CronJob：`kubectl -n <namespace> get cronjob <release>-identity-key-rotation`（`LAST SCHEDULE`、`LAST SUCCESSFUL`）與 `kubectl -n <namespace> get jobs -l app.kubernetes.io/component=identity-key-rotation`（保留最近一個成功、三個失敗的 Job）。最近一次做了什麼：`kubectl -n <namespace> logs job/<job>`，最後一行同樣是 `the internal identity signing keys are in place`，多寫下一次到期的時間（`rotation_due_at`，Unix 時間）；輪替那次另有 `generated signing key <kid> (generation <代>) to replace key <kid>, published <時間>, which reached internalIdentity.maxKeyAgeSeconds`。
- 身分服務正在用哪一把簽：`kubectl -n <namespace> logs deploy/<release>-identity -c identity | grep 'signing internal identity tokens'`，最後一行的 `kid`。
- 某個服務的 pod 用哪幾把驗證：`kubectl -n <namespace> logs <pod> | grep 'verifying internal identity tokens with these public keys'`，最後一行的 `keys`。

**自動輪替**（`maxKeyAgeSeconds` 不是 0，預設）：不必做任何事。金鑰公開超過 `maxKeyAgeSeconds` 之後的第一次 CronJob 執行加一把同一代的新金鑰，照下面手動輪替第 2 到 4 步的同一套排程換掉舊的（先公開、`keyPropagationSeconds` 後才簽章、token 過期後移除），不重啟任何 pod。代不變，所以手動輪替與撤銷照常以 values 的 `keyGeneration` 加一。

**手動輪替**（想立刻換，或關掉了自動輪替）：

1. 把 values 的 `internalIdentity.keyGeneration` 加一（SaaS 改 `deploy/flux/clusters/saas/trustiot/values-instance.yaml`，地端改安裝實例的覆寫檔），升級。只有身分服務滾動重啟；第一個新 pod 的 init container 產生新金鑰，先公開公鑰，記下新金鑰在 `internalIdentity.keyPropagationSeconds`（預設 600 秒）之後才開始簽章。
2. 確認 init container 的日誌有 `generated signing key <kid> (generation <新的代>)` 與 `published the public keys …`，ConfigMap 同時列出新舊兩把。
3. `keyPropagationSeconds` 之後，身分服務每個副本的日誌有 `signing internal identity tokens` 與新的 kid。服務不必做任何事：它們已經在重讀檔案時換上了新舊兩把。
4. 舊金鑰在它簽的 token 全部過期之後（新金鑰開始簽章再過 `keyPropagationSeconds` 加 305 秒）的下一次身分服務啟動或輪替 CronJob 的下一次執行（預設一天內）時移除。想當場移除時，過了 init container 日誌寫的 `retire_at` 之後執行 `kubectl -n <namespace> rollout restart deployment/<release>-identity`；不移除也不影響安全，舊私鑰沒有外洩。

代只往上走：`helm rollback` 或 Flux 升級失敗後的 rollback 渲染出較小的值時，init container 不產生、也不換金鑰（身分服務因為 pod template 改變而重啟一次），之後再升級回來也不換；自動輪替照樣以 Secret 裡最高的代加金鑰。

**自動輪替失敗或沒在跑**：失敗不影響服務，現有的金鑰繼續簽章。

- Job 失敗（`kubectl get jobs` 的 `STATUS` 是 `Failed`）：`kubectl -n <namespace> logs job/<job>` 的 ERROR `the internal identity signing keys are not in place` 寫出原因，處理方式與 init container 相同（例如 Secret 被刪見下方「Secret 遺失」）。修好之後等下一次排程，或立刻執行一次：`kubectl -n <namespace> create job <release>-identity-key-rotation-manual --from=cronjob/<release>-identity-key-rotation`，完成後刪掉這個 Job。
- CronJob 沒在跑（被 `suspend`、映像拉不到、控制器停擺）時沒有 Job 失敗。身分服務在正在簽章的金鑰公開超過 `maxKeyAgeSeconds + keyPropagationSeconds + 2 天` 時記一次 ERROR `the internal identity signing key is older than internalIdentity.maxKeyAgeSeconds allows …`（日誌管線會送進 SIEM）；看 `kubectl -n <namespace> describe cronjob <release>-identity-key-rotation` 的事件與 `.spec.suspend`。
- 要關掉自動輪替：`internalIdentity.maxKeyAgeSeconds: 0` 再升級（CronJob 被移除，身分服務滾動重啟一次，不換金鑰），之後改回手動輪替。

**私鑰外洩（撤銷）**：

1. 把 `internalIdentity.revokeBeforeGeneration` 設成新的一代，`keyGeneration` 調到不小於它（例如目前是 3：兩者都設成 4；chart 檢查 `keyGeneration` 不小於 `revokeBeforeGeneration`），升級。init container 從 Secret 與 ConfigMap 移除代比它小的每一把，新金鑰立即開始簽章；新的身分服務 pod 從環境變數拿到這個值，不會用被撤銷的金鑰簽章，在 kubelet 把新金鑰交給它之前不就緒。
2. 等身分服務滾動完成：`kubectl -n <namespace> rollout status deployment/<release>-identity`。這時每個身分服務的副本都只用新金鑰簽章。
3. 逐一確認每個驗證內部身分的服務 pod（`kubectl -n <namespace> get pods`，身分服務與每個 TrustIoT 服務）都換上了新的公鑰：上面「看現況」的最後一行 `keys` 不再有被撤銷的 kid。kubelet 正常在 1 到 2 分鐘內更新每個 pod 的檔案（2026-10-15 lab 的 k3s 實測 3.9 到 84.4 秒，[測試報告](../test-reports/2026-10-15-404-identity-keys-in-cluster.md)），但沒有保證的上限。超過幾分鐘還沒換的 pod 直接刪掉（`kubectl -n <namespace> delete pod <pod>`），或 `kubectl -n <namespace> rollout restart deployment/<release>-<服務>`，再對取代它的 pod 做同樣的檢查。每個 pod 都換過之前，外洩的金鑰簽的 token 仍可能被還沒換的 pod 接受。
4. `revokeBeforeGeneration` 之後留著不改，也不會再撤銷什麼（之後的代都不比它小）。依「外洩處理」找出私鑰怎麼外洩：能讀它的只有身分服務的 ServiceAccount、排了身分服務 pod 或輪替 CronJob 的 pod 的節點，以及 namespace 裡能讀 Secret 的主體。自動輪替產生的金鑰與它同一代，撤銷照樣移除它們。

撤銷期間已更新與還沒更新的 pod 認的金鑰不同，一部分請求會被拒絕（401），換取外洩的金鑰盡快失效。

**Secret 遺失**（有人刪了 Secret 或它的 key）：已經在跑的身分服務保留記憶體裡最後一組可用的金鑰繼續簽章。新的身分服務 pod 的 init container 失敗並寫出處理方式，pod 停在 `Init`，舊 pod 繼續服務。處理：把 `internalIdentity.keyGeneration` 加一再升級；Helm 重建空的 Secret，init container 把它當成資料遺失，產生新金鑰並立即開始簽章，ConfigMap 只剩新公鑰。舊私鑰沒了，所以這個情況沒有重疊期：kubelet 更新各 pod 的檔案之前（1 到 2 分鐘），一部分請求被拒絕（401）。不要自己建立這個 Secret：init container 不信任不是它填過的物件（uid 不同），會把裡面的金鑰當成資料遺失丟掉。

**ConfigMap 被改或刪掉**：服務遇到刪掉或改壞的內容時保留上一組，改成另一組合法的公鑰時換上它。身分服務比對它的金鑰與看到的公鑰，不一致超過 `keyPropagationSeconds` 時記 ERROR（`the public keys every service reads have differed from the signing keys …`）。處理：`kubectl -n <namespace> rollout restart deployment/<release>-identity`，init container 依 Secret 重寫 ConfigMap；ConfigMap 被刪掉時先升級一次（Helm 重建空的 ConfigMap）再重啟。能改這個 ConfigMap 的主體能讓服務接受自選的公鑰，所以有人改過它，要當成可能的入侵處理（「外洩處理」），並撤銷金鑰。

**從人建立的私鑰搬過來**（Laysi/trustiot-v2#404 之前的安裝：values 設了 `internalIdentity.signingKeySecret` 與 `internalIdentity.publicKeys`）。三步各是一次升級（SaaS 各一個 PR），每一步都不中斷服務：

1. 升級到新版 chart，`signingKeySecret` 與 `publicKeys` 先不動。init container 以原本的 kid、代 0 匯入那把私鑰，日誌有 `imported the signing key of internalIdentity.signingKeySecret as key <kid>`；服務改從 ConfigMap 讀公鑰，裡面就是這一把。確認每個 Deployment 都 `rollout status` 完成。
2. 從 values 移除 `signingKeySecret` 與 `publicKeys` 再升級：`keyGeneration`（至少 1）大於匯入的代 0，init container 產生新金鑰，照上面的定期輪替換掉匯入的那一把。
3. ConfigMap 不再有匯入的 kid 之後（上面第 4 步），刪掉人建立的 Secret：地端 `kubectl -n <namespace> delete secret <名稱>`；SaaS 刪掉 `deploy/flux/` 裡它的 SOPS 檔與 kustomization 的引用。

SaaS 目前沒有人建立的這個 Secret（`deploy/flux` 的 values 沒有設定內部身分），不必搬。

### Cloudflare Tunnel token

在 `deploy/host/README.md`（Laysi/trustiot-v2#64）。原則：token 檔案只有 root 能讀，以 systemd `LoadCredential` 交給 cloudflared；換 token 後重啟 cloudflared，確認網站與維運 SSH 都恢復，再於 Cloudflare 撤銷舊 token。維運 SSH 也經這條 tunnel，所以先確認有其他進入主機的方式（例如實體主控台）再操作。

展示環境的 tunnel token（展示機的 `cloudflared-demo.service`，Laysi/trustiot-v2#466）在[展示環境 runbook](demo-environment.md)「輪替 tunnel token」：新 token 以 RSA-OAEP 交付給展示機，再以 `scripts/demo/tunnel.ts install` 換上。展示機的維運 SSH 走另一條 tunnel，不受影響。

### CA 私鑰

確切指令在 [services/certs/README.md](../../services/certs/README.md)（Laysi/trustiot-v2#78）：「發簽發 CA 憑證」、「輪替」、「根 CA 私鑰的備份」與「簽發 CA 金鑰遺失或外洩」。原則：

- **根 CA 私鑰**：只在簽發 CA 的儀式與備份檢查時取出，在離線的機器上以 `certs-ca` 操作（私鑰只經 stdin、stdout 與 age），完成後兩位保管人各自以 `certs-ca root check` 確認自己那一份離線備份還能解密、是這張根憑證的私鑰。根 CA 換新是重大變更：所有閘道器的信任設定都要更新，另開 issue 規劃。
- **簽發 CA 私鑰**：在叢集內產生，人不碰。輪替是調大 `certificateAuthority.keyGeneration`，以根 CA 簽新一代的 CSR，新舊兩張同時列在 `certificateAuthority.issuingCertificates`；最新的一代簽新憑證，舊的一代簽過的憑證全部到期後，以 `removeKeysBeforeGeneration` 移除它。
- **外洩**：`keyGeneration` 與 `removeKeysBeforeGeneration` 同時調到新的一代：舊金鑰立即移除，它的 CRL 不再發布，broker 拒絕它簽的每張憑證；簽新一代之後閘道器經換發端點取得新憑證（決策 12，不必寄回）。

### 叢集內 ClickHouse 的管理帳號密碼

`clickhouse.mode: bundled` 時，bootstrap 管理帳號（`clickhouse.bundled.adminUser`，預設 `trustiot_admin`）的密碼由 ClickHouse pod 的 init container `admin-password` 在叢集內產生，放在 Secret `<release>-clickhouse-admin-password`（key 是帳號名稱），沒有人建立或複製它（Laysi/trustiot-v2#403；設計見 `docs/architecture/secrets.md`「叢集內 ClickHouse 管理帳號的密碼」）。ClickHouse 只在啟動時讀這個密碼，所以輪替就是讓 init container 先產生新密碼、再讓伺服器重啟：

1. 把 values 的 `clickhouse.bundled.adminPasswordGeneration` 加一（SaaS 改 `deploy/flux/clusters/saas/trustiot/values-instance.yaml`，地端改安裝實例的覆寫檔），升級。ClickHouse 的 pod 重啟，init container 產生新密碼寫進 Secret，伺服器以新密碼啟動；migration Job 不重跑，服務不重啟（它們不用這個帳號）。ClickHouse 是單一副本，重啟期間 collector 寫不進日誌，在 5 分鐘內重試。
2. 確認：`kubectl -n <namespace> logs <release>-clickhouse-0 -c admin-password` 有 `generated the password of <帳號> in Secret <release>-clickhouse-admin-password`（日誌只有 Secret 與帳號的名稱與產生的原因）；`kubectl -n <namespace> get secret <release>-clickhouse-admin-password -o jsonpath='{.metadata.annotations.trustiot\.io/password-generation}'` 是新的代；ClickHouse 的 `/ping` 探針恢復（pod Ready）。
3. 確認伺服器接受 Secret 裡的密碼，密碼不離開 pod：`kubectl -n <namespace> exec <release>-clickhouse-0 -c clickhouse -- bash -c 'exec clickhouse-client --user "$CLICKHOUSE_USER" --password "$(cat "$CLICKHOUSE_PASSWORD_FILE")" --query "SELECT currentUser()"'`，輸出是帳號名稱。需要以管理帳號手動執行 SQL 時（例如 [migration Job 的 runbook](migrations.md) 的 `DROP USER`、`REVOKE`）也用這個方式。
4. 下一次 migration Job（例如下一次升級）以新密碼連線。要當場確認，同一次升級把 `migrate.revision` 也改成新的值，Job 換名重跑。

代只往上走：`helm rollback` 或 Flux 升級失敗後的 rollback 渲染出較小的值時，init container 保留現有的密碼（pod 因為 checksum 改變而重啟一次，密碼不變），之後再升級回來也不換。

**Secret 遺失**（有人刪了 key 或整個 Secret）：已經在跑的 ClickHouse 不受影響，但下一次 Job 讀不到密碼，等到 `migrate.wait.databases` 用完失敗，日誌寫出 Secret 的名稱與這個處理方式。把 `clickhouse.bundled.adminPasswordGeneration` 加一再升級：Helm 重建被刪掉的 Secret，ClickHouse 重啟，init container 產生新密碼。Secret 不存在時 init container 不能建立它，會失敗並寫出同樣的處理方式，ClickHouse 停在 `Init`。不要自己建立這個 Secret：init container 不信任不是它填過的物件（uid 不同），會換掉裡面的值。

**從人建立的 Secret 搬過來**（Laysi/trustiot-v2#403 之前的安裝，values 設了 `clickhouse.bundled.adminSecret.name`，例如 `trustiot-clickhouse-admin`）：

1. 在一次升級把 `clickhouse.bundled.adminSecret.name` 改回空白（刪掉這個欄位）；`adminSecretRevision` 之後不再有作用，也可一併刪掉。ClickHouse 重啟，init container 產生新密碼，伺服器改用它；migration Job 因為讀密碼的方式改變而換名重跑，以新密碼連線（服務與 collector 隨新的 Job 名稱滾動更新一次）。
2. 確認 ClickHouse Ready、新的 Job 完成，並照上面第 3 步確認伺服器接受新密碼。
3. 刪掉舊的 Secret：地端 `kubectl -n <namespace> delete secret <舊名稱>`；SaaS 刪掉 `deploy/flux/clusters/saas/trustiot/` 裡它的 SOPS 檔與 kustomization 的引用。舊密碼已經不能登入（bootstrap 帳號由映像的 entrypoint 依每次啟動的密碼寫成）。

### 叢集內 Postgres 的 owner 密碼與 TLS 憑證

`postgres.mode: bundled` 時，CloudNativePG 在叢集內產生這些 Secret，值不經過人（`<cluster>` 是 `<release>-postgres`，例如 `trustiot-postgres`；細節見 `deploy/chart/README.md` 的「叢集內的 Postgres」）。superuser 沒有密碼可輪替：`postgres` 只能在 pod 內經 local socket 登入。

資料庫 owner `trustiot_owner` 的密碼（Secret `<cluster>-app`，只給 migration Job）：刪掉 Secret，operator 會以新產生的密碼重建它，並把資料庫裡的角色改成新密碼。

1. `kubectl -n <namespace> delete secret <cluster>-app`
2. 等 Secret 重建：`kubectl -n <namespace> get secret <cluster>-app` 再次出現，`AGE` 從頭起算。2026-10-12 在實驗機上，Secret 在刪除後 0.1 秒內重建，資料庫的角色再過 0.2 秒換成新密碼（[測試報告](../test-reports/2026-10-12-58-cnpg-postgres.md)）。
3. 確認新密碼能以 TLS 登入，密碼經管線傳入、不出現在命令列：
   `{ kubectl -n <namespace> get secret <cluster>-app -o jsonpath='{.data.password}' | base64 -d; echo; } | kubectl -n <namespace> exec -i <cluster>-1 -c postgres -- bash -c 'read -r PGPASSWORD; export PGPASSWORD; psql "host=<cluster>-rw dbname=trustiot user=trustiot_owner sslmode=require" -tAc "select current_user"'`
   印出 `trustiot_owner` 即成功；以舊密碼執行同一個指令會被拒（`password authentication failed`）。
4. 舊密碼立即不能建立新連線，但已建立的連線不會被中斷。懷疑外洩時，以 superuser 中止這個角色現有的連線：`kubectl -n <namespace> exec -i <cluster>-1 -c postgres -- psql -d trustiot -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = 'trustiot_owner'"`。migration Job（Laysi/trustiot-v2#60）下次執行時讀新值。

TLS 憑證（`<cluster>-ca`、`<cluster>-server`、`<cluster>-replication`）由 operator 自動換發（效期 90 天，到期前 7 天換發），不必人工處理；服務每條新連線都重讀掛載的 CA 檔，不必重啟。CA 換發時沿用同一把私鑰，所以 CA 私鑰（`<cluster>-ca` 的 `ca.key`）外洩時自動換發沒有用，要換一把新的 CA；這個步驟還沒在叢集上驗證過，遇到時依「外洩處理」先止血並開 `security` issue，驗證後補在這裡。

### 叢集內產生的帳號

ClickHouse、MQTT dynsec、Grafana 的帳號由叢集內產生，值不經過人。各自的 issue 補上指令（Laysi/trustiot-v2#59、Laysi/trustiot-v2#84、Laysi/trustiot-v2#183）；Postgres 的 owner 見上一節。各服務自己的 Postgres 角色與 OpenTelemetry collector 的 ClickHouse 帳號，密碼由 migration Job 產生並設定（`<release>-<服務>-password`、`<release>-otel-collector-password`），沒有人經手；輪替是調大 `migrate.passwordGeneration`，步驟見 [migration Job 的 runbook](migrations.md) 的「輪替服務角色與 collector 的密碼」，資料遺失時見同一份的「密碼 Secret 的資料遺失」。原則：在叢集內重新產生值、更新資料庫裡的帳號、重啟使用它的服務，三步在一次維護中完成；只有懷疑外洩或人員異動時才需要輪替。

### OTA 簽章私鑰

Laysi/trustiot-v2#164 補上。原則：閘道器的 updater 內建公鑰，所以輪替要先發一版同時信任新舊公鑰的 updater，確認全部閘道器都已更新，才改用新私鑰簽章。

### SOPS 叢集私鑰

以下是 SaaS 叢集。展示叢集的私鑰在展示機上產生、沒有離線備份，換新的步驟見 [deploy/flux/README.md](../../deploy/flux/README.md)「展示叢集的 age 金鑰」。

1. 在可信的機器上 `age-keygen -o <新檔案>`，記下輸出的公鑰。
2. 在 `.sops.yaml` 的收件者加入新公鑰（舊的先保留），對 `deploy/flux/` 下每個 SOPS 檔案執行 `sops updatekeys -y <檔案>`，開 PR 合併。
3. 在主機上更新叢集的 Secret，私鑰從檔案經 stdin 傳入：
   `sudo k3s kubectl -n flux-system create secret generic sops-age --from-file=age.agekey=/dev/stdin --dry-run=client -o yaml < <新檔案> | sudo k3s kubectl apply -f -`
4. 確認 Flux 仍能解密：`sudo k3s kubectl -n flux-system annotate --overwrite kustomization/flux-system reconcile.fluxcd.io/requestedAt="$(date +%s)"` 後，`sudo k3s kubectl -n flux-system get kustomization flux-system` 的 READY 為 True。
5. 從 `.sops.yaml` 移除舊公鑰，再對每個檔案 `sops updatekeys -y`，並以 `sops rotate -i <檔案>` 換掉資料金鑰，開 PR 合併。
6. 更新離線備份，銷毀舊私鑰與 `<新檔案>`。

### Flux 的 deploy key

叢集以這把唯讀金鑰經 SSH 讀 main（[deploy/flux/README.md](../../deploy/flux/README.md)「建立叢集」第 3 步）。GitHub 允許同一個 repo 有多把 deploy key，所以先加新的再刪舊的，同步不中斷：

1. 產生新金鑰：`ssh-keygen -t ed25519 -N '' -C trustiot-saas-flux-<YYYY-MM-DD> -f flux-deploy-key`，在 GitHub 的 repo 設定加入 `flux-deploy-key.pub`，**不勾** Allow write access。
2. 把三個檔案複製到主機，更新叢集的 Secret：

   ```sh
   sudo k3s kubectl -n flux-system create secret generic flux-system \
     --from-file=identity=flux-deploy-key --from-file=identity.pub=flux-deploy-key.pub \
     --from-file=known_hosts=github-known-hosts --dry-run=client -o yaml | sudo k3s kubectl apply -f -
   shred -u flux-deploy-key
   ```

3. 立刻同步並確認：`sudo k3s kubectl -n flux-system annotate --overwrite gitrepository/flux-system reconcile.fluxcd.io/requestedAt="$(date +%s)"`，再 `sudo k3s kubectl -n flux-system get gitrepository flux-system` 的 READY 為 True。
4. 在 GitHub 刪除舊的 deploy key。

以上是 SaaS 叢集。展示叢集的 deploy key 由 `scripts/demo/flux.ts deploy-key` 在展示機上產生、不離開展示機，它不取代已經存在的 Secret，換新的步驟見 [deploy/flux/README.md](../../deploy/flux/README.md)「在展示機安裝 Flux」。

外洩時先在 GitHub 刪除該 deploy key（同步會停止，正在跑的服務不受影響），再依上面的步驟換新的。它是唯讀的，外洩的影響是 repo 內容被讀取：依「外洩處理」檢查範圍。

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
