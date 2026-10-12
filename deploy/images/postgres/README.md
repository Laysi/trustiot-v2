# postgres：含 Apache AGE 的 CloudNativePG Postgres 映像

CloudNativePG 的 PostgreSQL 18 operand 映像加上自建的 Apache AGE（決策 21、檢查清單 M8，Laysi/trustiot-v2#57）。release.yml 把它和其他映像一起發佈成 `ghcr.io/laysi/trustiot-v2/postgres:<版本>`，只有 linux/arm64，以 cosign 簽章並附 SBOM（[CI 與發佈](../../../docs/architecture/ci-release.md) 的「release.yml」）。chart 以 CloudNativePG 部署它（Laysi/trustiot-v2#58）。

## 內容與釘選

| 項目 | 目前的值 | 釘在哪裡 |
| --- | --- | --- |
| 基底 | `ghcr.io/cloudnative-pg/postgresql:18.6-minimal-trixie`（PostgreSQL 18.6，Debian 13） | `Containerfile` 的兩行 `FROM`，tag 加 multi-arch index 的 digest，兩行要相同 |
| Apache AGE | 1.8.0（PG18 版的原始碼發行檔） | `AGE_VERSION`、`AGE_SHA512`：archive.apache.org 上與發行檔一起公布的 SHA-512 |
| aarch64 的伺服器標頭 | `postgresql-server-dev-18` 18.6-1.pgdg13+2 | `PG_PACKAGE_VERSION`、`PG_SERVER_DEV_SHA256`：與 PGDG 簽章的 apt 索引中的 SHA-256 相同 |
| 建置工具 | Debian trixie 的 `gcc-aarch64-linux-gnu`、flex、bison | `DEBIAN_SNAPSHOT`：snapshot.debian.org 的時間點，與基底映像建置時用的相同 |

選 `minimal` 而不是 `standard`：`standard` 多出的 pgvector、pgaudit 與全部 locale 我們用不到（向量放 Qdrant，決策 19），映像較小、套件較少。資料庫用 `C.UTF-8` 或 ICU 定序，兩者 `minimal` 都有。

AGE 的檔案只有 `age.so`、`age.control`、`age--*.sql`，另外附 AGE 的 `LICENSE` 與 `NOTICE`（`/usr/share/doc/apache-age/`）。映像的其餘部分與基底相同，以 UID 26（postgres）執行。

## 建置方式

arm64 映像裡不執行任何程式，所以 x86 的機器不用模擬器也能建置。release.yml 原本在 x86 的代管 runner 上建置，Laysi/trustiot-v2#310 起改在 arm64 runner 上，這條規則由它的 plan 檢查。`Containerfile` 分三個階段：

1. `base`：CloudNativePG 的 arm64 映像，只當檔案來源與最終映像的基底。
2. `build`：同一個 multi-arch index 中建置機架構的映像（`--platform=$BUILDPLATFORM`），所以 `pg_config` 回報的版本與路徑和 arm64 映像相同。從 `base` 複製 arm64 的 PGXS makefile（編譯旗標）與 dpkg 狀態，確認兩邊的 `postgresql-18` 版本都等於 `PG_PACKAGE_VERSION`；裝上交叉編譯器；下載 aarch64 的 `postgresql-server-dev-18` 取得伺服器標頭（兩種架構的標頭只差 `pg_config.h`，但它決定標頭裡 CPU 相關的程式路徑，所以必須是 aarch64 的）；下載並驗證 AGE 原始碼（來源與重試見下一節），以 `aarch64-linux-gnu-gcc` 交叉編譯。
3. 最終映像：`base` 加上複製進來的 AGE 檔案，沒有 `RUN`。

### 下載的重試與來源

建置時有兩個對外下載，都以釘選的雜湊驗證，所以重試或換來源不影響安全性：

- 兩個下載共用 `Containerfile` 的 `fetch` 函式：`curl --retry 5 --retry-all-errors --retry-delay 5 --connect-timeout 20`。curl 的 `--retry` 只重試逾時與 HTTP 408、429、5xx，連不上伺服器（curl 錯誤碼 7）不重試，一次暫時的網路問題就會讓 CI、release 與 nightly 的建置失敗（Laysi/trustiot-v2#369）；`--retry-all-errors` 補上這一類。`--connect-timeout` 讓不通的位址不必等到系統預設的逾時（約 76 秒）才放棄。
- AGE 原始碼先從 Apache 的鏡像（`AGE_MIRROR_URL`，預設 `dlcdn.apache.org`）下載，連不上、下載失敗或雜湊不符時改用 `archive.apache.org`（`AGE_ARCHIVE_URL`）。鏡像只留目前的發行版，版本被移出後仍能從存檔取得；兩個來源都以同一個 SHA-512 驗證，都失敗時建置失敗。兩個 `ARG` 只在 `build` 階段，不影響最終映像的內容。測試失敗路徑時，可把 `AGE_MIRROR_URL` 指向連不上的位址（`--build-arg AGE_MIRROR_URL=https://127.0.0.1:9/age`）。
- Debian snapshot 的 apt 不必改：`apt-get` 已帶 `-o Acquire::Retries=5`，apt 的重試包含連線失敗。PGDG 的 deb 沒有備援來源，因為 apt-archive.postgresql.org 是唯一保留舊版本的地方。

