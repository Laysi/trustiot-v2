# 密鑰與權限（M3）

功能重建清單 M3 的規則與密鑰清冊（2026-10-10，Laysi/trustiot-v2#72）。舊系統的缺陷都來自沒有統一規則：session 密鑰寫死（[#100](https://github.com/Laysi/trustiot/issues/100)）、Docker build context 把 Git 忽略的 `.env` 與金鑰送進 BuildKit（[#120](https://github.com/Laysi/trustiot/issues/120)）、GHCR token 留在 SSM Run Command 紀錄（[#122](https://github.com/Laysi/trustiot/issues/122)）、一次性 token 不是真的只能用一次又放在網址（[#105](https://github.com/Laysi/trustiot/issues/105)）、stage 部署角色是 AdministratorAccess（[#158](https://github.com/Laysi/trustiot/issues/158)）。

- 本文件：規則、注入方式、密鑰清冊。
- 輪替與外洩處理的步驟：[密鑰輪替 runbook](../runbooks/secrets.md)。
- SaaS 的祕密怎麼進叢集：[CI 與發佈](ci-release.md) 的「SaaS 以 GitOps 部署」（SOPS 加 age）。
- CI 與 GitHub 的權限：[CI 與發佈](ci-release.md) 的通用規則與「GitHub 的 Actions 設定」。
- repo 公開後，什麼資訊不能寫進 commit、PR、issue、測試報告與 CI log：下方「公開 repo 的資訊規則」。

新增任何密鑰（新的外部服務、新的帳號、新的簽章金鑰）時，在同一個變更裡加進下方清冊與 runbook。

## 規則

