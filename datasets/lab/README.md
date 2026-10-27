# datasets/lab

lab 實機錄下的資料集的索引與標註（功能重建清單 M7）：供 AI 開發、警報回測、補送與分析回歸使用。資料集本身放物件儲存，這裡只放索引與標註，例如每段資料的來源、時間範圍、物件儲存位置，以及插入的人工異常片段與預期的觸發結果。唯一的例外是合成 fixture（[`synthetic/`](synthetic/)）：它很小、內容固定，資料檔直接進版控。

讀索引、檢查資料、載入 ClickHouse 與錄製的程式是 [`tools/lab-dataset`](../../tools/lab-dataset/README.md)（Laysi/trustiot-v2#679）。

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
| `[[anomalies]]` | 人工異常片段與預期的觸發標註，見「人工異常片段」 |

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
| `location` | 分段檔的位置：相對於索引所在目錄的路徑（合成 fixture，例如 `segments/part-1.jsonl`），或 `s3://<bucket>/<key>`（S3 相容的物件儲存，見「存放位置與存取權限」；載入工具目前讀不了，要先下載）。相對路徑不能有 `.`、`..`、絕對路徑，指向的檔案（含 symlink 解析後）必須在資料集目錄內；目前只接受 `.jsonl` |
| `bytes`、`sha256` | 檔案（以儲存的樣子）的大小與 SHA-256（小寫 16 進位）；分段檔最大 64 MiB，錄製工具依此切段 |
| `rows` | 讀值筆數（行數） |
| `reading_time`、`receive_time` | 讀值時間（`ts`）與接收時間（`received_at`）的最早與最晚值：`{ start = "…", end = "…" }` |
| `batches` | 每個串流在這段的批次範圍：`{ stream, first, last }`。同一個串流的序號連續（與 [protocol](../../crates/protocol/README.md)「遙測與確認」規則 1 相同），下一段接著上一段的 `last + 1` |
| `uncommitted` | 已寫入、但沒有完成紀錄的批次：`{ stream, batch_seq }`（I1：資料寫完、確認前斷線） |
| `series` | 每個資料點的筆數：`{ device, point, rows }`，加總等於 `rows` |

### 規則

載入工具（`Dataset::open`）在寫入任何資料之前檢查全部規則，任何一項不符就拒絕整個資料集：

- 索引本身：上表的格式；代號不重複；轉換器加站號不重複；分段依接收時間排列（下一段的 `receive_time.start` 不早於上一段的 `end`）；串流的批次跨段連續；`uncommitted` 在該段的批次範圍內；`series` 指向宣告過的資料點；`[[anomalies]]` 的規則見「人工異常片段」。
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

選 JSON lines 的理由：一行一筆，在 git 裡的差異就是增刪的讀值；ClickHouse 以 `JSONEachRow` 原生讀得到；型別由 JSON 本身區分（字串有引號、布林是 `true`/`false`），不必另外跳脫。lab 資料集在物件儲存可以再壓縮，格式在第一次上傳 lab 資料集時決定（Laysi/trustiot-v2#680）。

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

## 人工異常片段

警報回測與分析回歸要知道「什麼時候發生了什麼、應該觸發什麼」，所以在錄下的資料上插入人工異常片段（清單 M7）：突波（`spike`）、停機（`outage`）、換料（`changeover`）。事件時間由參數決定，每段都標好預期的觸發結果。兩種來源，效果相同：

- **索引的 `[[anomalies]]`**：資料集本身帶的標註，隨資料集版本化（加、改、刪都是內容改變，`version` 加 1）。
- **測試程式插入**：`Dataset::with_anomalies([...])` 回傳帶著這些片段的資料集，之後的 `readings()` 與 `load()` 都套用它們。測試自己決定事件時間，資料集不用改版。

```rust
use lab_dataset::{Anomaly, Dataset, time};

let at = |text| time::parse(text).unwrap();
let dataset = Dataset::open(lab_dataset::index_for_tests())?.with_anomalies([
    Anomaly::spike("power-spike", "meter-1", &["power_kw"],
                   at("2026-01-05T22:20:00.000Z"), at("2026-01-05T22:30:00.999Z"), 480.5)
        .expecting("power-high", true),
    Anomaly::outage("meter-stop", "meter-1", &["energy_kwh", "power_kw"],
                    at("2026-01-06T00:50:00.000Z"), at("2026-01-06T01:00:00.999Z"))
        .expecting("machine-stopped", true),
])?;
let loaded = dataset.load(&writer, &target, shift).await?;
// loaded.anomalies：同樣的片段，start、end 已加上位移，就是它們在 ClickHouse 裡的時間
```

