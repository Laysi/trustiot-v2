# scripts

Bun + TypeScript 腳本：本地一鍵啟動（清單 M6）、種子資料、CI 檢查與發佈流程（`release/`）。Bun 版本鎖在根目錄 `.prototools`。不是 Cargo workspace 成員。腳本只用 Bun 內建的 API，執行時不需要安裝任何 npm 套件；`package.json` 只列型別檢查用的開發相依（`typescript`、`@types/bun`），版本鎖在 `bun.lock`，見下方「型別檢查與測試」。

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
| 啟動 | `bun scripts/dev.ts up` | 產生新的密碼、起好所有容器並等到每項健康檢查都通過，再以 migration Job（`tools/migrate`）建立每個服務的角色、套用它的 migration，並為每個登入角色產生密碼（見「migration Job 與服務的登入角色」）；有任何一項失敗就以非 0 結束。身分服務的簽章金鑰在這時產生（已有就沿用）；身分服務本身不由 `up` 啟動，見下方「身分服務」 |
| 種子資料 | `bun scripts/dev.ts seed` | 載入開發用資料（含身分服務的開發使用者、租戶成員資格與 session），可重複執行，結束前讀回確認；印出開發 session 的 cookie |
| 狀態 | `bun scripts/dev.ts status` | 重跑健康與安全檢查（含「錯誤密碼與匿名連線被拒」） |
| 連線設定 | `bun scripts/dev.ts env` | 以 `KEY=value` 印出連線設定與密碼，輸出含標準的 `PG*` 變數（含 TLS：`PGSSLMODE=verify-full`、`PGSSLROOTCERT`）與服務的 `TRUSTIOT_POSTGRES_*` 平台設定，bash 可用 `set -a; eval "$(bun scripts/dev.ts env)"; set +a`，之後直接 `psql`；見下方「Postgres 的 TLS」；每個服務登入角色的帳密（`identity_app`、`assets_app`、`assets_platform`）見「migration Job 與服務的登入角色」；身分服務要的其他設定（公鑰、私鑰路徑、路由）見「身分服務」 |
| 關閉 | `bun scripts/dev.ts down` | 移除容器與網路，刪除密碼、憑證與伺服器私鑰；資料磁碟區保留，身分服務的簽章金鑰與開發 session 的 cookie 也保留（資料磁碟區裡的 session 還在，見「身分服務」） |
| 清除 | `bun scripts/dev.ts purge` | 移除容器、網路、資料磁碟區（含 session）、建好的服務映像、密碼、憑證、簽章金鑰與狀態；加 `--images` 連拉下來的映像一併移除 |

全新 clone 後：`proto install`，再 `bun scripts/dev.ts up`。

### 啟動了什麼

所有埠只綁 `127.0.0.1`，不接受其他機器連線。

| 元件 | 映像（以 digest 鎖定，見 `lib/stack.ts`） | 本機位址 | 預設埠 |
| --- | --- | --- | --- |
| Postgres 18 + Apache AGE 1.8（決策 4、21） | `apache/age:release_PG18_1.8.0` | `127.0.0.1:15432`，資料庫 `trustiot`，只接受 TLS | `TRUSTIOT_DEV_POSTGRES_PORT` |
| ClickHouse 26.8（決策 14） | `clickhouse/clickhouse-server:26.8` | `http://127.0.0.1:18123`（HTTP 介面） | `TRUSTIOT_DEV_CLICKHOUSE_PORT` |
| Mosquitto 2.1.2（決策 6） | `eclipse-mosquitto:2.1.2-alpine` | `127.0.0.1:11883`（不加 TLS） | `TRUSTIOT_DEV_MQTT_PORT` |
| `example-service`（service-kit 的示範服務） | 本機編譯後包進 `distroless/cc-debian13` | `http://127.0.0.1:18080`，`/healthz`、`/readyz` | `TRUSTIOT_DEV_PORT_EXAMPLE_SERVICE` |

映像一律用這台機器的原生平台（`linux/amd64` 或 `linux/arm64`，由 `process.arch` 推得，見 `lib/stack.ts` 的 `nativePlatform`）：建置範例服務映像的 `podman build` 與啟動 digest 映像的 `podman run` 都帶 `--platform`。原因：podman 以名稱在本機儲存裡找映像，不看架構；儲存裡已有同一個基底的別的架構的複本（例如照下方「本機跑一次完整流程」在 x86_64 工作站建過 arm64 映像）時，沒有 `--platform` 會直接用那份複本，建出的服務映像起不來，錯誤只有 `exec container process ... No such file or directory`（Laysi/trustiot-v2#406）。有 `--platform` 時 podman 會改拉原生架構的複本。`up` 建完服務映像還會檢查它的架構，不符就停止並說明原因與處理方式。

其他環境變數：`TRUSTIOT_DEV_INSTANCE`（預設 `default`；同時要跑兩份環境時換名稱，並用上面的變數換埠）。

容器之間在同一個 podman 網路，以元件名稱互連（`postgres`、`clickhouse`、`mosquitto`）。

### 帳號與密碼

- 每次 `up` 都用 `crypto.randomBytes` 產生新的 256 位元密碼（Postgres、ClickHouse、MQTT 各一組，加上每個有資料庫角色的服務一組）；repo 裡沒有任何預設或固定密碼。
- 密碼只存在 `$XDG_STATE_HOME/trustiot-v2/dev/<實例>/state.json`（預設 `~/.local/state/trustiot-v2/dev/default/`），目錄 0700、檔案 0600，不在 repo 內。`down` 與 `purge` 會刪除它。
- 密碼不出現在任何指令列：Postgres、ClickHouse 與服務角色的密碼以 `--env-file`（0600，建立容器後立即刪除）交給容器，Mosquitto 只收到雜湊（`sha512-pbkdf2`），Postgres 的更新密碼以 stdin 送入。`up`、`status`、`seed` 不會把密碼印到終端機；要取得密碼只有 `env` 指令。
- 資料磁碟區在 `down` 之後保留，所以 `up` 會在既有資料上換成新密碼：Postgres 以 `ALTER ROLE` 更新，ClickHouse 與 Mosquitto 每次重建容器時重新設定。
- 身分服務的簽章私鑰與開發 session 的 cookie 不是每次 `up` 重新產生的帳密：它們放在 `<實例>/identity/`（目錄 0700、檔案 0600），`down` 保留、`purge` 刪除，細節見「身分服務」。
- 同一個使用者的 podman 可以用 `podman inspect` 看到容器的環境變數（含密碼）；這與能讀 `state.json` 的是同一個人，開發機的信任邊界相同。

### Postgres 的 TLS

service-kit 連 Postgres 一律用 `sslmode=verify-full`（Laysi/trustiot-v2#53），所以本機的 Postgres 也只接受 TLS，服務在這裡與在叢集裡用同一組設定。

- 每次 `up` 由 `lib/tls.ts` 用 `node:crypto` 產生一個新的開發 CA 與一張伺服器憑證（ECDSA P-256，效期 90 天），不需要 openssl。CA 的私鑰只存在於產生它的函式內，簽完伺服器憑證就丟棄，從不寫入磁碟。
- 伺服器憑證的名稱（SAN）：`postgres`（容器網路上的位址，服務用它連線）、`localhost`、`127.0.0.1`、`::1`（本機工具與直接在主機上跑的服務）。連線時用的名稱不在這份清單裡，`verify-full` 就失敗。
- 檔案的位置，全都不在 repo 內，也不會被 git 看到：
  - CA 憑證（公開）：`$XDG_STATE_HOME/trustiot-v2/dev/<實例>/tls/ca.crt`，目錄 0700。`env` 輸出它的路徑。
  - 伺服器私鑰與憑證：只在 podman 磁碟區 `postgres-config`（私鑰 0600、擁有者是容器裡的 postgres 使用者），給 Postgres 的容器唯讀掛載。`down` 與 `purge` 都會移除這個磁碟區，所以私鑰不會比容器活得久。
  - 服務拿到的是另一個磁碟區 `postgres-ca`，只有 CA 憑證，掛在 `/etc/trustiot/postgres-ca/ca.crt`（與 chart 相同的路徑），所以服務讀得到 CA、讀不到伺服器私鑰。
