# 資料保存期限與磁碟水位

原始遙測資料在 ClickHouse 存多久、閘道器的緩衝最多存多久、兩者的關係，以及 ClickHouse 磁碟快滿時的告警（清單 I2、G1、B3；Laysi/trustiot-v2#92，2026-10-21 訂定）。彙總資料（1 分鐘與 1 小時彙總表，至少 13 個月）由 Laysi/trustiot-v2#141 另訂；地端以 values 自訂保存期限由 Laysi/trustiot-v2#446 接上。

## 訂定的值

| 項目 | 值 | 在哪裡 |
|---|---|---|
| 閘道器緩衝的最長保存時間 | 30 天 | `crates/protocol` 的 `telemetry::BUFFER_MAX_AGE_DAYS` |
| 原始資料的保存期限（全平台統一，`trustiot.telemetry`） | 180 天 | `services/ingest/src/retention.rs` 的 `RAW_RETENTION_DAYS` |
| 批次完成表的保存期限（`trustiot.batch_commits`） | 與原始資料相同（180 天） | 同上；migration Job 讓兩張表一致 |
| 保存期限可設定的範圍 | 31 到 3650 天 | `RawRetention::from_days` |

保存期限一律以伺服器收到資料的時間計算：`telemetry` 用 `received_at`，完成表用 `committed_at`，不用資料本身的時間戳。時鐘未校正的閘道器送來的舊時間戳，不會讓剛收到的資料或高水位提早被清掉（[Laysi/trustiot#180](https://github.com/Laysi/trustiot/issues/180)）；`tools/migrate/tests/clickhouse.rs` 以帶三年前時間戳、剛收到的資料驗證。

## 限制與理由

1. **保存期限必須長於緩衝的最長保存時間。** 閘道器送出一批、還沒收到確認就斷線時，這一批留在緩衝裡，恢復後重送。ingest 靠完成表的高水位判斷這批是否已經寫過（清單 I1）；完成紀錄若在重送之前就過期，重送的批次會被當成新資料再寫一次，資料重複。緩衝裡的批次最多 30 天，所以完成紀錄至少要留超過 30 天。設定值不大於 30 天時，migration Job 在連任何資料庫之前就拒絕（結束碼 2，訊息寫出設定的 key 與 30 天的限制）。
2. **完成表不短於原始資料。** `committed_at` 不早於同一批的 `received_at`，兩張表天數相同時，完成紀錄一定比它的資料晚過期。
3. **最高方案看得到全部保存期限，其他方案較短。** [方案與權益](../product/plans.md)：原始資料的可查期間 Starter 例 7 天、Business 例 90 天、Enterprise 平台保存期全開。資料都在，方案只限制可查期間，升級後立即看得到更早的資料（清單 B3）。保存期限因此要比 Business 長，Enterprise 才有看得到的差別。
4. **取滿足限制的最小值。** 原始資料只拿來看近期細節與重算；長期趨勢與年增率看彙總資料（至少 13 個月，Laysi/trustiot-v2#141），不需要原始資料存一年以上。180 天是 Business 範例的兩倍、涵蓋半年的比較，也遠大於 30 天的緩衝。之前 schema 裡的 400 天是暫定值，換成 180 天後儲存量約少 55 %。
5. **緩衝 30 天。** 現場斷網多半是幾小時到幾天，長假（例如農曆年）約 9 到 10 天；30 天涵蓋整整一個月的停機，又遠短於保存期限。超過的批次丟棄並在 heartbeat 的 `buffer` 計數（[`crates/protocol` README](../../crates/protocol/README.md)），緩衝的時間由 Laysi/trustiot-v2#90 實作，校時造成的時間跳動不能讓批次留得更久。

儲存量估算：2026-10-09 的實測每筆約 3.1 bytes（[可行性測試結果](../feasibility/2026-10-09-cloud-poc.md)，30 天 52 億筆 15.2 GiB）。同樣的負載存 180 天約 91 GiB，加上 merge 與系統表的 1.5 倍約 137 GiB；叢集內 ClickHouse 的磁碟大小照 `deploy/chart/values.yaml` 的 `clickhouse.bundled.storage.size` 說明估算。

## 設定與變更

- 設定：migration Job 的 `TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS`，不設就是 180 天。chart 目前不設它（SaaS 與展示環境都用平台的值）；地端的 values key 由 Laysi/trustiot-v2#446 加入，同樣經過上面的檢查。
- Job 每次執行都把兩張表的 TTL 對齊設定（先比對、不同才改），不另寫 migration 檔：天數是設定值，不同部署可以不同。`0002_telemetry.sql`、`0003_batch_commits.sql` 建表時的 400 天不改（已套用的檔案不能改），新安裝建表後同一次執行就改成設定的天數。
- 變更順序讓完成表任何時刻都不短於原始資料：延長時先改完成表、縮短時先改 `telemetry`。中途失敗時下一次執行接著完成。
- 表的 TTL 不是 Job 管理的形式（例如有人手動加了條件或改了欄位）時，Job 停止並寫出表名，不覆蓋；處理方式在 [migration Job runbook](../runbooks/migrations.md) 的「原始資料的保存期限」。
- 改 TTL 時 ClickHouse 在背景執行 `MATERIALIZE TTL`，重寫每個 part 的 TTL 資訊並刪掉過期的列，會占用 I/O 與暫時的磁碟空間；縮短保存期限不會立刻釋放空間。大的表改值前先看磁碟水位。

### 既有部署怎麼轉移

既有部署（展示環境、nightly 都是全新部署，不受影響）升級到這一版時，migration Job 的下一次執行把兩張表的 TTL 從 400 天改成 180 天，並套用 `0007_monitoring.sql`。2026-10-21 時沒有任何部署的資料超過 180 天，所以不會刪到資料；背景的 `MATERIALIZE TTL` 只重寫 TTL 資訊。不需要手動步驟；要回到舊版時，舊版的 Job 不管 TTL，表維持 180 天，要 400 天就以新版設定 `TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS=400` 再跑一次。

## 磁碟水位告警

ClickHouse 的磁碟滿了就停擺：寫入失敗，套用保存期限的 merge 與 mutation 也需要空間重寫 part，連刪資料都做不到。

- 指標：每個本機磁碟的使用比例 `used_ratio = 1 - free_space / total_space`，從 `system.disks` 讀（ClickHouse 查詢時直接讀檔案系統）。規則在 [`services/ingest/alerts/clickhouse_disk_watermark.sql`](../../services/ingest/alerts/clickhouse_disk_watermark.sql)，每個磁碟一列，帶 `severity`。
- 門檻：0.80 起 `warning`、0.90 起 `critical`；持續 5 分鐘才通知，避免 merge 的暫時 part 造成誤報。80 % 時還有時間擴充磁碟或調整保存期限；90 % 時大的 merge 已經會因空間不足而延後，part 開始累積。
- 權限：角色 `trustiot_monitoring`（`0007_monitoring.sql`）只有 `system.disks` 的 `SELECT`，讀不到任何 TrustIoT 資料庫或其他系統表。評估告警的帳號（Grafana）拿到這個角色，由 Laysi/trustiot-v2#183 建立帳號並在 Grafana 設定規則。
- 測試：`services/ingest/tests/disk_watermark.rs` 把真的 ClickHouse 的資料目錄放在 256 MiB 的 tmpfs，填到 85 % 與 95 %，確認依序觸發 `warning`、`critical`，釋放後回到 `ok`；監控角色讀不到其他東西，沒有角色的帳號讀不到 `system.disks`。
- 觸發時：先確認是資料量成長還是別的東西（日誌表、系統表、卡住的 mutation）。擴充磁碟（StorageClass 支援 `allowVolumeExpansion` 時改 PVC 大小，外部 ClickHouse 照供應商的方式）；或縮短保存期限（不能短於 31 天，而且要等 `MATERIALIZE TTL` 跑完才釋放空間，它自己也需要空間，所以要在 critical 之前做）。
