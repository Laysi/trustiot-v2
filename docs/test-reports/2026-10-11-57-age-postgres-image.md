# 含 Apache AGE 的 CloudNativePG Postgres 映像（arm64）

對應 issue：Laysi/trustiot-v2#57。映像與升級步驟見 [deploy/images/postgres](../../deploy/images/postgres/README.md)。原始資料在 [data/2026-10-11-57-age-postgres-image](data/2026-10-11-57-age-postgres-image/)。

## 環境

- 建置：工作站（x86_64，Fedora 45，podman 6.1.3，Bun 1.4.2，syft 1.52.0），沒有註冊 arm64 模擬器（`/proc/sys/fs/binfmt_misc` 只有 `register`、`status`），與 release.yml 的 x86 runner 相同的限制。分支 `feat/57-age-postgres-image`，基於 main `0a5b01c`。
- 執行：實驗機 `trustiot-mac-m1`（Mac mini M1，Fedora Asahi Remix 44，aarch64，16k page kernel，podman 5.8.7，k3s v1.36.5+k3s1），取得 lab lock 後執行，namespace `e2e-57`，完成後全部移除。
- 映像：`deploy/images/postgres/Containerfile`，基底 `ghcr.io/cloudnative-pg/postgresql:18.6-minimal-trixie@sha256:37ade18d…`（PostgreSQL 18.6，`18.6-1.pgdg13+2`），Apache AGE 1.8.0（PG18），伺服器標頭 `postgresql-server-dev-18` 18.6-1.pgdg13+2 arm64，建置工具取自 Debian snapshot `20260918T000000Z`。
- CloudNativePG 1.30.1（官方 release manifest，暫時安裝）。

## 指令

```sh
# 來源驗證（AGE 的 Apache 簽章、伺服器標頭與 PGDG 簽章索引的 SHA-256）：見 data/.../source-provenance.txt

# release 的建置路徑（只留 postgres 映像，略過 Rust 的交叉編譯）
bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run --output plan-all.json
# plan.json = plan-all.json 的 images 只保留 name == "postgres"
bun scripts/release/build.ts --plan plan.json --out release

# 可重現性：不使用快取重建，比對 age.so
podman build --no-cache --platform linux/arm64 --format oci --timestamp 1791619200 \
  --tag localhost/trustiot-57/postgres:nocache deploy/images/postgres

# 工作站上的測試（檔案檢查；Postgres 檢查因為不能執行 arm64 而略過）
POSTGRES_IMAGE=localhost/trustiot-57/postgres:dev bun test deploy/images/postgres

# 實驗機：把 release 建出的 OCI 壓縮檔載入 lab 的 podman，以 podman remote 跑同一份測試
scp release/images/postgres.oci.tar trustiot-mac-m1:e2e-57/
ssh trustiot-mac-m1 'podman pull oci-archive:$HOME/e2e-57/postgres.oci.tar'   # 再 tag 成 localhost/trustiot-57/postgres:release-local
ssh trustiot-mac-m1 'setsid nohup podman system service --time=0 tcp://127.0.0.1:39057 < /dev/null > ~/e2e-57/service.log 2>&1 &'
ssh -f -N -L 127.0.0.1:39057:127.0.0.1:39057 trustiot-mac-m1
CONTAINER_HOST=tcp://127.0.0.1:39057 POSTGRES_IMAGE=localhost/trustiot-57/postgres:release-local \
  bun test deploy/images/postgres

# 實驗機：CloudNativePG 1.30.1 + ImageCatalog（major 18）+ Cluster（shared_preload_libraries: age）
sudo k3s kubectl apply --server-side -f cnpg-1.30.1.yaml
sudo k3s ctr -n k8s.io images import --digests postgres.oci.tar
sudo k3s kubectl apply -f cluster.yaml   # 內容見 data/.../lab-notes.txt
sudo k3s kubectl -n e2e-57 wait --for=condition=Ready cluster/age --timeout=170s
```

## 結果