- `pg_hba.conf` 只信任容器內的 unix socket；其他位址一律要 TLS 加 SCRAM，沒有 TLS 的連線被拒絕（`hostssl` 加 `host … reject`）。
- 服務拿到的平台設定：`TRUSTIOT_POSTGRES_HOST=postgres`、`TRUSTIOT_POSTGRES_SSLMODE=verify-full`、`TRUSTIOT_POSTGRES_CA_FILE=/etc/trustiot/postgres-ca/ca.crt`。服務的資料庫角色（`SERVICES` 的 `database.role`，目前是 `example_service`）由 `up` 建立：一般的登入角色，不是 superuser、沒有 `BYPASSRLS`、不擁有任何物件；`up` 也套用 service-kit 的 `crates/service-kit/sql/database.sql`（收回 `PUBLIC` 的 `TEMPORARY`），這是 service-kit 的連線檢查要求的。角色的密碼每次 `up` 重新產生，以 `--env-file` 交給容器（`TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD`）。範例服務有了資料庫角色，`/readyz` 的 `postgres` 檢查就代表它以 `verify-full` 連上了本機 Postgres。
- 直接在主機上跑服務時：`set -a; eval "$(bun scripts/dev.ts env)"; set +a` 後，`TRUSTIOT_POSTGRES_*` 已是對的值（主機 `127.0.0.1`、CA 檔案的路徑），服務的角色在 `TRUSTIOT_DEV_<服務名稱>_DATABASE_USERNAME` 與 `..._PASSWORD`（例如 `TRUSTIOT_DEV_EXAMPLE_SERVICE_DATABASE_USERNAME`），照服務的設定名稱設成 `TRUSTIOT__DATABASE__USERNAME` 與 `TRUSTIOT__DATABASE__PASSWORD`。`psql` 等 libpq 工具讀 `PGSSLMODE` 與 `PGSSLROOTCERT`。`TRUSTIOT_DEV_POSTGRES_URL` 不含 TLS 參數，要用它的工具得自己加 `sslmode=verify-full` 與 CA；沒有 TLS 的連線會被伺服器拒絕，不會默默退回明文。
- 憑證 90 天後過期；長時間開著的環境在 `status` 會失敗，`down` 再 `up` 即換新。
- 從加入 TLS 之前啟動的環境升級：`status`、`seed`、`env` 會說狀態檔是舊版寫的；先 `bun scripts/dev.ts down`（不讀狀態檔，移除舊容器，資料磁碟區保留，沒有 TLS 磁碟區也沒關係）再 `up`，資料與種子都還在，不需要 `purge`。

### migration Job 與服務的登入角色

`up` 以 migration Job（`tools/migrate`）建立 `PRODUCT`（`tools/migrate/src/registry.rs`）每個服務的角色與 schema，並讓服務的登入角色能以 `env` 印出的帳密登入（Laysi/trustiot-v2#336、Laysi/trustiot-v2#407）。這一節適用每個服務；身分服務自己的前提（簽章金鑰、開發 session）見下一節「身分服務」。

#### `up` 做的事

1. 建立 migration 角色 `trustiot_owner`（LOGIN、CREATEROLE、不是 superuser、沒有 BYPASSRLS，並成為資料庫 `trustiot` 的擁有者）：這是 migration Job 要求的形狀（[tools/migrate](../tools/migrate/README.md) 「一次執行做什麼」第 2 步），也是 chart 的 CloudNativePG 給 `trustiot_owner` 的樣子。它的密碼每次 `up` 重新產生，只存在 `up` 的行程與它啟動的 Job 的環境裡，不寫進檔案。
2. 以 `cargo build --package migrate` 建出 migration Job 的 binary，**不加參數、不用本機專用的設定**執行它（環境變數與 chart 的 Job 相同：`TRUSTIOT_POSTGRES_*`、`TRUSTIOT__POSTGRES__*`、`TRUSTIOT__PASSWORDS__*`），所以每個服務的 `<schema>_owner`、`<schema>_app`（有平台角色的服務還有 `<schema>_platform`）、schema、`database.sql`、`services/<名稱>/migrations/postgres/` 的每個檔案、outbox 表與 RLS 都由 Job 的程式建立，`dev.ts` 沒有另一套套用 DDL 的邏輯。Job 不處理 ClickHouse（沒有設定 ClickHouse 的連線）。
3. Job 為每個綁了 Secret 的登入角色產生的密碼存進本機的 state 檔，與其他服務的登入一起（`env` 印出它們，見下）。

`example-service` 的角色與密碼仍由 `up` 自己建立（`lib/secrets.ts` 的 `serviceRoleSql`）：它不在 Job 的服務清單（`PRODUCT`）裡。Job 與 `up` 的角色互不干擾，Job 只動它自己標記過的登入角色。

`up` 也以 superuser 收回 PUBLIC 對 `postgres` 與 `template1` 的 `CONNECT, TEMPORARY`，與 bundled Cluster 相同（Laysi/trustiot-v2#427）：語句取自 chart 的 `deploy/chart/files/postgres/system-databases.sql` 裡的兩句 `REVOKE`（只有一份來源；該檔另建的 CloudNativePG 角色 `streaming_replica` 與 `cnpg_metrics_exporter` 在這裡沒有 operator、也沒有密碼可登入，所以不建立）。語句冪等，每次 `up` 都執行，所以舊的開發環境再 `up` 一次就補上，不必 `purge`。Job 對其他資料庫的警告（`warnings`）會在 `up` 的輸出以 `WARNING:` 列出，沒有警告時 `migration Job finished` 那一行顯示 `0 warning(s)`。端對端測試斷言服務角色連不上這兩個資料庫、Job 沒有警告。

#### 哪些角色綁密碼

登入角色只在綁了 Secret 時能登入，Job 把沒有綁定的設成 NOLOGIN（`tools/migrate/src/lib.rs` 的 `Report::roles_without_password`）。本機綁的角色與 chart 相同（`deploy/chart/templates/_migrate.tpl` 的 `trustiot.migrate.passwordSecrets`）：

- `PRODUCT` 每個服務的 `<schema>_app`；
- chart 的 `trustiot.migrate.platformLogins` 列出的服務的 `<schema>_platform`（outbox 的消費端以它讀 producer 的事件，Laysi/trustiot-v2#65；目前是 `assets`）。其他平台角色（`identity_platform`）沒有程式以它連線，chart 不綁，本機也不綁，維持 NOLOGIN；`<schema>_owner` 只供 Job 與 `seed` 以 `SET ROLE` 使用，永遠不能登入。

清單在 `lib/product-logins.ts`（`PRODUCT_SERVICES`、`PLATFORM_LOGIN_SERVICES`），`up` 與 `env` 都由它得出角色、Secret 與 state 的鍵：每個服務一個 Secret `dev-<服務>-password`（與 chart 的 `<release>-<服務>-password` 一樣，一個 Secret 放兩個 key），state 檔的鍵是服務名稱（平台角色是 `<服務>-platform`）。`tests/product-logins.test.ts` 讀 `registry.rs` 與 `_migrate.tpl`，在 `PRODUCT` 多一個服務、或 chart 多綁一個平台角色而這份清單沒跟上時失敗，訊息說明要改 `lib/product-logins.ts` 的哪一個清單。

**新增一個服務到 `PRODUCT` 時**：把服務名稱加進 `PRODUCT_SERVICES`；它的平台角色若由 chart 綁（加進 `trustiot.migrate.platformLogins`），也加進 `PLATFORM_LOGIN_SERVICES`。其餘（Secret 的宣告、`env` 的變數、state 檔）都由清單產生，不必再改。

#### 密碼的決定（2026-10-14）

叢集裡，Job 把每個服務登入角色的密碼產生到 Kubernetes Secret（`TRUSTIOT__PASSWORDS__*`：API 位址、namespace、token 檔、CA 檔），沒有綁定 Secret 的登入角色是 NOLOGIN。本機沒有 Kubernetes，所以要決定服務登入角色的密碼從哪裡來。

