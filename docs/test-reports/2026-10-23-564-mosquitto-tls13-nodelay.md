# Mosquitto chart：閘道器 listener 的 ChaCha20 優先與 `set_tcp_nodelay`（2026-10-23）

對應 issue：Laysi/trustiot-v2#564（決策 6）。

## 環境

- 機器：開發者工作站（x86_64 Linux），podman 6.1.4，OpenSSL 4.0.3（TLS 用戶端），helm 4.3.0、bun 1.4.2（`.prototools` 釘選）。
- broker：chart 釘選的映像 `docker.io/library/eclipse-mosquitto:2.1.2-alpine@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408`（Mosquitto 2.1.2），以 amd64 執行。
- 程式碼：分支 `fix/564-mosquitto-tls13-nodelay`，實測時基於 main `34f777e9`，之後 rebase 到 main `1cd3da21`；兩者之間 broker 的兩份設定（`files/mosquitto/mosquitto.conf`、`templates/_mqtt-broker.tpl`）沒有變動。「修正前」用 main `34f777e9` 的 `deploy/chart`。
- 設定：以 SaaS 形態（`values-saas.yaml` 加 `tests/values/saas-instance.yaml`）、`mqttBroker.enabled=true` 渲染 chart，取 ConfigMap `trustiot-mqtt-broker-config` 的 `mosquitto.conf`，原封不動給容器使用。

## 指令

1. chart 規則測試（修正前、修正後各一次；修正前只把兩份 broker 設定還原成 main 的版本，測試保留）：

   ```sh
   bun test deploy/chart -t "Laysi/trustiot-v2#564"
   bun test deploy/chart
   ```

2. TLS 實測：[`render.ts.txt`](data/2026-10-23-564-mosquitto-tls13-nodelay/render.ts.txt) 渲染 chart 並取出 `mosquitto.conf`；[`tls-check.sh.txt`](data/2026-10-23-564-mosquitto-tls13-nodelay/tls-check.sh.txt) 在執行時產生一次性的 CA、伺服器憑證、閘道器用戶端憑證（EKU clientAuth）與空的 CRL，放在 repo 以外的暫存目錄，像 chart 的 pod 一樣以 UID/GID 1883、唯讀根目錄、唯讀掛載設定檔與憑證目錄啟動一個 broker 容器，再以 OpenSSL 用 TLS 1.3 帶用戶端憑證連 8883 兩次：一次 ChaCha20 排第一（閘道器的順序），一次 AES 排第一。結束時 `podman rm --force --volumes` 移除容器並刪除憑證。

   ```sh
   bun render.ts "$(proto bin helm)" deploy/chart fixed.conf
   bash tls-check.sh fixed.conf fixed TLS_CHACHA20_POLY1305_SHA256   # 寫出 tls-fixed.log
   # 修正前：chart 目錄改用 main 34f777e9 的 deploy/chart（git archive 匯出）
   bash tls-check.sh main.conf main TLS_AES_256_GCM_SHA384
   ```

   腳本只在每次握手都完成、是 TLSv1.3、伺服器憑證驗證 OK、且套件等於第三個參數時以 0 結束，否則印出 OpenSSL 的完整輸出並以非 0 結束。自我檢查：以修正後的設定、預期 `TLS_AES_256_GCM_SHA384` 執行，兩次握手都判為 FAIL，結束碼 1。

   每次握手的指令：

   ```sh
   openssl s_client -connect 127.0.0.1:18883 -tls1_3 -ciphersuites <順序> \
     -CAfile ca.pem -cert client.pem -key client.key -servername localhost -verify_return_error -brief
   ```

## 結果

### chart 規則測試

| 狀態 | 結果 |
| --- | --- |
| 修正前（main 的兩份設定） | 失敗：`listener 8883` 區段的 `ciphers_tls1.3` 是空的（[`chart-test-before-fix.txt`](data/2026-10-23-564-mosquitto-tls13-nodelay/chart-test-before-fix.txt)） |
| 修正後 | 通過（[`chart-test-after-fix.txt`](data/2026-10-23-564-mosquitto-tls13-nodelay/chart-test-after-fix.txt)） |

