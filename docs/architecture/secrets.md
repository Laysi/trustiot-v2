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
| Kubernetes Secret（YAML 或 JSON）一律以 SOPS 加密：`data` 與 `stringData` 的每個值都是 SOPS 寫出的格式（AES256_GCM、32 位元組 IV、16 位元組 tag），`sops` 區塊有版本、修改時間、加密的 MAC 與 age 收件者及其以 age 格式包裝的資料金鑰；值、IV、tag、MAC 與包裝的資料金鑰解碼後都不能是可讀文字（只有 `ENC[` 開頭、把明文 base64 後套上格式或手寫的區塊都不算）。每個以 SOPS 加密的檔案（有頂層 `sops` 區塊的文件）都只是一個 Secret（`apiVersion: v1`），頂層只有 `apiVersion`、`kind`、`metadata`、`type`、`data`、`stringData`、`sops`；`sops.encrypted_regex` 剛好是 `^(data|stringData)$`，沒有 `unencrypted_regex`、`encrypted_suffix`、`unencrypted_suffix`、`encrypted_comment_regex`、`unencrypted_comment_regex`、`mac_only_encrypted`（檔案自己的 `sops` 區塊決定加密範圍，不受 `.sops.yaml` 約束）；`data`、`stringData` 以外沒有 `ENC[` 的值；`metadata` 只有 `name`、`namespace` 與 label `reconcile.fluxcd.io/watch: Enabled`，沒有其他 label、annotation、ownerReferences 或 finalizers。審查的人看得到值以外的一切，所以加密範圍不能擴大到 label、annotation 或 type，Secret 也不能藉 Helm 的歸屬 label 或 annotation 被某個 release 收編（2026-10-20 起，Laysi/trustiot-v2#464）。每個叢集能有哪些 SOPS Secret（名稱與 type）列在 `scripts/flux/lib.ts` 的 `SOPS_SECRETS`，由 `bun test deploy/flux` 與產生 Secret 的腳本檢查。CI 沒有解密金鑰（GitHub 不持有部署憑證），所以無法以密碼學證明內容真的加密，這一項擋的是不小心提交的明文；真正的驗證在 Flux 解密時檢查 MAC，失敗就不套用；不用 Kustomize 的 `secretGenerator`（它的 `literals`、`files`、`envs` 都是檢查看不到的明文） | repo 內每個 YAML 與 JSON 檔 |
| Helm 樣板以兩種方式讀（2026-10-14 起，Laysi/trustiot-v2#398：先前只認得 `kind: Secret` 這種寫法的行，換個寫法的 `kind` 與由樣板動作印出的 Secret 都能通過，helm 卻渲染出帶 `data` 的 Secret）。**一、當成 YAML**：拿掉樣板動作（印在一行之中的換成佔位字，獨立成行或不印東西的直接拿掉），檔案本身與每個 `define` 各自交給 YAML 解析器。解析器找到的每個 `kind` key（不分大小寫、任何深度）都必須來自寫成 `kind: <Kind>` 的一行（值可以加引號，或是樣板動作）；其他寫法（加引號或跳脫的 key、tag、anchor 或 alias、merge key、冒號前有空白、值在下一行或在 block scalar、flow 或 JSON 形式、由動作與文字組出的 key）不論值是什麼一律擋下，同一個物件有兩個 kind、或一行 kind 經 anchor 套用到多個物件也擋。解析器認得 YAML 的每一種寫法，所以這條規則不靠列舉寫法。這些行再依解析出的物件判斷：Secret 與樣板化的 kind 一律擋下，例外只有以名稱引用 Secret（沒有 `apiVersion`，有 `name` 或 `group`，例如 Gateway 的 `certificateRefs`），以及 migration Job 填入的空 Secret：帶標籤 `trustiot.io/filled-by: migrate`，只有 `apiVersion`、`kind`、`metadata`、`type` 四個 key（文字與解析出的物件都檢查），含樣板動作的行只能是檢查程式列出的三行（名稱 `name: {{ .name }}`、chart 的標準標籤 include、range 的 `{{- end }}`），逐字比對（2026-10-13 起，Laysi/trustiot-v2#60 第四輪：先前「整行動作只能渲染在更深的縮排」的寬鬆判斷可以被同一行的 `data:`、以 values 或 `randAlphaNum` 產生的 annotation 繞過）。`List` 結尾的 kind 也擋，因為 `items` 可以由動作印出。會被渲染的樣板（檔名不以 `_` 開頭）拿掉動作後必須能解析，頂層的 key 也不能由動作組出。**二、依動作印出的內容**：動作印出的文字解析器看不到，所以動作不能印在輸出會成為 manifest 本身 key 的位置（文件層級：沒有縮排在某個 key 底下，包括 `nindent` 了但上方沒有 key 的情形，以及同一行前面有文字、沒有 `nindent` 的 `toYaml`），例外只有 `include` 或 `template` 一個本身宣告完整 manifest 的 chart helper：同名的每個 `define` 都能解析、沒有由動作組出的頂層 key、不宣告任何 Secret（連 Job 的空 Secret 也不行，因為印出它的樣板可以在後面補上 `data`；Job 的空 Secret 只能寫在樣板本身，helper 與 `define` 以外的文字都不行），而且它在文件層級也只印這樣的 helper。所以以 `toYaml`、`toJson` 輸出 `.Values` 的子樹或 `dict`、印出 values 的字串、`.Files.Get`，都只能以 `nindent` 縮排在 key 底下。動作裡的字串不能寫出 kind key（`"kind: Secret"`、`dict "kind" ...`，Go 字串的跳脫先解開再比對），也不能含換行（YAML 讀者把 CR、LF、NEL、LS、PS 都當換行）；動作不讀 chart 的檔案（`.Files`），也不用 `tpl`（它把 values 當樣板渲染）。印在一行之中的值（`name: {{ .name }}`）不能開出新的一行或新的文件：chart 渲染時以 `templates/values-check.yaml` 拒絕任何含換行的 values 字串、map key 與清單項目（`trustiot.values.singleLine`，略過 schema 驗證時也一樣）。任何 Helm 樣板與 helper 都不能用產生或讀取密鑰材料的函式：`lookup`、`rand*`、`uuidv*`、`shuffle`、`gen*` 憑證與金鑰、`derivePassword`、`htpasswd`、`bcrypt`、AES 函式、`getHostByName`。這些規則放在每個 PR 都跑的 `check-secret-handling.ts`，而不是只在 chart 變動時才跑的 chart 測試，因為它涵蓋 repo 裡所有樣板。文字檢查看不到刻意在渲染時才拼出 kind 的樣板（例如把字串拆開再組合），這部分由 chart 測試以每個可選元件都開啟的設定渲染後檢查（見下方） | 每個 chart 的 `templates/` 下所有檔案（不論副檔名，`NOTES.txt` 除外，因為 Helm 會把它們都當成 manifest 渲染；同一個 chart 的樣板一起讀，因為樣板會印出其他檔案定義的 helper），以及 repo 內其他含 `{{` 的 YAML 與 `.tpl` |

chart 的規則由 `bun test deploy/chart` 檢查，見 [deploy/chart/README.md](../../deploy/chart/README.md)：

- values 只以名稱引用 Secret，values 沒有明文密鑰。
- chart 不渲染任何密鑰值。它可以宣告空的 Secret，由叢集內的元件填入；目前只有 tools/migrate 的程式：migration Job（下方「migration Job 產生的密碼」）、叢集內 ClickHouse 的 init container（下方「叢集內 ClickHouse 管理帳號的密碼」）、身分服務的 init container 及金鑰輪替的 CronJob（下方「內部身分簽章金鑰」）與 certs 服務的 init container（下方「閘道器憑證 CA」）。除了兩種 shape 的渲染，chart 也以每個可選元件都開啟的設定渲染：`values.yaml` 每個布林的 `enabled` 都設為 `true`，`values.schema.json` 每個 `mode` 的每個值各渲染一次，兩種 shape 涵蓋 `global.deploymentMode` 與 `ingress.exposure`；開關從 values 與 schema 讀出，新增的元件自動涵蓋（2026-10-14 起，Laysi/trustiot-v2#398）。渲染結果交給 `check-secret-handling.ts` 的同一條規則（每個 kind 都寫成 `kind: <Kind>`，Secret 只能是 tools/migrate 填的空 Secret），`scripts/ci/check-secret-handling.ts` 變動時也會觸發這個測試。測試檢查渲染出的每個 Secret 都沒有 `data`、`stringData`，都是 Job、ClickHouse、身分服務或 certs 服務的 init container 填入的那幾個，metadata 逐欄等於預期（名稱、chart 的標準標籤加 `trustiot.io/filled-by: migrate`、annotation 只有 `helm.sh/resource-policy: keep` 與說明；chart 不渲染 tools/migrate 自己寫的 annotation）；Job 的 Role 對 `secrets` 只有它填的那幾個的 `get`、`patch` 與 ClickHouse 管理帳號 Secret 的 `get`，ClickHouse 的 Role 只有它自己那一個的 `get`、`patch`，身分服務的 Role 只有簽章金鑰的 Secret 與公鑰 ConfigMap 的 `get`、`patch`，certs 服務的 Role 只有簽發 CA 金鑰的 Secret 與 CSR 的 ConfigMap 的 `get`、`patch`，都以 `resourceNames` 限定；chart 的其他 Role 都不碰 `secrets`，也不能寫 `configmaps`。

### migration Job 產生的密碼

各服務的 Postgres 登入角色（`<服務>_app`，以及 outbox 消費端的平台角色 `<服務>_platform`）與 OpenTelemetry collector 的 ClickHouse 帳號，是 migration Job 自己建立的內部帳號，和「ClickHouse 帳號：ingest 寫入」一樣屬於叢集內產生的類別：沒有人建立、複製或輪替它們的密碼（2026-10-13 依使用者意見，取代原本由人建立 Secret 的做法，Laysi/trustiot-v2#60）。