| 做法 | 取捨 |
| --- | --- |
| **本機的 Secret API 替身（採用）**：`lib/secret-api.ts`，`up` 在 Job 執行的這段時間於 127.0.0.1 起一個只回答 Job 問的兩種請求（`GET` 與帶 `resourceVersion` 的 merge `PATCH`）的伺服器，宣告空的 Secret（每個服務一個，同 chart），Job 用它的一般路徑填入 | `tools/migrate` 一行都不改，Job 的程式路徑（產生 256 位元密碼、patch、以密碼登入、登不進去才 `ALTER ROLE`、再 `LOGIN`）與叢集相同；代價是多維護一個約 150 行的替身，它得跟 Job 對 API 的用法同步（`tests/secret-api.test.ts` 與端對端測試會在不同步時失敗） |
| 讓 `tools/migrate` 接受本機用的另一個密碼來源（例如從檔案或環境變數讀） | 本機不需要替身，但 Job 多出只有本機走的路徑：產品程式裡的「本機例外」會讓叢集與本機行為分岔，也多一條可能被誤設的設定（`tools/migrate` 的信任邊界是 Secret API） |
| `dev.ts` 自己 `ALTER ROLE ... LOGIN PASSWORD` | 最簡單，但等於在 `dev.ts` 另寫一套 Job 的角色邏輯，Job 把角色留成 NOLOGIN 時本機與叢集看到的狀態不同，違反「不要另寫一套」 |

替身的細節：

- 只聽 `127.0.0.1` 的隨機埠，每個請求要帶本次產生的 bearer token（256 位元，放在 `kube-token.<隨機>` 檔，0600，Job 結束就刪除；Job 的 `TRUSTIOT__PASSWORDS__API_URL` 是 `http://127.0.0.1:<埠>`，這是 Job 為測試與這個用途保留的唯一的非 TLS 位址）。
- Secret 只在記憶體裡，`up` 讀回每個登入角色的值之後伺服器就關閉；密碼不寫進替身自己的檔案，只進 state 檔（`state.json`，0600，`down` 與 `purge` 刪除），與其他本機帳密相同。每次 `up` 的 Secret 是新的物件，所以 Job 每次都產生新密碼並以 `ALTER ROLE` 換掉資料磁碟區裡舊的，行為與「Secret 被刪除後重建」相同。
- 密碼不出現在任何指令列：Job 只透過環境變數與 token 檔收到設定（不帶參數，環境不繼承呼叫者的 `TRUSTIOT*`）；Job 失敗時印出它的日誌，印出前把已知的密碼與 token 取代掉（Job 本來就不記錄密碼）；`up`、`status`、`seed` 都不印密碼，`env` 是唯一印出服務登入密碼的指令（issue 的範圍要求）。

#### `env` 印出的服務登入

每個綁了密碼的登入角色印兩個變數，名稱照 `TRUSTIOT_DEV_<服務>_DATABASE_{USERNAME,PASSWORD}` 的慣例（服務名稱大寫、`-` 寫成 `_`）：

| 變數 | 角色 |
| --- | --- |
| `TRUSTIOT_DEV_IDENTITY_DATABASE_USERNAME`、`..._PASSWORD` | `identity_app` |
| `TRUSTIOT_DEV_ASSETS_DATABASE_USERNAME`、`..._PASSWORD` | `assets_app` |
| `TRUSTIOT_DEV_ASSETS_PLATFORM_DATABASE_USERNAME`、`..._PASSWORD` | `assets_platform` |

使用者把它們設成服務讀的 `TRUSTIOT__DATABASE__USERNAME` 與 `TRUSTIOT__DATABASE__PASSWORD`（平台角色是 chart 給服務的 `TRUSTIOT__DATABASE__PLATFORM_USERNAME` 與 `TRUSTIOT__DATABASE__PLATFORM_PASSWORD`）。端對端測試以這些帳密逐一以 `verify-full` 登入本機 Postgres。

### 身分服務

身分服務（`services/identity`，Laysi/trustiot-v2#61）在本機的前提由 `up` 與 `seed` 備好，服務本身在主機上以 `cargo run -p identity` 啟動（Laysi/trustiot-v2#336；容器映像與 chart 的啟用在 Laysi/trustiot-v2#68、Laysi/trustiot-v2#308）。本機沒有入口 gateway 或 Istio，ext_authz 直接呼叫服務的 `/internal/ext-authz<路徑>`。

#### `up` 做的事

身分服務的前提有兩步在 `up` 做；Job 與它的角色見上一節。

1. 產生 Ed25519 簽章私鑰（`node:crypto`，PKCS#8 PEM），存在 `$XDG_STATE_HOME/trustiot-v2/dev/<實例>/identity/signing-key.pem`（目錄 0700、檔案 0600，與 Postgres 的 CA 同一個狀態目錄）。已經有金鑰就沿用並檢查：不是 Ed25519 PKCS#8 PEM、或其他使用者讀得到的檔案，`up` 拒絕並說明怎麼處理，錯誤訊息不引用檔案內容。沿用是為了讓正在跑的身分服務簽出的 token 不因 `up` 失效，也讓公鑰的 `kid` 不變。
2. 從這把私鑰寫出叢集裡身分服務 pod 的 init container 寫的同樣兩個檔案（Laysi/trustiot-v2#404，格式在 `crates/identity-keys`），同一個目錄、都是 0600，每次 `up` 重寫（壞掉的檔案因此會修好）：簽章金鑰檔 `signing-keys.json`（這一把，代 1，`published_at` 與 `signs_from` 是 0，立即簽章）與公鑰檔 `jwks.json`（同一把的 JWK Set，只有公鑰）。服務每 5 秒重讀這兩個檔案，與叢集裡重讀掛載的 Secret 與 ConfigMap 相同。
#### `env` 印出的變數怎麼依服務區分

`env` 一次印出全部，服務各讀自己要的（服務忽略不認得的平台設定，kit 的規則）；含 JSON 的值以單引號包起來，`eval` 與 dotenv 讀得懂，`podman --env-file` 不會用到它們：

| 變數 | 誰讀 | 內容 |
| --- | --- | --- |
| `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` | 所有以 service-kit 驗證內部身分的服務，也包含身分服務自己 | 公鑰檔 `jwks.json` 的**路徑**（JWK Set，只有公鑰；`kid` 是 `dev-` 加公鑰雜湊的前 12 個字元，換金鑰就換 `kid`）；身分服務只用列在這裡的金鑰簽章 |
| `TRUSTIOT_IDENTITY_SIGNING_KEYS_FILE` | 只有身分服務 | 簽章金鑰檔 `signing-keys.json` 的**路徑**，不是內容 |
| `TRUSTIOT_INGRESS_ROUTES` | 只有身分服務 | 本機的 cookie 路由：`localhost` 的每個路徑（`PathPrefix /`）給 `assets`（kit 的範例服務 `GET /v1/whoami` 接受的受眾）；沒有入口 gateway，所以呼叫 ext_authz 的人自己帶 `Host: localhost` |
| `TRUSTIOT_DEV_IDENTITY_DATABASE_USERNAME`、`..._PASSWORD` | 只有身分服務 | `identity_app` 與 Job 產生的密碼，見上一節「`env` 印出的服務登入」 |
| `TRUSTIOT_POSTGRES_*` | 所有服務 | 與原本相同（`verify-full`、開發 CA 的路徑） |

#### 在主機上啟動身分服務

```sh
bun scripts/dev.ts up && bun scripts/dev.ts seed
set -a; eval "$(bun scripts/dev.ts env)"; set +a
TRUSTIOT__DATABASE__USERNAME="$TRUSTIOT_DEV_IDENTITY_DATABASE_USERNAME" \
TRUSTIOT__DATABASE__PASSWORD="$TRUSTIOT_DEV_IDENTITY_DATABASE_PASSWORD" \
  cargo run -p identity
```

預設監聽 8080（API）與 8081（健康埠），`TRUSTIOT__HTTP__LISTEN_ADDR` 與 `TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR` 可改；`/readyz` 回 200 代表它以 `identity_app` 與 `verify-full` 連上了資料庫、有可以簽章的金鑰、路由可用。帳密經由行程的環境變數交給服務，不在 `cargo` 的參數裡。

#### 開發 session 與試 ext_authz

身分服務在 MS0 沒有登入（Laysi/trustiot-v2#103），也沒有寫入 session 的權限。`seed` 比照 [services/identity/README.md](../services/identity/README.md) 「測試工具怎麼建立 session」，以 `identity_owner` 的身分（`SET ROLE`，經容器內的 unix socket，沒有密碼）寫資料：