測試把渲染出的設定依 `listener` 分段：全域段要有且只有 `set_tcp_nodelay true`，任何 listener 段都不能有；`listener 8883` 要有 `tls_version tlsv1.3` 與 `ciphers_tls1.3 TLS_CHACHA20_POLY1305_SHA256:TLS_AES_256_GCM_SHA384:TLS_AES_128_GCM_SHA256`；`listener 1883` 不能有任何 TLS 設定。既有的測試另外檢查 ConfigMap 的內容等於 `files/mosquitto/mosquitto.conf` 加上 `memory_limit`，所以 template 與 `files/` 兩份一致。

### TLS 協商（閘道器的 listener 8883）

| 設定 | 用戶端順序 | 協商結果 |
| --- | --- | --- |
| 修正前（main） | ChaCha20、AES-256-GCM、AES-128-GCM | `TLS_AES_256_GCM_SHA384` |
| 修正前（main） | AES-256-GCM、AES-128-GCM、ChaCha20 | `TLS_AES_256_GCM_SHA384` |
| 修正後 | ChaCha20、AES-256-GCM、AES-128-GCM | `TLS_CHACHA20_POLY1305_SHA256` |
| 修正後 | AES-256-GCM、AES-128-GCM、ChaCha20 | `TLS_CHACHA20_POLY1305_SHA256` |

四次都是 TLSv1.3，憑證驗證 OK。兩種用戶端順序得到同一個結果，表示由 broker 的順序決定，與 Laysi/trustiot-v2#86 在 Mosquitto 2.1.2 上的觀察相同。修正後的 broker 日誌沒有錯誤或警告，`set_tcp_nodelay` 與 `ciphers_tls1.3` 都被接受，三個 listener 都開啟。原始日誌（用戶端位址遮蔽為 `<client>`）：[`tls-after-fix.log`](data/2026-10-23-564-mosquitto-tls13-nodelay/tls-after-fix.log)、[`tls-before-fix.log`](data/2026-10-23-564-mosquitto-tls13-nodelay/tls-before-fix.log)。

### 其他檢查

- `bun test deploy/chart`：610 個測試全部通過。第一次執行有一個與本變更無關的失敗（onprem 形態的 kubeconform 下載 schema 超過 60 秒被終止），重跑後通過。
- `CI=1 cargo test -p migrate --test mqtt_broker`：4 個測試通過。它在同一個釘選映像裡跑 `files/mosquitto/mosquitto.conf`，包括閘道器憑證的握手、服務帳號、SIGKILL 後的持久化，以及「探針後的日誌沒有錯誤、警告或 OpenSSL 訊息」，所以新增的兩行沒有讓 broker 的既有行為改變。
- `bun test scripts`：1590 通過、9 略過、0 失敗。

## 結論

- 驗收條件 1（閘道器的內層 TLS 協商出 `TLS_CHACHA20_POLY1305_SHA256`，以 chart 產生的 mosquitto.conf 對 Mosquitto 容器實測）：通過，見上表與日誌。用戶端是 OpenSSL，不是閘道器的 rustls；以 rustls 或閘道器本身再確認一次屬於第二來源佐證，待測（延後到後續 issue）。
- 驗收條件 2（chart 測試檢查兩行存在，修正前失敗、修正後通過）：通過。
- 驗收條件 3（決策 6 與 testkit 文件不再寫 chart 沒有這兩行）：main（`34f777e9`）上沒有這些敘述；它們只在 Laysi/trustiot-v2#86 的分支上（`docs/architecture/decisions.md` 決策 6、`crates/testkit/README.md`、`crates/testkit/src/mqtt/mosquitto.rs`）。兩個分支誰後合併，就在那一邊更正。
- `set_tcp_nodelay` 對 CONNACK 延遲的效果沒有重測：本次只確認 broker 接受這個設定；約 40 ms 的差異已在 `docs/feasibility/2026-10-12-tpm-poc.md` 實測過。
