# lab-dataset

lab 資料集的工具（功能重建清單 M7，Laysi/trustiot-v2#679）：讀 `datasets/lab/<名稱>/index.toml`、檢查索引與分段檔，並把資料集照 ingest 服務的寫法寫進 ClickHouse 的遙測窄表與批次完成表。索引與分段檔的格式、版本化規則、合成 fixture 與下游測試的用法寫在 [datasets/lab/README.md](../../datasets/lab/README.md)，本頁只寫程式本身。

同時是 library（給 ingest、telemetry-query、assistant 的 Rust 整合測試當 `[dev-dependencies]`）與指令（`lab-dataset check|load`，手動載入用）。

| 模組 | 內容 |
| --- | --- |
| `index` | 索引的型別、解析與規則（`Index::parse`、`Location`） |
| `segment` | 分段檔（JSON lines）的解析與對索引的檢查（`segment::parse`、`Reading`、`Value`） |
| `time` | 唯一接受的時間寫法與 `TimeShift`（固定毫秒位移，`ending_before` 取整段位移） |
| `anomaly` | 人工異常片段：`Anomaly`（`spike`、`outage`、`changeover` 與 `expecting`）、規則與套用到讀值的方式 |
| `clickhouse` | 寫入 `trustiot.telemetry` 與 `trustiot.batch_commits`；`commit_batch` 是唯一寫完成紀錄的地方 |
| `cli` | `lab-dataset` 指令的參數與執行 |
| 根模組 | `Dataset`（`open`、`with_anomalies`、`readings`、`recorded_readings`、`ids`、`load`）、`Target`、`Loaded`、`derived_id`、`synthetic_index`、`index_for_tests` |

## 設計

- **檢查在寫入之前**：`Dataset::open` 讀索引與每個分段檔並檢查全部規則；`load` 再讀一次每個分段檔、檢查通過後才開始寫入，所以壞掉或被改過的檔案不會寫進一半。分段檔整份讀進記憶體（上限 64 MiB）以先驗雜湊。
- **像 ingest 寫的資料**：一批一次 INSERT（`JSONEachRow`，同步寫入：`async_insert=0`），依分段檔的行順序；每批寫完才寫它的完成紀錄，`uncommitted` 的批次不寫。表的鍵與欄位依目前的 `services/ingest/migrations/clickhouse/0002_telemetry.sql`、`0003_batch_commits.sql`；Laysi/trustiot-v2#639 改 `batch_commits` 的鍵時，改 `src/clickhouse.rs` 的 `commit_batch`。
- **ID**：設備與串流的 UUID 由 `derived_id`（SHA-256 前 16 bytes 的 UUID v8）從資料集名稱、租戶、閘道器與代號推導，可重現、各租戶不同；設備也可以由呼叫端指定（`Target::with_device`），重複或全零的 ID 被拒絕。
- **HTTP**：用 migration Job 的 client（`migrate::http`）：`http://`，或只信任指定 CA 的 `https://`（`ClickHouse::new` 的 `ca_file`），URL 不能帶帳密；不用 reqwest 的理由見 `tools/migrate/src/http.rs`。`ClickHouse` 的 `Debug` 不印密碼，錯誤訊息只有 ClickHouse 的錯誤文字。
- **人工異常片段只改值**：`anomaly::apply` 在讀值上改值（依讀值時間排序每個資料點，所以補送造成的亂序不影響），不增刪讀值，所以分段檔的雜湊、索引的筆數與批次都照舊成立；`open` 與 `with_anomalies` 套用一次來檢查需要讀值的規則，`readings` 與 `load` 每次重新套用。
- **SQL**：只有兩個靜態的 `INSERT ... FORMAT JSONEachRow`，資料放在 request body，不組 SQL 字串（`tools/sql-check`）。

## 指令

```sh
cargo run -p lab-dataset -- --help
cargo run -p lab-dataset -- check datasets/lab/synthetic/index.toml
CLICKHOUSE_URL=<url> CLICKHOUSE_USER=<帳號> CLICKHOUSE_PASSWORD=<密碼> \
  cargo run -p lab-dataset -- load datasets/lab/synthetic/index.toml \
  --tenant <uuid> --gateway <uuid> --end-before now
```

帳密只從環境變數讀（[docs/architecture/secrets.md](../../docs/architecture/secrets.md) 規則 3）；變數名稱與本地開發環境 `bun scripts/dev.ts env` 印出的 `CLICKHOUSE_URL`、`CLICKHOUSE_USER`、`CLICKHOUSE_PASSWORD` 相同。`https://` 另設 `CLICKHOUSE_CA_FILE`。指令會寫進它拿到的任何 ClickHouse，不要指向正式環境。

## 測試

```sh
cargo test -p lab-dataset
```

- 單元測試（`src/`）：索引與分段檔的每條規則各有違反的例子、時間的寫法與位移、ID 推導、寫入的 JSON 列、指令參數。
- `tests/anomalies.rs`：在合成 fixture 上插入四種片段（突波、兩種停機，其中一段跨過 32 位元計數器的溢位、一段在補送的資料上，以及換料）：時間窗內的讀值照種類改變、窗外不變，累計資料點在窗內不動、窗外每一步的增量與原本相同；標註原樣保留；寫在索引的 `[[anomalies]]` 與以 API 插入的結果相同，`check` 指令數得到它們，README 的索引範例可以解析；時間窗內沒有讀值、停機跨過歸零、代號重複、沒有標註的片段被拒絕。
- `tests/synthetic_fixture.rs`：合成 fixture 通過檢查，並從檔案內容確認它涵蓋累計電表歸零、計數器溢位、資料缺漏、亂序與補送、同一台轉換器下兩台設備與未完成的批次；改過一個數字、目錄名稱不符、symlink 指到目錄外的副本都被拒絕。
- `tests/clickhouse_load.rs`：以 podman 或 docker 起暫時的 ClickHouse 容器（`services/ingest/tests/common/mod.rs` 的 `start_throwaway`，以 `#[path]` 引用；沒有容器執行環境時略過，設了 `CI` 時失敗）。一律用容器、不用 `TEST_CLICKHOUSE_URL`：測試會停掉 merge、建一個讀得到所有列的帳號，這些不能留在共用的伺服器上，套用 migration Job 的 `PRODUCT` ClickHouse schema，以 library 載入租戶 A、以 `lab-dataset load` 指令載入租戶 B，逐項比對：總筆數與每個資料點的筆數等於索引；每一列的 ID、位移後的時間與值；每批一個 part、依抵達順序寫入（停掉 merge 後比對 `_part` 的 block 編號）；未完成以外的每批都有正確的完成紀錄；兩個租戶的設備與串流 ID 不重疊。第三個租戶載入插入了 `tests/support` 的片段的資料集：每一列等於套用片段後的讀值，`Loaded::anomalies` 的時間窗等於參數加上位移，ClickHouse 裡讀成突波值的列剛好是時間窗內的那幾筆。
