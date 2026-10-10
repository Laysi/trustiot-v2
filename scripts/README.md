# scripts

Bun + TypeScript 腳本：本地一鍵啟動（清單 M6）與種子資料。Bun 版本鎖在根目錄 `.prototools`。不是 Cargo workspace 成員，也沒有 `package.json`，沒有 npm 相依套件。

## 本地開發環境

一個指令在本機起好整套開發環境：Postgres（含 Apache AGE）、ClickHouse、Mosquitto，以及 `SERVICES`（`lib/services.ts`）列出的 Rust 服務。以 podman 執行，不需要 Kubernetes。

### 前置

- `proto install`（安裝 `.prototools` 鎖定的 Bun 與 Rust）。
- podman（rootless 即可），能連到 docker.io 與 gcr.io 拉映像。腳本不使用 docker。
- 啟動服務時要編譯 Rust；記憶體少的機器可設 `CARGO_BUILD_JOBS=2`。

### 指令

以下都在 repo 根目錄執行，每個動作就是一個 `bun` 指令：

| 動作 | 指令 | 說明 |
| --- | --- | --- |
| 啟動 | `bun scripts/dev.ts up` | 產生新的密碼、起好所有容器並等到每項健康檢查都通過；有任何一項失敗就以非 0 結束 |
| 種子資料 | `bun scripts/dev.ts seed` | 載入開發用資料，可重複執行，結束前讀回確認 |
| 狀態 | `bun scripts/dev.ts status` | 重跑健康與安全檢查（含「錯誤密碼與匿名連線被拒」） |
| 連線設定 | `bun scripts/dev.ts env` | 以 `KEY=value` 印出連線設定與密碼，輸出含標準的 `PG*` 變數，bash 可用 `set -a; eval "$(bun scripts/dev.ts env)"; set +a`，之後直接 `psql` |
| 關閉 | `bun scripts/dev.ts down` | 移除容器與網路，刪除密碼；資料磁碟區保留 |
| 清除 | `bun scripts/dev.ts purge` | 移除容器、網路、資料磁碟區、建好的服務映像、密碼與狀態；加 `--images` 連拉下來的映像一併移除 |

全新 clone 後：`proto install`，再 `bun scripts/dev.ts up`。

### 啟動了什麼

所有埠只綁 `127.0.0.1`，不接受其他機器連線。

| 元件 | 映像（以 digest 鎖定，見 `lib/stack.ts`） | 本機位址 | 預設埠 |
| --- | --- | --- | --- |
| Postgres 18 + Apache AGE 1.8（決策 4、21） | `apache/age:release_PG18_1.8.0` | `127.0.0.1:15432`，資料庫 `trustiot` | `TRUSTIOT_DEV_POSTGRES_PORT` |
| ClickHouse 26.8（決策 14） | `clickhouse/clickhouse-server:26.8` | `http://127.0.0.1:18123`（HTTP 介面） | `TRUSTIOT_DEV_CLICKHOUSE_PORT` |
| Mosquitto 2.1.2（決策 6） | `eclipse-mosquitto:2.1.2-alpine` | `127.0.0.1:11883`（不加 TLS） | `TRUSTIOT_DEV_MQTT_PORT` |
| `example-service`（service-kit 的示範服務） | 本機編譯後包進 `distroless/cc-debian13` | `http://127.0.0.1:18080`，`/healthz`、`/readyz` | `TRUSTIOT_DEV_PORT_EXAMPLE_SERVICE` |

其他環境變數：`TRUSTIOT_DEV_INSTANCE`（預設 `default`；同時要跑兩份環境時換名稱，並用上面的變數換埠）。

容器之間在同一個 podman 網路，以元件名稱互連（`postgres`、`clickhouse`、`mosquitto`）。

### 帳號與密碼

- 每次 `up` 都用 `crypto.randomBytes` 產生新的 256 位元密碼（Postgres、ClickHouse、MQTT 各一組）；repo 裡沒有任何預設或固定密碼。
- 密碼只存在 `$XDG_STATE_HOME/trustiot-v2/dev/<實例>/state.json`（預設 `~/.local/state/trustiot-v2/dev/default/`），目錄 0700、檔案 0600，不在 repo 內。`down` 與 `purge` 會刪除它。
- 密碼不出現在任何指令列：Postgres 與 ClickHouse 的密碼以 `--env-file`（0600，建立容器後立即刪除）交給容器，Mosquitto 只收到雜湊（`sha512-pbkdf2`），Postgres 的更新密碼以 stdin 送入。`up`、`status`、`seed` 不會把密碼印到終端機；要取得密碼只有 `env` 指令。
- 資料磁碟區在 `down` 之後保留，所以 `up` 會在既有資料上換成新密碼：Postgres 以 `ALTER ROLE` 更新，ClickHouse 與 Mosquitto 每次重建容器時重新設定。
- 同一個使用者的 podman 可以用 `podman inspect` 看到容器的環境變數（含密碼）；這與能讀 `state.json` 的是同一個人，開發機的信任邊界相同。

### 健康與安全檢查

`up` 結束前與 `status` 都會檢查，全部通過才算成功：