- `services/identity/seed/postgres/001-dev-user.sql`（一般的服務 seed 檔，可重複執行）：開發使用者 `00000000-0000-4000-8000-000000000a01` 與租戶 `00000000-0000-4000-8000-000000000b01` 的 `admin` 成員資格。
- 一筆 session：cookie 值是 32 bytes 的隨機數（base64url，43 個字元），資料庫只收它的 SHA-256；有效期 7 天。cookie 值存在 `<實例>/identity/dev-session-cookie`（0600，這是唯一的一份：資料庫只有雜湊）。再執行 `seed` 時，檔案裡的 cookie 還對得上一筆至少還有一小時效期、沒撤銷的 session 就沿用、印出同一個值；否則產生新的。

`seed` 結束時印出 `Cookie: __Host-trustiot-session=<值>`，這是本機的開發憑證，也是 `seed` 唯一印出的祕密：

```sh
curl -i -H 'Host: localhost' -H 'Cookie: __Host-trustiot-session=<seed 印出的值>' \
  http://127.0.0.1:8080/internal/ext-authz/api/anything     # 200，帶一個 x-internal-identity
```

換成無效的 cookie 回 401；主機名稱不是 `localhost` 回 403（路由表對不到）。回應的 token 以 `env` 印出的公鑰檔驗證（端對端測試做的就是這件事）。session 在資料磁碟區裡，所以 `down` 之後再 `up` 與 `seed`，同一個 cookie 還能用；`purge` 連資料磁碟區與 cookie 檔一起刪除。

#### 私鑰在哪裡、誰看得到

私鑰只存在 `<實例>/identity/signing-key.pem` 與從它寫出的 `signing-keys.json`（兩者都是 0600），不在 repo、不進容器、不出現在 `env`、`status` 與任何日誌（`env` 只印路徑；`status` 檢查金鑰檔存在、權限 0600、解得開、公鑰推得出來，兩個寫出的檔案存在、權限 0600、由這把金鑰寫出，只印 `kid`；端對端測試掃描每個指令的輸出與身分服務的日誌，確認不含私鑰內容與資料庫密碼）。`down` 保留它（資料磁碟區裡的 session 沒有因此失效，簽出的 token 也不必換 `kid`）；`purge` 刪除整個 `identity/`。

### 健康與安全檢查

`up` 結束前與 `status` 都會檢查，全部通過才算成功：

- 每個元件的本機埠可連線；Postgres 登入並確認 AGE 擴充已載入、ClickHouse 以帳密執行 `SELECT 1`、MQTT 以帳密連線。
- 服務的 `GET /healthz` 與 `GET /readyz` 回 200。
- 錯誤密碼被 Postgres 拒絕、本機埠上的 Postgres 要求 SCRAM 密碼（`pg_hba.conf` 只信任容器內的 unix socket）、匿名的 ClickHouse 與 MQTT 連線被拒。
- Postgres 的 TLS：不用 TLS 的連線被拒；以開發 CA 與 `127.0.0.1` 驗證後的 TLS 連線能進到 SCRAM；用另一個 CA 驗證會失敗，用憑證沒有的主機名稱驗證也會失敗（兩者都要確實是憑證被拒絕，不是連不上）；容器內的 `psql` 以 `verify-full` 登入並讀回 `pg_stat_ssl` 確認連線有加密。
- 範例服務的 `/readyz` 含 `postgres` 檢查，所以它通過就是服務以 `verify-full` 連上了資料庫。
- 身分服務的簽章金鑰檔存在、權限 0600（目錄 0700）、是 Ed25519 PKCS#8 金鑰（`status` 只印 `kid`，不印內容）。

### 種子資料

`seed` 依序套用：

1. `scripts/seed/postgres/*.sql`：`dev_seed` schema 的 `machine` 表與 AGE 圖 `dev_seed_graph`（兩台機台和一條 `FEEDS` 關係）。
2. `scripts/seed/clickhouse/*.sql`：`dev_seed.telemetry` 表與 240 筆決定性的遙測資料（每個 `.sql` 檔一條陳述式）。
3. MQTT：retained message `trustiot-dev/seed/hello`。
4. 服務自己的種子：`services/<名稱>/seed/postgres/*.sql` 與 `services/<名稱>/seed/clickhouse/*.sql`，依服務名稱與檔名排序。服務要提供開發資料時放進這個目錄，不必改腳本。

種子一律走容器內的 unix socket，不經 TLS。平台的種子只是煙霧測試資料（證明三個儲存都能寫入、讀回），不是產品的資料表；各服務的資料表由它自己的 migration 建立。

### 新增服務

在 `lib/services.ts` 的 `SERVICES` 加一筆：服務名稱（chart 的 `services` key）、Cargo package 與 bin、預設本機埠、`TRUSTIOT__<SECTION>__<KEY>` 形式的服務設定。`up` 會以 cargo 編譯、包成映像（`FROM` 是以 digest 鎖定的 distroless）、接上網路、把平台設定（`TRUSTIOT_<NAME>`，名稱與 [deploy/chart/README.md](../deploy/chart/README.md) 相同）當環境變數交給它，並檢查 `/healthz` 與 `/readyz`。服務監聽 8080（chart 的約定）。服務要用 Postgres 時，在 `SERVICES` 的項目加 `database: { role: "<角色名稱>" }`，`up` 會建立這個角色並把帳密交給服務（見上方「Postgres 的 TLS」）；平台設定已含 `verify-full` 與 CA。正式環境的每個服務一個資料庫角色（決策 4）、migration 與角色的權限由 migration Job（[tools/migrate](../tools/migrate/README.md)）處理。`up` 現在會跑這個 Job（見「migration Job 與服務的登入角色」），所以服務已在 Job 的服務清單（`registry.rs` 的 `PRODUCT`）裡的話，不要再在 `database` 宣告角色：Job 建它的 `<schema>_app`，密碼經 Secret 替身取得，要在 `lib/product-logins.ts` 登記（`tests/product-logins.test.ts` 在漏登記時失敗）；不在清單裡的服務（例如 `example-service`）仍由 `up` 建立一般的登入角色，沒有任何資料表權限。

### 測試

```sh
bun test scripts                              # 純函式：密碼產生、檔案權限、埠綁定規則、映像 digest、通訊協定編碼、種子探索、固定密碼與金鑰掃描；憑證的結構（含 openssl 驗證）與用假的 Postgres 驗證「對的 CA 通過、錯的 CA 與錯的主機名稱被拒」
TRUSTIOT_DEV_E2E=1 bun test scripts/tests/e2e.test.ts   # 用 podman 起一份獨立環境，走完 up、seed、換密碼重啟、down、purge，並在主機上啟動身分服務試 ext_authz
```

端對端測試要編譯 `migrate`、`identity` 與範例服務（記憶體少的機器設 `CARGO_BUILD_JOBS=2`），用自己的實例名稱、狀態目錄與隨機埠，不會動到平常使用的環境。身分服務的部分：Job 建立的角色形狀（`PRODUCT` 每個服務綁了密碼的登入角色 `identity_app`、`assets_app`、`assets_platform` 能登入，`identity_owner`、`identity_platform`、`assets_owner` 不能，`trustiot_owner` 擁有資料庫）、`env` 印出的每個登入角色的帳密都能以 `verify-full` 登入本機 Postgres、沒綁密碼的角色登不進去、再 `up` 之後每個登入角色換了密碼且舊密碼失效、`env` 印出的設定能在主機上啟動身分服務並通過 `/readyz`、以 `seed` 印出的 cookie 呼叫 ext_authz 得到 200 與剛好一個 `x-internal-identity`（以 `env` 的公鑰驗證簽章與 claims）、無效、缺少與格式錯誤的 cookie 回 401、對不到路由的主機回 403、私鑰檔權限 0600 且是狀態目錄裡唯一的私鑰、私鑰內容與密碼不在任何指令輸出與服務日誌、`down` 保留金鑰與 cookie（再 `up` 後換了 `identity_app` 的密碼，同一個 cookie 與公鑰仍有效）、`purge` 刪除它們。其餘它另外證明：範例服務以 `verify-full` 連上 Postgres 並通過 `/readyz`；同一個服務映像換成另一個 CA，或換成憑證沒有的主機名稱，仍在執行（`/healthz` 200）但不會就緒（`/readyz` 503）；`down` 之後憑證與私鑰磁碟區都沒了，`purge` 之後狀態目錄與 `tls/` 也沒了；repo 的工作目錄沒有因此多出任何檔案。另有兩個情境測試（Laysi/trustiot-v2#406）：先以 `podman pull --platform` 在儲存裡放進別的架構的服務基底，或別的架構的 Mosquitto，再 `up`，證明服務映像與 Mosquitto 都是這台機器的架構、服務就緒；儲存裡本來就有別的架構的複本時測試直接拒絕執行、不改動任何東西；否則測試結束時把名稱還原成原生複本，並以映像 ID 移除自己拉的那份，不動儲存裡其他映像。因為會改動 podman 儲存裡的映像，同一台機器上不要同時跑兩份端對端測試。

