# 叢集內的 Postgres：CloudNativePG 與 AGE（2026-10-12）

對應 issue：Laysi/trustiot-v2#58（決策 4、5、21、29，清單 M3、M8）。原始資料在 [data/2026-10-12-58-cnpg-postgres/](data/2026-10-12-58-cnpg-postgres/)。

## 環境

- **受測程式碼**：分支 `feat/58-cnpg-postgres`，`bcee1fc`（Laysi/trustiot-v2#57 的分支 `e89bcf9` 合併 main `dddd9ed`）加上本 PR 的變更，測試時尚未提交。測試之後 chart 的 values 沒有再改，templates 只改了超過 50 個字元的 Cluster 名稱（加上雜湊避免撞名，測試用的 `trustiot-postgres` 不受影響）；`deploy/chart/tests/e2e/postgres.e2e.ts` 只在安裝前多了一項檢查（namespace 裡已有同名的 Cluster、ImageCatalog 或它的 PVC 時直接失敗，避免清理時刪掉別人的資料庫），這項檢查沒有在叢集上重跑；其他改的是文件、nightly 腳本（nightly 自建的 Postgres 映像改名為 `localhost/trustiot-e2e/postgres:nightly`，因為 `containerBuildArgs` 只接受這個前綴）與 chart 單元測試對 smoke 映像名稱的預期值。
- **實驗機**：lab 的 Mac mini M1（Fedora Linux Asahi Remix 44，aarch64，8 核、7.5 GB 記憶體），k3s v1.36.5+k3s1，Istio 1.30.5 ambient 與 Gateway API CRD，k3s 內建的 local-path 儲存。實驗機上同時跑著其他 namespace 的 PoC，本次沒有碰它們。
- **CloudNativePG**：1.30.1，依 `deploy/cloudnative-pg/README.md` 安裝：下載 `cnpg-1.30.1.yaml`、SHA-256 相符後以 `kubectl apply --server-side` 套用（[cnpg-apply.txt](data/2026-10-12-58-cnpg-postgres/cnpg-apply.txt)），`cnpg-system` 設 Pod Security `restricted`，operator 的 pod 建立時沒有任何警告。operator 映像是 manifest 裡的 `ghcr.io/cloudnative-pg/cloudnative-pg:1.30.1`（multi-arch，拉到 arm64）。
- **映像**（在 x86_64 Linux 建置機上建出 arm64，`docker save` 後傳到實驗機，以 `k3s ctr -n k8s.io images import` 匯入）：
  - Postgres：`deploy/images/postgres` 以 `docker buildx build --platform linux/arm64` 建出（arm64 階段沒有 `RUN`，不需要模擬器），`localhost/trustiot-e2e/postgres:58`，image ID `sha256:51c58c2a713345206eda01439c3db69da39126a16fadfe991afcd878b4571515`，272 MB。
  - example-service：`cargo zigbuild --locked --release --target aarch64-unknown-linux-musl -p service-kit --bin example-service`（zig 與 cargo-zigbuild 由 `.prototools` 鎖定；`rust-lld` 的做法在建置機上失敗，因為 `ring` 要 aarch64 musl 的 C 編譯器），以 `deploy/chart/tests/e2e/example-service/Containerfile` 建成 `localhost/trustiot-e2e/example-service:58`，image ID `sha256:153fc20d2dfa076ac2a1854b99bb396050eb532a40923f79a7a35426d2a114d7`。
- **namespace**：`e2e-58`（Pod Security `restricted`）。執行身分是該 namespace 的 ServiceAccount `e2e-58-runner`：namespace 內綁定內建的 `admin`，另加一個 Role 給 `postgresql.cnpg.io` 的 clusters、imagecatalogs、databases、databaseroles，`gateway.networking.k8s.io` 的 gateways、httproutes 與 `security.istio.io` 的 authorizationpolicies（`admin` 都不含）。token 的 kubeconfig 只放在本機暫存目錄，API server 經 SSH 轉送，測完刪除。
- **本機工具**：bun 1.4.2、helm 4.3.0、kubectl 1.36.5，由 `.prototools` 鎖定。
- **時間**：台北時間 2026-10-12 11:44–12:00（UTC 03:44–04:00）。lab 鎖 UTC 03:45:59 取得，清理完成後釋放。