- 每個元件的本機埠可連線；Postgres 登入並確認 AGE 擴充已載入、ClickHouse 以帳密執行 `SELECT 1`、MQTT 以帳密連線。
- 服務的 `GET /healthz` 與 `GET /readyz` 回 200。
- 錯誤密碼被 Postgres 拒絕、本機埠上的 Postgres 要求 SCRAM 密碼（`pg_hba.conf` 只信任容器內的 unix socket）、匿名的 ClickHouse 與 MQTT 連線被拒。

### 種子資料

`seed` 依序套用：

1. `scripts/seed/postgres/*.sql`：`dev_seed` schema 的 `machine` 表與 AGE 圖 `dev_seed_graph`（兩台機台和一條 `FEEDS` 關係）。
2. `scripts/seed/clickhouse/*.sql`：`dev_seed.telemetry` 表與 240 筆決定性的遙測資料（每個 `.sql` 檔一條陳述式）。
3. MQTT：retained message `trustiot-dev/seed/hello`。
4. 服務自己的種子：`services/<名稱>/seed/postgres/*.sql` 與 `services/<名稱>/seed/clickhouse/*.sql`，依服務名稱與檔名排序。服務要提供開發資料時放進這個目錄，不必改腳本。

平台的種子只是煙霧測試資料（證明三個儲存都能寫入、讀回），不是產品的資料表；各服務的資料表由它自己的 migration 建立。

### 新增服務

在 `lib/services.ts` 的 `SERVICES` 加一筆：服務名稱（chart 的 `services` key）、Cargo package 與 bin、預設本機埠、`TRUSTIOT__<SECTION>__<KEY>` 形式的服務設定。`up` 會以 cargo 編譯、包成映像（`FROM` 是以 digest 鎖定的 distroless）、接上網路、把平台設定（`TRUSTIOT_<NAME>`，名稱與 [deploy/chart/README.md](../deploy/chart/README.md) 相同）當環境變數交給它，並檢查 `/healthz` 與 `/readyz`。服務監聽 8080（chart 的約定）。資料庫帳密與每個服務一個資料庫角色（決策 4）隨 Postgres 與 migration 的 issue 加入（Laysi/trustiot-v2#58、Laysi/trustiot-v2#60），目前服務收到的是連線位址，沒有帳密。

### 測試

```sh
bun test scripts                              # 純函式：密碼產生、檔案權限、埠綁定規則、映像 digest、通訊協定編碼、種子探索、固定密碼掃描
TRUSTIOT_DEV_E2E=1 bun test scripts/tests/e2e.test.ts   # 用 podman 起一份獨立環境，走完 up、seed、換密碼重啟、down、purge
```

端對端測試用自己的實例名稱、狀態目錄與隨機埠，不會動到平常使用的環境。

## 機制的決定（2026-10-10）

issue Laysi/trustiot-v2#71 把機制留給實作決定。選擇是 **rootless podman 直接跑容器，由 `scripts/lib/stack.ts` 與 `lib/services.ts` 宣告元件**，不使用 k3d 加 chart。

| 方案 | 取捨 |
| --- | --- |
| k3d + Helm chart | 與正式環境最接近，但要在工作站跑 Kubernetes（本專案規定不在工作站跑），需要 docker 相容的 socket 與 k3d，起一個叢集耗掉數百 MB 記憶體與數十秒；而且 chart 目前還沒有 Postgres（Laysi/trustiot-v2#57、Laysi/trustiot-v2#58）、ClickHouse（Laysi/trustiot-v2#59）與 broker（Laysi/trustiot-v2#84），無法用 chart 起整套 |
| 本機直接跑行程 | 最快，但資料庫與 broker 仍要容器，服務與依賴的連線方式與叢集差很多，也很難限制只綁 127.0.0.1 |
| **podman 容器（採用）** | 不需要 Kubernetes；每個服務與正式環境一樣是「一個 OCI 容器、HTTP 8080、`/healthz`、`/readyz`、同樣的環境變數名稱」；rootless、`--publish 127.0.0.1:…`、資源上限都由 podman 直接提供。代價：沒有 Istio、NetworkPolicy、入口 gateway 與 ServiceAccount，這些要到 lab 或 nightly 的叢集才驗證；元件清單要與 chart 各自維護，直到 chart 包含這些元件 |

之後改用 k3d 的成本低：服務的容器介面（埠、探針、環境變數）本來就照 chart 約定；種子資料、帳密產生與健康檢查不依賴 podman；只需換掉 `up` 與 `down` 啟動容器的那一層（`runArgs` 與 `commands.ts`），改成對 k3d 叢集 `helm install` 加 `kubectl port-forward --address 127.0.0.1`。與正式環境的已知差異：

- Postgres 映像是官方的 `apache/age`；正式環境是 CloudNativePG 加自建的 AGE 映像（Laysi/trustiot-v2#57）。
- 資料庫沒有 TLS，所以給服務的 `TRUSTIOT_POSTGRES_SSLMODE` 是 `disable`；chart 的 `external` 模式只接受 `verify-full`。service-kit 的 Postgres 連線也只接受 `verify-full`（Laysi/trustiot-v2#53），用 Postgres 的服務在這裡會拒絕啟動，第一個這樣的服務加進來之前，這裡的 Postgres 要改成 TLS。
- Mosquitto 用靜態的帳號、ACL 檔，不加 TLS、不用 dynsec、不經 mqtt-bridge（決策 6 的正式設定在 Laysi/trustiot-v2#84）。
