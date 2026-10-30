# lab-dataset

lab 資料集的工具（功能重建清單 M7，Laysi/trustiot-v2#679）：讀 `datasets/lab/<名稱>/index.toml`、檢查索引與分段檔、插入人工異常片段，並把資料集照 ingest 服務的寫法寫進 ClickHouse 的遙測窄表與批次完成表；從實際設備錄製資料集，以及把一台閘道器的資料從 ClickHouse 匯出成資料集。索引與分段檔的格式、人工異常片段、版本化規則、存放位置、錄製、合成 fixture 與下游測試的用法寫在 [datasets/lab/README.md](../../datasets/lab/README.md)，本頁只寫程式本身。

同時是 library（給 ingest、telemetry-query、assistant 的 Rust 整合測試當 `[dev-dependencies]`）與指令（`lab-dataset check|load|export|record`，手動載入、匯出與錄製用）。

| 模組 | 內容 |
| --- | --- |
| `index` | 索引的型別、解析與規則（`Index::parse`、`Location`） |
| `segment` | 分段檔（JSON lines）的解析與對索引的檢查（`segment::parse`、`Reading`、`Value`） |
| `time` | 唯一接受的時間寫法與 `TimeShift`（固定毫秒位移，`ending_before` 取整段位移） |
| `anomaly` | 人工異常片段：`Anomaly`（`spike`、`outage`、`changeover` 與 `expecting`）、規則與套用到讀值的方式 |
| `clickhouse` | 寫入 `trustiot.telemetry` 與完成表 `trustiot.stream_commits`；`commit_batch` 是唯一寫完成紀錄的地方 |
| `export` | 匯出工具：設定檔（`Config`，錄製設定檔加每台設備的 `id`）、依批次分組、重複列與完成紀錄的規則、組成資料集（`Exporter`、`Exported`） |
| `writer` | 錄製與匯出共用的分段檔與索引寫入（`SegmentWriter`）：切段、雜湊、統計、每關一個分段就重寫索引 |
| `record` | 錄製工具：設定檔（`Config`）的檢查、以 agent 的 `polling::run` 與 `Endpoints` 輪詢、組批、寫分段檔與索引（`Recorder`、`Recorded`） |
| `cli` | `lab-dataset` 指令的參數與執行 |
| 根模組 | `Dataset`（`open`、`with_anomalies`、`readings`、`recorded_readings`、`ids`、`load`）、`Target`、`Loaded`、`derived_id`、`synthetic_index`、`index_for_tests` |

## 設計