編譯時的兩個取捨：

- `with_llvm=no`：不產生 JIT inlining 用的 LLVM bitcode（需要 aarch64 的 clang）。呼叫 AGE 函式的查詢照常執行，JIT 仍會編譯計畫的其他部分，只是不會把 AGE 的函式 inline 進去。
- `age.so` 帶一個 ELF package note（systemd 的 ELF package metadata 格式），syft 由它認出 Apache AGE，所以 release 的 SBOM 列出 `apache-age 1.8.0`（purl `pkg:generic/apache-age@1.8.0`，CPE `cpe:2.3:a:apache:apache-age:1.8.0`），漏洞掃描查得到。基底的 Debian 套件由 dpkg 資料列出。

## 在 CloudNativePG 中使用

2026-10-11 在實驗機（Mac mini M1，k3s 1.36.5）以 CloudNativePG 1.30.1 驗證，見 [測試報告](../../../docs/test-reports/2026-10-11-57-age-postgres-image.md)。Laysi/trustiot-v2#58 寫 Cluster 時注意：

- **以 ImageCatalog（或 ClusterImageCatalog）指定映像，`major: 18`**。CloudNativePG 從 `spec.imageName` 的 tag 讀 PostgreSQL 主版本，我們的 tag 是 TrustIoT 的版本（例如 `0.1.0`），會被拒絕（`spec.imageName: Invalid value: ...: invalid version tag`）。catalog 的 `image` 寫成 `ghcr.io/laysi/trustiot-v2/postgres:<版本>@sha256:<digest>`。
- **`spec.postgresql.shared_preload_libraries: [age]`**。AGE 的 cypher() 要靠載入時掛上的 parser hook；預先載入後每個 session 都有，不必 `LOAD 'age'`。非 superuser 只能 `LOAD` `$libdir/plugins/` 下的程式庫，所以每租戶的登入角色（決策 21）一定要靠預先載入。
- 不必改 `postgresUID`、`postgresGID`（預設 26）。CloudNativePG 以 UID/GID 26、`runAsNonRoot`、唯讀根檔案系統、drop ALL capabilities、不允許提權執行這個映像，實測可正常初始化與運作。
- 每個資料庫要 `CREATE EXTENSION age` 一次：chart 的 Database 物件（`<cluster>-trustiot`）讓 CloudNativePG 以 superuser 建立（Laysi/trustiot-v2#58，見 [chart 的「叢集內的 Postgres」](../../chart/README.md#叢集內的-postgres)）；資料庫 owner 與服務的角色都不是 superuser，不能自己建立。查詢前把 `ag_catalog` 放進 `search_path` 或寫完整名稱。

## 升級 Postgres 與 AGE

Postgres 與 AGE 一起升級：AGE 依 PostgreSQL 主版本各自發行（`PG18/1.8.0`），伺服器標頭必須是基底映像裡那一個 PostgreSQL build 的。全部的釘選都在 `Containerfile`，改完以 `bun test deploy/images/postgres` 與 ci.yml 的 `postgres-image` job 驗證，PR 合併後下一次 release 發佈新映像，再由 chart 的 digest 換過去。

### PostgreSQL 小版本或基底重建（例如 18.6 → 18.7）

Renovate 會開 PR 更新 `FROM` 的 tag 與 digest（`renovate.json5` 只允許 18.x），但不會更新伺服器標頭，所以 `postgres-image` job 會以「the base image has postgresql-18 …, but PG_PACKAGE_VERSION is …」失敗。在同一個 PR：