`expect` 裡的 `trigger` 只是標籤（例如 `power-high`、`machine-stopped`），給之後的警報規則與分析工作比對；這裡不實作、也不執行任何警報規則。

### 欄位

| 欄位 | 說明 |
| --- | --- |
| `key` | 片段代號，在資料集內不重複（索引的與測試插入的合在一起算） |
| `kind` | `spike`、`outage` 或 `changeover` |
| `device`、`points` | 改變哪一台設備的哪些資料點；每個資料點在時間窗內至少要有一筆讀值 |
| `start`、`end` | 時間窗，資料集的時間基準、兩端都包含，比的是讀值時間 `ts`；載入時與資料一起位移（`Loaded::anomalies`） |
| `value` | 只用於 `spike`：時間窗內的值（整數或浮點數） |
| `state` | 只用於 `changeover`，而且列了文字狀態的資料點時必填：時間窗內的文字，例如 `setup` |
| `expect` | 預期的觸發，至少一項：`{ trigger, fires }`，`trigger` 是代號格式、同一段內不重複，`fires` 是預期觸發（`true`）或預期不觸發（`false`） |

索引裡的寫法：

```toml
[[anomalies]]
key = "press-changeover"
kind = "changeover"
device = "press-1"
points = ["running", "mode", "cycle_count"]
start = "2026-01-06T01:30:00.000Z"
end = "2026-01-06T01:40:00.999Z"
state = "changeover"
expect = [
  { trigger = "changeover-detected", fires = true },
  { trigger = "machine-stopped", fires = false },
]
```

### 每一種片段做什麼

片段只改變讀值的**值**：時間窗內（`start <= ts <= end`）的讀值，以及累計資料點在時間窗之後的讀值。**沒有讀值會增加或消失**，所以索引的筆數、批次、每點筆數與分段檔的雜湊都不變，`ts`、`received_at` 與批次也不變。資料缺漏與補送已經是錄下的資料本身有的狀況；停機是設備還在、讀得到、但讀值顯示它停了。

| 資料點（`kind`、`type`） | `spike` 突波 | `outage` 停機 | `changeover` 換料 |
| --- | --- | --- | --- |
| `gauge`，`float` 或 `int` | 讀成 `value`（`int` 資料點的 `value` 要是整數；`float` 資料點的整數 `value` 不能超過 2^53） | 讀成 0 | 讀成 0 |
| `cumulative`，`float` 或 `int` | 拒絕 | 停在時間窗之前的值，之後接著累計（見下方） | 同停機 |
| `state`，`bool` | 拒絕 | 讀成 `false` | 讀成 `false` |
| `state`，`string` | 拒絕 | 拒絕（停機沒有定義的文字） | 讀成 `state` |
| 其他組合 | 拒絕 | 拒絕 | 拒絕 |

換料在遙測裡就是一段停機加上文字狀態（例如 `mode`）顯示換料中：換成哪個料號記在生產紀錄（C5，[現場脈絡](../../docs/architecture/field-context.md)），不在遙測裡。

累計資料點在停機時不會倒退，也不會在恢復時跳一大段：

- 時間窗內的讀值都等於「停住的值」：時間窗之前最後一筆讀值的值；時間窗之前沒有讀值時，用時間窗內第一筆的值。
- 這個資料點在時間窗內累計的量（停住的值到時間窗內最後一筆）從之後的每一筆讀值扣掉，所以恢復後從停住的值接著累計，每一步的增量與原本的資料相同。
- 有 `wraps_at` 的計數器以 `wraps_at` 取模計算，跨越溢位也正確，值一直在 `0` 到 `wraps_at - 1`。
- 沒有 `wraps_at` 的資料點，數值變小就是歸零：時間窗之後的第一次歸零起，讀值保持原樣（歸零後本來就從 0 重新累計）；時間窗內有歸零的片段被拒絕，因為停住的值會有歧義，換一個時間窗。
- 浮點數相減的捨入可能讓結果比前一筆小最後一位，這時取前一筆的值，所以永遠不會倒退。

### 規則

開啟索引（`Dataset::open`）與插入（`with_anomalies`）時檢查，任何一項不符就拒絕：