- **檢查在寫入之前**：`Dataset::open` 讀索引與每個分段檔並檢查全部規則；`load` 再讀一次每個分段檔、檢查通過後才開始寫入，所以壞掉或被改過的檔案不會寫進一半。分段檔整份讀進記憶體（上限 64 MiB）以先驗雜湊。
- **像 ingest 寫的資料**：一批一次 INSERT（`JSONEachRow`，同步寫入：`async_insert=0`），依分段檔的行順序；每批寫完才寫它的完成紀錄，`uncommitted` 的批次不寫。表的鍵與欄位依目前的 `services/ingest/migrations/clickhouse/0002_telemetry.sql`、`0012_gateway_stream_ids.sql`：閘道器與串流 ID 是文字，載入工具寫的是 UUID 的文字形式（合乎協定的 ID 規則）；完成表的鍵或欄位再改時，改 `src/clickhouse.rs` 的 `commit_batch`。
- **ID**：設備與串流的 UUID 由 `derived_id`（SHA-256 前 16 bytes 的 UUID v8）從資料集名稱、租戶、閘道器與代號推導，可重現、各租戶不同；設備也可以由呼叫端指定（`Target::with_device`），重複或全零的 ID 被拒絕。完成紀錄的 `binding_id` 也這樣推導（種類 `binding`、代號空白，`Loaded::binding`）：載入工具沒有閘道器綁定的對照表，這個 ID 只是填欄位用，不對應任何真實的綁定。
- **HTTP**：用 migration Job 的 client（`migrate::http`）：`http://`，或只信任指定 CA 的 `https://`（`ClickHouse::new` 的 `ca_file`），URL 不能帶帳密；不用 reqwest 的理由見 `tools/migrate/src/http.rs`。`ClickHouse` 的 `Debug` 不印密碼，錯誤訊息只有 ClickHouse 的錯誤文字。
- **人工異常片段只改值**：`anomaly::apply` 在讀值上改值（依讀值時間排序每個資料點，所以補送造成的亂序不影響），不增刪讀值，所以分段檔的雜湊、索引的筆數與批次都照舊成立；`open` 與 `with_anomalies` 套用一次來檢查需要讀值的規則，`readings` 與 `load` 每次重新套用。
- **錄製工具放在這個 crate**：`lab-dataset record` 以 library 依賴 `edge/agent`（`modbus`、`polling`）與 `crates/protocol`（設定與 `BatchBuilder`），寫出的格式與檢查它的 `Dataset::open` 在同一個 crate，改格式時一起改。考慮過的另外兩個位置：放在 `edge/agent` 當第二個執行檔，會讓閘道器的 crate 依賴資料集格式、`chrono` 與 migration Job 的 HTTP client（`ring`），而 `edge/` 要能不靠 C 工具鏈建置成 aarch64；另開一個 `tools/lab-recorder` crate，則多一個成員、還是要依賴本 crate 的索引型別與檢查，只為了把兩個指令分開。代價是本 crate（以及只為了載入而依賴它的服務測試）多了 agent 與 protocol 兩個 path 依賴，都是 workspace 內的 crate，沒有新的第三方依賴。
- **錄製也經過 agent 的位址限制**：`Recorder::run` 以呼叫端給的 `Endpoints` 讀設備，指令用的是 `Recorder::endpoints()`，也就是 agent 的正式路徑，所以和閘道器一樣拒絕 loopback、未指定、本機自己網卡上的位址、多播、廣播與雲端 metadata 位址（[edge/agent](../../edge/agent/README.md)「連線位址的限制」，Laysi/trustiot-v2#653）。經本機 port forward（例如 ssh 的本機轉送）連到的轉換器因此會被拒絕：錄製時要直接連轉換器的位址。只有測試能放行位址：`tests/record.rs` 以 `exempt_for_tests` 放行假轉換器所在的 127.0.0.1，這個方法只在 `[dev-dependencies]` 打開 agent 的 `test-exempt-addresses` 時存在；本 crate 自己的程式沒有任何測試用的 feature 或 `cfg`，所以 `cargo build -p lab-dataset` 建出的指令放行不了任何位址。
- **錄製的時鐘**：讀值時間與接收時間都是開始時的系統時間加上 tokio 的單調時間，不在途中重讀系統時鐘，所以時鐘校正不會讓同一個資料點的時間倒退（那會違反「同一個時間只有一筆」與接收時間不倒退的規則）；測試以暫停的 tokio 時鐘（`testkit::clock::manual()`）讓讀值時間精確可比。
- **匯出是 `load` 的反向**：`Exporter::run` 每小時一次查詢（依 `received_at`）讀遙測窄表，再依串流的序號範圍查完成表，讀完、規則都檢查過才建目錄寫檔（被拒絕的匯出不留下檔案），寫完以 `Dataset::open` 檢查。時間不位移。規則（重複列、完成紀錄的筆數、序號連續、沒列出的設備）與理由在 `src/export.rs` 的模組說明與 [datasets/lab/README.md](../../datasets/lab/README.md)「匯出」；`group` 與 `finish` 是不碰 ClickHouse 的純函式，單元測試逐條涵蓋。
- **串流的代號**：ClickHouse 的串流 ID 是閘道器自己取的文字，不能直接當索引的代號（索引只認小寫英數與 `-`，也不該公開閘道器的 ID）：設定檔的 `[[streams]]` 指定，沒指定的依第一批抵達的先後叫 `stream-<n>`。
- **匯出的設定檔沿用錄製的**：`[[models]]` 與 `[[devices]]` 同一個格式（型別由同一個 `plan_point` 推出），每台設備加 `id`；`host`、`port`、`batch_s` 接受並忽略，所以同一份設定檔錄製與匯出都能用。
- **寫索引與分段檔的程式只有一份**（`src/writer.rs`）：錄製與匯出都用它，所以兩者寫出的格式相同、都由 `Dataset::open` 檢查。
- **SQL**：寫入只有兩個靜態的 `INSERT ... FORMAT JSONEachRow`，資料放在 request body；讀取只有兩個靜態的 `SELECT ... FORMAT JSONEachRow`，租戶、閘道器、串流、序號與時間以 `{name:Type}` 查詢參數綁定（`ClickHouse::select` 還拒絕不以 `SELECT ` 開頭的敘述），不送任何 setting（`readonly` 的帳號不能改 setting），不組 SQL 字串（`tools/sql-check`）。

## 指令

```sh
cargo run -p lab-dataset -- --help
cargo run -p lab-dataset -- check datasets/lab/synthetic/index.toml
CLICKHOUSE_URL=<url> CLICKHOUSE_USER=<帳號> CLICKHOUSE_PASSWORD=<密碼> \
  cargo run -p lab-dataset -- load datasets/lab/synthetic/index.toml \
  --tenant <uuid> --gateway <uuid> --end-before now
```