## 機制的決定（2026-10-10）

issue Laysi/trustiot-v2#71 把機制留給實作決定。選擇是 **rootless podman 直接跑容器，由 `scripts/lib/stack.ts` 與 `lib/services.ts` 宣告元件**，不使用 k3d 加 chart。

| 方案 | 取捨 |
| --- | --- |
| k3d + Helm chart | 與正式環境最接近，但要在工作站跑 Kubernetes（本專案規定不在工作站跑），需要 docker 相容的 socket 與 k3d，起一個叢集耗掉數百 MB 記憶體與數十秒；而且 chart 目前還沒有 Postgres（Laysi/trustiot-v2#57、Laysi/trustiot-v2#58）、ClickHouse（Laysi/trustiot-v2#59）與 broker（Laysi/trustiot-v2#84），無法用 chart 起整套 |
| 本機直接跑行程 | 最快，但資料庫與 broker 仍要容器，服務與依賴的連線方式與叢集差很多，也很難限制只綁 127.0.0.1 |
| **podman 容器（採用）** | 不需要 Kubernetes；每個服務與正式環境一樣是「一個 OCI 容器、HTTP 8080、`/healthz`、`/readyz`、同樣的環境變數名稱」；rootless、`--publish 127.0.0.1:…`、資源上限都由 podman 直接提供。代價：沒有 Istio、NetworkPolicy、入口 gateway 與 ServiceAccount，這些要到 lab 或 nightly 的叢集才驗證；元件清單要與 chart 各自維護，直到 chart 包含這些元件 |

之後改用 k3d 的成本低：服務的容器介面（埠、探針、環境變數）本來就照 chart 約定；種子資料、帳密產生與健康檢查不依賴 podman；只需換掉 `up` 與 `down` 啟動容器的那一層（`runArgs` 與 `commands.ts`），改成對 k3d 叢集 `helm install` 加 `kubectl port-forward --address 127.0.0.1`。與正式環境的已知差異：

- Postgres 映像是官方的 `apache/age`；正式環境是 CloudNativePG 加自建的 AGE 映像（[deploy/images/postgres](../deploy/images/postgres/README.md)，同為 PostgreSQL 18 與 AGE 1.8.0）。
- Postgres 的 TLS 憑證來自 `up` 當場產生的開發 CA（2026-10-11，Laysi/trustiot-v2#273），服務與在叢集裡一樣以 `verify-full` 連線；正式環境的憑證由 CloudNativePG 的 CA 或外部 Postgres 的 CA 簽發，CA 由 chart 的 `caSecret` 掛載。這裡的 CA 與伺服器憑證只管本機，每次 `up` 重新產生，信任範圍只有這個實例的服務與工具。
- 服務登入角色的密碼：叢集由 migration Job 產生進 Kubernetes Secret；本機的 Job 是同一個 binary，Secret 由 `lib/secret-api.ts` 的替身提供（「migration Job 與服務的登入角色」的「密碼的決定」）。替身不驗證 ServiceAccount 與 RBAC，那些由 lab 的 chart 測試驗證。
- Mosquitto 用靜態的帳號、ACL 檔，不加 TLS、不用 dynsec、不經 mqtt-bridge（決策 6 的正式設定在 Laysi/trustiot-v2#84）。

## CI 檢查

- `ci/check-workflows.ts`：CI 的 `workflow-policy` 檢查。每個 job 要明列 `permissions`，action 與 reusable workflow 要釘 40 字元 commit SHA（本地 action 不必釘，但要放在 `.github/actions/` 下），容器映像要釘 digest，不得使用 `pull_request_target` 與 `workflow_run`；規則與理由見 [CI 與發佈](../docs/architecture/ci-release.md)。執行：`bun scripts/ci/check-workflows.ts`。
- `ci/check-secret-handling.ts`：同一個 job 的密鑰處理檢查。repo 內每個 Dockerfile 與 Containerfile 不 COPY 金鑰類檔案（也不經目錄或萬用字元帶進 repo 裡的金鑰類檔案）、`ARG` 與 `ENV` 不放密鑰、複製整個 build context 時要有以 `*` 開頭的 ignore 檔；根目錄的 `.dockerignore` 只有 `*`，不用 Dockerfile 專屬的 ignore 檔與 `--ignorefile`；`--build-arg` 只能是字面值的 `NAME=value`、名稱不像密鑰；映像建置一律以 `lib/build-context.ts` 的 `containerBuildArgs(context, options)` 產生 `podman build` 的參數，它會先檢查實際的 context 並只接受白名單內的建置選項（不接受 `--build-context`、`--volume` 這類讓建置讀到 context 以外檔案的選項），其他地方不直接寫建置指令，也不以執行時才決定的子指令呼叫容器工具（`lib/podman.ts` 在執行時擋下不經 `containerBuildArgs` 的建置）；workflow 的 `run` 不寫 `${{ secrets.* }}`；repo 內的 Kubernetes Secret 一律以 SOPS 加密。規則與理由見 [密鑰與權限](../docs/architecture/secrets.md)。執行：`bun scripts/ci/check-secret-handling.ts`。測試檔（`*.test.ts`）不當成腳本掃描，因為它們刻意寫出違規的例子；`docs/test-reports/data/` 裡隨報告保存的測試替身（例如轉傳參數的 podman 包裝腳本）也不掃描，CI 與發佈不會執行它們。
- `ci/lint-workflows.ts`：CI 的 `workflow-policy` 的另一步，以 `.prototools` 釘選的 actionlint 檢查 workflow 的語法與各處可用的 context，擋下 GitHub 無法解析的檔案（例如 job 層 `env` 用了 `runner.temp`）；不啟用 shellcheck，理由見同一份文件的「Workflow 語法檢查」。執行：`proto install actionlint` 後 `bun scripts/ci/lint-workflows.ts`。
- `ci/changes.ts`：CI 的 `changes` job。比較 PR 的 merge commit 與它的第一個 parent（推到 main 時與 main 上最近一個 ci 通過的 commit 比較），依變更的路徑決定 `rust`、`cargo-deny`、`helm`、`host` 是否執行，寫進 `GITHUB_OUTPUT` 與 job summary；有任何問題都回答「全部執行」。它本身與它 import 的檔案（`release/lib.ts`）有變動時，全部 job 都執行。規則、守門測試與理由見 [CI 與發佈](../docs/architecture/ci-release.md) 的「依變更路徑決定 job」。測試：`bun test scripts/ci/changes.test.ts`；ci.yml 與 nightly.yml 的 `if` 與 concurrency 條件由 `bun test scripts/ci/workflow-conditions.test.ts` 以 GitHub 的 expression 引擎（`@actions/expressions`）驗證。

## nightly

`nightly/` 是 nightly workflow（`.github/workflows/nightly.yml`）的程式：`run.ts` 起 k3d 叢集、對兩份 values 各部署一次並做冒煙測試（`smoke.ts`、`manifests.ts`），輸出 `report.json`、`report.md` 與 `run.log`；`notify.ts` 依結果開、更新或關閉 `nightly-failure` issue。只在 CI 執行（需要 docker、k3d、kubectl、helm），做什麼與失敗怎麼辦見 [CI 與發佈](../docs/architecture/ci-release.md)。`service-image.ts` 以發佈的方式建出 Rust 服務的映像（`deploy/images/<名稱>/image.toml`）並匯入 k3d 節點，`run.ts` 以它取得 `image-cache.ts` 的 `NIGHTLY_IMAGES`（migration Job、身分服務與資產服務的映像）；`image-cache.ts` 算出映像輸入的快取 key，輸入沒變時 nightly 從 Actions 快取載入映像而不建置（見 [deploy/images/README.md](../deploy/images/README.md) 與 CI 與發佈的「服務映像」）。`api.ts` 是 smoke 經入口呼叫身分與資產服務 API 的檢查（Laysi/trustiot-v2#65）。`bun test scripts/nightly` 不需要叢集；`renovate.test.ts` 檢查 `renovate.json5` 的釘選規則與 `.prototools` 的 `# renovate:` 註解。