1. **不寫死**（[#100](https://github.com/Laysi/trustiot/issues/100)）：repo 裡沒有任何密鑰的值，連範例與預設值也沒有；需要密鑰的設定一律沒有預設值，缺少時服務拒絕啟動。本機開發環境每次啟動都產生新的密碼（Laysi/trustiot-v2#71）。由 CI 的 gitleaks（完整歷史）、chart 的 `values.schema.json`（values 不能放明文）與 `scripts/tests/policy.test.ts`（scripts 不能有固定密碼）把關。
2. **不進 build context**（[#120](https://github.com/Laysi/trustiot/issues/120)）：
   - build context 只能是映像自己的目錄，或只放建置產物的產生目錄（`release.yml` 與本機開發環境都是後者）；repo 根目錄永遠不當 build context，根目錄的 `.dockerignore` 只有 `*`。不用 Dockerfile 專屬的 ignore 檔（`<檔名>.dockerignore`）與 podman 的 `--ignorefile`：它們會取代 context 自己的 ignore 檔。
   - `COPY` 或 `ADD` 整個 build context（`.`、`*`）時，Containerfile 所在目錄往上到 repo 根目錄的每一層（都可能被建置指令當成 context）都要有以 `*` 開頭的 ignore 檔（`.containerignore` 或 `.dockerignore`），再以 `!` 列出映像需要的檔案；`!` 不能點名金鑰類檔案，也不能用可能比對到金鑰類檔案的萬用字元（例如 `!certs/*`），要逐一列出檔名。
   - 不 `COPY`、`ADD` 金鑰類檔案（私鑰、keystore、dotenv 檔（`.env`、`.env.*`、`*.env`、`.envrc`，`.example` 等範本除外）、kubeconfig、`.npmrc`、`.ssh/`、`.git`、檔名裡有 `credentials`、`secret`、`token`、`service-account`、`client_secret`、`api_key`、`private-key` 等字的檔案等），從其他建置階段複製也一樣；來源不用變數（`$FILE`），否則檢查不到實際複製的路徑；以目錄或萬用字元複製時，repo 裡實際存在的金鑰類檔案也要被 ignore 檔排除；context 由建置指令決定，所以從 Containerfile 所在目錄往上到 repo 根目錄的每一層都當成可能的 context 檢查。
   - context 由建置指令在執行時決定，靜態檢查看不到，所以腳本裡的每一次映像建置都以 `scripts/lib/build-context.ts` 的 `containerBuildArgs(context, options)` 產生 `podman build` 的參數：它先檢查這個 context，再把同一個目錄（與 `--file`）以絕對路徑放進參數，檢查的就是實際建置的目錄，換了工作目錄也一樣。建置選項只接受白名單（`--file`、`--tag`、`--label`、`--platform`、`--secret` 等），會讓建置程式讀到 context 以外檔案的選項（`--build-context`、`--volume`、`--ignorefile`、`--build-arg-file`）與多出來的目錄一律拒絕；`--build-arg` 也拒絕，因為它的值留在映像歷史裡，而執行時才組出的值無法證明不是密鑰，建置時要用的密鑰走 `--secret`。會存進映像設定的值（label、平台、時間戳記、tag 等）只接受固定的形式，label 只能是 release 映像的 OCI label（`source`、`revision`、`version`、`title`）與本地開發環境的實例 label，值也要符合各自的格式；tag 只能是本地開發環境的 `localhost/trustiot-dev-<實例>-<服務>:dev` 與 release 建置的 `localhost/trustiot-release/<映像>:<版本>`；任何值裡有 20 個字元以上字母數字混合的片段（token 的形狀）都拒絕，沒有放進密鑰的空間。podman 實際讀的建置檔（`--file` 指定的檔案，或 context 裡的 Containerfile／Dockerfile）也在建置前以同樣的 Containerfile 規則對實際的 context 檢查，所以產生的或放在 context 外的建置檔一樣受檢；`--file` 只能有一個，而且必須是一般檔案（不能是 stdin 或網址）。context 裡有沒被 ignore 檔排除的金鑰類檔案就失敗，因為 podman 與 Docker 會把整個 context 送給建置程式，不論映像有沒有複製它。其他地方（其他腳本、shell 腳本、workflow 的 step）不直接寫建置指令；子指令在執行時才決定的容器指令（`[podman, op]`、`podman ${op}`）、子指令前有全域選項的容器指令（`docker --context prod build` 照樣算建置，`["docker", "--context", "prod", op]` 被擋）也不行，因為靜態檢查看不出它是不是建置。本地開發環境的 podman 執行器（`scripts/lib/podman.ts`）會原樣傳遞呼叫端的參數，所以它在執行前以 `assertCheckedBuild` 擋下參數不是來自 `containerBuildArgs` 的建置。這些規則攔的是一般寫法；刻意躲過檢查的寫法（替執行器取別名、在執行時組出工具名稱）留給 review。
   - 建置時需要的密鑰（例如私有套件庫的 token）以 `podman build --secret` 加 `RUN --mount=type=secret` 交給單一步驟，不寫進任何一層。`ARG` 與 `ENV` 不能放密鑰：`ARG` 留在映像歷史，`ENV` 留在映像設定。
3. **不經指令參數**（[#122](https://github.com/Laysi/trustiot/issues/122)）：指令列參數會出現在 `ps`、shell 歷史、CI 紀錄與遠端執行服務的紀錄。密鑰改以權限 0600 的檔案（`--env-file`、掛載檔案）、stdin 或環境變數交給程式。具體做法：
   - 建立 Kubernetes Secret 用 `kubectl create secret ... --from-file`，不用 `--from-literal`。
   - `podman`／`docker` 不用 `--build-arg` 傳密鑰（見上），也不用 `-e NAME=值`，改用 `--env-file` 或 `--secret`。
   - GitHub Actions 中，`${{ secrets.* }}` 只能出現在 step 的 `env` 或 `with`，由指令讀環境變數，不寫進 `run` 的腳本內容；workflow 與 job 層級的 `env` 會把密鑰交給範圍內每一個 step，所以也不放。例外只有交給 reusable workflow 的 `jobs.<id>.secrets`，以及 job container 與 service 的 registry `credentials`。
4. **不放網址、不進日誌**：URL 不帶帳密（chart 的 schema 擋下 `user:password@`）；token、密碼、私鑰不寫進日誌與錯誤訊息（`crates/service-kit` 遮蔽設定中的密鑰，內部身分 token 與公鑰不進日誌）；一次性 token 的規則見下節。
5. **只給需要的服務**：每個 Secret 只注入需要它的服務（chart 的 `services.<name>.credentials`，AI 供應商金鑰只給 `ai-gateway`，見 [deploy/chart/README.md](../../deploy/chart/README.md)）；平台設定 ConfigMap 不放任何密鑰（chart 測試檢查）；服務沒有 ServiceAccount token。
6. **連線一律驗證對方**（[#109](https://github.com/Laysi/trustiot/issues/109)）：外部 Postgres 只接受 `verify-full`，ClickHouse、Qdrant 等外部端點在 SaaS 只接受 https。
7. **最小權限的 CI 與部署**（[#158](https://github.com/Laysi/trustiot/issues/158)）：GitHub 不持有任何部署憑證，SaaS 由叢集內的 Flux 拉取；CI 權限與 repo 的 Actions 設定見 [CI 與發佈](ci-release.md)。
8. **能不用密鑰就不用**：session 是伺服器端狀態（決策 13），cookie 只是 256 位元隨機值、伺服器只存雜湊，所以沒有可以寫死或外洩的 session 簽章密鑰；cosign 用 keyless 簽章（Laysi/trustiot-v2#68）；CI 需要雲端憑證時用 OIDC。

規則 2 與規則 3 的建置部分，以及 SOPS 加密，由 CI 的 `scripts/ci/check-secret-handling.ts` 檢查（`workflow-policy` job），涵蓋 repo 內每一個 Dockerfile 與 Containerfile（`Dockerfile`、`Containerfile`、`*.Dockerfile`、`*.Containerfile`、`Dockerfile.*`、`Containerfile.*`），新加的檔案自動納入：

| 檢查 | 範圍 |
| --- | --- |
| `COPY`、`ADD` 的來源不用變數、不點名金鑰類檔案，也不經目錄或萬用字元帶進 context 裡實際存在的金鑰類檔案（依 ignore 檔的規則判斷）；整個 context 時，從 Containerfile 所在目錄到 repo 根目錄的每一層都要有以 `*` 開頭的 ignore 檔，`!` 不點名、也不以萬用字元比對到金鑰類檔案；`ADD` 的網址不帶帳密 | 每個 Dockerfile、Containerfile |
| `ARG`、`ENV` 不宣告名稱像密鑰的變數（與 chart 同一條規則：含 SECRET、PASSW、PASSPHRASE、PWD、TOKEN、API_KEY、DSN、PRIVATE、CREDENTIAL，或以 `_KEY` 結尾） | 每個 Dockerfile、Containerfile |
| repo 根目錄的 `.dockerignore`（與 `.containerignore`，若有）只有 `*`，沒有 `!` 例外；repo 內沒有 Dockerfile 專屬的 ignore 檔；建置指令不用 `--ignorefile` | 根目錄與整個 repo |
| `--build-arg` 必須是一整個字面值 `NAME=value`（只寫 `NAME` 時值取自建置環境的環境變數，檢查看不到，所以也擋）：名稱不像密鑰；值只能是字面文字，或列在白名單的公開執行資訊（`github.sha`、`ref`、`ref_name`、`run_id`、`run_number`、`run_attempt`、`repository`、`repository_owner`、`server_url`），其他執行時才算出的值（字串相加、`$VAR`、`${...}`、`${{ github.token }}`、`${{ env.* }}`、step 的輸出、`${{ secrets.* }}`）一律擋下，因為檢查看不到它的內容；不用 `--build-arg-file`；workflow step 的 `build-args` 輸入同樣檢查 | `.github/` 的 workflow 與 action、repo 內的 `.ts`、`.js`、`.sh`（測試檔除外） |
| 映像建置只經 `containerBuildArgs`（`scripts/lib/build-context.ts`）產生參數；其他腳本、shell 腳本與 workflow 不直接寫 `podman build`、`podman image build`、`docker build`、`docker buildx build`、`buildah bud`、`docker compose build`、`docker buildx bake` 等建置指令，也不以執行時才決定的子指令呼叫容器工具；也不用 compose（`docker compose`、`podman compose`、`docker-compose`），因為 `up`、`run`、`create` 也會依 compose 檔的 `build:` 建置映像；workflow 不用建置映像的 action（`docker/build-push-action`、`docker/bake-action`、`redhat-actions/buildah-build`、kaniko 等），因為它們送出的 context 檢查看不到；podman 執行器在執行前擋下不經 `containerBuildArgs` 的建置 | repo 內的 `.ts`、`.js`、`.sh`（測試檔與 `docs/test-reports/data/` 裡隨報告保存的測試替身除外）與 `.github/` |
| 讀 `secrets` context 的運算式（不分大小寫，含 `toJSON(secrets)`）只出現在 step 的 `env`、`with`，reusable workflow 的 `secrets` 與 registry `credentials`；不出現在 `run`、workflow 與 job 層級的 `env` 或其他欄位 | `.github/` 的 workflow 與 action |
| Kubernetes Secret（YAML 或 JSON）一律以 SOPS 加密：`data` 與 `stringData` 的每個值都是 SOPS 寫出的格式（AES256_GCM、32 位元組 IV、16 位元組 tag），`sops` 區塊有版本、修改時間、加密的 MAC 與 age 收件者及其以 age 格式包裝的資料金鑰；值、IV、tag、MAC 與包裝的資料金鑰解碼後都不能是可讀文字（只有 `ENC[` 開頭、把明文 base64 後套上格式或手寫的區塊都不算）。CI 沒有解密金鑰（GitHub 不持有部署憑證），所以無法以密碼學證明內容真的加密，這一項擋的是不小心提交的明文；真正的驗證在 Flux 解密時檢查 MAC，失敗就不套用；不用 Kustomize 的 `secretGenerator`（它的 `literals`、`files`、`envs` 都是檢查看不到的明文）；Helm 樣板（無法直接解析的 `.yaml` 與 `.tpl`）不宣告 Secret，`kind` 也不能是樣板，唯一的例外是 migration Job 填入的空 Secret：帶標籤 `trustiot.io/filled-by: migrate`，只有 `apiVersion`、`kind`、`metadata`、`type` 四個 key，含樣板動作的行只能是檢查程式列出的三行（名稱 `name: {{ .name }}`、chart 的標準標籤 include、range 的 `{{- end }}`），逐字比對（2026-10-13 起，Laysi/trustiot-v2#60 第四輪：先前「整行動作只能渲染在更深的縮排」的寬鬆判斷可以被同一行的 `data:`、以 values 或 `randAlphaNum` 產生的 annotation 繞過）；渲染結果另由 chart 測試逐欄比對（annotation 與標籤的集合與值）。任何 Helm 樣板與 helper 都不能用產生或讀取密鑰材料的函式：`lookup`、`rand*`、`uuidv*`、`shuffle`、`gen*` 憑證與金鑰、`derivePassword`、`htpasswd`、`bcrypt`、AES 函式、`getHostByName`；這條規則放在每個 PR 都跑的 `check-secret-handling.ts`，而不是只在 chart 變動時才跑的 chart 測試，因為它涵蓋 repo 裡所有樣板 | repo 內每個 YAML 檔與 `.tpl` |

chart 的規則由 `bun test deploy/chart` 檢查，見 [deploy/chart/README.md](../../deploy/chart/README.md)：

- values 只以名稱引用 Secret，values 沒有明文密鑰。
- chart 不渲染任何密鑰值。它可以宣告空的 Secret，由叢集內的元件填入；目前只有 migration Job（下方「migration Job 產生的密碼」）。測試檢查渲染出的每個 Secret 都沒有 `data`、`stringData`，都是 Job 填入的那幾個，metadata 逐欄等於預期（名稱、chart 的標準標籤加 `trustiot.io/filled-by: migrate`、annotation 只有 `helm.sh/resource-policy: keep` 與說明；chart 不渲染 Job 自己寫的 annotation）；Job 的 Role 對 `secrets` 只有 `get`、`patch`，而且以 `resourceNames` 限定這幾個；chart 的其他 Role 都不碰 `secrets`。

### migration Job 產生的密碼

各服務的 Postgres 登入角色（`<服務>_app`）與 OpenTelemetry collector 的 ClickHouse 帳號，是 migration Job 自己建立的內部帳號，和「ClickHouse 帳號：ingest 寫入」一樣屬於叢集內產生的類別：沒有人建立、複製或輪替它們的密碼（2026-10-13 依使用者意見，取代原本由人建立 Secret 的做法，Laysi/trustiot-v2#60）。

- **放在哪裡**：chart 為每個消費者宣告一個 Secret（`<release>-<服務>-password`、`<release>-otel-collector-password`），key 是角色或帳號名稱。每個消費者一個，而不是共用一個，所以節點的 kubelet（node authorizer）只讀得到排在該節點的 pod 用到的密碼。
- **誰填**：migration Job 在 migration 鎖內，以作業系統的 CSPRNG 為缺的 key 產生 256 位元的密碼（64 個十六進位字元），以帶 `resourceVersion` 的 merge patch 寫入；之後把角色與帳號設成 Secret 裡的值（Postgres 只送 SCRAM-SHA-256 verifier、ClickHouse 只送加鹽的 SHA-256，明文不進 SQL）。日誌、錯誤訊息與測試輸出只有名稱，沒有密碼、密碼的 hash 或 Secret 的資料。
- **權限**：Job 的 projected token 只掛進 Job 的容器；它的 Role 只有這些 Secret 的 `get`、`patch`（`resourceNames`）。沒有 `create`：它無法以名稱限制，而且能替 namespace 裡任何 ServiceAccount 建立 `kubernetes.io/service-account-token` Secret；也沒有 `list`、`watch`。服務與 collector 的 ServiceAccount 沒有新的權限，各自以 `secretKeyRef` 讀自己的 key。
- **輪替**：調大 `migrate.passwordGeneration`；Secret 的 annotation `trustiot.io/password-generation` 記錄已套用的值，以數字比較、只往上走，所以重跑不會再換一次，rollback 渲染出較小的值時也不換（之後升級回來同樣不換）。服務與 collector 隨 Job 名稱滾動更新。舊 pod 在被換掉之前開新連線會被拒（Postgres 一個角色只有一個密碼），已開的 Postgres 連線不受影響；步驟與這段時間見 [docs/runbooks/secrets.md](../runbooks/secrets.md)。
- **只信任 Job 填過的物件**：Job 寫入時在同一個 patch 記下 Secret 的 `metadata.uid`（annotation `trustiot.io/password-secret-uid`，chart 不渲染）；uid 不符時（有人刪掉再建一個同名的 Secret、或從備份還原）每個 key 都重新產生，所以預先建好、帶著自選密碼的 Secret 不會被採用。同一個 patch 也拿掉 chart 與 Job 不設定的 metadata：白名單以外的每個 annotation 與標籤（保留 chart 為這些 Secret 設定的、Helm 的 ownership annotation、Flux helm-controller 的兩個標籤，以及 Job 自己的兩個 annotation）、`ownerReferences` 與 `finalizers`。所以預先建立的 Secret 帶著的 replicator 或 reflector annotation 不會讓產生的密碼被複製到別的 namespace，owner 也不會讓它被垃圾回收；這些 metadata 和產生的資料不會同時存在，因為兩者在同一個 patch 裡。日誌與報告只寫被拿掉的 key，不寫值。已綁定的 Secret 之後被改的 metadata 不再處理（2026-10-14，Laysi/trustiot-v2#60 第五輪）。剩下的風險：Kubernetes 的 RBAC 分開授予 `get` 與 `patch`，但能 patch 這些 Secret 的主體即使沒有 `get`，也能把 `data` 改成自選的密碼（Job 會把角色設成它，因為 uid 沒變），或在綁定之後加上 replicator 的 annotation；所以 patch 權限等同能取得這些密碼，只能給受信任的主體。chart 只把它給 Job 自己（Role 以 `resourceNames` 限定），namespace 裡其他能 patch Secret 的 Role 或 ClusterRole 都要當成能讀這些密碼來審查。
- **資料遺失**：Helm 不擁有這些 Secret 的資料（渲染結果沒有 `data`），升級與 rollback 都保留 Job 寫的值；有人刪了 key 或 Secret、或以 `helm upgrade --force-replace` 換掉它時（不要用它：它清空 Secret 之後在取代 Job 時失敗，升級停在 FAILED 而 Job 不再跑），改 `migrate.revision` 再升級：Helm 重建 Secret，Job 產生新密碼並設定（[docs/runbooks/migrations.md](../runbooks/migrations.md)）。
- **不再需要的帳號**：Job 只讓目前的設定用得到的帳號登入。部署關掉的服務（沒有綁定 Secret）與服務清單已經沒有的服務，它們的 Postgres 登入角色改成 `NOLOGIN`；只挑 Job 標記過（`COMMENT ON ROLE`）且 migration 角色有 ADMIN OPTION 的角色，不依名稱挑，不 `DROP`（角色可能擁有物件，NOLOGIN 可以還原）。collector 關閉或改帳號時，舊的 ClickHouse 帳號改成 `HOST NONE`（只挑拿到 `trustiot_otel_collector` 角色的帳號）。留下的 Secret（`keep`）裡的密碼因此登不進去，可依 runbook 刪除（2026-10-13，Laysi/trustiot-v2#60 第四輪）。
- **不提供外部覆寫**：客戶自己的 secret manager 不能指定這些密碼。它們不離開叢集，只給 Job 建立的帳號使用；讓人指定就回到人要建立、保管、輪替密碼的做法。客戶有這個需求時另開 issue 決定。

## 公開 repo 的資訊規則

2026-10-12 起 Laysi/trustiot-v2 是公開 repo（Laysi/trustiot-v2#345）。以下內容任何人都讀得到，而且刪除後仍可能留在 fork、快取與第三方的存檔：

- 所有 commit 與歷史、分支、tag、PR 的標題與內文與 review 留言、issue 與留言（含編輯歷史）、GitHub Release 與附件。
- `docs/`，包括 `docs/test-reports/` 與它的原始資料。
- Actions 的 log 與 artifact。artifact 的下載是否需要登入未驗證，一律當作公開。
- 之後若把 GHCR 套件改為公開，連映像與 chart 也是（見 [CI 與發佈](ci-release.md) 的「GHCR 套件的可見性」）。

**不得寫進以上任何一處：**

1. 密鑰：token、密碼、私鑰、kubeconfig、未加密的 Secret、連線字串（規則 1 到 4 已經要求，現在連 PR 描述與 log 也一樣）。以 SOPS 加密的密文是例外，那是設計上要進版控的（[CI 與發佈](ci-release.md) 的「SaaS 的祕密怎麼進叢集」）。
2. 內部主機名稱與位址：維運用的主機別名、辦公室與實驗室的區域網路位址、不對外的網域與主機名稱、VPN 與內網網段。叢集內的 Pod 與 Service 位址（例如 k3s 的 `10.42.0.0/16`、`*.svc.cluster.local`）不是祕密，可以寫。
3. 客戶名稱、客戶現場的資訊（廠區、設備、網路配置）與任何個人資料（姓名、email、電話、帳號）。git 的作者 email 本來就隨 commit 公開，不算在內。
4. 還沒修好的安全弱點的細節：改以 GitHub 的私人安全通報或私下通知處理，修好之後才寫進公開的 issue 與文件。

**寫法：**

- 範例與測試一律用保留的假值：`example.com`、`example.internal`、`192.0.2.0/24`（RFC 5737 的文件用位址）、`<host>` 這類佔位符，不用真實的主機名稱與位址。
- 測試報告與 CI log：貼指令輸出前先看過。叢集與主機的輸出（`kubectl get nodes -o wide`、`ip addr`、systemd 與 cloudflared 的 log）常帶節點位址與主機名稱，要先遮蔽或刪掉那幾欄。
- 腳本與 workflow 不把環境變數、設定檔或指令列整份印進 log（規則 4 已要求遮蔽祕密，主機名稱與位址同樣不印）。
- 推送前檢查 diff 與 PR 描述；gitleaks 只認祕密的形狀，擋不住主機名稱、位址、客戶名稱與個人資料，這些靠人。

**寫進去了怎麼辦：**

- 祕密：視同外洩，依 [密鑰輪替 runbook](../runbooks/secrets.md) 的外洩處理立刻輪替；刪掉 commit、issue 或留言不夠，內容已被複製的風險仍在。
- 其他資訊：盡快移除，並評估該位址或名稱是否需要更換；歷史改寫是破壞性的，要由使用者決定。

**既有內容：**本規則從 2026-10-12 起適用。公開前已存在於 repo 的內容（例如早期測試報告裡實驗室的主機別名與區域網路位址）是否要清理，由使用者決定，另案處理。

**GitHub 端的掃描設定（待使用者決定）：**公開時使用者只改了可見性，沒有動 secret scanning 與 push protection，現況未驗證。GitHub 文件寫 secret scanning 對公開 repo 自動執行且免費（[About secret scanning](https://docs.github.com/en/code-security/secret-scanning/introduction/about-secret-scanning)）；push protection 在公開 repo 是否預設開啟、是否需要手動啟用，文件頁面沒有寫，未驗證。選項：A. 維持現況，只靠 CI 的 gitleaks（掃完整歷史，但是在推送之後才掃）；B. 在 repo 設定開啟 push protection，祕密在推送時就被擋下，不會進入公開歷史，代價是誤判時推送的人要手動繞過（細節未驗證）。公開 repo 的祕密一旦推上去就算外洩，所以建議 B。

## 一次性 token 與驗證碼

適用於 email 登入驗證碼（A1，Laysi/trustiot-v2#103）、成員邀請（A3，Laysi/trustiot-v2#105），以及之後任何「只能用一次」的 token（例如換發挑戰，決策 12）。來源是 [#105](https://github.com/Laysi/trustiot/issues/105)：舊系統先讀取、驗證、再清除，兩個併發請求能同時通過；清除失敗仍回 200；token 放在網址的 query。

- **產生**：以作業系統的 CSPRNG 產生。連結用的 token 至少 128 位元（建議 256 位元，base64url）。給人輸入的短驗證碼熵低，必須同時有短效期與嘗試次數上限，超過上限就作廢該驗證碼；數值由 Laysi/trustiot-v2#103 決定並寫進文件。
- **只存雜湊**：資料庫只存 token 的雜湊（SHA-256），不存明文，資料庫外洩也拿不到可用的連結 token。短驗證碼的組合少，外洩的雜湊可以離線窮舉，所以它的安全性靠短效期與嘗試上限，不靠雜湊。
- **原子消耗**：驗證與消耗在同一個 SQL 陳述式完成，例如 `UPDATE ... SET used_at = now() WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now() RETURNING ...` 或 `DELETE ... RETURNING ...`。只有拿到 RETURNING 結果的請求成功；沒有拿到就回錯誤。消耗與它換到的東西（建立 session、加入成員）在同一筆交易，任何一步失敗就整筆回滾並回錯誤，不能回 200。
- **不放網址**：token 不出現在 path 或 query，伺服器也不接受從 query 讀 token（網址會進瀏覽器歷史、Cloudflare 與入口的存取紀錄、Referer）。API 一律以 POST body 傳送。email 裡的連結需要帶 token 時放在 fragment（`https://<app>/invite#token=...`）：瀏覽器不會把 fragment 送到伺服器，SPA 讀出後立即以 `history.replaceState` 從網址列移除，再以 POST body 送出。SPA 的回應設 `Referrer-Policy: no-referrer` 或 `strict-origin-when-cross-origin`（Laysi/trustiot-v2#123）。
- **綁定用途**：token 綁定用途與對象（哪個 email、哪個租戶的哪個邀請），用於 A 的 token 不能拿去做 B。
- **測試**：併發兩個請求消耗同一個 token，只有一個成功（整合測試）；過期、已用、錯誤用途都被拒；token 不出現在任何日誌。

## 注入方式

| 方式 | 用在 | 說明 |
| --- | --- | --- |
| Kubernetes Secret，以 `valueFrom.secretKeyRef` 成為環境變數 | 短的字串：API key、密碼、DSN | chart 的 `services.<name>.credentials` 與各 `*Secret` 欄位，values 只寫 Secret 名稱與 key；名稱像密鑰的環境變數只能來自 Secret（schema 與 chart 測試） |
| Kubernetes Secret，以唯讀 volume 掛成檔案 | 私鑰、憑證、多行的金鑰 | 例如資料庫的 CA（`/etc/trustiot/<postgres|clickhouse>-ca/ca.crt`，Postgres 的只掛進使用它的服務）；內部身分簽章私鑰（`/etc/trustiot/identity-signing-key/signing-key.pem`，只掛進身分服務）；之後的 CA 私鑰、broker 伺服器金鑰也掛成檔案，只掛進使用它的那個 pod |
| 叢集內元件自己產生的 Secret | 叢集內資料庫的帳號、CA 與伺服器私鑰；migration Job 建立的帳號的密碼 | 例如 CloudNativePG 為叢集內 Postgres 產生的 CA、伺服器憑證與 owner 帳號（Laysi/trustiot-v2#58），以及 migration Job 產生的服務角色與 collector 密碼（chart 宣告空的 Secret，Job 填入，見「migration Job 產生的密碼」）。值只存在叢集內，不進 git，也不經過人；chart 把這些 Secret 整個列為服務的自訂 `env` 不能讀的對象，服務只拿到 CA 憑證（掛載時只選 `ca.crt`）或自己的密碼 |
| 主機上只有 root 能讀的檔案，以 systemd `LoadCredential` 交給服務 | SaaS 主機上的 cloudflared | 不在叢集內，k3s 故障時仍能運作（Laysi/trustiot-v2#64） |
| GitHub Actions secret，經 step 的 `env` 或 `with` | 發佈流程用的少數金鑰 | ci.yml 不用任何 secret；fork 來的 PR 拿不到 secret（見 [CI 與發佈](ci-release.md)） |
| 開發者本機的狀態目錄（0700 目錄、0600 檔案） | 本機開發環境 | 每次 `up` 產生，`down` 刪除（[scripts/README.md](../../scripts/README.md)） |

SaaS 叢集的 Secret 來源是 `deploy/flux/` 內以 SOPS 加密的檔案，由 Flux 解密後套用；地端由客戶以自己的方式建立 Secret（`kubectl create secret --from-file`、自己的祕密管理工具或 External Secrets Operator），chart 只認 Secret 名稱與 key。

## 密鑰清冊

「SaaS」欄是我們自營 SaaS 的保管位置，「地端」欄是客戶安裝時的位置。「SOPS」指 `deploy/flux/` 內以 SOPS 加密、由 Flux 解密的 Secret（ci-release.md）。輪替步驟見 [runbook](../runbooks/secrets.md) 的對應小節。還沒實作的元件，保管與注入方式是本清冊定下的規則，實作 issue 照做並補上 runbook 的指令。

### 平台信任根與簽章金鑰

| 密鑰 | 用途與使用者 | SaaS | 地端 | 注入方式 | 實作 issue |
| --- | --- | --- | --- | --- | --- |
| 根 CA 私鑰 | 簽發 CA 憑證的信任根（閘道器、工作站、broker 憑證的源頭，決策 6、12） | 離線保管：不在叢集、不在 git（加密也不放）、不在任何連網主機；以 age 加密存兩份離線媒體，分別由兩位維運人員保管；只在簽發或撤銷簽發 CA 時取出 | 依 Laysi/trustiot-v2#78 定的地端 CA 階層 | 不注入任何服務 | Laysi/trustiot-v2#78 |
| 簽發 CA 私鑰 | certs 服務簽發閘道器與工作站憑證、發 CRL | SOPS | 客戶叢集的 Secret（Laysi/trustiot-v2#78 決定由安裝產生或客戶匯入） | 檔案，只掛進 `certs` | Laysi/trustiot-v2#78 |
| broker 伺服器憑證私鑰 | Mosquitto 的 TLS 伺服器金鑰（閘道器 mTLS 的內層） | 由簽發 CA 簽發，存在叢集 Secret | 同左 | 檔案，只掛進 broker | Laysi/trustiot-v2#84 |
| 內部身分簽章私鑰（Ed25519） | 身分服務簽內部身分 token（決策 27） | SOPS | 客戶叢集的 Secret | 檔案：chart 的 `internalIdentity.signingKeySecret` 指向的 Secret（PKCS#8 PEM），只掛進 `identity` 的 `/etc/trustiot/identity-signing-key/signing-key.pem`，權限 0440（root 與 pod 的 fsGroup）；對應的公鑰不是祕密，chart 的 `internalIdentity.publicKeys` 組成 JWK Set 放在平台設定 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`（ConfigMap） | Laysi/trustiot-v2#61（[services/identity/README.md](../../services/identity/README.md)；格式與輪替：Laysi/trustiot-v2#52、[crates/service-kit/README.md](../../crates/service-kit/README.md)） |
| OTA 簽章私鑰 | 簽閘道器 agent 與 updater 的發佈檔（決策 11） | GitHub environment secret，只有 main 上的 tag 能用（ci-release.md）；公鑰內建在 updater | 地端使用同一份發佈檔，不另持有 | 只給 release workflow 的簽章步驟 | Laysi/trustiot-v2#164（注意：release.yml 的 `release` environment 已建立，但私有庫在目前的 GitHub Free 方案設不了必要審查者與 tag 規則（決策 30），environment secret 限制不了「只有 main 上的 tag 能用」，Laysi/trustiot-v2#164 要先解決保管方式。2026-10-12 repo 已公開，Free 方案設得了 environment 的 tag 規則與必要審查者，但尚未設定，限制仍未成立，選項見 [CI 與發佈](ci-release.md) 的「發佈核准」，Laysi/trustiot-v2#345） |
| SOPS 叢集私鑰（age） | Flux 解密 `deploy/flux/` 的 Secret | 叢集 `flux-system` namespace 的 Secret `sops-age`，由維運人員在建立叢集時直接建立（不經 git）；離線備份與根 CA 私鑰放在一起 | 不適用（地端不裝 Flux） | 只給 Flux kustomize-controller | Laysi/trustiot-v2#70 |
| 維運人員的 age 私鑰 | 編輯、輪替 SOPS 檔案；叢集私鑰遺失時的備援 | 各自保管在自己的裝置（建議放在密碼管理器或硬體金鑰），不放共用主機 | 不適用 | 不注入任何服務 | Laysi/trustiot-v2#70 |

### 資料庫與叢集內元件

| 密鑰 | 用途與使用者 | SaaS | 地端 | 注入方式 | 實作 issue |
| --- | --- | --- | --- | --- | --- |
| Postgres superuser（`postgres`，`postgres.mode: bundled`） | 只有 CloudNativePG 在 pod 內經 local socket 使用（建立 AGE extension、角色） | 沒有可用的密碼：Cluster 設 `enableSuperuserAccess: false`，`postgres` 沒有密碼、沒有 `<cluster>-superuser` Secret（2026-10-12 實驗機 e2e 驗證） | 同左；`external`：由客戶保管，不交給 chart | 不注入任何服務 | Laysi/trustiot-v2#58 |
| 叢集內 Postgres 的 CA 與伺服器私鑰（Secret `<cluster>-ca`、`<cluster>-server`） | CloudNativePG 簽發與換發伺服器憑證（90 天，到期前 7 天自動換發） | operator 在叢集內產生，不進 git | 同左；`external` 時客戶只提供 CA 憑證（`postgres.external.caSecret`） | 只有 `<cluster>-ca` 的 `ca.crt` 以唯讀 volume 掛進在 `mesh.calls` 列出 `postgres` 的服務；`ca.key` 與伺服器私鑰不進任何服務（chart 測試） | Laysi/trustiot-v2#58 |
| Postgres 的 migration 角色：`bundled` 是資料庫 owner `trustiot_owner`（有 CREATEROLE，沒有 SUPERUSER、BYPASSRLS），`external` 是客戶提供的同等角色 | migration Job 套用 migration、建立服務的角色 | `bundled`：密碼由 CloudNativePG 產生在 `<cluster>-app`，不進 git | `bundled` 同左；`external`：客戶建立的 Secret（`postgres.external.migrationSecret`） | 環境變數，只給 migration Job；服務的自訂 `env` 不能讀它（chart 測試） | Laysi/trustiot-v2#58（owner）、Laysi/trustiot-v2#60 |
| Postgres 的服務登入角色 `<服務>_app`（決策 4、29），每個服務一個 Secret `<release>-<服務>-password`，key 是角色名稱 | 各服務連 Postgres；migration Job 產生並以同一個值設定角色的密碼（只送 SCRAM-SHA-256 verifier，明文不進 SQL 與日誌） | migration Job 在叢集內產生，不進 git、不經過人（上方「migration Job 產生的密碼」） | 同左 | 服務：環境變數 `TRUSTIOT__DATABASE__PASSWORD`（`secretKeyRef`，只有自己的 Secret）；migration Job：經 Kubernetes API 讀寫，Role 只有這些 Secret 的 `get`、`patch`；其他服務的自訂 `env` 不能讀它（chart 測試） | Laysi/trustiot-v2#60 |
| AGE 每租戶登入角色（決策 21） | field-context 以租戶專屬角色執行 Cypher | 由程式在建立租戶時產生，不進 git、不經過人 | 同左 | 由 Laysi/trustiot-v2#176 決定 | Laysi/trustiot-v2#176 |
| 叢集內 ClickHouse 的 bootstrap 管理帳號密碼（`clickhouse.mode: bundled`），Secret `trustiot-clickhouse-admin`、key `password` | ClickHouse 本身；migration Job（Laysi/trustiot-v2#60）以它套用 DDL、建立 collector 帳號 | SOPS（`deploy/flux/`，以 `openssl rand -hex 32` 產生） | 客戶在安裝前建立（`deploy/chart/README.md`「叢集內的 ClickHouse」的指令） | 唯讀 volume 掛成檔案 `/etc/trustiot/clickhouse-admin/password`（`CLICKHOUSE_PASSWORD_FILE`），只掛進 ClickHouse 的 pod；服務的自訂 `env` 不能讀它（chart 測試） | Laysi/trustiot-v2#59、Laysi/trustiot-v2#60 |
| ClickHouse 帳號：ingest 寫入、每租戶唯讀（決策 14） | ingest、telemetry-query | 還沒有：ingest 的寫入帳號在 ingest 服務加入時由 migration Job 以管理帳號建立（Laysi/trustiot-v2#91；目前的 Job 只建立角色 `trustiot_ingest`，不建帳號）；每租戶唯讀帳號由 telemetry-query 建立租戶時產生（Laysi/trustiot-v2#119） | 同左 | 環境變數，只給使用該帳號的服務 | Laysi/trustiot-v2#91、Laysi/trustiot-v2#119 |
| OpenTelemetry collector 的 ClickHouse 帳號密碼，Secret `<release>-otel-collector-password`，key 是帳號名稱 | collector 寫 `trustiot_logs` 的兩張日誌表（決策 14、22）；migration Job 產生密碼並以它建立帳號 `trustiot_collector`，只授與角色 `trustiot_otel_collector`（`services/ingest/migrations/clickhouse/0006_logs.sql`） | migration Job 在叢集內產生，不進 git、不經過人（上方「migration Job 產生的密碼」） | 同左 | 環境變數 `CLICKHOUSE_PASSWORD`（`secretKeyRef`），只給 collector；collector 以 init container 等 Job 完成才啟動；服務的自訂 `env` 不能讀它（chart 測試） | Laysi/trustiot-v2#73、Laysi/trustiot-v2#60 |
| ClickHouse 帳號 `trustiot_tenant_view_definer` | view `trustiot.tenant_telemetry` 的 DEFINER（`services/ingest/migrations/clickhouse/0005_tenant_view.sql`） | 不是密鑰：`IDENTIFIED WITH no_password HOST NONE`，不能登入、沒有密碼要保管或輪替；只有 `SELECT ON trustiot.telemetry` | 同左 | 不注入任何服務 | Laysi/trustiot-v2#260 |
| MQTT dynsec 管理帳密 | certs、gateway-ops 註冊與停用閘道器（決策 6） | 部署時在叢集內產生的 Secret | 同左 | 環境變數，只給需要的服務 | Laysi/trustiot-v2#82、Laysi/trustiot-v2#84 |
| Grafana 管理者帳密 | 維運看板（決策 23） | 部署時在叢集內產生；登入改用 SSO 時移除 | 客戶的 Secret | Grafana 自己讀 Secret | Laysi/trustiot-v2#183 |

### 外部服務

| 密鑰 | 用途與使用者 | SaaS | 地端 | 注入方式 | 實作 issue |
| --- | --- | --- | --- | --- | --- |
| LLM、embedding、rerank 的 API key | AI 閘道呼叫供應商（決策 15、26、28） | SOPS | 客戶端點需要時，客戶建立的 Secret | 環境變數，只給 `ai-gateway`（chart 強制） | Laysi/trustiot-v2#139 |
| Qdrant 唯讀與寫入 API key | knowledge 檢索與背景處理（決策 19） | SOPS（Qdrant Cloud 主控台發的兩把 key） | `bundled`：部署時在叢集內產生 | 環境變數，只給 `knowledge`（chart 強制） | Laysi/trustiot-v2#194 |
| 物件儲存存取金鑰 | PDF、頁面圖、附件（決策 20） | SOPS | 客戶建立的 Secret | 環境變數，只給列出 `object-storage` 的服務 | Laysi/trustiot-v2#133 |
| 備份用的物件儲存金鑰 | Postgres 的 WAL 與完整備份、ClickHouse 備份（M8） | SOPS；與服務用的金鑰分開、只能寫備份 bucket | 客戶建立的 Secret | 只給備份元件 | Laysi/trustiot-v2#134、Laysi/trustiot-v2#135 |
| SMTP 帳密 | 通知與報表寄信（決策 18） | SOPS | 客戶的 relay 需要驗證時，客戶建立的 Secret | 環境變數，只給列出 `smtp` 的服務 | Laysi/trustiot-v2#169 |
| Sentry DSN | 錯誤回報（決策 24） | SOPS | 預設關閉；客戶開啟時由客戶建立 | 環境變數 `SENTRY_DSN`，每個服務 | Laysi/trustiot-v2#184 |
| SIEM 轉送的認證標頭與 CA（`observability.collector.forward.authHeader.valueSecret`、`observability.collector.forward.caSecret`） | collector 把遮蔽後的日誌以 OTLP 轉送到客戶的 SIEM（決策 22） | 預設不轉送 | 客戶開啟轉送時由客戶建立 | 標頭值是環境變數 `FORWARD_AUTH_HEADER`（`secretKeyRef`），CA 以唯讀 volume 掛成 `/etc/trustiot/forward-ca/ca.crt`，都只給 collector | Laysi/trustiot-v2#73 |

### 主機、映像與發佈

| 密鑰 | 用途與使用者 | 保管 | 注入方式 | 實作 issue |
| --- | --- | --- | --- | --- |
| Cloudflare Tunnel token | SaaS 主機上的 cloudflared 連出（決策 27） | 主機上只有 root 能讀的檔案；不在叢集、不在 git | systemd `LoadCredential` | Laysi/trustiot-v2#64（runbook 在 `deploy/host/README.md`） |
| 維運 SSH | 經 tunnel 進 SaaS 主機 | 沒有共用密鑰：Cloudflare Access 以個人身分驗證，主機只接受個人的 SSH 公鑰 | 不適用 | Laysi/trustiot-v2#64 |
| GHCR 拉取 token | SaaS 叢集拉映像與 chart（決策 5） | 專用機器帳號的 classic personal access token，只有 `read:packages`、設定到期日（GHCR 只接受 classic token）；以 SOPS 加密的 `kubernetes.io/dockerconfigjson` Secret `ghcr-pull`（namespace `trustiot`）進叢集，檔案 `deploy/flux/clusters/saas/trustiot/ghcr-pull.sops.yaml`，以 `bun scripts/flux/ghcr-pull-secret.ts` 從 stdin 產生。GHCR 套件的可見性獨立於 repo：repo 公開後套件仍是私有，所以仍需要這把 token；套件若改為公開才不需要（不可逆，見 [CI 與發佈](ci-release.md) 的「GHCR 套件的可見性」，Laysi/trustiot-v2#345） | `global.imagePullSecrets` 與 Flux 的 OCIRepository `trustiot` 的 `secretRef` | Laysi/trustiot-v2#70 |
| Flux 的 deploy key | SaaS 叢集的 Flux 以 SSH 讀 main（決策 5） | 只給這個叢集的 ed25519 金鑰；公鑰在 GitHub repo 設定為 read-only deploy key，私鑰只在叢集 `flux-system` namespace 的 Secret `flux-system`（建立叢集時直接建立，不經 git，[deploy/flux/README.md](../../deploy/flux/README.md)） | 只給 Flux source-controller | Laysi/trustiot-v2#70 |
| 地端的映像取得 | 客戶安裝時拉映像 | 每個客戶一把自己的唯讀 token，或離線安裝包（MS6）；不共用 SaaS 的 token | 客戶建立的 `imagePullSecrets` | Laysi/trustiot-v2#46 |
| 發版說明的 `AI_API_KEY` | release.yml 以模型整理發版說明（選用） | repo 的 Actions secret | 只經 action 的 `with` 輸入給 `plan` job 產生發版說明的那一步；執行建置程式碼的 `build` job 拿不到任何 secret | Laysi/trustiot-v2#68 |
| `GITHUB_TOKEN` | 每個 job 的短效 token。發佈用到的權限：`publish` 以 `packages: write` 推映像與 chart 到 GHCR、以 `contents: write` 建 GitHub Release；`verify` 以 `packages: read` 拉回驗證；`report` 以 `contents: write`、`statuses: write` 寫驗證結果與 commit status；`plan` 的 `actions: read` 只讀 ci 的執行結果；`build` 只有 `contents: read` | GitHub 每次執行時產生，job 結束失效；repo 預設唯讀，每個 workflow 設 `permissions: {}`、每個 job 明列權限（`workflow-policy` 檢查）。推 GHCR 不用個人或機器帳號的 token | 不需保管；只經 step 的 `env` 交給需要的指令 | Laysi/trustiot-v2#67、Laysi/trustiot-v2#68 |
| 映像與 chart 的簽章（cosign keyless） | 只有 `publish` job 有 `id-token: write`：以 GitHub 的 OIDC token 向 Sigstore Fulcio 換約 10 分鐘的簽章憑證，簽章紀錄寫進公開的 Rekor（決策 30） | 沒有長期簽章金鑰，OIDC token 與憑證都是一次性的；簽章身分綁定 release.yml 與 tag，驗證指令見 ci-release.md「release.yml」 | 不需保管 | Laysi/trustiot-v2#68 |
| `release` environment | `publish` job 的執行環境 | 不是密鑰：目前沒有 environment secret，也沒有設必要審查者（2026-10-10 私有庫設不了，決策 30；2026-10-12 repo 公開後設得了，尚未設定，Laysi/trustiot-v2#345）；新增 environment secret 前要先確認它只交給 `publish` | 不適用 | Laysi/trustiot-v2#68、Laysi/trustiot-v2#164 |

### 裝置端與應用資料

| 密鑰 | 保管 | 說明 | 實作 issue |
| --- | --- | --- | --- |
| 閘道器私鑰 | 閘道器的 TPM，不能匯出 | 雲端只存出廠登錄的公鑰（決策 12）；不需要輪替，換 TPM 或主機板視同新設備 | Laysi/trustiot-v2#74、Laysi/trustiot-v2#80、Laysi/trustiot-v2#83 |
| 產線工作站憑證私鑰 | 工作站本機，能用 TPM 時放在 TPM | 由簽發 CA 簽發的工作站憑證，到期換發；工作站汰換時撤銷 | Laysi/trustiot-v2#79、Laysi/trustiot-v2#80 |
| 使用者 session | 伺服器只存 cookie 值的雜湊 | 沒有簽章密鑰（規則 8）；登出、停權在下一個請求生效（決策 13） | Laysi/trustiot-v2#103 |
| 一次性 token 與驗證碼 | 只存雜湊 | 見上方「一次性 token 與驗證碼」 | Laysi/trustiot-v2#103、Laysi/trustiot-v2#105 |
| 租戶設定的祕密：Webhook 簽章密鑰、企業 OIDC 的 client secret | Postgres，屬於租戶資料 | 建立後 API 不再回傳明文、不進日誌；靜態加密方式由實作 issue 決定並加進本清冊 | Laysi/trustiot-v2#169、Laysi/trustiot-v2#43 |
| 本機開發帳密 | 開發者的 `$XDG_STATE_HOME/trustiot-v2/dev/<實例>/state.json`（0600） | 每次 `up` 產生、`down` 刪除，不在 repo 內 | Laysi/trustiot-v2#71 |
