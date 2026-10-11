# 本機開發環境的 Postgres 改用 TLS 驗收報告

對應 issue：Laysi/trustiot-v2#273。原始資料：[data/2026-10-11-273-dev-postgres-tls/](data/2026-10-11-273-dev-postgres-tls/)。

## 環境

- 機器：本機工作站（Fedora，x86_64，與其他 agent 共用）。沒有使用 lab 的 Mac mini M1，也沒有執行任何 Kubernetes。
- 工具：podman 6.1.3（rootless）、Bun 1.4.2、Rust 1.99.0（皆為 `.prototools` 鎖定的版本）、openssl（系統內建，只用來交叉驗證憑證，開發環境本身不需要它）。
- 映像：與 Laysi/trustiot-v2#71 相同，皆以 digest 鎖定；Postgres 為 `apache/age:release_PG18_1.8.0`（PostgreSQL 18、AGE 1.8.0）。
- 測試對象：分支 `feat/273-dev-postgres-tls`，在 `main` 的 `e8f180b` 之上。
- 單元測試與型別檢查另在建置主機 `ruisi-wsl` 以 `rcheck.sh` 跑過；podman 端對端測試只能在本機跑（建置主機沒有 podman）。

## 指令

```sh
bun run --cwd scripts typecheck
bun test scripts                                        # 含 tls.test.ts、env.test.ts、policy.test.ts
CARGO_BUILD_JOBS=2 TRUSTIOT_DEV_E2E=1 bun test scripts/tests/e2e.test.ts

# 手動走一遍，用自己的實例與狀態目錄（不動平常的環境）
export TRUSTIOT_DEV_INSTANCE=ev273 XDG_STATE_HOME=<scratch>/state
export TRUSTIOT_DEV_POSTGRES_PORT=25432 TRUSTIOT_DEV_CLICKHOUSE_PORT=28123 TRUSTIOT_DEV_MQTT_PORT=21883 TRUSTIOT_DEV_PORT_EXAMPLE_SERVICE=28080
bun scripts/dev.ts up
bun scripts/dev.ts env                                  # 密碼已遮蔽後存檔
openssl s_client -connect 127.0.0.1:25432 -starttls postgres -CAfile <ca.crt> -verify_return_error -verify_hostname <名稱>
bun scripts/dev.ts down
bun scripts/dev.ts purge
```

## 結果

| 項目 | 結果 | 原始資料 |
| --- | --- | --- |
| `up` | exit 0，`every check passed`；新增的 TLS 檢查都是 `ok`：沒有 TLS 的連線被拒、TLS 加驗證後要求 SCRAM、另一個 CA 被拒（`UNABLE_TO_VERIFY_LEAF_SIGNATURE`）、憑證沒有的主機名稱被拒（`ERR_TLS_CERT_ALTNAME_INVALID`）、容器內 `psql` 以 `verify-full` 登入並讀回 `pg_stat_ssl`（`tls true`） | [01-up.txt](data/2026-10-11-273-dev-postgres-tls/01-up.txt) |
| 範例服務就緒 | `GET /readyz` 回 200 `{"status":"ready"}`。範例服務有資料庫角色時會註冊 `postgres` 檢查（`SELECT 1`，先過 service-kit 的連線檢查），所以就緒代表它（rustls）以 `verify-full` 連上了 | [01-up.txt](data/2026-10-11-273-dev-postgres-tls/01-up.txt)、[02](data/2026-10-11-273-dev-postgres-tls/02-running-evidence.txt) |
| `env` | 含 `TRUSTIOT_POSTGRES_{HOST,PORT,DATABASE,SSLMODE,CA_FILE}`（`SSLMODE=verify-full`）、`PGSSLMODE`、`PGSSLROOTCERT`、服務角色的帳號與密碼變數；密碼不在存檔內 | [02](data/2026-10-11-273-dev-postgres-tls/02-running-evidence.txt) |
| openssl 對發佈的埠驗證 | 對的 CA 與名稱 `postgres`：`Verification: OK`（TLSv1.3）；名稱 `evil.example`：`hostname mismatch`；不給 CA：`unable to get local issuer certificate` | [02](data/2026-10-11-273-dev-postgres-tls/02-running-evidence.txt) |
| 檔案位置與權限 | state 目錄 0700，裡面只有 `state.json`（0600）與 `tls/ca.crt`；目錄內沒有任何 `PRIVATE KEY`。伺服器私鑰只在磁碟區 `postgres-config`，0600、擁有者是容器裡的 postgres（999） | [02](data/2026-10-11-273-dev-postgres-tls/02-running-evidence.txt) |
| `down` | 容器移除；`postgres-config` 與 `postgres-ca` 磁碟區（含私鑰）一併移除，資料磁碟區保留；state 目錄（含 `tls/`）刪除 | [03](data/2026-10-11-273-dev-postgres-tls/03-down-purge.txt) |
| `purge` | 磁碟區、映像、state 目錄全部刪除，沒有剩下任何檔案 | [03](data/2026-10-11-273-dev-postgres-tls/03-down-purge.txt) |
| podman 端對端測試 | 1 通過、0 失敗，在合併 `origin/main` 前後各跑一次（65 秒，編譯快取已暖；合併後 479 秒，含重新編譯）。包含：同一個服務映像在容器網路上以對的 CA 與名稱啟動就緒，換成另一個 CA 或憑證沒有的主機名稱則維持 `/healthz` 200、`/readyz` 503；`down` 後憑證與私鑰磁碟區消失；第二次 `up` 的 CA 與密碼都是新的；`purge` 後 `tls/` 與 state 目錄不存在；工作目錄的 `git status --ignored` 前後相同 | [04-e2e.txt](data/2026-10-11-273-dev-postgres-tls/04-e2e.txt) |
| `bun test scripts/tests` | 94 通過、1 略過（e2e，要 `TRUSTIOT_DEV_E2E=1`）、0 失敗（合併 `origin/main` 後整個 `bun test scripts`：276 通過、1 略過、0 失敗；建置主機 `ruisi-wsl` 上合併前 185 通過、0 失敗）。新增：憑證結構（node 的 `X509Certificate` 與 openssl 兩種解析）、用假的 Postgres 驗證 TLS 檢查（對的 CA 與每個名稱通過、另一個 CA 被拒、憑證沒有的名稱被拒、連不上不算「被拒」）、`env` 輸出、不得提交金鑰的規則 | [05](data/2026-10-11-273-dev-postgres-tls/05-bun-test-scripts-tests.txt) |
| 型別檢查 | `bun run --cwd scripts typecheck` 通過 | [06](data/2026-10-11-273-dev-postgres-tls/06-typecheck.txt) |

第一次端對端執行在冷的 cargo 編譯下超過 10 分鐘的測試上限而逾時（機器上同時有其他 agent 在編譯），不是測試失敗；之後先預編再跑，並把上限放寬到 20 分鐘。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| `up` 後，範例服務以 `verify-full` 連上本機 Postgres 並通過 readiness（podman e2e 擴充） | 通過 |
| 用錯的 CA 或錯的主機名稱連線會失敗（測試） | 通過：單元測試（假的 Postgres，真的 TLS 握手）、`up`／`status` 的檢查、e2e（rustls 的服務映像）、openssl 手動驗證 |
| 私鑰與憑證不進 repo（gitleaks 通過）；`purge` 後相關檔案都被刪除（測試） | 通過（gitleaks 結果見 PR） |
| `bun test scripts` 與 `bun run --cwd scripts typecheck` 通過；README 已更新 | 通過 |