## 展示機（`demo/`）

展示環境（Laysi/trustiot-v2#457）跑在 lab 的 Mac mini M1（展示機）的 k3s，namespace `trustiot`。`demo/` 的腳本在有這個 repo 的工作站執行，經 ssh 在展示機上以 `sudo k3s kubectl` 操作，只用 Bun 內建的 API；共用的部分在 `demo/lib.ts`。展示機的 ssh 主機以 `--ssh-host` 或環境變數 `DEMO_SSH_HOST` 指定。

會改動叢集或刪除資料的腳本都遵守同一套規則：

- 不加 `--apply` 時只讀：印出計畫與確認碼（計畫的 SHA-256 前 12 位）。`--apply --confirm <確認碼>` 才執行，確認碼對不上就停。
- `--apply` 先取得 lab lock（展示機的 `~/e2e.lock`，與 [deploy/flux/README.md](../deploy/flux/README.md) 的「lab 測試」共用），在 lock 內重新規劃，與確認過的計畫不同就停，結束時只釋放自己的 lock。lock 的擁有者寫成 `<issue> <UTC 時間> demo <步驟>`，`<issue>` 是該腳本自己的 issue（本節兩支腳本是 `Laysi/trustiot-v2#465`）。已經自己持有 lock 時加 `--lock-held`：擁有者要以 `<issue>` 加一個空白開頭，其他 issue 的 lock（包括 #457 其他子 issue 的）一律不算。
- 紀錄寫在 `$XDG_STATE_HOME/trustiot-v2/demo/`（未設定時 `~/.local/state/trustiot-v2/demo/`），或 `--log-dir` 指定的目錄：log、計畫、變更前後的狀態（叢集層級物件的名稱與 SHA-256、每個 ClusterRoleBinding 的 subjects、每個 namespace 的 workload 與 Service 的 uid）。腳本自己建立的目錄是 0700、檔案 0600；已經存在的目錄不改權限，其他使用者能讀或進入的目錄（例如 `--log-dir .`）直接拒絕。
- 維運用的 tunnel（展示機的 `cloudflared.service`，ssh 經過它）只讀狀態（`ActiveState`、`MainPID`、`NRestarts`、`ExecMainStartTimestamp`）：不是 active 時不動手；結束時（包括失敗結束）確認還是同一個行程、沒有重啟過；從不改它。
- 可以重複執行：先看現況，已經是目標狀態就什麼都不做。

### 中斷之後

Ctrl-C、終端機關閉（SIGHUP）或筆電休眠時，腳本來不及釋放 lab lock，也可能有一個 kubectl 還在展示機上執行。lock 留著時，下一次 `--apply` 會停在「the lab lock is held by: ...」。釋放 ssh 送不到時，腳本會印出 `WARNING: releasing the lab lock failed (ssh)` 與自己的擁有者行，不會說成「別人的 lock」。處理方式：

1. 在展示機上讀 `~/e2e.lock/owner`，確認是這次中斷的執行寫的（issue、時間與步驟都對得上）；不是的話不要動，那是別人的。
2. 確認沒有這次執行留下的 kubectl 還在跑（`ps -ef | grep 'k3s kubectl'`）；有的話等它結束。腳本對單一指令的上限：刪除 namespace 後輪詢最多 600 秒，刪除單一物件的 `--wait` 最多 180 秒，istiod 的 rollout 最多 300 秒（等 mesh config 是另外的 600 秒輪詢）。
3. 兩者都確認後才 `rm -rf ~/e2e.lock`，然後先跑一次不加 `--apply` 的 dry run 看現況，再決定要不要重跑。腳本都可以重跑：已經刪掉或已經改好的部分不會再動。

### 清除 PoC 與平台對齊的順序

```sh
: "${DEMO_SSH_HOST:?set DEMO_SSH_HOST to the ssh host of the demo machine}"
bun scripts/demo/cleanup.ts --ssh-host "$DEMO_SSH_HOST"     # cleanup 的計畫與確認碼，只讀
bun scripts/demo/cleanup.ts --ssh-host "$DEMO_SSH_HOST" --apply --confirm <cleanup 的確認碼>
bun scripts/demo/platform.ts --ssh-host "$DEMO_SSH_HOST"    # cleanup 之後：檢查、計畫與確認碼，只讀
bun scripts/demo/platform.ts --ssh-host "$DEMO_SSH_HOST" --apply --confirm <platform 的確認碼>
bun scripts/demo/cleanup.ts --ssh-host "$DEMO_SSH_HOST"     # 確認：nothing to remove
bun scripts/demo/platform.ts --ssh-host "$DEMO_SSH_HOST"    # 確認：nothing to change
```

先 cleanup 再 platform：PoC 的 AuthorizationPolicy 還在引用舊的 provider 時，platform 的 `--apply` 會拒絕。platform 的確認碼涵蓋 istiod values 的變更（改前與改後的完整文字的 SHA-256）與叢集裡的 Gateway 清單。cleanup 刪掉 PoC 的 Gateway `edge` 之後清單會變，所以 platform 的確認碼要在 cleanup 之後的 dry run 取得。

### `demo/cleanup.ts`：清除可行性測試的殘留

2026-10-20 決定刪除 2026-10-09、10 可行性測試留在展示機上的 PoC（Laysi/trustiot-v2#465；結果已記在 [docs/feasibility](../docs/feasibility/)）：

| 刪除 | 條件 |
| --- | --- |
| namespace `trustiot`：雲端 PoC 的 app、clickhouse、grafana、mosquitto、otel-collector、postgres、qdrant，以及它們的 local-path volume 與資料 | PoC 的 7 個 workload 都在，而且沒有 Helm 或 Flux 管理的物件 |
| namespace `poc`：閘道器端 PoC 的 bridge、consumer、identity、mosquitto-poc 與 probe-a、probe-b、probe-c | PoC 的 4 個 workload 都在，而且沒有 Helm 或 Flux 管理的物件 |
| namespace `istio-ingress` 裡 PoC 的 Gateway `edge` 與它的 AuthorizationPolicy、NetworkPolicy、ConfigMap | namespace 只有這些與預設物件、而且都沒有被 Helm 或 Flux 管理時整個刪除；還有其他東西時只刪這幾個，namespace 保留 |
| subjects 全部在上面這些 namespace 的 ClusterRoleBinding，以及其他（保留的）namespace 裡這樣的 RoleBinding。subject 算在某個 namespace 裡：那裡的 ServiceAccount、User `system:serviceaccount:<ns>:<name>`，或 Group `system:serviceaccounts:<ns>`（那裡的每個 ServiceAccount，包括之後建立的） | 沒有被 Helm 或 Flux 管理；它綁的 ClusterRole 或 Role 保留，只列在計畫裡。要刪的 namespace 裡面的 RoleBinding 跟著 namespace 刪除 |