- **放在哪裡**：chart 為每個消費者宣告一個 Secret（`<release>-<服務>-password`、`<release>-otel-collector-password`），key 是角色或帳號名稱。每個消費者一個，而不是共用一個，所以節點的 kubelet（node authorizer）只讀得到排在該節點的 pod 用到的密碼。以平台角色登入的服務（outbox 的消費端，chart 的 `trustiot.migrate.platformLogins`，目前是 `assets`，Laysi/trustiot-v2#65）兩個角色的密碼在它自己的同一個 Secret，兩個 key；兩個密碼給的是同一個 pod，分成兩個 Secret 不會減少任何節點讀得到的東西。
- **誰填**：migration Job 在 migration 鎖內，以作業系統的 CSPRNG 為缺的 key 產生 256 位元的密碼（64 個十六進位字元），以帶 `resourceVersion` 的 merge patch 寫入；之後把角色與帳號設成 Secret 裡的值（Postgres 只送 SCRAM-SHA-256 verifier、ClickHouse 只送加鹽的 SHA-256，明文不進 SQL）。日誌、錯誤訊息與測試輸出只有名稱，沒有密碼、密碼的 hash 或 Secret 的資料。
- **權限**：Job 的 projected token 只掛進 Job 的容器；它的 Role 只有這些 Secret 的 `get`、`patch`（`resourceNames`）。沒有 `create`：它無法以名稱限制，而且能替 namespace 裡任何 ServiceAccount 建立 `kubernetes.io/service-account-token` Secret；也沒有 `list`、`watch`。服務與 collector 的 ServiceAccount 沒有新的權限，各自以 `secretKeyRef` 讀自己的 key。
- **輪替**：調大 `migrate.passwordGeneration`；Secret 的 annotation `trustiot.io/password-generation` 記錄已套用的值，以數字比較、只往上走，所以重跑不會再換一次，rollback 渲染出較小的值時也不換（之後升級回來同樣不換）。服務與 collector 隨 Job 名稱滾動更新。舊 pod 在被換掉之前開新連線會被拒（Postgres 一個角色只有一個密碼），已開的 Postgres 連線不受影響；步驟與這段時間見 [docs/runbooks/secrets.md](../runbooks/secrets.md)。
- **只信任 Job 填過的物件**：Job 寫入時在同一個 patch 記下 Secret 的 `metadata.uid`（annotation `trustiot.io/password-secret-uid`，chart 不渲染）；uid 不符時（有人刪掉再建一個同名的 Secret、或從備份還原）每個 key 都重新產生，所以預先建好、帶著自選密碼的 Secret 不會被採用。同一個 patch 也拿掉 chart 與 Job 不設定的 metadata：白名單以外的每個 annotation 與標籤（保留 chart 為這些 Secret 設定的、Helm 的 ownership annotation、Flux helm-controller 的兩個標籤，以及 Job 自己的兩個 annotation）、`ownerReferences` 與 `finalizers`。所以預先建立的 Secret 帶著的 replicator 或 reflector annotation 不會讓產生的密碼被複製到別的 namespace，owner 也不會讓它被垃圾回收；這些 metadata 和產生的資料不會同時存在，因為兩者在同一個 patch 裡。日誌與報告只寫被拿掉的 key，不寫值。已綁定的 Secret 之後被改的 metadata 不再處理（2026-10-14，Laysi/trustiot-v2#60 第五輪）。剩下的風險：Kubernetes 的 RBAC 分開授予 `get` 與 `patch`，但能 patch 這些 Secret 的主體即使沒有 `get`，也能把 `data` 改成自選的密碼（Job 會把角色設成它，因為 uid 沒變），或在綁定之後加上 replicator 的 annotation；所以 patch 權限等同能取得這些密碼，只能給受信任的主體。chart 只把它給 Job 自己（Role 以 `resourceNames` 限定），namespace 裡其他能 patch Secret 的 Role 或 ClusterRole 都要當成能讀這些密碼來審查。
- **資料遺失**：Helm 不擁有這些 Secret 的資料（渲染結果沒有 `data`），升級與 rollback 都保留 Job 寫的值；有人刪了 key 或 Secret、或以 `helm upgrade --force-replace` 換掉它時（不要用它：它清空 Secret 之後在取代 Job 時失敗，升級停在 FAILED 而 Job 不再跑），改 `migrate.revision` 再升級：Helm 重建 Secret，Job 產生新密碼並設定（[docs/runbooks/migrations.md](../runbooks/migrations.md)）。
- **不再需要的帳號**：Job 只讓目前的設定用得到的帳號登入。部署關掉的服務（沒有綁定 Secret）與服務清單已經沒有的服務，它們的 Postgres 登入角色改成 `NOLOGIN`；只挑 Job 標記過（`COMMENT ON ROLE`）且 migration 角色有 ADMIN OPTION 的角色，不依名稱挑，不 `DROP`（角色可能擁有物件，NOLOGIN 可以還原）。collector 關閉或改帳號時，舊的 ClickHouse 帳號改成 `HOST NONE`（只挑拿到 `trustiot_otel_collector` 角色的帳號，不含 Job 執行時用的帳號；ClickHouse 沒有標記帳號的地方，所以外部 ClickHouse 上不要把這個角色給自己的帳號，否則它也會被鎖）。留下的 Secret（`keep`）裡的密碼因此登不進去，可依 runbook 刪除（2026-10-13，Laysi/trustiot-v2#60 第四輪）。
- **不提供外部覆寫**：客戶自己的 secret manager 不能指定這些密碼。它們不離開叢集，只給 Job 建立的帳號使用；讓人指定就回到人要建立、保管、輪替密碼的做法。客戶有這個需求時另開 issue 決定。

### 叢集內 ClickHouse 管理帳號的密碼

叢集內 ClickHouse（`clickhouse.mode: bundled`）的 bootstrap 管理帳號（`clickhouse.bundled.adminUser`，預設 `trustiot_admin`）只給 migration Job 用，同樣在叢集內產生，沒有人建立、看到或複製它（2026-10-15 起，Laysi/trustiot-v2#403；取代原本由人以 `openssl rand -hex 32` 建立 Secret、SaaS 以 SOPS 放進 `deploy/flux/` 的做法）。

- **放在哪裡**：chart 宣告空的 Secret `<release>-clickhouse-admin-password`，key 是帳號名稱，metadata 與上一節的 Secret 相同（`trustiot.io/filled-by: migrate`、`keep`）。
- **誰填**：不是 Job，而是 ClickHouse pod 的 init container（migration 映像的 `migrate clickhouse-admin-password`），規則與上一節相同：缺的、不能用的、代較舊的或不是它填過的物件裡的 key 以 CSPRNG 產生 256 位元的新密碼，以帶 `resourceVersion` 的 merge patch 寫入，記下代與 uid，拿掉白名單以外的 metadata。之後它把 Secret 裡的值寫進記憶體 emptyDir 的檔案（0400，原子替換），ClickHouse 的映像以 `CLICKHOUSE_PASSWORD_FILE` 讀它。由 init container 填，而不是 Job，是因為 ClickHouse 只在啟動時讀這個密碼：產生新密碼與伺服器換上它必須在同一個 pod 啟動裡依序發生；由 Job 產生時，Job 無法讓 ClickHouse 在它寫入之後重啟，輪替時兩者會不一致。
- **權限**：ClickHouse 的 pod 與 ServiceAccount 都不自動掛 token，只有 init container 掛 1 小時效期的 projected token；Role `<release>-clickhouse` 只有這一個 Secret 的 `get`、`patch`。Job 的 Role 對它只有 `get`（patch 權限等同能取得密碼，見上一節，所以只給 ClickHouse）：Job 經 Kubernetes API 讀密碼，不以 `secretKeyRef` 讀，所以首次安裝時 Job 的 pod 不會因為 Secret 還沒填好而停在 `CreateContainerConfigError`，而是等 init container 填入；ClickHouse 拒絕時（輪替途中）重讀再試。服務與 collector 的 pod 讀不到它：Secret 在 `trustiot.dependencySecretNames`，自訂 `env` 不能讀，也不掛進它們的 pod（chart 測試）。
- **輪替**：調大 `clickhouse.bundled.adminPasswordGeneration`。值進到 ClickHouse pod template 的 checksum，升級時 ClickHouse 重啟，init container 先產生新密碼，伺服器以新密碼啟動；只往上走，rollback 不換。步驟見 [docs/runbooks/secrets.md](../runbooks/secrets.md)。
- **取捨**：init container 用 migration 映像（只有 arm64），所以 ClickHouse 只排到 arm64 節點，而且每次換 TrustIoT 版本 ClickHouse 也重啟一次（單一副本，短暫中斷，collector 重試）。不另外做一個 ClickHouse 自己的小映像，避免多維護一個發佈物。
- **過渡**：Laysi/trustiot-v2#403 之前的安裝設定了 `clickhouse.bundled.adminSecret.name`，這時 chart 照舊掛那個人建立的 Secret，行為與之前相同（升級不重啟 ClickHouse）；把它改回空白的那次升級就搬到產生的密碼，之後刪掉舊 Secret（[deploy/chart/README.md](../../deploy/chart/README.md)「叢集內的 ClickHouse」）。
- 外部 ClickHouse（`clickhouse.mode: external`）的 `clickhouse.external.migrationSecret` 不變，仍由客戶提供。

### 內部身分簽章金鑰

身分服務簽內部身分 token 的 Ed25519 金鑰對（決策 27）在叢集內產生，沒有人建立、看到、複製或輪替它；公鑰自動交給每個服務，服務不重啟就換上新的公鑰（2026-10-15 設計，Laysi/trustiot-v2#404；取代人以 `openssl genpkey` 建立私鑰、SaaS 以 SOPS 保存、把公鑰手寫進 `internalIdentity.publicKeys` 的做法）。正在簽章的金鑰超過 `internalIdentity.maxKeyAgeSeconds`（預設 90 天）時，叢集自動輪替它（2026-10-15 設計，Laysi/trustiot-v2#429，下方「依金鑰年齡自動輪替」）。

- **放在哪裡**：
  - 私鑰：chart 宣告的空 Secret `<release>-identity-signing-keys`（`templates/passwords.yaml`，metadata 與上兩節的 Secret 相同：`trustiot.io/filled-by: migrate`、`helm.sh/resource-policy: keep`）。key `signing-keys.json` 是一份 JSON，列出目前的每一把金鑰：kid、代、Ed25519 私鑰、公開的時間 `published_at` 與開始簽章的時間 `signs_from`（還沒公開的金鑰沒有這兩個時間）。kid 是公鑰的 RFC 7638 thumbprint（SHA-256，base64url，43 個字元）；從人建立的私鑰匯入的金鑰保留原本的 kid（下方「從人建立的私鑰搬過來」）。
  - 公鑰：chart 宣告的空 ConfigMap `<release>-internal-identity-keys`（`templates/internal-identity.yaml`，同樣的標籤與 `keep`）。key `jwks.json` 是 RFC 7517 的 JWK Set，只有公開的成員（`kty`、`crv`、`kid`、`x`、`alg`、`use`）。它一律由 Secret 推出：Secret 裡有哪幾把金鑰，ConfigMap 就列哪幾把的公鑰，不多也不少。
  - 兩者的 `data` 都不屬於 Helm（渲染結果沒有 `data`），升級與 rollback 都不動它們；uninstall 後留下，重新安裝時沿用。
