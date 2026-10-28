# 資料保存期限與磁碟水位

原始遙測資料在 ClickHouse 存多久、閘道器的緩衝最多存多久、兩者的關係，以及 ClickHouse 磁碟快滿時的告警（清單 I2、G1、B3；Laysi/trustiot-v2#92，2026-10-21 訂定）；1 分鐘與 1 小時彙總表存多久（Laysi/trustiot-v2#141，2026-10-25 訂定，見下方「彙總資料的保存期限」）；租戶看得到多久以前的資料，以及由誰強制（Laysi/trustiot-v2#143，2026-10-26 訂定，見下方「可查期間」）；AI 助理刪除的對話留多久（Laysi/trustiot-v2#644，見下方「AI 助理刪除的對話」）。地端以 values 自訂保存期限由 Laysi/trustiot-v2#446 接上。

## 訂定的值

| 項目 | 值 | 在哪裡 |
|---|---|---|
| 閘道器緩衝的最長保存時間 | 30 天 | `crates/protocol` 的 `telemetry::BUFFER_MAX_AGE_DAYS` |
| 原始資料的保存期限（全平台統一，`trustiot.telemetry`） | 180 天 | `services/ingest/src/retention.rs` 的 `RAW_RETENTION_DAYS` |
| 批次完成表的保存期限（`trustiot.stream_commits`，以閘道器 + 串流為鍵，Laysi/trustiot-v2#639） | 與原始資料相同（180 天） | 同上；migration Job 讓兩張表一致 |
| 保存期限可設定的範圍 | 33 到 3650 天（2026-10-26 負責人決定，原本是 31 天起） | `RawRetention::from_days`，下限是 `MIN_RAW_RETENTION_DAYS` |
| I1 對帳往回看的收到日 | 31 天（緩衝加一天，2026-10-26 負責人決定） | `services/ingest/src/retention.rs` 的 `RECONCILE_WINDOW_DAYS` |
| 1 分鐘彙總（`trustiot.telemetry_1m`） | 13 個月 | `services/ingest/src/retention.rs` 的 `AGGREGATE_1M_RETENTION_MONTHS` |
| 1 小時彙總（`trustiot.telemetry_1h`） | 48 個月（4 年） | 同上，`AGGREGATE_1H_RETENTION_MONTHS` |
| 彙總可設定的範圍 | 1 分鐘 13 到 120 個月；1 小時不短於 1 分鐘、不超過 120 個月；1 分鐘的月數至少涵蓋原始資料的天數 | `AggregateRetention::new` |