- 計畫列出每個會刪除的物件，以及每個 namespace、ClusterRoleBinding 與 RoleBinding 的 uid；被其他物件擁有的（Pod、ReplicaSet、Gateway 產生的 Deployment 與 Service 等）跟著擁有者刪除，不另外列；叢集自己產生的 Endpoints 與 Event 也不列。盤點以 jsonpath 只讀名稱、擁有者、Secret 的型別、標籤與 Helm 的 release 註記，不讀 Secret 的內容。「Helm 或 Flux 管理」指 `app.kubernetes.io/managed-by: Helm` 標籤、`meta.helm.sh/release-name` 註記、Helm 的 release Secret、Flux 的標籤，或任何 `*.toolkit.fluxcd.io` group 的物件。
- 以下情況整個執行停下，什麼都不刪，由人判斷：只剩一部分 PoC；PoC 旁邊有 Helm 或 Flux 管理的物件；`istio-ingress` 裡與 PoC 同名的物件由 Helm 或 Flux 管理；ClusterRoleBinding 或 RoleBinding 的 subjects 一部分在要刪的 namespace、一部分在別處，或它由 Helm 或 Flux 管理；webhook 設定、APIService 或 CRD 的 conversion webhook 呼叫要刪的 namespace 裡的服務（以 `service` 指定，或以 host 為 `<svc>.<ns>.svc`、`<svc>.<ns>.svc.cluster.local` 的 `url` 指定）。沒有任何 PoC workload 的 namespace（例如之後展示環境的 `trustiot`）不動。
- `--apply` 的順序：先刪 Gateway `edge`，在刪任何 namespace 之前（它的 CUSTOM policy 呼叫 `poc` 裡 PoC 的身分服務；Istio 再移除它為 Gateway 產生的物件）；再刪 ClusterRoleBinding 與 RoleBinding（中斷時不會留下綁在展示環境會沿用的 namespace 名稱上的權限；每個刪除前重新讀一次，uid、subjects、綁的角色或 Helm／Flux 的管理與計畫不同就停）；`istio-ingress` 保留時，刪它裡面 PoC 的其他物件前也重新確認 namespace 的 uid 沒變、namespace 與這些物件沒有被 Helm 或 Flux 接手；再逐一刪 namespace，`istio-ingress` 先。每個 namespace 刪除前都重新盤點一次：uid 變了、出現計畫裡沒有的物件，或有了 Helm 或 Flux 管理的物件，就停在那裡、不刪它（前面的刪除可能要好幾分鐘）；同時重新讀叢集層級的參照，計畫之後才出現、subjects 在這個 namespace 的 ClusterRoleBinding 或（其他 namespace 的）RoleBinding，或呼叫進這個 namespace 的 webhook 等，也讓它停下（否則展示環境之後在同名 namespace 建立的同名 ServiceAccount 會繼承那些權限）。namespace、ClusterRoleBinding 與 RoleBinding 的刪除都帶著計畫裡的 uid 作為 Kubernetes 的刪除前提（`kubectl delete --raw` 以 stdin 送出 `DeleteOptions` 的 `preconditions.uid`；binding 另外帶刪除前重新讀到的 `resourceVersion`，讀取之後有人改過它就不刪）：最後一次確認與刪除之間同名重建的物件，API server 會拒絕刪除，腳本停下，新的那個保留。刪 namespace 不以 `--wait` 等，而是以短指令輪詢到它消失（每個最多 600 秒），因為 ssh 會切斷超過約 3 分鐘沒有輸出的指令。接著等 PoC 的 PersistentVolume 被 local-path 刪除（reclaim policy `Delete`，最多 300 秒），重新盤點確認計畫已經是空的，最後比對其他 namespace 的 workload 與 Service 沒有變化。lock 內重新規劃時，可列出的資源種類也重新讀一次。
- 之前中斷過、namespace 已經不在時，指向這些 namespace 的 ClusterRoleBinding 與 RoleBinding 仍會列入計畫。
- 叢集層級的影響：刪除 namespace、PersistentVolume 與上述 ClusterRoleBinding；其他 namespace 裡的上述 RoleBinding 也會刪除。istiod 裡指向 PoC 的 provider 由下一步的 platform.ts 換掉。展示機的檔案系統（PoC 的程式與紀錄）不動。

### `demo/platform.ts`：平台元件與 repo 的釘選對齊

比對的版本是 nightly 安裝的版本：`.github/workflows/nightly.yml` 的 `ISTIO_VERSION`、`GATEWAY_API_VERSION`、`CNPG_VERSION`（`bun test scripts/nightly` 確保它們與 [deploy/istio/README.md](../deploy/istio/README.md)、[deploy/cloudnative-pg/README.md](../deploy/cloudnative-pg/README.md) 一致）。

| 元件 | 檢查 | 不一致時 |
| --- | --- | --- |
| Istio 的四個 HelmChart（`kube-system`） | 版本、沒有 Failed；values 與 `deploy/istio` 相同（istio-base 沒有 values 或用 `values-k3s.yaml` 都算，兩者渲染結果相同，2026-10-20 以 1.30.5 確認）；沒有 `spec.set`、`spec.valuesSecrets`；istiod、ztunnel、istio-cni 執行中的映像版本 | istiod 的 values 由 `--apply` 改；其他列為差異 |
| istiod 的 mesh config（ConfigMap `istio-system/istio`） | extension providers 與路徑正規化確實是 values 的內容 | 列為差異 |
| 叢集裡所有 Gateway：Gateway API 的 Gateway（包括 waypoint，以 class 標示）與 Istio 自己的 `networking.istio.io` Gateway（標示為 `=networking.istio.io`）；只在各自的 CRD 存在時讀取 | 列進計畫與確認碼：mesh 層級的變更套用到它們 | 只列出 |
| Gateway API 的 Gateway 與 HTTPRoute CRD、GatewayClass `istio` | standard channel、版本、GatewayClass 已 Accepted | 列為差異 |
| CloudNativePG operator | 映像版本、Available、chart 用到的 CRD、`cnpg-system` 的 Pod Security `restricted` | 列為差異 |

列為差異的，腳本不會修改，由人依該元件的 README 處理；腳本不安裝也不升級叢集層級的元件。

唯一會改的是 HelmChart `kube-system/istiod` 的 `spec.valuesContent`：改成 `values-k3s.yaml` 加 `istiod-values.yaml`（依 Helm 的合併方式），把 PoC 的 provider `identity`（指向 `identity.poc`）換成 `trustiot-identity`（指向 `trustiot-identity.trustiot`，展示環境的身分服務），並開啟 mesh 的 `DECODE_AND_MERGE_SLASHES`。這是 mesh 層級的設定，同一個叢集的每個 gateway 與 waypoint 都套用（deploy/istio/README.md 的「路徑正規化」）。

修改是 compare-and-swap：在 lock 內讀一次 HelmChart，這份就是備份，也是 JSON patch `test` 比對的值；`test` 確認 `metadata.resourceVersion` 仍是讀到的那個、`spec.valuesContent` 仍是確認過的那段文字，`replace` 換成新的值，並把寫入內容的 SHA-256 記在註記 `trustiot.io/demo-platform-values-sha256`（原本沒有任何註記時整個註記欄位一起加上）。讀取與修改之間 HelmChart 有任何變動（values、別人加的註記）時，API server 拒絕整個 patch，什麼都不改。這個 patch 不在 ssh 斷線時重送（成功之後重送會因 resourceVersion 不符而失敗，誤報為失敗）；斷線時讀回 HelmChart，已經是新的 values 與註記就當作成功，否則停下，要求先以不帶 `--apply` 的執行確認現況。k3s 的 helm-controller 以 Job `kube-system/helm-install-istiod` 升級 istiod；腳本等到 ConfigMap `istio` 顯示新的 providers 與路徑正規化（最多 600 秒），再等 istiod 的 rollout 完成。

`--apply` 在以下情況拒絕，什麼都不改：

- istiod 的 HelmChart 不在釘選的版本或已經 Failed；
- 還有別的東西也在設定 istiod 的 values：`spec.set`、`spec.valuesSecrets`、k3s AddOn 的註記 `objectset.rio.cattle.io/*`（k3s 重啟時會從 manifests 目錄重新套用），或疊在 Istio chart 上的 HelmChartConfig（lab 的 e2e 測試正在用，見 [deploy/chart/tests/e2e/README.md](../deploy/chart/tests/e2e/README.md) 入口測試的前置條件 2）；
- 有 CUSTOM 的 AuthorizationPolicy 引用新 values 裡沒有的 provider（PoC 的 `istio-ingress/edge-cookie-auth`：先跑 cleanup.ts）。

為什麼直接改 HelmChart，而不是疊一個 HelmChartConfig：lab 的 e2e 測試以 `HelmChartConfig istiod` 加入自己的 provider，測完刪掉；展示環境的 provider 如果放在 HelmChartConfig 裡，會跟著被刪。代價是展示機上當初以 `kubectl apply` 套用的檔案仍是 PoC 的 values，再套用一次就會把 provider 換回去，所以不要再套用它（deploy/istio/README.md 的「安裝（k3s）」）。