## 指令

CloudNativePG 的安裝照 `deploy/cloudnative-pg/README.md` 的「安裝」逐行執行（以 k3s 的 kubectl）。

叢集 e2e，兩種型態各一次（在 repo 根目錄，`<暫存>` 是本機暫存目錄）：

```sh
for shape in saas onprem; do
  KUBECONFIG=<暫存>/kubeconfig \
  E2E_NAMESPACE=e2e-58 \
  E2E_SHAPE=$shape \
  E2E_POSTGRES_IMAGE=localhost/trustiot-e2e/postgres:58 \
  E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:58 \
  E2E_IMAGE_PULL_POLICY=Never \
  E2E_REPORT_DIR=<暫存> \
  bun test --timeout 900000 --reporter=junit --reporter-outfile=<暫存>/postgres-e2e-$shape.junit.xml \
    ./deploy/chart/tests/e2e/postgres.e2e.ts
done
```

owner 密碼輪替：以 `values-onprem.yaml`、`tests/values/onprem-instance.yaml`、`tests/e2e/values-postgres-e2e.yaml` 與上面的 Postgres 映像 `helm install --wait`，接著依 `docs/runbooks/secrets.md`「叢集內 Postgres 的 owner 密碼與 TLS 憑證」的步驟刪除 `trustiot-postgres-app`，記錄 Secret 重建、資料庫裡角色的密碼驗證值（`pg_authid.rolpassword`）改變的時間，並以新舊密碼各從 pod 內以 TLS 登入一次。密碼只經 stdin 傳入，輸出只記 SHA-256 的前 12 個字元。最後 `helm uninstall` 並刪除保留下來的 Cluster、ImageCatalog 與 PVC。

記憶體：測試期間在實驗機上每 15 秒記錄一次 `free -m` 與 `kubectl top pod -A` 中本次的 pod（`e2e-58` 與 `cnpg-system`）。

叢集層級的物件：裝 operator 前、測試中、清理後各執行一次 `kubectl get crd,mutatingwebhookconfiguration,validatingwebhookconfiguration,clusterrole,clusterrolebinding,ns,pv -o name | sort`，並記下所有 pod 的 namespace、名稱與狀態。清理是刪除 namespace `e2e-58`，再 `kubectl delete -f cnpg-1.30.1.yaml`。

## 結果

### 叢集 e2e

兩種型態都是 7 個測試全部通過、50 個斷言：saas 151.8 秒，onprem 129.4 秒（[postgres-e2e-saas.junit.xml](data/2026-10-12-58-cnpg-postgres/postgres-e2e-saas.junit.xml)、[postgres-e2e-onprem.junit.xml](data/2026-10-12-58-cnpg-postgres/postgres-e2e-onprem.junit.xml)）。查到的設定、角色與拒絕訊息在 [postgres-e2e-saas.json](data/2026-10-12-58-cnpg-postgres/postgres-e2e-saas.json) 與 [postgres-e2e-onprem.json](data/2026-10-12-58-cnpg-postgres/postgres-e2e-onprem.json)，兩份除了時間與 pod IP 之外相同。

在這之前的第一次執行（只跑 onprem）6 個通過、1 個失敗：測試預期 CA volume 的 `secret` 只有 `secretName` 與 `items`，但 API server 會補上預設的 `defaultMode: 420`。這是測試的寫法問題，改成只比對這兩個欄位後重跑，就是上面的結果。