保存期限一律以伺服器收到資料的時間計算：`telemetry` 用 `received_at`，完成表用 `committed_at`，不用資料本身的時間戳。時鐘未校正的閘道器送來的舊時間戳，不會讓剛收到的資料或高水位提早被清掉（[Laysi/trustiot#180](https://github.com/Laysi/trustiot/issues/180)）；`tools/migrate/tests/clickhouse.rs` 以帶三年前時間戳、剛收到的資料驗證。

## 限制與理由

1. **保存期限必須長於緩衝的最長保存時間。** 閘道器送出一批、還沒收到確認就斷線時，這一批留在緩衝裡，恢復後重送。ingest 靠完成表的高水位判斷這批是否已經寫過（清單 I1）；完成紀錄若在重送之前就過期，重送的批次會被當成新資料再寫一次，資料重複。緩衝裡的批次最多 30 天，所以完成紀錄至少要留超過 30 天。
2. **保存期限至少比 I1 對帳的窗口長兩天（最小 33 天，2026-10-26 負責人決定）。** I1 重寫一批之後，ingest 以原始列對帳彙總表，只往回看 31 個收到日（[services/ingest](../../services/ingest/README.md)「I1 重寫與對帳」）。窗口比緩衝多一天：第一次在某天 23:59 收到的一批，可能在它第 30 天結束的午夜之後才重寫（重送與重寫要花時間，或閘道器的時鐘有偏差），只看 30 天會漏掉第一次嘗試的那個收到日，那些格就一直多算。原始資料再比窗口多留兩天，對帳比較的原始列才一定還在，碰不到在午夜前後過期的列，也碰不到時鐘走得比較快的 replica 上的 TTL merge。設定值小於 33 天時，migration Job 在連任何資料庫之前就拒絕（結束碼 2，訊息寫出設定的 key、30 天的緩衝、31 天的窗口與 33 天的下限）。
3. **完成表不短於原始資料。** `committed_at` 不早於同一批的 `received_at`，兩張表天數相同時，完成紀錄一定比它的資料晚過期。
4. **最高方案看得到全部保存期限，其他方案較短。** [方案與權益](../product/plans.md)：原始資料的可查期間 Starter 例 7 天、Business 例 90 天、Enterprise 平台保存期全開。資料都在，方案只限制可查期間，升級後立即看得到更早的資料（清單 B3）。保存期限因此要比 Business 長，Enterprise 才有看得到的差別。
5. **取滿足限制的最小值。** 原始資料只拿來看近期細節與重算；長期趨勢與年增率看彙總資料（至少 13 個月，Laysi/trustiot-v2#141），不需要原始資料存一年以上。180 天是 Business 範例的兩倍、涵蓋半年的比較，也遠大於 30 天的緩衝。之前 schema 裡的 400 天是暫定值，換成 180 天後儲存量約少 55 %。
6. **緩衝 30 天。** 現場斷網多半是幾小時到幾天，長假（例如農曆年）約 9 到 10 天；30 天涵蓋整整一個月的停機，又遠短於保存期限。超過的批次丟棄並在 heartbeat 的 `buffer` 計數（[`crates/protocol` README](../../crates/protocol/README.md)），緩衝的時間由 Laysi/trustiot-v2#90 實作，校時造成的時間跳動不能讓批次留得更久。

儲存量估算：2026-10-09 的實測每筆約 3.1 bytes（[可行性測試結果](../feasibility/2026-10-09-cloud-poc.md)，30 天 52 億筆 15.2 GiB）。同樣的負載存 180 天約 91 GiB，加上 merge 與系統表的 1.5 倍約 137 GiB；叢集內 ClickHouse 的磁碟大小照 `deploy/chart/values.yaml` 的 `clickhouse.bundled.storage.size` 說明估算。

## 設定與變更

- 設定：migration Job 的 `TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS`，不設就是 180 天。chart 目前不設它（SaaS 與展示環境都用平台的值）；地端的 values key 由 Laysi/trustiot-v2#446 加入，同樣經過上面的檢查。
- Job 每次執行都把兩張表的 TTL 對齊設定（先比對、不同才改），不另寫 migration 檔：天數是設定值，不同部署可以不同。`0002_telemetry.sql` 建表時的 400 天不改（已套用的檔案不能改），新安裝建表後同一次執行就改成設定的天數。完成表 `stream_commits` 由 `0012_gateway_stream_ids.sql` 以 3650 天（可設定的上限）建立，在 Job 對齊之前也不短於 `telemetry`，同一次執行就改成設定的天數。`0003_batch_commits.sql` 建的舊完成表從 `0012` 起停用（ingest 沒有它的權限，見 [services/ingest](../../services/ingest/README.md#批次完成表-trustiotstream_commits)），Job 不再管它的 TTL，維持停用當時的天數。
- 變更順序讓完成表任何時刻都不短於原始資料：延長時先改完成表、縮短時先改 `telemetry`。中途失敗時下一次執行接著完成。
- 表的 TTL 不是 Job 管理的形式（例如有人手動加了條件或改了欄位）時，Job 停止並寫出表名，不覆蓋；處理方式在 [migration Job runbook](../runbooks/migrations.md) 的「原始資料的保存期限」。
- 改 TTL 時 ClickHouse 在背景執行 `MATERIALIZE TTL`，重寫每個 part 的 TTL 資訊並刪掉過期的列，會占用 I/O 與暫時的磁碟空間；縮短保存期限不會立刻釋放空間。大的表改值前先看磁碟水位。
- 縮短保存期限會永久刪掉超過新天數的資料，是產品決定，不是維運手段：SaaS 的天數是平台承諾（[方案與權益](../product/plans.md)），由產品負責人決定；地端由客戶的管理者決定。不要拿縮短保存期限解決磁碟壓力（見下方「磁碟水位告警」）。
- Job 管理的 TTL 只經設定改。手動以 `ALTER TABLE ... MODIFY TTL` 改天數（例如為了保全資料而延長），下一次部署時 Job 會改回設定的天數；要保留超過保存期限的資料，先把資料匯出。
- 目前 Job 不擋縮短：設定沒有給或是空值時就用平台的 180 天，比現在的天數短也照改。地端可以設定之前，Laysi/trustiot-v2#446 要先讓縮短需要明確確認（例如設定要寫出目前的天數），chart 在 values 是空值時失敗、不退回平台值（2026-10-21 安全審查）。

### 既有部署怎麼轉移

既有部署（展示環境、nightly 都是全新部署，不受影響）升級到這一版時，migration Job 的下一次執行把兩張表的 TTL 從 400 天改成 180 天，並套用 `0007_monitoring.sql`。2026-10-21 時沒有任何部署的資料超過 180 天，所以不會刪到資料；背景的 `MATERIALIZE TTL` 照樣重寫 part（見上一節），只是沒有過期的列要刪。不需要手動步驟；要回到舊版時，舊版的 Job 不管 TTL，表維持 180 天，要 400 天就以新版設定 `TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS=400` 再跑一次。

## 彙總資料的保存期限

1 分鐘與 1 小時彙總表（`trustiot.telemetry_1m`、`trustiot.telemetry_1h`，[services/ingest](../../services/ingest/README.md)「彙總表」；2026-10-25 訂定）。值在上方「訂定的值」。

- **以月為單位。** 月增率、年增率與月報都以日曆月計算，13 個月才涵蓋去年同一個月的整個月。
- **以收到日計算。** TTL 是 `received_day + toIntervalMonth(<月數>) + toIntervalDay(1)`，`received_day` 是伺服器收到資料的日期（UTC）：一格要等它的收到日整天都超過保存期限才過期，比它最新一列的「收到時間加上月數」最多晚一天，不會提早。和原始資料一樣不用讀值時間，時鐘錯的閘道器不會讓彙總提早過期。月份加法碰到月底時取那個月的最後一天（1 月 31 日加 1 個月是 2 月的最後一天）。
- **1 分鐘至少 13 個月（清單 I2）。** 年增率要比到去年同一個月。bucket 以 UTC 切，有半小時或 15 分鐘時差的時區（例如 +5:30、+6:30、+5:45）只能從 1 分鐘表得到本地的整日，所以 1 分鐘表的長度就是這些時區能精確算日、月數字的範圍；整點時差的時區（台灣 UTC+8）從 1 小時表就能精確算。
- **1 小時 4 年。** 長期趨勢、跨年的年增率與碳排月報看 1 小時表。[方案](../product/plans.md)裡 Business 例的 3 年彙總可查期間由它涵蓋；和原始資料一樣，保存期限比 Business 長，看得到全部保存期限的 Enterprise 才有看得到的差別（B3）。各方案在每張表的可查期間由 Laysi/trustiot-v2#453 訂，不能超過這裡的值。
- **粗的不短於細的。** 1 小時不短於 1 分鐘，1 分鐘不短於原始資料：從粗到細查詢一定接得上，較細的資料不會比較粗的留得久。比較月數與天數時，月數取它至少涵蓋的天數（`min_days_in_months`：每 12 個月 365 天，其餘的月取最短的連續日曆月，也就是從 2 月開始），這個下限與 ClickHouse 26.8 的月份加法比對過 2000 到 2299 年的每個起始日。
- **上限 120 個月（十年）。** 與原始資料 3650 天的上限相同；更大的數字幾乎一定是單位寫錯（把天數寫成月數）。
- 儲存量還沒有實測。每格一列，鍵含串流與收到日，所以 1 分鐘表每個資料點每分鐘至少一列；取樣間隔比 1 分鐘短時比原始資料小，1 分鐘一筆時列數與原始資料相當、每列較大。實測後補上估算。

設定與變更：

- 設定：migration Job 的 `TRUSTIOT__CLICKHOUSE__AGGREGATE_1M_RETENTION_MONTHS` 與 `TRUSTIOT__CLICKHOUSE__AGGREGATE_1H_RETENTION_MONTHS`，不設就是 13 與 48 個月。不符合上面的規則時，Job 在連任何資料庫之前以結束碼 2 停止，訊息寫出設定的 key（`clickhouse.aggregate_1m_retention_months` 或 `clickhouse.aggregate_1h_retention_months`）與理由。原始資料設到 3650 天時，1 分鐘彙總也要設到 120 個月。chart 目前不設；地端的 values key 由 Laysi/trustiot-v2#446 一併加入，縮短同樣要明確確認。
- Job 每次執行都把四張表的 TTL 對齊設定（先比對、不同才改），不另寫 migration 檔。它先讀完四張表的 TTL，有一張不是 Job 管理的形式就在改任何一張之前停止（處理方式在 [migration Job runbook](../runbooks/migrations.md) 的「原始資料的保存期限」）。
- 變更順序讓每張表任何時刻都不短於下一張（1 小時、1 分鐘、完成表、原始資料）：先延長要變長的表，從留最久的開始；再縮短要變短的表，從留最短的開始。中途失敗時下一次執行接著完成。
- 改 TTL 的代價、縮短是產品決定、手動改的月數會被改回設定值：與原始資料相同（上方「設定與變更」）。縮短彙總的保存期限會永久刪掉原始資料早已不在的年份，無法重算。

既有部署升級到這一版時，migration Job 建立兩張彙總表與它們的 view，之後（TTL 對齊設定之後）把 `telemetry` 現有的列補進彙總表，只補彙總缺的格（[services/ingest](../../services/ingest/README.md)「既有資料的 backfill」）。原始資料只有 180 天，backfill 又只補原始列一定都還在的收到日（平台值下是最近 178 個收到日，含今天），所以補出的彙總只從那時開始；之前已過期的資料補不回來。

## 可查期間

保存期限是資料在 ClickHouse 留多久；可查期間是租戶看得到多久以前的資料，依方案而定（[方案與權益](../product/plans.md)，清單 B3、J3），不能長於保存期限。2026-10-26 負責人決定由資料庫強制（Laysi/trustiot-v2#143）：

- **由 ClickHouse 強制。** 每個租戶帳號帶三個 `CONST` 設定，一張表一個時間窗：`SQL_queryable_raw_days`（原始資料，天）、`SQL_queryable_1m_months` 與 `SQL_queryable_1h_months`（彙總，日曆月）。`0011_queryable_period.sql` 在租戶過濾的同一批 row policy 加上 `ts >= now64() - <天數>`、`bucket >= now64() - <月數>`，所以歷史 API、語意工具與 AI 的 SQL 讀到的都是同一個範圍，不論查詢寫了什麼條件；帳號是 `readonly`、設定是 `CONST`，帳號自己改不了。規則、部署與回復見 [services/ingest](../../services/ingest/README.md)「可查期間」；以租戶帳號直接驗證的繞過清單在 `services/telemetry-query/tests/queryable_period.rs`。
- **方案變更從下一個查詢起生效。** telemetry-query 建帳號時寫入時間窗，每個查詢前讀回，不同就以 `ALTER USER ... MODIFY SETTINGS` 改成方案的值（[services/telemetry-query](../../services/telemetry-query/README.md)「可查期間」）。升級後下一個查詢就看得到更早的資料，降級後立即看不到；資料本身不動。目前所有租戶固定為 Starter 例（原始資料 7 天、彙總 3 個月），依方案的值由 Laysi/trustiot-v2#453 接上，每張表的值不能超過該表的保存期限。
- **以讀值時間計算，與保存期限不同。** 保存期限用收到時間，可查期間用讀值時間（`ts`，彙總格的 `bucket`）：客戶問的是「哪一段時間的資料」。所以閘道器斷線後補送的舊讀值照樣保存，但讀值時間早於原始資料的可查期間時，只看得到它的彙總（在彙總的可查期間內時）。
- **缺設定或值錯誤時讀不到。** 帳號沒有時間窗設定時每次讀取都報錯，值是負數、太大或不是數字時也一樣，不會變成看得到全部；telemetry-query 在下一個查詢前把值補回方案的值。

## 磁碟水位告警

ClickHouse 的磁碟滿了就停擺：寫入失敗，套用保存期限的 merge 與 mutation 也需要空間重寫 part，連刪資料都做不到。

- 指標：每個本機磁碟的使用比例 `used_ratio = 1 - free_space / total_space`，從 `system.disks` 讀（ClickHouse 查詢時直接讀檔案系統）。規則在 [`services/ingest/alerts/clickhouse_disk_watermark.sql`](../../services/ingest/alerts/clickhouse_disk_watermark.sql)，每個磁碟一列，帶 `severity`。
- 門檻：0.80 起 `warning`、0.90 起 `critical`；持續 5 分鐘才通知，避免 merge 的暫時 part 造成誤報。80 % 時還有時間擴充磁碟；90 % 時大的 merge 已經會因空間不足而延後，part 開始累積。
- 權限：角色 `trustiot_monitoring`（`0007_monitoring.sql`）只有 `system.disks` 的 `SELECT`，讀不到任何 TrustIoT 資料庫或其他系統表。評估告警的帳號（Grafana）拿到這個角色，由 Laysi/trustiot-v2#183 建立帳號並在 Grafana 設定規則。
- 測試：`services/ingest/tests/disk_watermark.rs` 把真的 ClickHouse 的資料目錄放在 256 MiB 的 tmpfs，填到 85 % 與 95 %，確認依序觸發 `warning`、`critical`，釋放後回到 `ok`；監控角色讀不到其他東西，沒有角色的帳號讀不到 `system.disks`。
- 告警查詢失敗時（ClickHouse 停擺或過載、角色不見、`system.disks` 被拒）規則不回傳任何列。Laysi/trustiot-v2#183 在 Grafana 設定這條規則時，NoData 與 Error 要設成告警，不能是 OK，否則告警正好在它要防的停擺期間沉默。
- 觸發時：先確認是資料量成長還是別的東西（日誌表、系統表、卡住的 mutation），清掉不需要的部分。例行的處理只有擴充磁碟（StorageClass 支援 `allowVolumeExpansion` 時改 PVC 大小，外部 ClickHouse 照供應商的方式）。不要縮短保存期限來釋放空間：那會永久刪掉每個租戶的原始資料，SaaS 上也違反對方案的承諾（見上方「設定與變更」）；而且 `MATERIALIZE TTL` 自己也需要暫時的空間，接近滿時反而更糟。

## AI 助理刪除的對話

AI 助理的對話存在 Postgres（assistant 的 schema），不在 ClickHouse。

| 項目 | 值 | 在哪裡 |
|---|---|---|
| 使用者刪除的對話可以還原的期間；期滿後連同訊息、捨棄的回合與提問紀錄清除 | 30 天（2026-10-24 產品負責人決定，Laysi/trustiot-v2#644） | `services/assistant/src/store.rs` 的 `DELETED_RETENTION_DAYS`；資料庫的同一個值寫在 `services/assistant/migrations/postgres/0002_conversation_threads.sql` 的 policy，單元測試比對兩者 |

- 30 天從刪除的時間起算，用資料庫時鐘的 `deleted_at`；服務角色不能指定或往前改這個時間。
- 刪除後只有擁有者的「已刪除」清單看得到它，其他地方（含擁有者）都當它不存在。期滿後服務角色不能更改或還原它（`expired_stay_deleted`），只有清除用的平台角色 `assistant_platform` 刪得掉它；平台角色不論交易設了什麼租戶，都只看得到、刪得掉刪除滿 30 天的對話（restrictive policy `platform_expired_only`）。
- 清除由排程執行（Laysi/trustiot-v2#645，預設每小時一輪，`services/assistant` 的 `purge.interval`），所以實際刪除最多晚於滿 30 天的時間點一個間隔，加上那一輪本身的時間（所有副本都停著時更晚，重新啟動後的第一輪補上）。另一個工作階段一直鎖住的過期對話會被那一輪略過（日誌的 WARN 有 `skipped` 個數），留到發現它沒被鎖的那一輪；其他過期的對話不受影響。在清除之前，資料仍在資料庫與備份裡；清除之後，資料仍留在清除前做的備份裡，直到那些備份的保存期限屆滿。
- 沒有刪除的對話（含它捨棄的回合）保存多久由 Laysi/trustiot-v2#652 決定；在那之前一直保存。
- 細節見 [services/assistant](../../services/assistant/README.md) 的「對話的儲存」。
