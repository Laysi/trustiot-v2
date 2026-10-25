# datasets/lab

lab 實機錄下的資料集的索引與標註（功能重建清單 M7）：供 AI 開發、警報回測、補送與分析回歸使用。資料集本身放物件儲存，這裡只放索引與標註，例如每段資料的來源、時間範圍、物件儲存位置，以及插入的人工異常片段與預期的觸發結果。唯一的例外是合成 fixture（[`synthetic/`](synthetic/)）：它很小、內容固定，資料檔直接進版控。

讀索引、檢查資料與載入 ClickHouse 的程式是 [`tools/lab-dataset`](../../tools/lab-dataset/README.md)（Laysi/trustiot-v2#679）。

## 目錄結構

```
datasets/lab/
├── README.md
└── <資料集名稱>/
    ├── index.toml          # 索引：本頁「索引格式」
    └── segments/*.jsonl    # 只有合成 fixture 把分段檔放在 repo 裡；lab 資料集的分段檔在物件儲存
```

資料集名稱就是目錄名稱，也是索引的 `name`（小寫英文、數字與 `-`），載入工具檢查兩者相同。

## 索引格式（format 1）

索引是 TOML。不認得的欄位一律拒絕（打錯欄位名稱不會被靜默忽略）；`format` 不是 1 也拒絕。索引只寫設備與資料點：轉換器只以資料集內的邏輯代號（例如 `rs485-a`）出現，**不寫轉換器的位址、內部主機名稱或客戶的設備名稱**（repo 是公開的，見 [docs/architecture/secrets.md](../../docs/architecture/secrets.md)「公開 repo 的資訊規則」）。

最上層：

| 欄位 | 說明 |
| --- | --- |
| `format` | 索引格式的版本，目前是 `1`。載入工具與索引在同一個 repo、一起改，所以只認目前的版本 |
| `name` | 資料集名稱，等於目錄名稱 |
| `version` | 資料集版本，從 1 起算，見「版本化規則」 |
| `description` | 一句話說明資料集內容 |
| `[[streams]]` | 閘道器的批次串流：`key`、`description` |
| `[[devices]]` | 設備，見下表 |
| `[[segments]]` | 分段檔，依抵達順序排列，見下表 |
| `[[anomalies]]` | 人工異常片段（保留，見下方） |

`[[devices]]`：

| 欄位 | 說明 |
| --- | --- |
| `key` | 設備代號，例如 `meter-1` |
| `model` | 通用的型號名稱，例如 `three-phase-meter` |
| `converter` | RS485 轉換器的邏輯代號；Modbus TCP 設備不寫 |
| `unit` | Modbus 站號；轉換器下的設備是 1–247，同一個轉換器下不重複（不同轉換器可以重複，例如都是 1） |
| `interval_s` | 輪詢間隔（秒）。同一個資料點兩筆相鄰讀值的間隔大於它，就是資料缺漏 |
| `points` | 資料點：`key`（1–128 字元的英數、`_`、`-`、`.`，與遙測窄表的 `point` 相同）、`type`（`float`、`int`、`bool`、`string`，決定寫進窄表的哪一欄）、`kind`（`gauge` 瞬間值、`cumulative` 累計值、`state` 狀態）、`unit`（工程單位，可省略）、`wraps_at`（只用於會溢位的累計整數，例如 32 位元暫存器寫 `4294967296`，每筆讀值都小於它） |

`[[segments]]`：

| 欄位 | 說明 |
| --- | --- |
| `key` | 分段代號 |
| `location` | 分段檔的位置：相對於索引所在目錄的路徑（合成 fixture，例如 `segments/part-1.jsonl`），或 `s3://<bucket>/<key>`（S3 相容的物件儲存，供應商到 Laysi/trustiot-v2#133 才選定）。相對路徑不能有 `.`、`..`、絕對路徑，指向的檔案（含 symlink 解析後）必須在資料集目錄內；目前只接受 `.jsonl` |
| `bytes`、`sha256` | 檔案（以儲存的樣子）的大小與 SHA-256（小寫 16 進位）；分段檔最大 64 MiB，錄製工具依此切段 |
| `rows` | 讀值筆數（行數） |
| `reading_time`、`receive_time` | 讀值時間（`ts`）與接收時間（`received_at`）的最早與最晚值：`{ start = "…", end = "…" }` |
| `batches` | 每個串流在這段的批次範圍：`{ stream, first, last }`。同一個串流的序號連續（與 [protocol](../../crates/protocol/README.md)「遙測與確認」規則 1 相同），下一段接著上一段的 `last + 1` |
| `uncommitted` | 已寫入、但沒有完成紀錄的批次：`{ stream, batch_seq }`（I1：資料寫完、確認前斷線） |
| `series` | 每個資料點的筆數：`{ device, point, rows }`，加總等於 `rows` |