- **誰產生、誰讀得到**：身分服務 pod 的 init container `signing-keys`（migration 映像的 `migrate identity-signing-keys`，在 `wait-for-migrations` 之前執行，因為它不需要資料庫）以身分服務 ServiceAccount 的 1 小時 projected token 讀寫這兩個物件。依金鑰年齡輪替的 CronJob `<release>-identity-key-rotation`（Laysi/trustiot-v2#429）以同一個 ServiceAccount 執行同一個指令，不另外授權（下方「依金鑰年齡自動輪替」）。
  - Role `<release>-identity-signing-keys` 只讓身分服務的 ServiceAccount 對這一個 Secret 與這一個 ConfigMap `get`、`patch`（`resourceNames`）；沒有 `create`（無法以名稱限制）、`list`、`watch`、`delete`。
  - 身分服務的主容器沒有 token：`automountServiceAccountToken: false`，chart 對身分服務強制，設成 `true` 就渲染失敗。它只以唯讀 volume 讀這個 Secret（`/etc/trustiot/identity-signing-keys/signing-keys.json`，0440，只有 root 與 pod 的 fsGroup 讀得到，與現行相同）。
  - 其他 pod 都不掛這個 Secret（輪替的 CronJob 也不掛，它經 API 讀寫），自訂 `env` 也不能讀它（它在 `trustiot.dependencySecretNames`），chart 的其他 Role 都不碰它。所以讀得到私鑰的只有身分服務的 ServiceAccount，以及排了身分服務 pod 的節點的 kubelet（node authorizer 讓 kubelet 讀它的 pod 用到的 Secret，與現行相同）；輪替的 CronJob 執行期間，排了它的節點的 kubelet 也持有身分服務 ServiceAccount 的 token（下方「依金鑰年齡自動輪替」）。chart 測試證明這點：以每個可選元件都開啟的設定渲染，檢查每個 Role、ClusterRole、RoleBinding 與每個 pod spec；nightly 另以 `kubectl auth can-i` 對 namespace 裡的每個 ServiceAccount 確認。私鑰不出現在 values、渲染結果、日誌、事件與測試報告。
  - 能 patch 這個 ConfigMap，就能讓每個服務接受自選的公鑰，等於能偽造任何人的內部身分，所以它和 patch 那個 Secret 一樣只給身分服務的 ServiceAccount。namespace 裡其他能 patch ConfigMap 或 Secret 的 Role 或 ClusterRole，都要當成能偽造內部身分來審查。

  比較過的做法：

  | 誰產生 | 誰讀寫 Secret | 取捨 |
  | --- | --- | --- |
  | 身分服務 pod 的 init container（採用） | 身分服務的 ServiceAccount，token 只掛進 init container | 私鑰只有身分服務的 ServiceAccount 碰得到。處理外來 cookie 與請求的主容器沒有 token：它被攻破時攻擊者拿得到當下的私鑰（任何做法都一樣），但不能把自選的金鑰寫進 Secret 或 ConfigMap、在輪替之後留下來。規則與程式和叢集內 ClickHouse 管理帳號的密碼相同（`tools/migrate/src/passwords.rs` 的綁定 uid、帶 `resourceVersion` 的 patch）。身分服務的 pod 本來就每次發版都重啟（`wait-for-migrations`）、只有 arm64 映像，沒有新的取捨。代價是它只在身分服務的 pod 啟動時執行，所以舊金鑰要等下一次身分服務啟動才移除 |
  | migration Job | Job 的 ServiceAccount | Job 能讀寫 Secret 就讀得到私鑰（patch 的回應也帶 `data`），違反「只有身分服務讀得到」；Job 還以 owner 角色執行 SQL、讀 ClickHouse 管理帳號的密碼，是又一個能拿到私鑰的工作負載 |
  | 身分服務的主容器 | 身分服務的 ServiceAccount，token 掛進主容器 | 主容器被攻破時，攻擊者拿到能 patch 這兩個物件的 token，可以把自己的公鑰寫進 ConfigMap，換金鑰也趕不走；也違反規則 5「服務沒有 ServiceAccount token」 |
  | 另一個常駐的控制器或 CronJob | 它自己的 ServiceAccount，或借用身分服務的 | 又一個持有私鑰的工作負載。Laysi/trustiot-v2#404 時它的用處只有讓舊金鑰準時移除；簽章的切換已經以時間寫進資料（下方「輪替」），不需要它。Laysi/trustiot-v2#429 為了依年齡輪替加上一個借用身分服務 ServiceAccount 的 CronJob，取捨見下方「依金鑰年齡自動輪替」；產生第一把金鑰與人發起的輪替仍由 init container 做 |