帳密只從環境變數讀（[docs/architecture/secrets.md](../../docs/architecture/secrets.md) 規則 3）；變數名稱與本地開發環境 `bun scripts/dev.ts env` 印出的 `CLICKHOUSE_URL`、`CLICKHOUSE_USER`、`CLICKHOUSE_PASSWORD` 相同。`https://` 另設 `CLICKHOUSE_CA_FILE`。`load` 會寫進它拿到的任何 ClickHouse，不要指向正式環境；`export` 只讀，但一樣不要把管理帳號交給它（管理帳號也讀不到列，見 datasets/lab/README.md「匯出」）。

## 測試

```sh
cargo test -p lab-dataset
```

- 單元測試（`src/`）：索引與分段檔的每條規則各有違反的例子、時間的寫法與位移、ID 推導、寫入的 JSON 列、指令參數。
- `tests/anomalies.rs`：在合成 fixture 上插入四種片段（突波、兩種停機，其中一段跨過 32 位元計數器的溢位、一段在補送的資料上，以及換料）：時間窗內的讀值照種類改變、窗外不變，累計資料點在窗內不動、窗外每一步的增量與原本相同；標註原樣保留；寫在索引的 `[[anomalies]]` 與以 API 插入的結果相同，`check` 指令數得到它們，README 的索引範例可以解析；時間窗內沒有讀值、停機跨過歸零、代號重複、沒有標註的片段被拒絕。
- `tests/record.rs`：以 testkit 的假 RS485 轉換器（`FakeModbus`，`Transport::RtuOverTcp`，聽在 127.0.0.1，以 `exempt_for_tests` 放行）在同一個端點放站號 1、2 兩台設備（五個資料點，四種值型別，兩個會溢位的計數器），在暫停的時鐘上每秒推進、等每次輪詢結束才再推進，錄 10 秒：兩台設備共用一條連線、只送功能碼 1–4；錄出的資料集通過 `Dataset::open`，每筆讀值的時間與值等於端點當時提供的；索引沒有位址或 port、轉換器只有邏輯代號；批次每 4 秒一批、分段在批次之間切開；寫進已存在的目錄被拒絕；最後載入套用產品 schema 的暫時 ClickHouse（與 `clickhouse_load.rs` 相同的容器規則），每個資料點的筆數與完成紀錄數等於索引。單元測試（`src/record.rs`）涵蓋設定檔的每條規則、`type` 的推導、超出 `wraps_at` 的讀值被略過、README 的設定範例可以使用。
- `tests/export.rs`：把合成 fixture 載入拋棄式 ClickHouse（套用產品 schema，三個租戶），經只有 `SELECT` 並被 row policy 點名的帳號匯出：整份匯出的讀值、批次（串流、序號、接收時間）、未完成的批次、設備與每個資料點的筆數都等於 fixture 加上載入的位移，`Dataset::open` 通過；索引與分段檔沒有位址、port、租戶、閘道器、設備與串流 ID；以指令匯出一段範圍（`live` 的第 3 到 6 批）再以 `check` 檢查；管理帳號與只點名別的租戶的帳號讀到 0 列時報錯、不留檔案；讀得到列卻讀不到完成紀錄的帳號被拒；少列一台設備的設定檔被拒；注入同一批的重複列時只留一筆並計數、同一筆讀值有兩個值時被拒。
- `tests/synthetic_fixture.rs`：合成 fixture 通過檢查，並從檔案內容確認它涵蓋累計電表歸零、計數器溢位、資料缺漏、亂序與補送、同一台轉換器下兩台設備與未完成的批次；改過一個數字、目錄名稱不符、symlink 指到目錄外的副本都被拒絕。
- `tests/clickhouse_load.rs`：以 podman 或 docker 起暫時的 ClickHouse 容器（`services/ingest/tests/common/mod.rs` 的 `start_throwaway`，以 `#[path]` 引用；沒有容器執行環境時略過，設了 `CI` 時失敗）。一律用容器、不用 `TEST_CLICKHOUSE_URL`：測試會停掉 merge、建一個讀得到所有列的帳號，這些不能留在共用的伺服器上，套用 migration Job 的 `PRODUCT` ClickHouse schema，以 library 載入租戶 A、以 `lab-dataset load` 指令載入租戶 B，逐項比對：總筆數與每個資料點的筆數等於索引；每一列的 ID、位移後的時間與值；每批一個 part、依抵達順序寫入（停掉 merge 後比對 `_part` 的 block 編號）；未完成以外的每批都有正確的完成紀錄；兩個租戶的設備與串流 ID 不重疊。第三個租戶載入插入了 `tests/support` 的片段的資料集：每一列等於套用片段後的讀值，`Loaded::anomalies` 的時間窗等於參數加上位移，ClickHouse 裡讀成突波值的列剛好是時間窗內的那幾筆。