- 上面兩張表：欄位、參數與資料點的組合。
- 代號不重複；同一台設備的同一個資料點，兩個片段的時間窗不能重疊（不重疊的片段依 `start` 先後套用）。
- `points` 裡每個資料點在時間窗內至少有一筆讀值（例如 fixture 的 `press-1` 在 00:00–00:25 連不上，那段放不了 `press-1` 的片段）。
- 累計資料點的時間窗內沒有歸零。

## 版本化規則

- 資料集發布後，分段檔不再修改：資料或索引內容（註解以外）有任何改變，`version` 加 1。
- 物件儲存的物件依版本放在不同的鍵下，例如 `s3://<bucket>/lab/<name>/v<version>/<segment>.jsonl`；新版本不覆寫舊版本的物件，舊版本的索引留在 git 歷史裡，取出舊的 commit 就能重跑當時的測試。
- 索引以 `sha256` 釘住每個分段檔：同一個版本的資料不可能悄悄改變，下載到的檔案與索引不符時載入工具拒絕。
- `format` 只在索引或分段檔的格式改變時調整；載入工具與所有索引在同一個變更裡一起更新。
- 合成 fixture 遵守同樣的規則：改內容就加版本，並更新檢查它的測試。

## 存放位置與存取權限

2026-10-25 決定（Laysi/trustiot-v2#679）：

- **S3 相容的物件儲存**。索引的 `location` 寫可搬移的 `s3://<bucket>/<key>`，不寫某一家供應商的網址，換供應商只改 bucket 的設定，不改索引格式。
- **bucket 在 Laysi/trustiot-v2#133 選定**（供應商、區域與 bucket 名稱一起決定），現在不建立任何 bucket 或帳號。在那之前不上傳任何 lab 資料集，repo 裡只有合成 fixture（它的分段檔進版控，不放物件儲存）。
- **鍵的配置依版本**，見「版本化規則」：`lab/<name>/v<version>/<segment>.jsonl`（壓縮格式在第一次上傳 lab 資料集時決定，見 Laysi/trustiot-v2#680；壓縮後的檔名與 `sha256` 指的是儲存的樣子）。發布後的物件不覆寫、不刪除，舊的 commit 才能重跑當時的測試。
- **公開讀取**：下載不需要任何憑證，任何人拿到 repo 就能重跑用到 lab 資料集的測試（例如 Laysi/trustiot-v2#681）。
- **上傳只給維護者**：寫入物件儲存的憑證照 [docs/architecture/secrets.md](../../docs/architecture/secrets.md) 處理（不進 repo、不經指令參數、不寫進日誌），只發給維護者；CI 與測試只讀、不需要它。
- **資料本身也公開**：上傳的分段檔任何人都下載得到，與索引一樣受「公開 repo 的資訊規則」約束：不能有客戶的資料、客戶的設備名稱、位址或主機名稱。lab 資料集只錄 lab 的設備，設備與資料點用通用名稱（`meter-1`、`power_kw`），轉換器只以邏輯代號出現（錄製工具寫出的索引本來就是這樣，見「錄製」）。上傳前先檢查分段檔的文字值（`string` 資料點）沒有這類內容。

載入工具不讀 `s3://` 的位置：下載用的網址形式（路徑式或虛擬主機式、端點的網域）要等 Laysi/trustiot-v2#133 選定供應商才知道，現在寫死一種就可能要改。遇到 `s3://` 的分段時 `Dataset::open` 回報「not supported yet」，並說明要先把分段檔下載到索引旁邊、改用相對路徑。供應商選定後再加上直接下載（屆時一起決定快取的位置），索引格式不變。

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
- **換資料集**：`lab_dataset::index_for_tests()` 預設是合成 fixture，設了 `TRUSTIOT_LAB_DATASET_INDEX`（索引檔路徑）就改用那份。測試的預期值從 `Dataset::readings()` 算、不寫死 fixture 的數字，換成 lab 資料集（Laysi/trustiot-v2#681）時只換索引、不改測試。彙總表的測試 `tools/lab-dataset/tests/rollups.rs` 就是這樣：讀值與批次來自資料集，要算電能的資料點是索引裡 `kind = "cumulative"` 的每一個（溢位值取它的 `wraps_at`），例如 `TRUSTIOT_LAB_DATASET_INDEX=<索引檔> cargo test -p lab-dataset --test rollups -- --nocapture`；語意查詢 API 的測試 `services/telemetry-query/tests/semantic.rs` 也一樣（`cargo test -p telemetry-query --test semantic -- --nocapture`），fixture 才有的性質（一次重置、一次 32 位元回捲、漏讀）只在沒有設定這個變數時檢查。
- **寫入帳號**：要能 INSERT `trustiot.telemetry` 與 `trustiot.batch_commits`（`trustiot_ingest` 角色，或測試的管理帳號）。讀回比對時注意 row policy：沒有 policy 點名的帳號（包括管理帳號）讀這兩張表得到 0 列，見 [services/ingest](../../services/ingest/README.md)「租戶隔離」；`tools/lab-dataset/tests/clickhouse_load.rs` 示範建一個測試用的讀取帳號。
- **只載一次**：同一個資料集對同一個租戶與閘道器載入兩次，會以同樣的 ID 寫兩次，不支援；中途失敗時已寫入的批次留著，換一個租戶或閘道器重載。