`[[anomalies]]` 是人工異常片段與預期的觸發結果，欄位先保留：`key`、`kind`（`spike` 突波、`outage` 停機、`changeover` 換料）、`device`、`points`、`start`、`end`（資料集的時間基準，載入時與資料一起位移）、`expect`（`{ trigger, fires }` 的陣列）。插入片段的 API 在 Laysi/trustiot-v2#679 的第二階段完成；在那之前，列有異常片段的索引可以檢查，但載入工具拒絕載入它。

### 規則

載入工具（`Dataset::open`）在寫入任何資料之前檢查全部規則，任何一項不符就拒絕整個資料集：

- 索引本身：上表的格式；代號不重複；轉換器加站號不重複；分段依接收時間排列（下一段的 `receive_time.start` 不早於上一段的 `end`）；串流的批次跨段連續；`uncommitted` 在該段的批次範圍內；`series` 指向宣告過的資料點。
- 分段檔對索引：大小與 SHA-256 相同；筆數、兩個時間範圍、每個資料點的筆數、每個串流的最後一批都與索引相同。
- 每一行：見下節。

## 分段檔格式

一行一筆讀值的 JSON lines（`.jsonl`），每行是遙測窄表的一列、少了載入時才決定的 ID：

```json
{"stream":"live","batch_seq":1,"received_at":"2026-01-05T22:10:02.500Z","device":"meter-1","point":"energy_kwh","ts":"2026-01-05T22:00:00.120Z","value_type":"float","value":15230.4}
```

| 欄位 | 寫進窄表的 |
| --- | --- |
| `stream`、`batch_seq` | `stream_id`（由串流代號換成 UUID）、`batch_seq` |
| `received_at` | `received_at`：ingest 收到這批的時間 |
| `device`、`point` | `device_id`（由設備代號換成 UUID）、`point` |
| `ts` | `ts`：閘道器的讀值時間 |
| `value_type`、`value` | `value_type` 與對應的值欄位，其他值欄位寫預設值 |

選 JSON lines 的理由：一行一筆，在 git 裡的差異就是增刪的讀值；ClickHouse 以 `JSONEachRow` 原生讀得到；型別由 JSON 本身區分（字串有引號、布林是 `true`/`false`），不必另外跳脫。lab 資料集在物件儲存可以再壓縮，格式在第二階段決定存放方式時一起定。

每一行的規則：

- 欄位剛好是上面八個；設備與資料點在索引裡；`value_type` 等於資料點宣告的 `type`；值符合型別：`float` 是有限的數（整數形式的數也可以，但不能超過 2^53），`int` 是 64 位元整數，`bool` 是 `true`/`false`，`string` 最多 1024 bytes（與閘道器協定相同）；有 `wraps_at` 的資料點，值在 0 到 `wraps_at - 1`。
- 時間只接受一種寫法：UTC、毫秒、`Z`，例如 `2026-01-05T22:00:00.120Z`。同一個時間只有一種寫法，檔案的雜湊才有意義。
- 行的順序就是抵達順序，也是載入工具 INSERT 的順序。同一批的列連續、`received_at` 相同（一批一起到達）；同一個串流的批次依序號連續出現；不同串流可以交錯；`received_at` 從一批到下一批不會變小。
- 同一個資料點在同一個時間只有一筆（整個資料集內）。
- 檔案以換行結尾，沒有空行，UTF-8。

## 時間：讀值時間與接收時間

`ts` 是讀值時間，`received_at` 是 ingest 收到的時間。兩者分開記，資料集才能表現補送：離線緩衝的讀值 `received_at` 遠晚於 `ts`，而且比更新的讀值晚到（亂序）。載入工具把兩者原樣寫進窄表（加上同一個位移），並依行的順序一批一批 INSERT，所以 materialized view 這類依 INSERT 觸發的處理看到的順序也與真實抵達順序相同。

原始資料只能查 7 天、彙總資料 3 個月，所以測試要把資料集搬到現在附近：載入時指定一個固定的毫秒位移（`TimeShift`），加到每一個 `ts`、`received_at` 與完成紀錄的 `committed_at` 上。所有時間差、順序與缺漏都不變。`Dataset::shift_ending_before(now, 1 天)` 取整天的位移，讓資料集最後的時間落在現在之前的一天內，每筆讀值在 UTC 的分、時、日之中的位置也不變（1 分鐘與 1 小時彙總的邊界不會因位移而改變）。人工異常片段的事件時間用同一個時間基準，載入時一起位移。