| 項目 | 結果 |
| --- | --- |
| 沒有模擬器的 x86 上以 release 的 `build.ts` 建置 linux/arm64 | 成功；manifest `sha256:cb840e1939e3cf785d3ff68f9ba62b9d196a3987e4fd705170a4489735ffea9c`，OCI 壓縮檔 90.7 MB（[local-release-build.txt](data/2026-10-11-57-age-postgres-image/local-release-build.txt)） |
| `age.so` | aarch64 ELF，只依賴 `libc.so.6`、`ld-linux-aarch64.so.1`，帶 `.note.package` |
| SBOM（syft，SPDX 2.3） | 142 個套件，含 `apache-age@1.8.0`（`pkg:generic/apache-age@1.8.0`、`cpe:2.3:a:apache:apache-age:1.8.0`）與 `postgresql-18@18.6-1.pgdg13+2`（[sbom-packages.txt](data/2026-10-11-57-age-postgres-image/sbom-packages.txt)） |
| 可重現性 | 不使用快取重建的 `age.so` 與 release 建出的逐位元相同（SHA-256 `7301f910…`） |
| 工作站上的 `image.test.ts` | 5 通過、3 略過（不能執行 arm64） |
| 實驗機上對 release 映像的 `image.test.ts` | 8 通過、0 失敗（[lab-notes.txt](data/2026-10-11-57-age-postgres-image/lab-notes.txt)、逐步輸出 [lab-arm64-run.txt](data/2026-10-11-57-age-postgres-image/lab-arm64-run.txt)）：PID 1 是 postgres，UID/GID 26、CapEff 0、NoNewPrivs 1、根檔案系統唯讀；`CREATE EXTENSION age` 裝上 1.8.0；cypher() 建立並查回 `"press-1" \| "oven-2"`；非 superuser `graph_reader` 在唯讀交易中不經 `LOAD` 查回 2 個節點 |
| CloudNativePG 1.30.1 | `spec.imageName` 直接寫我們的映像會被拒絕（`invalid version tag`，因為 tag 不是 PostgreSQL 版本）；改用 ImageCatalog `major: 18` 後 Cluster 進入 healthy。pod 以 `runAsUser/runAsGroup/fsGroup 26`、`runAsNonRoot`、`readOnlyRootFilesystem`、drop ALL 執行，`shared_preload_libraries` 是 `age`，`CREATE EXTENSION age` 與 cypher() 查詢成功（[lab-cnpg-cluster.txt](data/2026-10-11-57-age-postgres-image/lab-cnpg-cluster.txt)） |
| 實驗機還原 | 叢集層級物件清單（CRD、webhook、ClusterRole、namespace、PV）前後 SHA-256 相同；匯入的映像與 operator 映像已刪除；`poc` 的 8 個 pod 前後都 Running |

## 結論

- 「release.yml 建出 arm64 映像並推到 GHCR」：**部分通過**。映像以 release.yml 的 `build.ts` 在與 runner 相同限制（x86、無模擬器）下建出 linux/arm64，SBOM 列出 AGE，release 不必修改。推到 GHCR、簽章與 verify 要一次真正的 release（rc tag），**待測**：tag 對外可見，由 coordinator 決定何時打。
- 「映像內 `CREATE EXTENSION age` 成功，並能執行一個 cypher() 查詢（自動測試）」：**通過**。`deploy/images/postgres/image.test.ts` 在實驗機的 arm64 上對 release 建出的同一個映像 8 項全過，CloudNativePG 實際部署也成功；ci.yml 新增的 `postgres-image` job 在 x86 runner 以 qemu 執行同一份測試（PR 的 CI 結果另見 PR）。
- 「chart 以 digest 引用此映像」：**待測**。digest 要等第一次 release 發佈後才有（本機與 runner 的 podman 版本不同，digest 不保證相同，不能用本機的值）；發佈後把 digest 寫進 chart。
- 映像以非 root（UID 26）執行，符合 CloudNativePG 的預設 securityContext：**通過**。