## 指令

手動載入用 `lab-dataset` 指令（載入本地開發環境的步驟見 [scripts/README.md](../../scripts/README.md)「載入 lab 資料集」）：

```sh
# 只檢查索引與分段檔，印出摘要
cargo run -p lab-dataset -- check datasets/lab/synthetic/index.toml

# 載入 CLICKHOUSE_URL 指的 ClickHouse；帳密只從環境變數讀
CLICKHOUSE_URL=http://127.0.0.1:18123 CLICKHOUSE_USER=<帳號> CLICKHOUSE_PASSWORD=<密碼> \
  cargo run -p lab-dataset -- load datasets/lab/synthetic/index.toml \
  --tenant <租戶 UUID> --gateway <閘道器 UUID> --end-before now
```

`https://` 的 ClickHouse 另外設 `CLICKHOUSE_CA_FILE`（只信任這個 CA）。選項見 `cargo run -p lab-dataset -- --help`：`--device <key>=<uuid>` 指定設備 ID，`--shift-ms` 指定確切的位移，`--end-before <now|時間>` 加 `--align-s`（預設 86400）取整段的位移。載入結果（與 `Loaded` 相同，含位移後的人工異常片段）以 JSON 印到標準輸出。

錄製見下一節：

```sh
cargo run --release -p lab-dataset -- record <repo 外的設定檔> --out <repo 外的目錄> [--duration-s <秒>]
```

## 錄製

`lab-dataset record` 讀實際的設備，寫出本頁格式的資料集（索引加分段檔）；它就是 Laysi/trustiot-v2#680 到 lab 錄第一版時用的工具。

- **用閘道器 agent 的程式讀設備**：設備與型號是 `crates/protocol` 的設定（`DeviceModel`、`Device`），先經過與閘道器相同的設定驗證；每台設備由 agent 的 `polling::run` 依固定節拍輪詢（逾時、重連退避、watchdog 都相同），連線由 `Endpoints` 分配，所以同一台 RS485 轉換器下的設備共用一條連線、一次一個請求，RTU 框架丟掉其他站號的回應（[edge/agent](../../edge/agent/README.md)，Laysi/trustiot-v2#87、Laysi/trustiot-v2#88）。讀取失敗的資料點沒有讀值，就是資料缺漏，與閘道器看到的一樣。
- **錄製工具也是一個主站**：轉換器把每台設備的回應送給所有 TCP 連線（[現場硬限制](../../docs/requirements/field-constraints.md)），所以錄製期間不能有其他程式連同一台轉換器，包括 agent。[milestones](../../docs/roadmap/milestones.md)「展示切片」規定 agent 開始輪詢 lab 的 RS485 轉換器後，其他程式不再連它：Laysi/trustiot-v2#680 要在那之前錄。
- **寫出的目錄**是 `--out` 之下以資料集名稱命名的目錄，必須還不存在（寫好的資料集不會被改寫）；`--out` 放在 repo 外，分段檔不進版控。錄到 `--duration-s` 秒或按 Ctrl-C 為止，最後一批寫完後以 `Dataset::open` 檢查整份資料集。每關閉一個分段檔就重寫一次索引，所以中途被強制結束時，索引仍然是已完成分段的有效資料集（寫到一半的分段檔不在索引裡）。
- **結果**以 JSON 印到標準輸出：筆數、批次數、分段數、時間範圍，以及被略過的讀值數（`dropped_readings`：例如計數器讀到不小於 `wraps_at` 的值，這種讀值不符合索引的規則）。

### 設定檔留在 repo 外