沒有位移的資料集保留原本的時間（合成 fixture 在 2026-01）：比部署的保存期限（`received_at` 起算，見 [資料保存期限](../../docs/architecture/data-retention.md)）還舊的列會被 TTL 刪掉，所以測試一律位移到現在附近。

## 批次與完成紀錄

每一批（`stream` + `batch_seq`）一次 INSERT 進 `trustiot.telemetry`，然後在 `trustiot.batch_commits` 寫一筆完成紀錄（`row_count` 是該批筆數，`committed_at` 是該批的 `received_at`），就像 ingest 寫完並落盤後才記錄完成（I1）。索引 `uncommitted` 列出的批次只寫資料、不寫完成紀錄，用來測試只計入已完成批次的處理（例如 Laysi/trustiot-v2#141 的彙總）與批次重寫。寫完成紀錄的程式只有 `tools/lab-dataset/src/clickhouse.rs` 的 `commit_batch` 一處；Laysi/trustiot-v2#639 改變 `batch_commits` 的鍵時在那裡一起改。

## 版本化規則

- 資料集發布後，分段檔不再修改：資料或索引內容（註解以外）有任何改變，`version` 加 1。
- 物件儲存的物件依版本放在不同的鍵下，例如 `s3://<bucket>/lab/<name>/v<version>/<segment>.jsonl`；新版本不覆寫舊版本的物件，舊版本的索引留在 git 歷史裡，取出舊的 commit 就能重跑當時的測試。
- 索引以 `sha256` 釘住每個分段檔：同一個版本的資料不可能悄悄改變，下載到的檔案與索引不符時載入工具拒絕。
- `format` 只在索引或分段檔的格式改變時調整；載入工具與所有索引在同一個變更裡一起更新。
- 合成 fixture 遵守同樣的規則：改內容就加版本，並更新檢查它的測試。

## 存放位置與存取權限

待定：在 Laysi/trustiot-v2#679 的第二階段決定並寫進本節。已確定的部分：`location` 用可搬移的 S3 相容 URI（`s3://<bucket>/<key>`），供應商到 Laysi/trustiot-v2#133 才選定；存取物件儲存的金鑰照 [docs/architecture/secrets.md](../../docs/architecture/secrets.md) 處理，不進 repo、不經指令參數。目前載入工具只讀相對路徑的分段檔，遇到 `s3://` 位置會回報尚未支援。

## 下游測試怎麼載入資料集

ingest、telemetry-query 與 assistant 的 Rust 整合測試（Laysi/trustiot-v2#141、Laysi/trustiot-v2#142、Laysi/trustiot-v2#150）以 `lab-dataset` crate 載入。在服務的 `Cargo.toml`：

```toml
[dev-dependencies]
lab-dataset = { path = "../../tools/lab-dataset" }
```

測試裡（ClickHouse 由 `services/ingest/tests/common/mod.rs` 起，並套用產品的 ClickHouse schema；telemetry-query 的 `tests/support` 已經這樣做）：

```rust
use lab_dataset::{ClickHouse, Dataset, Target};

let dataset = Dataset::open(lab_dataset::index_for_tests())?;
let shift = dataset.shift_ending_before(chrono::Utc::now(), chrono::TimeDelta::days(1))?;
let writer = ClickHouse::new(&server.url, &server.admin.user, &server.admin.password, None)?;

// 租戶與閘道器由測試決定；跨租戶測試把同一份資料載入兩個租戶。
let a = dataset.load(&writer, &Target::new(tenant_a, gateway_a), shift).await?;
let b = dataset.load(&writer, &Target::new(tenant_b, gateway_b), shift).await?;

let meter = a.device("meter-1");               // 寫入的設備 UUID
let expected = dataset.readings()?;            // 預期值從資料集本身算，時間加上 a.shift()
```