- **公鑰怎麼交給服務**：每個服務的 pod 以唯讀 volume 掛這個 ConfigMap（`/etc/trustiot/internal-identity/`，掛整個 ConfigMap、不用 `subPath`，`optional: true`），平台設定 `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` 指向其中的 `jwks.json`，取代放 JWK Set 本身的 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`。ConfigMap 改變後，kubelet 在下一次定期同步 pod 時更新掛載的檔案，整個目錄以 symlink 原子替換，讀到的不是舊的一份就是新的一份（`subPath` 掛載不會更新，所以不用）。service-kit 每 5 秒重讀這個檔案，內容是合法的 JWK Set 就整組換上，記一行 INFO 與 kid，所以服務拿到新公鑰不必重啟。
  - 還沒有任何公鑰時（全新安裝，init container 還沒填入，或 kubelet 還沒更新檔案）：服務照常啟動，但 `/readyz` 回 503、不接流量，拿到第一組才就緒。
  - 檔案不見、空的或不是合法的 JWK Set 時（ConfigMap 被刪或被改壞）：保留上一組可用的公鑰繼續驗證，記 WARN（只寫原因，不寫內容）；啟動時就沒有可用的一組則不就緒。
  - 身分服務停機時：沒有影響。服務只讀自己掛載的檔案，不在執行時向身分服務抓取（決策 27），已經發出去的 token 照樣驗證。
- **輪替**：調大 `internalIdentity.keyGeneration`。值進到身分服務 pod 的 init container 的環境變數（`TRUSTIOT__KEYS__GENERATION`），pod template 因此改變，所以升級時身分服務滾動重啟（其他服務不重啟，chart 測試檢查它們的 pod template 與平台設定不變），第一個新 pod 的 init container 依序：
  1. 以作業系統的 CSPRNG 產生新的私鑰，加進 Secret（帶 `resourceVersion` 的 merge patch）；這時它還沒有公開時間，誰都不會用它簽章。
  2. 把 ConfigMap 的 `jwks.json` 改成 Secret 裡每一把的公鑰，新舊並存。
  3. 第 2 步成功之後再 patch Secret，記下新金鑰的 `published_at`（這一刻）與 `signs_from = published_at + internalIdentity.keyPropagationSeconds`。

  之後不必再有人或程式介入：
  - **簽章**：身分服務的每個副本每 5 秒重讀掛載的 Secret 與 ConfigMap，以同一條規則選簽章的金鑰：`signs_from` 已到、代不小於 `revokeBeforeGeneration`（下方「私鑰外洩」）、自己持有私鑰、而且在自己看到的 `jwks.json` 裡，這幾把之中 `signs_from` 最晚的一把。所以多個副本在同一時間用同一把（各節點的時鐘差在 5 秒內，交界的幾秒兩把都還有效），早就在跑的副本也會換，不必重啟。一把都沒有時不就緒，cookie 檢查回 503（fail closed）。
  - **「每個服務都拿到了」以實測的時間加餘裕判斷，不是保證**：Kubernetes 文件寫的延遲是同步週期（kubelet 的 `syncFrequency`，預設 1 分鐘）加上快取的延遲（預設的 Watch 策略是 watch 本身的延遲），再加上服務每 5 秒重讀，正常情況下 2 分鐘內每個 pod 都看到新的一組。這不是 Kubernetes 保證的上限：例如節點與 API server 的 watch 卡住時，那個節點的 pod 可以更久都是舊的檔案。`keyPropagationSeconds` 預設 600 秒，比正常情況多約 5 倍的餘裕；schema 的下限是 120 秒，調大了 `syncFrequency` 的叢集要跟著加大它。2026-10-15 在 lab 的 k3s 以兩種 shape 實測（[測試報告](../test-reports/2026-10-15-404-identity-keys-in-cluster.md)）：已經在跑的服務 pod 在 init container 寫入 ConfigMap 之後 3.9 到 84.4 秒換上新的一組（輪替 55.4 與 59.9 秒、撤銷 84.4 與 78.9 秒；kubelet 的定期同步會隨機延後，所以可能超過 1 分鐘），新啟動的 pod 一啟動就讀到；nightly 的 k3d 量的是全新安裝（下方「全新安裝」）。不採用自動逐一確認每個 pod（見「不做的事」），所以設計的安全性不依賴這個時間：某個 pod 超過它還沒更新時的後果，依情況是：
    - 輪替：那個 pod 拒絕新金鑰簽的 token（401，`reason` 是 `unknown_key`，WARN 寫出 kid），直到它的檔案更新；它不會接受不該接受的 token（fail closed），影響只有可用性，而且只在那個 pod。
    - 移除舊金鑰：那個 pod 多接受舊金鑰一段時間；舊金鑰沒有外洩，不影響安全。
    - 撤銷：那個 pod 在檔案更新之前仍接受外洩的金鑰，這是唯一影響安全的情況，所以撤銷的步驟由人逐一確認每個 pod（下方「私鑰外洩」）。
  - **私鑰比公鑰先到**：第 1 步把私鑰寫進 Secret 比第 2 步公開公鑰早，所以正常情況下到 `signs_from` 時，每個身分服務的副本也都已經拿到新私鑰。還沒拿到的副本繼續用舊的一把，舊的一把那時還公開著；它的 Secret 更新慢到超過下面移除舊金鑰的時間，它簽的 token 才會被已更新的服務拒絕（401，只影響可用性）。
  - **舊公鑰保留到它簽的 token 全部過期之後**：新金鑰開始簽章後，舊金鑰簽的 token 最晚在 `signs_from` 之後 305 秒失效。305 秒是驗證端接受的最長效期 300 秒加 5 秒時鐘誤差（`crates/service-kit` 的 `MAX_LIFETIME_SECS`、`LEEWAY_SECS`），不靠身分服務實際簽的 60 秒，所以身分服務的效期改了（上限 300 秒）也成立。init container 在 `signs_from + keyPropagationSeconds + 305 秒` 之後的第一次執行（下一次發版，或任何身分服務的 pod 啟動；開了自動輪替時，也包括 CronJob 的下一次執行，預設一天內）從 Secret 與 ConfigMap 移除比新金鑰舊的每一把；多等一個 `keyPropagationSeconds`，是給 Secret 更新得慢的副本換金鑰的餘裕。舊公鑰因此常常留得比必要的久，這不影響安全：舊私鑰沒有外洩，也只有身分服務讀得到。
  - 新金鑰還在等 `signs_from` 時又調大一代：那一把沒簽過任何 token，直接換成新的一把，重新計時。Secret 最多 8 把（service-kit 接受的上限），超過時 init container 失敗並寫出最早可以移除的時間。
  - 全新安裝，或沒有任何還能用的金鑰時（下面的資料遺失與撤銷），新金鑰立即開始簽章（`signs_from = published_at`），因為沒有舊金鑰可以撐過等待。全新安裝時，pod 建立時掛上的是 init container 填入之前的空內容，kubelet 同步這個 pod 時才換成新的內容；身分服務的 init container 在 `wait-for-migrations` 之前執行，主容器與服務都等 migration Job 完成才啟動，所以通常一啟動就讀到，沒有的話等 kubelet 下一次更新（正常不超過一個同步週期）才就緒。2026-10-15 lab 的 k3s 實測（兩種 shape）：公開之後 23 到 27 秒（等 migration Job）主容器啟動，身分服務 1 秒內開始簽章、25 到 28 秒就緒；資產服務 27.4 到 29.9 秒載入公鑰（on-prem 那次在啟動後 5.9 秒才讀到）、28 到 31 秒就緒。
- **依金鑰年齡自動輪替**（2026-10-15 設計，Laysi/trustiot-v2#429）：正在簽章的金鑰公開超過 `internalIdentity.maxKeyAgeSeconds`（預設 7776000 秒，即 90 天；0 關閉）時，叢集自動加一把新金鑰，照上面「輪替」的同一套排程換掉它：先公開，`keyPropagationSeconds` 之後才簽章，舊金鑰在它簽的 token 全部過期後移除。沒有人要記得去換（內部祕密不靠人處理），外洩而沒被發現的金鑰可被利用的時間也有了上限。
  - **觸發：CronJob**。chart 的 CronJob `<release>-identity-key-rotation` 依 `internalIdentity.keyRotationSchedule`（預設每天一次 `17 3 * * *`，時區 `Etc/UTC`）執行 migration 映像的 `migrate identity-signing-keys`：與 init container 同一個指令、同一份程式，「誰在何時簽章、何時移除」都由 `crates/identity-keys` 決定，只多一個設定 `TRUSTIOT__KEYS__MAX_KEY_AGE_SECONDS`。它以身分服務的 ServiceAccount 執行，token 以與 init container 相同的 1 小時 projected volume 掛進它唯一的容器，所以不新增任何 ServiceAccount、Role 或 RoleBinding；它不掛 Secret，經 API 讀寫。比較過的觸發方式：

    | 觸發 | 取捨 |
    | --- | --- |
    | CronJob（採用） | 不依賴 pod 重啟：地端好幾個月不升級、身分服務也沒重啟，照樣輪替。舊金鑰也在過期後的下一次執行就移除，不必等身分服務重啟。代價是又一個在記憶體裡拿到私鑰的 pod（Laysi/trustiot-v2#404 不採用 CronJob 的理由）：它與 init container 同一個 ServiceAccount、映像與程式，執行完就結束（通常幾秒），沒有人能連進它（下面「網路」），所以拿得到私鑰的主體沒有變多。變多的是節點：排了這個 pod 的節點的 kubelet 在 pod 執行期間持有身分服務 ServiceAccount 的 token（1 小時），與排了身分服務 pod 的節點相同 |
    | 身分服務 pod 的 init container 在啟動時檢查年齡 | 不多一個工作負載，但只在 pod 啟動時執行：地端長期不升級、身分服務也沒重啟時金鑰永遠不換，年齡沒有上限，達不到目的 |
    | 身分服務的主容器自己輪替 | 主容器要有能 patch Secret 與 ConfigMap 的 token，違反上面「主容器沒有 token」的理由：被攻破時能把自選的金鑰寫進去 |

  - **何時到期**：年齡從最新一把的 `published_at` 起算。每一把都已經開始簽章（沒有還沒公開或還在等 `signs_from` 的金鑰，也就是沒有進行中的輪替），而且最新一把的 `published_at + maxKeyAgeSeconds` 已到時「到期」，那次執行加一把新金鑰；沒到期時只做移除與撤銷。所以一把金鑰最多簽 `maxKeyAgeSeconds` 加一個排程間隔（預設一天）再加執行的時間。
  - **範圍**：`maxKeyAgeSeconds` 是 0，或介於 `2 × keyPropagationSeconds + 305` 秒與 31536000 秒（365 天）之間（chart 渲染時與 `migrate` 讀設定時都檢查）。下限保證到期時上一把舊金鑰的移除時間（新金鑰的 `published_at + 2 × keyPropagationSeconds + 305`，上方「舊公鑰保留到……」）已經過了，同一次執行就移除它，所以 Secret 平常兩把、移除前最多三把，碰不到 8 把的上限。要更久就設 0，改由人調大 `keyGeneration`。
  - **代**：自動輪替的新金鑰沿用 Secret 裡最高的代，不另外加一，所以代仍然只由 values 決定：
    - values 的 `keyGeneration` 是下限：不會產生比它小的代；`helm rollback` 渲染出較小的值時，自動輪替照樣以 Secret 裡最高的代加金鑰，代不往回走。
    - 人調大 `keyGeneration` 一定大於 Secret 裡的每一把，照上面「輪替」產生新的一代，與自動輪替做過幾次無關。
    - 撤銷 `revokeBeforeGeneration = N + 1` 一次移除第 N 代以前的每一把，包括自動輪替產生的。

    如果自動輪替把代加一，人看不到目前的代（只有身分服務的 ServiceAccount 讀得到 Secret，ConfigMap 不記代），手動輪替要先查出目前的代，依 values 算出的 `revokeBeforeGeneration` 也會漏掉自動產生的金鑰，撤銷無聲地失效，所以不採用。
  - **同時執行**：CronJob 與 init container（身分服務升級或重啟）同時執行，或兩次 CronJob 同時執行時，與兩個 init container 同時執行相同（上方「失敗情況」）：每一步都是帶 `resourceVersion` 的 patch，只有一個成功，另一個收到 409，重讀後重新決定。例如 CronJob 剛加了一把還沒公開的金鑰時，人調大 `keyGeneration` 的升級啟動了 init container：init container 把那把（沒簽過任何 token）換成新的一代，CronJob 重讀後看到進行中的輪替，不再加金鑰，只把公開補完。CronJob 以 `concurrencyPolicy: Forbid` 避免自己重疊，`startingDeadlineSeconds: 3600` 讓控制器停機很久之後不會因為錯過超過 100 次而停止排程。uid 綁定照舊：不是 init container 或 CronJob 填過的 Secret 一律不信任。
  - **人改 values 時**：`keyGeneration`、`revokeBeforeGeneration` 與 `keyPropagationSeconds` 同時交給 CronJob 與 init container，所以 CronJob 在身分服務重啟之前執行，也做同一件事。改 `maxKeyAgeSeconds` 不換金鑰，但身分服務會滾動重啟一次（下面的逾期檢查用這個值）；改小到已經到期時，下一次執行就輪替。改排程只改 CronJob。
  - **不渲染 CronJob 的情況**：`maxKeyAgeSeconds` 是 0、身分服務沒有啟用，或還在從人建立的私鑰搬過來（設了 `signingKeySecret`，那段期間不能換金鑰，下方「從人建立的私鑰搬過來」，第 2 步之後才開始依年齡輪替）。
  - **網路**：pod 以 `istio.io/dataplane-mode: ambient` 加入 mesh，與身分服務的 pod 相同，連 API server 的方式也與 init container 相同（API server 在 mesh 之外，ztunnel 直接放行；chart 的 NetworkPolicy 只管 ingress）。沒有人能連進它：沒有規則的 AuthorizationPolicy、`ingress: []` 的 NetworkPolicy，加上 namespace 的 default deny。它的 pod 沒有身分服務的 selector 標籤，所以只認身分服務 pod 的 NetworkPolicy（例如 Postgres 的）也不讓它連。
  - **維運人員看得到什麼**：
    - 每次執行在 Job 的日誌記一行 `the internal identity signing keys are in place`，寫出每把的 kid、正在簽章的 kid、最早可以移除舊金鑰的時間（`retire_at`）與下一次到期的時間（`rotation_due_at`）。輪替時多一行 `generated signing key <kid> (generation <代>) to replace key <kid>, published <RFC 3339 時間>, which reached internalIdentity.maxKeyAgeSeconds`，接著是公開與排程的變更。只有 kid、代與時間，沒有金鑰。
    - 失敗時程式以 1 結束，ERROR 寫出原因（與 init container 相同的訊息，例如 Secret 不見、API 拒絕）；Job 重試兩次仍失敗就是 Failed，CronJob 保留最近三個失敗的 Job（`kubectl get jobs`）與事件。失敗不影響服務：現有的金鑰繼續簽章，下一次排程再試。
    - CronJob 根本沒在跑時（被 suspend、映像拉不到、控制器停擺）沒有 Job 會失敗，所以身分服務自己檢查：正在簽章的金鑰的 `published_at` 已超過 `maxKeyAgeSeconds + keyPropagationSeconds + 2 天` 時記一次 ERROR（`the internal identity signing key is older than internalIdentity.maxKeyAgeSeconds allows …`），換了金鑰之後才會再記。2 天容許每天一次的排程失敗一次。排程必須每天至少一次：schema 只接受 `@daily`、`@midnight`、`@hourly` 與日、月、星期三欄都是 `*` 的 cron，否則金鑰會比 `maxKeyAgeSeconds` 多用好幾週，這行 ERROR 也會誤報。這行日誌與其他日誌一樣經日誌管線送進 SIEM。
    - nightly 在全新安裝上以 `kubectl create job --from=cronjob/…` 執行 CronJob 一次，確認它以身分服務的 ServiceAccount 執行、不掛 Secret、成功結束，而且沒有換金鑰（還沒到期），所以 chart、權限或網路規則的退化在合併前就看得到（[CI 與發佈](ci-release.md) 的 nightly）。
  - **驗證**（2026-10-15，[測試報告](../test-reports/2026-10-15-429-key-rotation-by-age.md)）：`crates/identity-keys` 測到期的邊界、代的下限、撤銷涵蓋自動金鑰、進行中的輪替不到期與最短年齡的連續輪替；`tools/migrate/tests/signing_keys.rs` 對假的 Secret API 測 CronJob 的輪替與移除、自動輪替開著時的手動輪替與撤銷，以及 CronJob 與 init container 同時執行；chart 測試證明 CronJob 不新增權限、不掛 Secret、沒有人能連進它，schema 拒絕比每天一次稀疏的排程。lab 的 k3s（`deploy/chart/tests/e2e/key-rotation.e2e.ts`，`maxKeyAgeSeconds` 900、每分鐘執行）：到期後 3 秒新金鑰公開，持續請求 327 次 0 次失敗，切換前取得的舊 token 在切換後仍被接受，沒有 pod 重啟；CronJob 在 `retire_at` 之後 57.3 秒移除舊金鑰；手動輪替與撤銷照常；`maxKeyAgeSeconds: 0` 時金鑰超過上限、身分服務重啟也沒有換。
  - **不做的事**：不加隨機延遲、不錯開多個安裝（各自的 Secret，彼此不相干）；不另做 metric 或告警（chart 還沒有 Prometheus，ERROR 日誌與 Job 失敗是現有的訊號）；init container 不依年齡輪替（只有一個觸發，推理較單純；它照舊做第一把金鑰、手動輪替、撤銷與移除）；不涵蓋外部 IdP 與使用者登入的金鑰。
- **私鑰外洩（撤銷）**：把 `internalIdentity.revokeBeforeGeneration` 設成一個新的代，`keyGeneration` 調到不小於它（chart 檢查），升級。init container 從 Secret 與 ConfigMap 移除代比它小的每一把；因為沒有還能用的金鑰了，新金鑰立即開始簽章。
  - 撤銷不是瞬間生效：每個 pod 在 kubelet 更新它掛的檔案（加上 5 秒的重讀）之後才拒絕外洩的金鑰，正常情況下 2 分鐘內，但沒有保證的上限（上方「輪替」）。這段時間外洩的金鑰簽的 token 仍可能被還沒更新的 pod 接受，已更新與還沒更新的 pod 認的金鑰不同，一部分請求會被拒絕（401）。現行做法（每個服務滾動重啟）同樣要等每個 pod 換掉，期間舊 pod 一樣接受外洩的金鑰。
  - 身分服務那一側：chart 把 `revokeBeforeGeneration` 也交給身分服務（pod 的環境變數，升級時新的 pod 一啟動就有），它不用代比這個值小的金鑰簽章，所以新的副本即使掛的檔案還是撤銷之前的內容，也不會用外洩的金鑰簽；還沒有新金鑰時不就緒。升級前的舊副本在滾動中被換掉，`kubectl rollout status deployment/<release>-identity` 完成就表示每個副本都換過。
  - 服務那一側由人逐一確認：每個服務在換上新的一組公鑰時記一行 INFO，寫出載入的 kid。撤銷的步驟（runbook）在身分服務滾動完成之後，檢查 namespace 裡每個驗證內部身分的 pod 最後載入的 kid 不再有被撤銷的那幾把，還有的就刪掉那個 pod（或對它的 Deployment `kubectl rollout restart`），再對取代它的 pod 做同樣的檢查。撤銷一年頂多幾次，而且本來就是人發起的緊急處理，所以這一步由人確認，不另做自動的回報機制。
  - 這個值之後留著不改，也不會再撤銷什麼（之後的代都不比它小）。
- **只往上走**：代以數字比較，Secret 裡已經有不小於要求的代的金鑰時不產生新金鑰，所以 `helm rollback` 渲染出較小的值不會再輪替一次，之後升級回來也不會；`revokeBeforeGeneration` 變小不會恢復已撤銷的金鑰；`keyPropagationSeconds` 只影響之後產生的金鑰。
- **只信任 init container 填過的物件**：與上兩節相同，第一次寫入時在同一個 patch 記下 Secret 的 `metadata.uid`，拿掉白名單以外的 metadata；uid 不符（被刪掉重建、從備份還原、事先建好帶著自選金鑰的同名 Secret）時，裡面的金鑰一律不採用，當成資料遺失處理。ConfigMap 不必綁定：每次執行都由 Secret 重新推出，有人手改的內容在下一次執行被蓋掉。
- **失敗情況**：
  - **init container 執行到一半停掉**：每一步都是一次帶 `resourceVersion` 的 patch，Secret 與 ConfigMap 都不會寫到一半。第 1 步之後停掉，新金鑰沒有 `signs_from`，誰都不會用它，下一次執行接著做第 2、3 步；第 2 步之後停掉，公鑰已經公開但沒有人用它簽章（無害），下一次執行補上時間（`published_at` 是那次執行的時間，只會讓它更晚開始簽章）。身分服務讀到不完整或不合法的文件時不採用（kubelet 原子替換檔案，所以只會是有人改壞），保留上一組。
  - **兩個 init container 同時執行**（多個副本一起啟動）：兩者讀到同一個 `resourceVersion`，只有一個 patch 成功，另一個收到 409、重讀後看到新金鑰已經在了，不再產生，它產生的那把丟棄、不寫出。ConfigMap 由 Secret 決定，兩邊寫出的內容相同。滾動中或 rollback 後的舊版 pod 要求的代較小，不會產生金鑰。
  - **Secret 被刪**：已經在跑的身分服務保留記憶體裡最後一組可用的金鑰繼續簽章（檔案不見時不換）。之後新 pod 的 init container 讀不到 Secret，失敗並寫出處理方式，新 pod 停在 `Init`，舊 pod 繼續服務。處理：調大 `keyGeneration` 再升級，Helm 重建空的 Secret，init container 把它當成資料遺失，產生新金鑰並立即開始簽章，ConfigMap 只剩新公鑰。私鑰沒了就不能再簽舊 kid 的 token，所以這個情況沒有重疊期：kubelet 更新各 pod 的檔案之前，一部分請求被拒絕（401）。
  - **ConfigMap 被手改或刪掉**：服務遇到刪掉、改壞的內容時保留上一組；改成另一組合法的公鑰時換上它（例如移除了正在用的一把，服務就拒絕身分服務簽的 token）。身分服務比對自己的 Secret 與看到的 `jwks.json`，不一致持續超過 `keyPropagationSeconds`（chart 也把它交給身分服務）時記 ERROR；正在用的金鑰從它看到的 `jwks.json` 消失時，它不再用那把簽章，沒有別把就不就緒。處理：`kubectl rollout restart deployment/<release>-identity`，init container 依 Secret 重寫 ConfigMap（被刪的話先升級，讓 Helm 重建空的 ConfigMap）。
  - **chart 的 rollback**：見上方「只往上走」。rollback 到 Laysi/trustiot-v2#404 之前的 chart，只在下面搬移的第 1 步之後、第 2 步之前可以（那時新舊版的 pod 都用同一把匯入的金鑰）；第 2 步之後新舊版的 pod 用不同的金鑰，滾動期間有請求被拒絕，而且舊版的 chart 又需要人建立的 Secret。
  - **身分服務沒有啟用**：沒有人產生金鑰，所以 chart 只在身分服務啟用時宣告 Secret、ConfigMap、Role 與 init container，也只在那時給服務掛 ConfigMap 與平台設定 `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`。身分服務沒有啟用時，驗證內部身分的服務因為缺這個設定拒絕啟動（與 Laysi/trustiot-v2#404 之前沒有 `publicKeys` 時相同），不會一直等不到金鑰；不驗證內部身分的元件（例如日誌管線 e2e 的 example service）照常執行。
- **從人建立的私鑰搬過來**（Laysi/trustiot-v2#404 之前的安裝：`internalIdentity.signingKeySecret.name` 指向人建立的 Secret，`publicKeys` 是手寫的；SaaS 的這個 Secret 以 SOPS 放在 `deploy/flux/`）。一次升級直接換成新金鑰的話，滾動期間舊版的服務只認舊公鑰、新版的服務只認新公鑰，請求會被拒絕，所以分三步，每一步都不中斷：
  1. 升級到新版 chart，`signingKeySecret` 與 `publicKeys` 先不動。init container 讀入人建立的私鑰（那個 Secret 只掛進 init container），在 `publicKeys` 找出它的 kid（找不到或有兩個時失敗，與現行身分服務的啟動檢查相同），以原本的 kid、代 0 匯入空的 Secret，立即開始簽章。滾動期間新舊版的 pod 都用這一把；新版的服務改從 ConfigMap 讀公鑰，裡面就是這一把。設了 `signingKeySecret` 的期間，init container 只在 Secret 還沒有金鑰時匯入，不產生新金鑰、不移除金鑰（撤銷除外），所以滾動完成之前不會換金鑰。`publicKeys` 裡其他的公鑰不帶過來，所以舊做法的輪替要在搬移之前做完。
  2. 滾動完成後，下一次升級從 values 移除 `signingKeySecret` 與 `publicKeys`（SaaS 改 `values-instance.yaml`）。`keyGeneration`（至少 1）大於匯入的代 0，init container 產生新金鑰，照上面的輪替換掉匯入的那一把。
  3. 匯入的金鑰移除之後（ConfigMap 不再有它的 kid），刪掉人建立的 Secret：地端 `kubectl delete secret`；SaaS 刪掉 `deploy/flux/` 裡的 SOPS 檔與 kustomization 的引用。
  - SaaS 目前沒有這個 Secret（`deploy/flux` 的 values 沒有設定內部身分，服務也還沒啟用），所以 SaaS 不必搬，values 也不必為金鑰加任何欄位。本機開發環境（`scripts/dev.ts`）改為在狀態目錄寫出同樣格式的兩個檔案，既有的開發私鑰沿用。
- **驗證**（2026-10-15，Laysi/trustiot-v2#404，[測試報告](../test-reports/2026-10-15-404-identity-keys-in-cluster.md)）：
  - chart 測試（`deploy/chart/tests/chart.test.ts`）：以每個可選元件都開啟的設定渲染，只有身分服務的 ServiceAccount 能 get、patch 這個 Secret 與 ConfigMap，只有身分服務的 pod 掛 Secret，每個服務都掛 ConfigMap；身分服務沒有啟用時三者都不宣告；身分服務的 `automountServiceAccountToken` 是 `true` 或 `keyGeneration` 小於 `revokeBeforeGeneration` 時渲染失敗；輪替只改身分服務的 pod template。
  - `tools/migrate/tests/signing_keys.rs`：init container 對假的 API 與假的時鐘，測排程、中途停掉、同時執行、uid 不符、匯入、撤銷與上限。
  - `crates/service-kit`：`a_rotation_reaches_a_running_service_through_its_keys_file` 等測試證明服務不重啟就換上新的一組、壞掉的檔案保留上一組。
  - 身分服務：`services/identity/tests/ext_authz.rs` 的 `a_key_rotation_needs_no_restart_and_tokens_verify_throughout` 以 init container 的同一份程式碼改檔案，對執行中、不重啟的身分服務走完整個輪替（重疊期間兩把金鑰的 token 在每個服務都驗證得過，`signs_from` 起改用新金鑰，舊金鑰移除後它的 token 不再被接受）與 `a_revoked_generation_never_signs`；`services/identity/tests/adversarial.rs` 的 `replicas_switch_keys_by_the_schedule_without_a_restart`（兩個副本照排程一起換）與 `the_service_signs_nothing_without_a_usable_published_signing_key`。
  - lab 的 k3s（`deploy/chart/tests/e2e/identity-keys.e2e.ts`，兩種 shape）：全新安裝、權限（namespace 每個 ServiceAccount 以 `kubectl auth can-i` 問）、輪替（重疊期間舊 token 與之後的新 token 都被資產服務接受）、rollback、舊金鑰移除、撤銷、私鑰不出現在日誌與其他物件、uninstall 保留 Secret 與 ConfigMap，並量測上面的時間。
  - nightly smoke：不事先建立任何金鑰，檢查 Secret 與 ConfigMap 的形狀與權限，記下全新安裝的就緒時間（`docs/architecture/ci-release.md` 的 nightly 第 8 步）。偽造身分標頭的檢查不自己簽 token，改從身分服務的 ext_authz 拿一個真的 token（第 11 步之 5），所以 nightly 不需要讀私鑰，「只有身分服務的 ServiceAccount 讀得到私鑰」沒有為測試開例外。
- **不做的事**：
  - 不由身分服務提供 JWKS 端點、服務不在執行時向身分服務抓公鑰：決策 27 的理由不變（不多一個執行期依賴與信任來源，身分服務停機或被冒充都換不了公鑰，地端離線一樣運作）。
  - 輪替時不自動逐一確認每個 pod 都載入了新公鑰：那需要每個 pod 回報，例如服務的主容器要有 API 權限寫自己的狀態，或身分服務要能列出並逐一呼叫每個 pod（服務的健康埠只對節點開放）。以實測的時間加餘裕取代；超過時只影響可用性（上方「輪替」），撤銷這個唯一影響安全的情況由人逐一確認。
  - 舊金鑰不在時間一到就移除，而是等下一次身分服務啟動或輪替 CronJob 的下一次執行（預設一天內；`maxKeyAgeSeconds` 是 0 時沒有 CronJob）：準時移除需要一個常駐、持有 API 權限的元件，而留得久的舊公鑰不影響安全。Laysi/trustiot-v2#404 時連自動定期輪替都不做，Laysi/trustiot-v2#429 加上（上方「依金鑰年齡自動輪替」）。
  - 不提供外部覆寫（客戶自己的 KMS、HSM 或自選的金鑰）：理由與服務密碼相同，客戶有需求時另開 issue。
  - 防不了 namespace 的管理者：能 patch 這個 Secret 或 ConfigMap 的主體本來就能偽造內部身分，與現行相同。
  - 外部 IdP 與使用者登入的金鑰：不在 Laysi/trustiot-v2#404 的範圍。

### 閘道器憑證 CA

閘道器 mTLS 的自建 CA（決策 6、12）分兩層（2026-10-21 決定，Laysi/trustiot-v2#78；做法、比較過的選項與儀式的每一步在 [services/certs/README.md](../../services/certs/README.md)）：

- **根 CA**（ECDSA P-384）：離線產生與保管，以 age 加密給兩位維運人員、存兩份離線媒體（下方清冊的「根 CA 私鑰」）。只在簽發 CA 的儀式與備份檢查時於離線的機器上取出，由 `certs-ca` 工具從 stdin 讀、只寫到 stdout，不經指令參數也不寫檔。根憑證是公開資料，放在 values 的 `certificateAuthority.rootCertificate`。
- **簽發 CA**（ECDSA P-256，每一代一把）：在叢集內產生，沒有人建立、看到、複製或輸入它，取代原本「SaaS 以 SOPS、地端由安裝產生或客戶匯入」的寫法。
  - chart 在 certs 啟用時宣告空的 Secret `<release>-certs-ca-keys`（`templates/passwords.yaml`，與上兩節的 Secret 相同的 metadata）與空的 ConfigMap `<release>-certs-ca-requests`（`templates/certs-ca.yaml`）。certs pod 的 init container `ca-keys`（migration 映像的 `migrate certs-ca-keys`，在 `wait-for-migrations` 之前）以 certs ServiceAccount 的 projected token 產生金鑰寫進 Secret，再把每把金鑰的 CSR 寫進 ConfigMap；Role `<release>-certs-ca-keys` 只有這兩個物件的 `get`、`patch`（`resourceNames`）。
  - Secret 的資料只在記著自己 uid 時採用（規則同「migration Job 產生的密碼」）：被刪除、清空或從備份還原的 Secret 當作遺失，產生新的一組，不沿用可能有別人拿過的副本。
  - certs 的主容器沒有 token（`services.certs.automountServiceAccountToken` 是 `true` 時渲染失敗），只以唯讀 volume 讀 `/etc/trustiot/certs-ca-keys/issuing-ca-keys.json`（0440，root 與 pod 自己的 `fsGroup` 65533）。其他 pod 都不掛它，任何服務的自訂 `env` 都不能讀它（chart 測試）。
  - 一把金鑰要等維運人員以根 CA 簽了它的 CSR、憑證放進 `certificateAuthority.issuingCertificates` 才簽章。能改 CSR 的 ConfigMap 的人可以把它換成自己的金鑰，所以簽發時工具印出金鑰 id，維運人員和 certs 服務日誌裡等待中的金鑰 id 比對一致才採用。
  - 輪替是調大 `certificateAuthority.keyGeneration` 再走一次儀式；舊的一代在它簽的閘道器憑證都過期後以 `removeKeysBeforeGeneration` 移除。外洩時兩個值同時調到新的一代：舊金鑰立即移除，它的 CRL 不再發布，broker 拒絕它簽的每張閘道器憑證，閘道器經換發端點（決策 12）取得新憑證。簽發 CA 私鑰不備份，遺失時以新的一代取代。
- **不做的事**：根 CA 的 CRL（簽發 CA 外洩時，閘道器驗 broker 伺服器憑證看不到撤銷，Laysi/trustiot-v2#84 決定 broker 憑證與閘道器的信任設定時處理）、根 CA 換新、KMS 或 HSM、地端的 CA 階層（地端延後，決策 1 的 2026-10-18 補充）。

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
- 每次推送分支之前（第一次與之後每一次），以 gitleaks 掃兩處，兩處都必須沒有發現（gitleaks 的版本鎖在 `.prototools`）：
  - 分支的 commit 範圍：`gitleaks git --redact --log-opts="origin/main..HEAD" .`。
  - 推送後會留下的 tree：`git archive HEAD | tar -x -C <目錄>`，再 `gitleaks dir --redact <目錄>`（`<目錄>` 放在 repo 之外，用完刪除）。
  - 理由：CI 會掃 PR 的每個 commit，事後的修正 commit 清不掉前面 commit 的發現，所以第一次推送前就要掃乾淨；main 上的 squash commit 是新的 SHA，tree 裡還留著的值會在它上面再報一次，`.gitleaksignore` 指不到這個還不存在的 SHA。

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
| Kubernetes Secret，以唯讀 volume 掛成檔案 | 私鑰、憑證、多行的金鑰 | 例如資料庫的 CA（`/etc/trustiot/<postgres|clickhouse>-ca/ca.crt`，Postgres 的只掛進使用它的服務）；內部身分簽章私鑰（`/etc/trustiot/identity-signing-keys/signing-keys.json`，只掛進身分服務的主容器）；簽發 CA 私鑰（`/etc/trustiot/certs-ca-keys/issuing-ca-keys.json`，只掛進 certs 的主容器）；之後的 broker 伺服器金鑰也掛成檔案，只掛進使用它的那個 pod |
| 叢集內元件自己產生的 Secret | 叢集內資料庫的帳號、CA 與伺服器私鑰；migration Job 建立的帳號的密碼 | 例如 CloudNativePG 為叢集內 Postgres 產生的 CA、伺服器憑證與 owner 帳號（Laysi/trustiot-v2#58），以及 migration Job 產生的服務角色與 collector 密碼（chart 宣告空的 Secret，Job 填入，見「migration Job 產生的密碼」）、叢集內 ClickHouse 的 init container 產生的管理帳號密碼（見「叢集內 ClickHouse 管理帳號的密碼」）、身分服務的 init container 產生的內部身分簽章私鑰（見「內部身分簽章金鑰」）、certs 服務的 init container 產生的簽發 CA 私鑰（見「閘道器憑證 CA」）。值只存在叢集內，不進 git，也不經過人；chart 把這些 Secret 整個列為服務的自訂 `env` 不能讀的對象，服務只拿到 CA 憑證（掛載時只選 `ca.crt`）或自己的密碼 |
| 主機上只有 root 能讀的檔案，以 systemd `LoadCredential` 交給服務 | SaaS 主機上的 cloudflared | 不在叢集內，k3s 故障時仍能運作（Laysi/trustiot-v2#64） |
| GitHub Actions secret，經 step 的 `env` 或 `with` | 發佈流程用的少數金鑰 | ci.yml 不用任何 secret；fork 來的 PR 拿不到 secret（見 [CI 與發佈](ci-release.md)） |
| 開發者本機的狀態目錄（0700 目錄、0600 檔案） | 本機開發環境 | 帳密與憑證每次 `up` 產生，`down` 刪除；身分服務的開發簽章私鑰與開發 session 的 cookie 沿用，`down` 保留、`purge` 刪除（[scripts/README.md](../../scripts/README.md)） |

SaaS 叢集的 Secret 來源是 `deploy/flux/` 內以 SOPS 加密的檔案，由 Flux 解密後套用；地端由客戶以自己的方式建立 Secret（`kubectl create secret --from-file`、自己的祕密管理工具或 External Secrets Operator），chart 只認 Secret 名稱與 key。

## 密鑰清冊

「SaaS」欄是我們自營 SaaS 的保管位置，「地端」欄是客戶安裝時的位置。「SOPS」指 `deploy/flux/` 內以 SOPS 加密、由 Flux 解密的 Secret（ci-release.md）。展示環境（`deploy/flux/clusters/demo`，Laysi/trustiot-v2#457）用自己的一套：叢集的 age 私鑰與 deploy key 在展示機上產生、不離開展示機（age 私鑰不做離線備份），GHCR 拉取 token（只有展示叢集有，SaaS 叢集等 Laysi/trustiot-v2#502）、SMTP 帳密與 Anthropic 金鑰（展示專用工作區，設用量上限）各自一份，與 SaaS 分開輪替，見 [deploy/flux/README.md](../../deploy/flux/README.md)「展示叢集」；展示機自己的 Cloudflare Tunnel token 也只屬於展示環境，見下方「主機、映像與發佈」表中的「展示環境的 Cloudflare Tunnel token」一列。輪替步驟見 [runbook](../runbooks/secrets.md) 的對應小節。還沒實作的元件，保管與注入方式是本清冊定下的規則，實作 issue 照做並補上 runbook 的指令。

### 平台信任根與簽章金鑰

| 密鑰 | 用途與使用者 | SaaS | 地端 | 注入方式 | 實作 issue |
| --- | --- | --- | --- | --- | --- |
| 根 CA 私鑰 | 簽發 CA 憑證的信任根（閘道器、工作站、broker 憑證的源頭，決策 6、12） | 離線保管：不在叢集、不在 git（加密也不放）、不在任何連網主機；以 age 加密存兩份離線媒體，分別由兩位維運人員保管；只在簽發或撤銷簽發 CA 時取出 | 地端的 CA 階層延後（上方「閘道器憑證 CA」） | 不注入任何服務；儀式與備份檢查用 `certs-ca`，私鑰只經 stdin、stdout（services/certs/README.md） | Laysi/trustiot-v2#78 |
| 簽發 CA 私鑰（ECDSA P-256），Secret `<release>-certs-ca-keys` | certs 服務簽發閘道器與工作站憑證、發 CRL | certs pod 的 init container 在叢集內產生，不進 git、不經過人；CSR 由離線根 CA 簽發（上方「閘道器憑證 CA」，2026-10-21 取代原本的 SOPS） | 同左 | 檔案：只以唯讀 volume 掛進 `certs` 的主容器（`/etc/trustiot/certs-ca-keys/issuing-ca-keys.json`，0440，root 與 pod 的 fsGroup）；init container 經 Kubernetes API 讀寫 | Laysi/trustiot-v2#78 |
| broker 伺服器憑證私鑰 | Mosquitto 的 TLS 伺服器金鑰（閘道器 mTLS 的內層） | 由簽發 CA 簽發，存在叢集 Secret | 同左 | 檔案，只掛進 broker | Laysi/trustiot-v2#84 |
| 內部身分簽章私鑰（Ed25519），Secret `<release>-identity-signing-keys` | 身分服務簽內部身分 token（決策 27） | 身分服務 pod 的 init container 在叢集內產生，不進 git、不經過人（上方「內部身分簽章金鑰」） | 同左 | 檔案：只以唯讀 volume 掛進 `identity` 的主容器（`/etc/trustiot/identity-signing-keys/signing-keys.json`，0440，root 與 pod 的 fsGroup）；init container 經 Kubernetes API 讀寫（Role 只有這個 Secret 與公鑰 ConfigMap 的 `get`、`patch`）。對應的公鑰不是祕密：ConfigMap `<release>-internal-identity-keys` 的 `jwks.json` 以唯讀 volume 掛進每個服務（`TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`）。Laysi/trustiot-v2#404 之前是人建立的 Secret（SaaS 以 SOPS 保存），公鑰手寫在 `internalIdentity.publicKeys`，搬移見上方 | Laysi/trustiot-v2#61、Laysi/trustiot-v2#404（[services/identity/README.md](../../services/identity/README.md)；格式與驗證：Laysi/trustiot-v2#52、[crates/service-kit/README.md](../../crates/service-kit/README.md)） |
| OTA 簽章私鑰 | 簽閘道器 agent 與 updater 的發佈檔（決策 11） | GitHub environment secret，只有 main 上的 tag 能用（ci-release.md）；公鑰內建在 updater | 地端使用同一份發佈檔，不另持有 | 只給 release workflow 的簽章步驟 | Laysi/trustiot-v2#164（注意：release.yml 的 `release` environment 已建立，但私有庫在目前的 GitHub Free 方案設不了必要審查者與 tag 規則（決策 30），environment secret 限制不了「只有 main 上的 tag 能用」，Laysi/trustiot-v2#164 要先解決保管方式。2026-10-12 repo 已公開，Free 方案設得了 environment 的 tag 規則與必要審查者，但尚未設定，限制仍未成立，選項見 [CI 與發佈](ci-release.md) 的「發佈核准」，Laysi/trustiot-v2#345） |
| SOPS 叢集私鑰（age） | Flux 解密 `deploy/flux/` 的 Secret | 叢集 `flux-system` namespace 的 Secret `sops-age`，由維運人員在建立叢集時直接建立（不經 git）；離線備份與根 CA 私鑰放在一起 | 不適用（地端不裝 Flux） | 只給 Flux kustomize-controller | Laysi/trustiot-v2#70 |
| 維運人員的 age 私鑰 | 編輯、輪替 SOPS 檔案；叢集私鑰遺失時的備援 | 各自保管在自己的裝置（建議放在密碼管理器或硬體金鑰），不放共用主機 | 不適用 | 不注入任何服務 | Laysi/trustiot-v2#70 |

### 資料庫與叢集內元件

| 密鑰 | 用途與使用者 | SaaS | 地端 | 注入方式 | 實作 issue |
| --- | --- | --- | --- | --- | --- |
| Postgres superuser（`postgres`，`postgres.mode: bundled`） | 只有 CloudNativePG 在 pod 內經 local socket 使用（建立 AGE extension、角色） | 沒有可用的密碼：Cluster 設 `enableSuperuserAccess: false`，`postgres` 沒有密碼、沒有 `<cluster>-superuser` Secret（2026-10-12 實驗機 e2e 驗證） | 同左；`external`：由客戶保管，不交給 chart | 不注入任何服務 | Laysi/trustiot-v2#58 |
| 叢集內 Postgres 的 CA 與伺服器私鑰（Secret `<cluster>-ca`、`<cluster>-server`） | CloudNativePG 簽發與換發伺服器憑證（90 天，到期前 7 天自動換發） | operator 在叢集內產生，不進 git | 同左；`external` 時客戶只提供 CA 憑證（`postgres.external.caSecret`） | 只有 `<cluster>-ca` 的 `ca.crt` 以唯讀 volume 掛進在 `mesh.calls` 列出 `postgres` 的服務；`ca.key` 與伺服器私鑰不進任何服務（chart 測試） | Laysi/trustiot-v2#58 |
| Postgres 的 migration 角色：`bundled` 是資料庫 owner `trustiot_owner`（有 CREATEROLE，沒有 SUPERUSER、BYPASSRLS），`external` 是客戶提供的同等角色 | migration Job 套用 migration、建立服務的角色 | `bundled`：密碼由 CloudNativePG 產生在 `<cluster>-app`，不進 git | `bundled` 同左；`external`：客戶建立的 Secret（`postgres.external.migrationSecret`） | 環境變數，只給 migration Job；服務的自訂 `env` 不能讀它（chart 測試） | Laysi/trustiot-v2#58（owner）、Laysi/trustiot-v2#60 |
| Postgres 的服務登入角色 `<服務>_app` 與 outbox 消費端的平台角色 `<服務>_platform`（決策 4、29），每個服務一個 Secret `<release>-<服務>-password`，key 是角色名稱 | 各服務連 Postgres（消費端另以平台角色讀 producer 的事件）；migration Job 產生並以同一個值設定角色的密碼（只送 SCRAM-SHA-256 verifier，明文不進 SQL 與日誌） | migration Job 在叢集內產生，不進 git、不經過人（上方「migration Job 產生的密碼」） | 同左 | 服務：環境變數 `TRUSTIOT__DATABASE__PASSWORD`、平台角色的 `TRUSTIOT__DATABASE__PLATFORM_PASSWORD`（`secretKeyRef`，只有自己的 Secret）；migration Job：經 Kubernetes API 讀寫，Role 只有這些 Secret 的 `get`、`patch`；其他服務的自訂 `env` 不能讀它（chart 測試） | Laysi/trustiot-v2#60、Laysi/trustiot-v2#65（平台角色） |
| AGE 每租戶登入角色（決策 21） | field-context 以租戶專屬角色執行 Cypher | 由程式在建立租戶時產生，不進 git、不經過人 | 同左 | 由 Laysi/trustiot-v2#176 決定 | Laysi/trustiot-v2#176 |
| 叢集內 ClickHouse 的 bootstrap 管理帳號密碼（`clickhouse.mode: bundled`），Secret `<release>-clickhouse-admin-password`，key 是帳號名稱（預設 `trustiot_admin`） | ClickHouse 本身；migration Job（Laysi/trustiot-v2#60）以它套用 DDL、建立 collector 帳號 | ClickHouse 的 init container 在叢集內產生，不進 git、不經過人（上方「叢集內 ClickHouse 管理帳號的密碼」） | 同左 | ClickHouse：init container 經 Kubernetes API 讀寫（Role 只有這個 Secret 的 `get`、`patch`），寫成記憶體 emptyDir 的檔案 `/etc/trustiot/clickhouse-admin/password`（`CLICKHOUSE_PASSWORD_FILE`）；migration Job：經 Kubernetes API 讀（Role 只有 `get`）；服務與 collector 的 pod 讀不到它（chart 測試） | Laysi/trustiot-v2#59、Laysi/trustiot-v2#60、Laysi/trustiot-v2#403 |
| ClickHouse 帳號：ingest 寫入、每租戶唯讀（決策 14） | ingest、telemetry-query | 還沒有：ingest 的寫入帳號在 ingest 服務加入時由 migration Job 以管理帳號建立（Laysi/trustiot-v2#91；目前的 Job 只建立角色 `trustiot_ingest`，不建帳號）；每租戶唯讀帳號由 telemetry-query 建立租戶時產生（Laysi/trustiot-v2#119） | 同左 | 環境變數，只給使用該帳號的服務 | Laysi/trustiot-v2#91、Laysi/trustiot-v2#119 |
| OpenTelemetry collector 的 ClickHouse 帳號密碼，Secret `<release>-otel-collector-password`，key 是帳號名稱 | collector 寫 `trustiot_logs` 的兩張日誌表（決策 14、22）；migration Job 產生密碼並以它建立帳號 `trustiot_collector`，只授與角色 `trustiot_otel_collector`（`services/ingest/migrations/clickhouse/0006_logs.sql`） | migration Job 在叢集內產生，不進 git、不經過人（上方「migration Job 產生的密碼」） | 同左 | 環境變數 `CLICKHOUSE_PASSWORD`（`secretKeyRef`），只給 collector；collector 以 init container 等 Job 完成才啟動；服務的自訂 `env` 不能讀它（chart 測試） | Laysi/trustiot-v2#73、Laysi/trustiot-v2#60 |
| ClickHouse 帳號 `trustiot_tenant_view_definer` | view `trustiot.tenant_telemetry` 的 DEFINER（`services/ingest/migrations/clickhouse/0005_tenant_view.sql`） | 不是密鑰：`IDENTIFIED WITH no_password HOST NONE`，不能登入、沒有密碼要保管或輪替；只有 `SELECT ON trustiot.telemetry` | 同左 | 不注入任何服務 | Laysi/trustiot-v2#260 |
| MQTT dynsec 管理帳密 | certs、gateway-ops 註冊與停用閘道器（決策 6） | 部署時在叢集內產生的 Secret | 同左 | 環境變數，只給需要的服務 | Laysi/trustiot-v2#82、Laysi/trustiot-v2#84 |
| Grafana 管理者帳密 | 維運看板（決策 23） | 部署時在叢集內產生；登入改用 SSO 時移除 | 客戶的 Secret | Grafana 自己讀 Secret | Laysi/trustiot-v2#183 |

### 外部服務

| 密鑰 | 用途與使用者 | SaaS | 地端 | 注入方式 | 實作 issue |
| --- | --- | --- | --- | --- | --- |
| AI 供應商憑證：Anthropic 與 OpenAI 的 API key；Vertex AI 優先用 Workload Identity Federation（不放長期金鑰，規則 8），不可行時才用服務帳號金鑰 | AI 閘道呼叫供應商（決策 15、26、28、32） | SOPS | 客戶端點需要時，客戶建立的 Secret（地端排在 SaaS 完成後） | 環境變數，只給 `ai-gateway`（chart 強制）；Vertex AI 的憑證形式在接入的 issue 決定 | Laysi/trustiot-v2#139、Laysi/trustiot-v2#440 |
| Qdrant 唯讀與寫入 API key | knowledge 檢索與背景處理（決策 19） | SOPS（Qdrant Cloud 主控台發的兩把 key） | `bundled`：部署時在叢集內產生 | 環境變數，只給 `knowledge`（chart 強制） | Laysi/trustiot-v2#194 |
| 物件儲存存取金鑰 | PDF、頁面圖、附件（決策 20） | SOPS | 客戶建立的 Secret | 環境變數，只給列出 `object-storage` 的服務 | Laysi/trustiot-v2#133 |
| 備份用的物件儲存金鑰 | Postgres 的 WAL 與完整備份、ClickHouse 備份（M8） | SOPS；與服務用的金鑰分開、只能寫備份 bucket | 客戶建立的 Secret | 只給備份元件 | Laysi/trustiot-v2#134、Laysi/trustiot-v2#135 |
| SMTP 帳密 | 登入驗證碼（Laysi/trustiot-v2#103）、通知與報表寄信（決策 18） | SOPS；2026-10-19 起暫用 Resend：帳號固定是 `resend`，密碼是 Resend 的 API key，權限只能寄信（`sending_access`）並限定寄件網域。Resend 主控台看得到寄出的信件內容（含驗證碼），主控台成員只限維運人員 | 客戶的 relay 需要驗證時，客戶建立的 Secret | 環境變數，只給列出 `smtp` 的服務 | Laysi/trustiot-v2#103、Laysi/trustiot-v2#169、Laysi/trustiot-v2#457 |
| Sentry DSN | 錯誤回報（決策 24） | SOPS | 預設關閉；客戶開啟時由客戶建立 | 環境變數 `SENTRY_DSN`，每個服務 | Laysi/trustiot-v2#184 |
| SIEM 轉送的認證標頭與 CA（`observability.collector.forward.authHeader.valueSecret`、`observability.collector.forward.caSecret`） | collector 把遮蔽後的日誌以 OTLP 轉送到客戶的 SIEM（決策 22） | 預設不轉送 | 客戶開啟轉送時由客戶建立 | 標頭值是環境變數 `FORWARD_AUTH_HEADER`（`secretKeyRef`），CA 以唯讀 volume 掛成 `/etc/trustiot/forward-ca/ca.crt`，都只給 collector | Laysi/trustiot-v2#73 |

### 主機、映像與發佈

| 密鑰 | 用途與使用者 | 保管 | 注入方式 | 實作 issue |
| --- | --- | --- | --- | --- |
| Cloudflare Tunnel token | SaaS 主機上的 cloudflared 連出（決策 27） | 主機上只有 root 能讀的檔案；不在叢集、不在 git | systemd `LoadCredential` | Laysi/trustiot-v2#64（runbook 在 `deploy/host/README.md`） |
| 展示環境的 Cloudflare Tunnel token | 展示機上 `cloudflared-demo.service` 連出（Laysi/trustiot-v2#457） | 展示機上只有 root 能讀的 `/etc/credstore/cloudflared-demo.tunnel-token`；交付時以展示機上產生的 RSA-4096 金鑰做 RSA-OAEP 加密，只有密文離開持有 token 的執行環境；交付用的私鑰與密文只放在 tmpfs（`/run/trustiot-demo/`，重開機即消失，記憶體不足時可能換到 swap 檔），安裝成功後刪除。`/etc` 是 btrfs，刪掉的 token 檔內容可能留在磁碟上，token 失效靠 Cloudflare 端的輪替或刪除 tunnel | systemd `LoadCredential` | Laysi/trustiot-v2#466（runbook 在 [展示環境](../runbooks/demo-environment.md)） |
| 維運 SSH | 經 tunnel 進 SaaS 主機 | 沒有共用密鑰：Cloudflare Access 以個人身分驗證，主機只接受個人的 SSH 公鑰 | 不適用 | Laysi/trustiot-v2#64 |
| GHCR 拉取 token | 展示叢集拉 chart 與映像（決策 5）；SaaS 叢集不用這把，改用 Laysi/trustiot-v2#502 的 registry | 一位組織成員的 classic personal access token，只有 `read:packages`、一年到期，不可使用帶 `repo` 或任何寫入 scope 的 token（GHCR 只接受 classic token；GitHub 組織以人頭計費，2026-10-21 決定不另建機器帳號）。`read:packages` 讀得到這位成員看得到的每一個 private 套件，不限於本 repo，token 外洩時這些套件都視為外洩。以 SOPS 加密的 `kubernetes.io/dockerconfigjson` Secret `ghcr-pull`（namespace `trustiot`）進叢集，檔案 `deploy/flux/clusters/demo/trustiot/ghcr-pull.sops.yaml`，以 `bun scripts/flux/ghcr-pull-secret.ts --cluster demo` 從 stdin 產生，步驟與 scope 檢查見 [deploy/flux/README.md](../../deploy/flux/README.md)「GHCR 拉取 token」。GHCR 套件的可見性獨立於 repo：chart 仍是私有，所以需要這把 token（見 [CI 與發佈](ci-release.md) 的「GHCR 套件的可見性」，Laysi/trustiot-v2#345） | `global.imagePullSecrets` 與 Flux 的 OCIRepository `trustiot` 的 `secretRef` | Laysi/trustiot-v2#70、Laysi/trustiot-v2#480、Laysi/trustiot-v2#502 |
| Flux 的 deploy key | SaaS 叢集的 Flux 以 SSH 讀 main（決策 5） | 只給這個叢集的 ed25519 金鑰；公鑰在 GitHub repo 設定為 read-only deploy key，私鑰只在叢集 `flux-system` namespace 的 Secret `flux-system`（建立叢集時直接建立，不經 git，[deploy/flux/README.md](../../deploy/flux/README.md)） | 只給 Flux source-controller | Laysi/trustiot-v2#70 |
| 地端的映像取得 | 客戶安裝時拉映像 | 每個客戶一個 Laysi/trustiot-v2#502 registry 的唯讀帳號，或離線安裝包（MS6）；不共用 SaaS 的帳號。不在 GHCR 給客戶 token：每位客戶都要佔一個組織席位 | 客戶建立的 `imagePullSecrets` | Laysi/trustiot-v2#46、Laysi/trustiot-v2#502 |
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
| 本機開發帳密 | 開發者的 `$XDG_STATE_HOME/trustiot-v2/dev/<實例>/state.json`（0600）；含 migration Job 產生的 `identity_app` 密碼（經本機的 Secret API 替身，只在 `up` 執行 Job 時存在於記憶體） | 每次 `up` 產生、`down` 刪除，不在 repo 內 | Laysi/trustiot-v2#71、Laysi/trustiot-v2#336 |
| 本機開發用的身分服務簽章私鑰（Ed25519）與開發 session 的 cookie | 開發者的 `$XDG_STATE_HOME/trustiot-v2/dev/<實例>/identity/`（目錄 0700、檔案 0600）；公鑰由私鑰推得，經 `env` 印出，不另存 | `up` 沒有就產生、有就沿用；`down` 保留（session 在保留的資料磁碟區），`purge` 刪除；`env`、`status` 與日誌不含私鑰內容；只在本機，不是任何叢集的金鑰 | Laysi/trustiot-v2#336 |
