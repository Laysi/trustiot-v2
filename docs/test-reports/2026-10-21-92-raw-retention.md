# 原始資料保存期限、TTL 與磁碟水位告警（2026-10-21）

對應 issue：Laysi/trustiot-v2#92（清單 I2）。值與理由見[資料保存期限與磁碟水位](../architecture/data-retention.md)。

## 環境

- 建置主機（x86_64 Linux，rootless podman），經共用的檢查佇列執行，`CI=1`（需要容器的測試沒有容器時失敗而不略過）。
- ClickHouse：`docker.io/clickhouse/clickhouse-server:26.8`，digest 與 `deploy/chart/values.yaml` 相同（伺服器回報 26.8.22.13），每個測試起一個拋棄式容器。
- Postgres：`tools/migrate` 測試自己起的拋棄式容器。
- 分支 `feat/92-raw-retention`（從 `main` 的 cc185b9 分出）。

## 指令

在 repo 根目錄：

```sh
# 保存期限：Job 設定 TTL、舊時間戳但剛收到的資料、變更順序、拒絕過短的設定、手動改過的 TTL
cargo test --locked -p migrate --test clickhouse -- --nocapture
# 設定檢查（含以環境變數載入）與保存期限的單元測試
cargo test --locked -p migrate --lib config
cargo test --locked -p ingest --lib
# 磁碟水位告警：256 MiB tmpfs 的 ClickHouse 填到 85 % 與 95 %
cargo test --locked -p ingest --test disk_watermark -- --nocapture
```

## 結果

原始輸出：[migrate-clickhouse.txt](data/2026-10-21-92-raw-retention/migrate-clickhouse.txt)、[ingest-disk-watermark.txt](data/2026-10-21-92-raw-retention/ingest-disk-watermark.txt)。

### 保存期限（`tools/migrate/tests/clickhouse.rs`）

| 步驟 | 結果 |
| --- | --- |
| 全新 ClickHouse 的第一次執行 | 套用 `0001`–`0007` 後把 `telemetry`、`batch_commits` 的 TTL 從建表的 400 天改成 180 天（先 `telemetry`、後完成表）；再跑一次沒有變更 |
| 插入三筆讀值與三筆完成紀錄後強制 merge（`OPTIMIZE ... FINAL`） | 留下「時間戳三年前、剛收到」與「179 天前收到」的讀值，181 天前收到的被清掉；完成紀錄留下現在與 179 天前的（seq 1、2），181 天前的（seq 3）被清掉 |
| 設定 365 天 | 先改 `batch_commits`、再改 `telemetry` |
| 回到 180 天 | 先改 `telemetry`、再改 `batch_commits`；再跑一次沒有變更 |
| 設定 30 天、7 天 | 被拒，訊息寫出 `clickhouse.raw_retention_days` 與緩衝的 30 天；ClickHouse 的帳號、角色與表沒有任何改變 |
| 手動把 `telemetry` 的 TTL 改成 `toDateTime(ts) + toIntervalDay(180)` | Job 停止，訊息寫出表名與 Job 管理的形式；改回 `received_at` 的 200 天後，下一次執行只把它改成 180 天 |

單元測試：設定不設時是 180 天；31、365、3650 天接受；0、1、30、3651 天拒絕；以環境變數 `TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS` 載入時 30、7、0 天是設定錯誤（結束碼 2 的路徑），-1 無法解析；TTL 解析只接受 `toDateTime(<欄位>) + toIntervalDay(<天數>)`（含 ClickHouse 26.8 存參數時的 `_CAST(<天數>, 'UInt32')`），其他欄位、其他單位、多段 TTL、負數與超過 u32 的天數都拒絕。

### 磁碟水位告警（`services/ingest/tests/disk_watermark.rs`）

| 步驟 | used_ratio | severity |
| --- | --- | --- |
| 空的資料目錄 | 0.001 | ok |
| 填到 85 % | 0.85 | warning |
| 填到 95 % | 0.95 | critical |
| 刪掉填充檔 | 0.001 | ok |

監控角色 `trustiot_monitoring` 的權限從 `system.grants` 讀回只有 `SELECT system.disks`，沒有被授與其他角色；以它的帳號讀 `telemetry`、`batch_commits`、`tenant_telemetry`、`otel_logs`，寫 `otel_logs`、改 `telemetry` 的 TTL、建帳號都得到 `ACCESS_DENIED`；沒有這個角色的帳號執行規則得到 `ACCESS_DENIED`。

### 其他檢查

`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo test --workspace --locked`、`cargo run --locked -p openapi -- --check`、`cargo build --locked --target aarch64-unknown-linux-gnu -p protocol`、`cargo deny --locked check`、`bun test scripts/ci/changes.test.ts`、`bun test deploy/chart`：2026-10-21 16:09 在建置主機全部通過（`cargo test --workspace` 113 個 test binary 都是 `ok`；`cargo deny` advisories、bans、licenses、sources 都 ok；`changes.test.ts` 255 pass；`deploy/chart` 460 pass）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 保存期限與理由寫進 docs/ | 通過：[資料保存期限與磁碟水位](../architecture/data-retention.md)，清單 B3、G1、I2 列 |
| 帶舊時間戳但剛收到的資料不會被 TTL 清掉（測試） | 通過：時間戳三年前、剛收到的讀值在 merge 後仍在，收到超過 180 天的被清掉 |
| 保存期限設定短於緩衝最長保存時間時被拒絕（測試） | 通過：30 天與 7 天被拒，什麼都沒改；單元測試涵蓋 0 到 30 天 |
| 磁碟水位超過門檻時告警指標觸發（測試） | 通過：85 % 時 warning、95 % 時 critical，釋放後回到 ok |
| 清單 B3 列與 docs/product/plans.md 對最高方案可查期間的寫法一致 | 通過：兩處都寫 Enterprise 可查到全部保存期限（180 天），其他方案較短 |

待測（進階驗證，另開 issue）：大表改 TTL 時 `MATERIALIZE TTL` 的時間、I/O 與暫時空間；Grafana 實際評估規則並通知（Laysi/trustiot-v2#183）；在叢集內 ClickHouse 的 PVC 上量測水位（本測試用 tmpfs）。