- **租戶與閘道器**：由呼叫端指定（`Target::new(tenant, gateway)`）。設備與串流的 UUID 由資料集名稱、租戶、閘道器與代號推導（UUID v8，同樣的輸入永遠得到同樣的 ID），所以兩個租戶各有自己的設備；`Target::with_device(key, uuid)` 改用測試自己建立的設備（例如在資產服務建好的設備）。`Loaded` 列出每個設備與串流代號對應的 UUID、位移與寫入的筆數。
- **時間**：見「時間」。比對預期值時，把資料集的時間加上 `loaded.shift()`。
- **換資料集**：`lab_dataset::index_for_tests()` 預設是合成 fixture，設了 `TRUSTIOT_LAB_DATASET_INDEX`（索引檔路徑）就改用那份。測試的預期值從 `Dataset::readings()` 算、不寫死 fixture 的數字，換成 lab 資料集（Laysi/trustiot-v2#681）時只換索引、不改測試。
- **寫入帳號**：要能 INSERT `trustiot.telemetry` 與 `trustiot.batch_commits`（`trustiot_ingest` 角色，或測試的管理帳號）。讀回比對時注意 row policy：沒有 policy 點名的帳號（包括管理帳號）讀這兩張表得到 0 列，見 [services/ingest](../../services/ingest/README.md)「租戶隔離」；`tools/lab-dataset/tests/clickhouse_load.rs` 示範建一個測試用的讀取帳號。
- **只載一次**：同一個資料集對同一個租戶與閘道器載入兩次，會以同樣的 ID 寫兩次，不支援；中途失敗時已寫入的批次留著，換一個租戶或閘道器重載。

## 指令

手動載入用 `lab-dataset` 指令（例如載入本地開發環境，步驟在第二階段補上）：

```sh
# 只檢查索引與分段檔，印出摘要
cargo run -p lab-dataset -- check datasets/lab/synthetic/index.toml

# 載入 CLICKHOUSE_URL 指的 ClickHouse；帳密只從環境變數讀
CLICKHOUSE_URL=http://127.0.0.1:18123 CLICKHOUSE_USER=<帳號> CLICKHOUSE_PASSWORD=<密碼> \
  cargo run -p lab-dataset -- load datasets/lab/synthetic/index.toml \
  --tenant <租戶 UUID> --gateway <閘道器 UUID> --end-before now
```

`https://` 的 ClickHouse 另外設 `CLICKHOUSE_CA_FILE`（只信任這個 CA）。選項見 `cargo run -p lab-dataset -- --help`：`--device <key>=<uuid>` 指定設備 ID，`--shift-ms` 指定確切的位移，`--end-before <now|時間>` 加 `--align-s`（預設 86400）取整段的位移。載入結果（與 `Loaded` 相同）以 JSON 印到標準輸出。

## 合成 fixture

[`synthetic/`](synthetic/) 是一份 4 小時、316 筆的小資料集，格式與 lab 資料集相同，給 Laysi/trustiot-v2#141、Laysi/trustiot-v2#142、Laysi/trustiot-v2#150 先驗收。它是固定的檔案，一次寫好後進版控，repo 裡沒有產生它的程式（M7 棄用測試資料產生器）；要改內容就直接改檔案、更新索引的大小、雜湊與統計，並把 `version` 加 1。

一台閘道器，輪詢間隔 5 分鐘，2026-01-05 22:00 到 2026-01-06 01:55（UTC）：

| 設備 | 轉換器、站號 | 資料點 |
| --- | --- | --- |
| `meter-1` 三相電表 | `rs485-a`，1 | `energy_kwh`（float 累計）、`power_kw`（float） |
| `meter-2` 脈衝計數表 | `rs485-a`，2 | `pulses`（int 累計，32 位元，`wraps_at = 4294967296`）、`power_kw`（float） |
| `press-1` 沖床控制器 | `rs485-b`，1 | `running`（bool）、`mode`（string）、`cycle_count`（int 累計） |

涵蓋真實資料常見的狀況：

| 狀況 | 內容 |
| --- | --- |
| 同一台轉換器下多台設備 | `meter-1`、`meter-2` 都在 `rs485-a`，站號 1、2；`press-1` 在另一台轉換器，站號也是 1 |
| 累計電表歸零 | `meter-1/energy_kwh` 在 23:30 由 15293.813 變成 0，之後重新累計 |
| 計數器溢位 | `meter-2/pulses` 在 23:55 越過 2^32 繞回小的值 |
| 資料缺漏 | `meter-2` 漏讀 22:35 一次；`rs485-b` 在 00:00–00:25 連不上，`press-1` 缺 6 次 |
| 亂序與補送 | 上行在 00:45–01:25 中斷，這段的 3 批放在 `backlog` 串流，01:41 恢復後 `live` 先送出 01:30–01:40 的新資料，`backlog` 的 63 筆隨後才到，最晚比讀值時間晚 56 分鐘 |
| 未完成的批次 | `live` 的最後一批（01:45–01:55，21 筆）已寫入但沒有完成紀錄 |

另外四種值型別都有，兩個分段檔（`part-1` 是中斷前、`part-2` 是恢復後）。`tools/lab-dataset/tests/synthetic_fixture.rs` 從檔案內容逐項檢查上表，`tools/lab-dataset/tests/clickhouse_load.rs` 把它載入套用產品 schema 的暫時 ClickHouse，比對筆數與索引相同。