回復：`--apply` 修改前把 HelmChart 存成紀錄目錄裡的 `platform-<時間>-helmchart-istiod-before.json` 並印出路徑（家目錄底下的路徑一律印成 `~/...`，`--rollback` 也接受這個寫法，加引號的 `'~/...'` 也可以）。`bun scripts/demo/platform.ts --ssh-host "$DEMO_SSH_HOST" --rollback <那個檔>` 印出目前與要放回的 values 及回復的確認碼（涵蓋兩段文字的 SHA-256 與 Gateway 清單），加上 `--apply --confirm <確認碼>` 把存下的 `valuesContent` 逐位元組放回去，同樣以 compare-and-swap 寫入，等到 mesh config 顯示回復的 providers。以下情況拒絕：

- istiod 目前的 values 不是這支腳本上次寫入的（註記裡的 SHA-256 對不上）：回復會蓋掉之後的修改；
- 上面列出的「還有別的東西也在設定 istiod 的 values」；
- 有 CUSTOM 的 AuthorizationPolicy 在用要拿掉的 provider（例如展示環境的入口）；
- 回復會改變或拿掉路徑正規化，而叢集裡有 Gateway 或 waypoint：`//internal/...` 與 `/%2finternal/...` 會重新繞過 chart 的 `/internal*` DENY（Laysi/trustiot-v2#255）。

### 測試

`bun test scripts/demo`，也包含在 `bun test scripts` 裡。不需要叢集或 ssh：以記憶體裡的假叢集與假的 ssh 驗證 lab lock 的擁有者比對、mkdir 不重試與只釋放自己的 lock、參數的引號、盤點、確認之前不刪除也不改、lock 內與每個 namespace 刪除前發現變化就停、刪除帶 uid 前提、ClusterRoleBinding 與 webhook 的處理、compare-and-swap、回復的條件與逐位元組還原、逾時、失敗時仍釋放 lock、重複執行時什麼都不做；istiod 的 values 以 repo 裡的 `deploy/istio` 檔案驗證。

## release/：發佈流程

`.github/workflows/release.yml` 的每個 job 各執行一支腳本，彼此以 JSON 交接（型別在 `release/lib.ts`）。流程、權限與驗證方式見 [CI 與發佈](../docs/architecture/ci-release.md) 的「release.yml」。

| 腳本 | job | 內容 |
| --- | --- | --- |
| `release/plan.ts` | plan | 檢查 tag（正式版要在 main 上，且該 commit 在 main 的 ci 通過），列出要發佈的映像（`deploy/images/`）與 chart，檢查 Containerfile 只在 `FROM --platform=$BUILDPLATFORM` 的階段 `RUN`（`release/containerfile.ts`），`helm lint --strict` |
| `release/build.ts` | build | 交叉編譯 Rust、建置 arm64 映像（OCI 壓縮檔）與 SPDX SBOM，不推送 |
| `release/publish.ts` | publish | 推送映像、cosign 簽章與 SBOM attestation，把這次的映像 digest 寫進 chart 的暫存複本後打包、推送並簽章 chart（`release/chart-images.ts`），建立 GitHub Release |
| `release/verify.ts` | verify | 以唯讀權限驗證發佈結果（含 chart 以 digest 固定這次的映像），輸出驗證報告 |

單元測試：`bun test scripts/release`，也包含在 `bun test scripts` 裡（`lib.test.ts` 不需要任何工具；`plan.test.ts` 在暫存目錄建立 git repo）。

### 本機跑一次完整流程

需要 podman、skopeo，以及 `proto install` 裝好的 bun、helm、zig、cargo-zigbuild、cargo-auditable、syft、cosign，在 repo 根目錄執行。發佈到本機的暫用 registry，並以暫用的 cosign 金鑰簽章：本機模式不寫入公開的透明紀錄，也不建立 GitHub Release，而且只接受本機（`localhost`、`127.0.0.1`）的 registry。`--local-test-key` 與 `--registry` 在 GitHub Actions 裡會被拒絕，正式發佈一律 keyless 簽章、只發佈到 GHCR。映像壓縮檔不小，工作目錄放在 `~/.cache` 而不是 `/tmp`。

```sh
mkdir -p ~/.cache && work="$(mktemp -d ~/.cache/trustiot-release.XXXXXX)"
podman run -d --rm --name release-registry -p 127.0.0.1:5068:5000 \
  docker.io/library/registry:3@sha256:ddf754342cfc8acc51a56d5d0ab6af06826461864460636d8bd5c546dab2a7b8
COSIGN_PASSWORD= cosign generate-key-pair --output-key-prefix "$work/test"
printf '### 其他\n\n- **測試**：本機發佈。\n' > "$work/notes.md"

bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run \
  --registry 127.0.0.1:5068/laysi/trustiot-v2 --output "$work/plan.json"
bun scripts/release/build.ts --plan "$work/plan.json" --out "$work/out"
COSIGN_PASSWORD= bun scripts/release/publish.ts --plan "$work/plan.json" --artifacts "$work/out" \
  --notes "$work/notes.md" --output "$work/published.json" --local-test-key "$work/test"
bun scripts/release/verify.ts --plan "$work/plan.json" --published "$work/published.json" \
  --output "$work/report.md" --local-test-key "$work/test"

podman stop release-registry
```

主機是 x86_64 或 arm64 都能跑，指令相同，不需要模擬器。release.yml 在 arm64 runner 上建置（Laysi/trustiot-v2#310），所以要重現某次發佈的服務映像 digest，要在 arm64 上跑：x86 上交叉編譯出的服務執行檔功能相同、位元不同（chart 相同），原因與 x86 上以 qemu-user 模擬 arm64 容器重現的做法見 [CI 與發佈](../docs/architecture/ci-release.md) 的「交叉編譯與可重現建置」與其測試報告。

podman 以一般使用者（rootless）或 root 執行都可以，Rust 服務映像的 digest 相同（Laysi/trustiot-v2#384）：`build.ts` 把 Rust 映像建在自己的 podman store（vfs driver，放在系統暫存目錄下的工作目錄，`CONTAINERS_STORAGE_CONF` 指向它，`podman info` 確認 podman 用的就是它），結束時刪除，不動使用者原本的 store；自帶 Containerfile 的映像（`postgres`）仍用使用者的 store。中途被中斷而留下 `/tmp/trustiot-release-*/store` 時，rootless 的檔案屬於 user namespace 的 subordinate ID，要以 `podman unshare rm -rf <目錄>` 刪除。`build.json` 記錄打包用的 podman、buildah 版本（有 dpkg 時也記錄套件版本），發佈時寫進 GitHub Release；podman 版本不同時，同一個執行檔可能打包出不同的 digest。

本機模式打包的 chart 除了寫入映像 digest，也把 `serviceDefaults.image.registry` 與 `postgres.bundled.image.repository` 改成本機的 registry，chart 指向的就是剛推送的映像；正式發佈不改 registry，chart 裡的 repository 與推送的位置不同時 publish 停止。檢查打包出的 chart：`helm pull oci://127.0.0.1:5068/laysi/trustiot-v2/charts/trustiot --version 0.0.0-rc.1 --plain-http`，再以 `helm show values` 看 digest。

`--dry-run` 以目前的 HEAD 規劃，tag 不必存在，也不檢查正式版是否在 main 上、main 的 ci 是否通過（後者以 `GITHUB_TOKEN` 讀 GitHub API）。只要建置映像，前兩步就夠了。同一個版本再跑一次 publish 會得到相同的 digest，不會重複推送；內容不同時 publish 會拒絕覆蓋。

## 型別檢查與測試

以下指令都在 repo 根目錄執行：

- 安裝型別檢查用的開發相依：`bun install --cwd scripts --frozen-lockfile`（裝在 `scripts/node_modules/`，不進版控；執行腳本不需要，測試需要：`ci/workflow-conditions.test.ts` 用到 `@actions/expressions`）
- 型別檢查（`scripts/` 下全部的 `.ts`，加上 `deploy/chart/tests/` 下全部的 `.ts`，後者含要有叢集才能跑的入口 e2e）：`bun run --cwd scripts typecheck`
- 測試：`bun run --cwd scripts test`，與上方的 `bun test scripts` 跑同一批測試檔；單一測試：`bun run --cwd scripts test -t "<test-name-filter>"`

CI 的 `workflow-policy` job 每次都跑型別檢查與全部測試。本地開發環境的端對端測試（`TRUSTIOT_DEV_E2E=1`）要以 podman 起整套環境並編譯服務，不在 CI 執行。