1. 兩行 `FROM` 的 tag 與 digest 相同：`skopeo inspect docker://ghcr.io/cloudnative-pg/postgresql:<tag> --format '{{.Digest}}'`。
2. 讀新基底的套件版本，填入 `PG_PACKAGE_VERSION`：`podman run --rm --platform linux/amd64 <image>@<digest> dpkg-query --show --showformat='${Version}\n' postgresql-18`（建置時會再確認 arm64 的版本相同）。
3. 下載 `https://apt-archive.postgresql.org/pub/repos/apt/pool/main/p/postgresql-18/postgresql-server-dev-18_<版本，+ 寫成 %2B>_arm64.deb`，把 SHA-256 填入 `PG_SERVER_DEV_SHA256`，並與簽章索引核對：`https://apt-archive.postgresql.org/pub/repos/apt/dists/trixie-pgdg-archive/InRelease`（以 PGDG 金鑰 `B97B 0AFC AA1A 47F0 44F2 44A0 7FCC 7D46 ACCC 4CF8` 驗證）列出 `main/binary-arm64/Packages.bz2` 的 SHA-256，`Packages.bz2` 裡該檔案的 `SHA256` 要與下載的相同。apt-archive 保留所有發佈過的版本，所以舊 commit 也能重建。
4. 想要較新的建置工具時，把 `DEBIAN_SNAPSHOT` 改成新基底的時間點（基底映像的 `/etc/apt/sources.list.d/debian.sources` 註解裡的 snapshot 網址）。

### AGE 新版本

1. 從 `https://archive.apache.org/dist/age/PG18/` 取新版的 `apache-age-<版本>-src.tar.gz`，以同目錄的 `.asc` 與 `https://archive.apache.org/dist/age/KEYS` 驗證簽章，把 `.sha512` 的值填入 `AGE_SHA512`，改 `AGE_VERSION`。
2. 讀 AGE 的發行說明：新版附 `age--<舊>--<新>.sql` 升級腳本時，既有資料庫以 `ALTER EXTENSION age UPDATE` 升級。同一個 PR 把 `deploy/chart/templates/_postgres.tpl` 的 `trustiot.postgres.ageVersion` 改成新版（chart 測試比對它與 `AGE_VERSION`），CloudNativePG 依 Database 物件以 superuser 執行 `ALTER EXTENSION`（Laysi/trustiot-v2#58，見 [chart 的「叢集內的 Postgres」](../../chart/README.md#叢集內的-postgres)）。

### PostgreSQL 主版本（例如 18 → 19）

刻意的升級，Renovate 不會提出。要等 AGE 發行該主版本的版本（`PG19/…`），再把 `Containerfile` 與本 README 的 `18` 全部換掉（路徑、套件名、`postgresql-server-dev-<主版本>`）、`image.test.ts` 的 `PG_MAJOR` 與 `renovate.json5` 的 `allowedVersions` 一起改；資料要以 CloudNativePG 的主版本升級流程搬移（Laysi/trustiot-v2#58 的部署文件），不能只換映像。

## 測試

`image.test.ts`（`bun test deploy/images/postgres`）：

- 不執行映像也能跑的檢查：每個 `FROM` 以 digest 釘選；只有 `--platform=$BUILDPLATFORM` 的階段有 `RUN`；沒有 `SHELL`，每個 `RUN` 都是以 `set -eu` 開頭的 POSIX sh（podman 建 OCI 映像時忽略 `SHELL`，2026-10-11 在 podman 5.8.7 上實際遇到）；最後是 `USER 26`；映像是 linux/arm64、以 UID 26 執行；`age.so` 是 aarch64 的 ELF，帶 AGE 版本的 package note；`age.control` 的版本與 `AGE_VERSION` 相同。
- 啟動 Postgres 的檢查（需要 arm64 主機或 binfmt 的 arm64 模擬器，沒有時本機略過，設了 `CI` 時失敗）：照 CloudNativePG 的方式以 UID/GID 26、唯讀根檔案系統、drop ALL capabilities、no-new-privileges 啟動，PID 1 就是 postgres；CloudNativePG 呼叫的 `initdb`、`pg_ctl`、`pg_controldata`、`pg_basebackup`、`pg_rewind` 在 PATH 上；`CREATE EXTENSION age` 裝上 `AGE_VERSION`，cypher() 建立並查回一張圖；非 superuser 角色在唯讀交易中不經 `LOAD` 執行 cypher() 查詢。

沒有設 `POSTGRES_IMAGE` 時測試先以 `podman build --platform linux/arm64` 建置；設了就測該映像。ci.yml 的 `postgres-image` job 在 arm64 runner 上以原生方式建置並執行測試，只在本目錄有變更時執行（規則在 `scripts/ci/changes.ts`）。arm64 上的原生建置驗證不到「arm64 階段不能有 `RUN`」，這一點由上面的靜態檢查與 release.yml 的 plan 檢查負責。在 arm64 機器上可直接跑；要測 release 建出的映像時，以 `bun scripts/release/build.ts` 建出 OCI 壓縮檔（[scripts/README.md](../../../scripts/README.md)），`podman pull oci-archive:<檔案>` 後把映像名稱給 `POSTGRES_IMAGE`。