| 測試 | 結果 | 摘要 |
| --- | --- | --- |
| Cluster 健康，一個 instance，在 mesh 外，符合 restricted | 通過 | phase `Cluster in healthy state`；pod 帶 `istio.io/dataplane-mode: none`；pod 以 UID／GID／fsGroup 26、`runAsNonRoot`、`seccompProfile: RuntimeDefault` 執行，`bootstrap-controller` 與 `postgres` 兩個容器都 `allowPrivilegeEscalation: false`、drop `ALL`、`readOnlyRootFilesystem: true`，在 `restricted` 的 namespace 建立成功 |
| 伺服器設定與角色 | 通過 | `ssl` 開啟，`shared_preload_libraries` 含 `age`，PostgreSQL 18；`trustiot` 的 AGE 版本 1.8.0（等於 Containerfile 的 `AGE_VERSION`），locale provider `b`（builtin）、`C.UTF-8`、`UTF8`，owner `trustiot_owner`；`postgres` 沒有密碼，`trustiot-postgres-superuser` Secret 不存在；`trustiot_owner` 與服務的角色都沒有 SUPERUSER、CREATEROLE、CREATEDB、BYPASSRLS |
| cypher() | 通過 | 以 superuser 經 local socket 建立圖、建立並查回 `"press-1"`→`"oven-2"`，再刪除圖 |
| 不走 TLS 的連線被拒 | 通過 | `FATAL: pg_hba.conf rejects connection for host "127.0.0.1", user "trustiot_owner", database "trustiot", no encryption` |
| 服務以 verify-full 連線 | 通過 | example-service（`services.assets`，在 `mesh.calls` 列出 `postgres`）以 DatabaseRole 建立的角色、chart 掛載的 `/etc/trustiot/postgres-ca/ca.crt`（volume 只選 `ca.crt`）連 `trustiot-postgres-rw.e2e-58.svc`，readiness 的 Postgres 檢查通過；伺服器端 `pg_stat_ssl` 看得到這個角色的 TLS 連線（1 條） |
| 錯誤 CA、錯誤主機名稱、沒有放行的 label 都被拒 | 通過 | 同一個 pod template 的獨立 pod：CA 換成叢集的根 CA 時 `invalid peer certificate: UnknownIssuer`；主機改成 primary 的 pod IP 時 `certificate not valid for name "10.42.0.112"; certificate is only valid for DnsName("trustiot-postgres-rw"), …`（憑證只列 `-rw`、`-r`、`-ro` 三個 Service 的各種寫法）；換掉 component label 的 pod `Connection reset by peer (os error 104)`（NetworkPolicy 擋下，ambient 的 ztunnel 重置連線，沒有交換任何憑證或密碼）；不改的對照組 Ready |
| `helm uninstall` 保留資料 | 通過 | uninstall 後 `cluster.postgresql.cnpg.io/trustiot-postgres`、`imagecatalog.postgresql.cnpg.io/trustiot-postgres` 與 PVC `trustiot-postgres-1` 都還在；Database 物件隨 release 刪除 |

安裝時間（`helm install --rollback-on-failure --wait --timeout 10m`，只裝資料庫）：

| 執行 | helm 結束 | helm 結束時 Cluster 已健康 | Database 的 AGE 套用完成 |
| --- | --- | --- | --- |
| 第一次 onprem | 26.1 秒 | 是 | 48.5 秒 |
| saas | 20.7 秒 | 是 | 43.1 秒 |
| onprem | 22.4 秒 | 是 | 44.8 秒 |

Helm 4 的 `--wait` 依 Cluster 的 `Ready` 條件等到它健康；Database 物件不在等待之列，AGE extension 約晚 20 秒才建立。用到 AGE 的 migration 要先等它出現。

其他觀察：

- owner 對 AGE 的權限：`trustiot_owner` 對 `ag_catalog` 的 USAGE、`ag_catalog.ag_graph` 的 INSERT、`ag_catalog.ag_label` 的 SELECT 都是 false（`f|f|f`），owner 不能自己建立圖。要給哪些權限、由誰建立圖，留給 Laysi/trustiot-v2#60 與 Laysi/trustiot-v2#176。
- 測試的 DatabaseRole 在建立後短暫顯示 `secrets ... is forbidden`（instance manager 的 ServiceAccount 還不能讀它的 basic-auth Secret），operator 把這個 Secret 加進 Cluster 的 Role 之後就套用成功，沒有人工介入。

### owner 密碼輪替