設定檔寫轉換器與 Modbus TCP 設備的位址或主機名稱，所以放在 repo 外（以路徑傳給指令），不提交。寫出的索引只有轉換器的邏輯代號與站號，沒有位址、主機名稱或 port，這是「索引格式」開頭的公開 repo 規則；上傳到物件儲存前另外檢查分段檔的文字值（見「存放位置與存取權限」）。

```toml
name = "lab"                       # 資料集名稱：輸出目錄與索引的 name
description = "一句話說明"
batch_s = 10                       # 每幾秒關一批（1–3600，預設 10）
segment_bytes = 67108864           # 分段檔的上限（1024 到 64 MiB，預設 64 MiB）

[[converters]]                     # RS485 轉換器（RTU over TCP）
key = "rs485-a"                    # 索引裡的邏輯代號
host = "<轉換器的位址>"            # 只在設定檔裡
port = 502

[[models]]                         # 型號：與 desired 設定的 DeviceModel 相同的讀法，加上索引的 kind、unit、wraps_at
name = "three-phase-meter"         # 通用的型號名稱，寫進索引的 model
points = [
  { key = "energy_kwh", function_code = 3, address = 0, data_type = "f32", kind = "cumulative", unit = "kWh" },
  { key = "pulses", function_code = 3, address = 2, data_type = "u32", kind = "cumulative", wraps_at = 4294967296 },
  { key = "power_kw", function_code = 4, address = 0, data_type = "u16", scale = 0.01, kind = "gauge", unit = "kW" },
]

[[devices]]
key = "meter-1"
model = "three-phase-meter"
converter = "rs485-a"              # 轉換器下的設備；Modbus TCP 設備改寫 host 與 port
unit = 1
interval_s = 60
```

資料點的欄位與 [protocol](../../crates/protocol/README.md) 的 `DataPoint` 相同（`function_code` 1–4、`address`、`data_type`、`byte_order`、`length`、`scale`、`offset`），寫進索引的 `type` 由它推出：`bool` 是 `bool`、整數型別是 `int`、`f32`/`f64` 是 `float`、`string` 是 `string`，設了 `scale` 或 `offset` 的一律是 `float`（agent 送出的值就是這樣）。`wraps_at` 只用於沒有 scale、offset 的 `u16`、`u32`、`u64` 累計值，介於 2 與 2 的位元數次方之間。

### 批次與接收時間

錄製沒有經過 ingest，所以批次與 `received_at` 由錄製工具決定：

- 每 `batch_s` 秒，把這段期間結束的輪詢讀到的值關成一批（超過協定的批次大小上限 256 KiB 時以 `BatchBuilder` 切成多批），全部在 `live` 一個串流，序號從 1 連續編號（與 [protocol](../../crates/protocol/README.md)「遙測與確認」規則 1 相同）。預設 10 秒，與閘道器雛形每 10 秒結束開啟中的批次相同（[閘道器記憶體可行性測試](../../docs/feasibility/2026-10-13-gateway-memory.md)），載入後 ingest 看到的到達節奏與閘道器上線後相近。
- `received_at` 是錄製工具關閉這批的時間，也就是「上行沒有延遲、沒有中斷」時 ingest 會蓋的時間；`ts` 是 agent 收到回應時的時間。兩者都來自同一個時鐘：開始時的系統時間加上之後經過的單調時間，錄製途中系統時鐘被校正也不會讓時間倒退或亂序。
- 錄製裡沒有補送、亂序或沒有完成紀錄的批次，這些狀況由合成 fixture 涵蓋。
- 分段檔在批次之間切開：下一批會讓檔案超過 `segment_bytes` 時先關檔（單一批次比上限大時自成一個分段）。

**限制：載入時每批一次 INSERT。** 載入工具照 ingest 的方式一批一次 INSERT，ClickHouse 每次 INSERT 產生一個 part。10 秒一批時，30 天的錄製約 259,200 批，載入工具不間斷地送出這麼多 INSERT，背景 merge 跟不上時，同一個 partition（遙測窄表依 `received_at` 的月份分）的 part 數超過 `parts_to_delay_insert` 會被延遲，超過 `parts_to_throw_insert` 會被拒絕（`TOO_MANY_PARTS`），載入在中途失敗（已寫入的批次留著，見「只載一次」）。目前不做緩解；錄長期資料時可以把 `batch_s` 拉長（60 秒時 30 天約 43,200 批）。若仍不夠，之後的選項是載入工具控制送出速度，或把連續幾批合成一次 INSERT（後者會讓「一批一個 part」不再成立，materialized view 看到的 INSERT 節奏也跟著改變）。

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