[owner-password-rotation.txt](data/2026-10-12-58-cnpg-postgres/owner-password-rotation.txt)：刪除 `trustiot-postgres-app` 後 0.1 秒內 operator 以新密碼重建它（新的 UID，密碼不同），再 0.2 秒資料庫裡角色的密碼驗證值改變；新密碼登入成功，舊密碼 `password authentication failed for user "trustiot_owner"`。

這是第二次執行的紀錄。第一次執行時，Cluster 剛變健康就以 `trustiot-postgres-app` 的密碼登入，psql 以結束碼 2 被拒（腳本沒有留下錯誤訊息），之後的輪替同樣成功（0.2 秒，新密碼可登入）。腳本改為先等到能以目前的密碼登入再開始，第二次第一次嘗試就成功。原因沒有查明；migration Job 第一次連線失敗時應該重試，而不是直接失敗。

### 記憶體

[lab-memory.log](data/2026-10-12-58-cnpg-postgres/lab-memory.log)，UTC 03:47:35–03:57:57 共 42 筆：

| 項目 | 峰值 |
| --- | --- |
| 實驗機已用記憶體（`free -m` 的 used，總量 7543 MB） | 4400 MB（開始前 4195 MB；可用記憶體最低 3142 MB） |
| Postgres pod（`trustiot-postgres-1`） | 72 Mi |
| CloudNativePG operator | 38 Mi |
| 入口 gateway | 34 Mi |
| example-service 與對照的 pod | 各 3 Mi 以下 |

已用記憶體包含實驗機上其他 namespace 的 PoC，不是本次元件的總和。Postgres 只有空資料庫與測試的連線，requests／limits（256Mi／1Gi）與 `shared_buffers` 128MB 的實際用量要等服務上線後以代表性負載再量（決策 5 的 MS3 重測）。

### 叢集層級的物件與清理

[cluster-wide.txt](data/2026-10-12-58-cnpg-postgres/cluster-wide.txt)：安裝期間多出 Namespace `cnpg-system` 與 `e2e-58`、11 個 CRD、7 個 ClusterRole、1 個 ClusterRoleBinding、mutating 與 validating webhook 設定各 1 個，和 `deploy/cloudnative-pg/README.md` 列的相同。清理後的清單（196 項）與安裝前逐行相同（SHA-256 `fb9974a8…`），所有 pod 的名稱與狀態也與安裝前相同（27 個，23 個 Running、4 個 Completed）。匯入的兩個映像與 operator 的映像已從節點刪除，實驗機與建置機上的暫存檔、本機的 kubeconfig 都已刪除。

### 本機

- `bun test deploy/chart`：285 個測試全部通過（含新增的「bundled Postgres (CloudNativePG, Laysi/trustiot-v2#58)」）。
- `bun test scripts/nightly`：80 個測試全部通過；`bun run --cwd scripts typecheck` 通過。
- 提交前合併 main 後的完整檢查結果記在 PR。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| k3d 上以 values-saas.yaml 與 values-onprem.yaml 各部署一次，Cluster 都達到健康狀態 | 待測（k3d） | k3d 的部署由 PR 的 nightly smoke（`smoke` label）執行：nightly 已加上 CloudNativePG 的安裝、Postgres 映像的建置與匯入，以及每個 Cluster 的健康檢查（`docs/architecture/ci-release.md`）。在實驗機的 k3s 上兩種型態都已達到 `Cluster in healthy state`（上方的叢集 e2e） |
| 服務以 verify-full 連線成功，以錯誤 CA 連線失敗（測試） | 通過 | 叢集 e2e 兩種型態：verify-full 連線成功，錯誤 CA（`UnknownIssuer`）與錯誤主機名稱失敗；明文連線與沒有放行的 pod 也被拒 |

範圍內其他項目：服務不用 superuser（沒有 superuser 密碼與 Secret，owner 與服務角色都是一般角色，通過）；資源設定兩種型態相同、在 8 GB 單節點內（閒置的 Postgres 72 Mi，峰值負載待 MS3 重測）；operator 不在 chart 內安裝，改為平台元件，理由在決策 5 的 2026-10-12 補充。
