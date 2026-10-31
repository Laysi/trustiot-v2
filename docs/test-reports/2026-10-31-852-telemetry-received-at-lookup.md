# 遙測表依接收時間找列：projection、skip index 或不改（2026-10-31）

Laysi/trustiot-v2#852：Laysi/trustiot-v2#837 之後，`lab-dataset export` 每個讀取視窗依接收時間（`received_at`）讀，但遙測表的主鍵 `(tenant_id, device_id, point, ts)` 不含它，所以一次讀取仍讀到回傳列數的數倍。這份報告量了兩個候選，依據它決定不改表。決定與重新評估的條件寫在 [datasets/lab/README.md](../../datasets/lab/README.md)「依接收時間找列的索引（評估過，不加）」。

## 環境

- **程式碼**：分支 `perf/852-telemetry-received-at-lookup`，基於 main `61dd8437`。量測在 `tools/lab-dataset/tests/export_scale.rs` 的 `received_at_lookup_candidates`（`#[ignore]`）。報告的數字來自本分支的測試程式；之後只改了模組說明的文字。
- **機器**：建置主機（x86_64 Linux）。同一個量測也在建置主機（arm64 Linux VM）跑過一次，三個候選、三種情境的讀取視窗、讀最多的視窗與一小時讀取的 `read_rows` 都相同，整次匯出的加總差不到 0.4%（完成表的 merge 狀態不同）。
- **工具與服務**：`.prototools` 釘選的 Rust；ClickHouse 26.8.22.13（`services/ingest/tests/common/mod.rs` 釘選的映像，拋棄式容器，套用產品的全部 ClickHouse migration）。匯出行程是 `lab-dataset` 指令本身。
- **資料**：測試行程在拋棄式 ClickHouse 裡以 `INSERT ... SELECT` 產生，不進 repo。每台閘道器與 Laysi/trustiot-v2#837 相同：30 天、每分鐘一批，兩台設備各五個資料點，432,000 列、43,200 批與完成紀錄。三種情境各一個租戶：
  - **依序上傳**：一台閘道器。
  - **四台閘道器**：同一個租戶四台這樣的閘道器，匯出其中一台。
  - **時鐘錯**：一台閘道器，第 15 天的讀值時間比接收時間早 30 天（1,440 批）。
  - 另有一個相鄰的租戶（UUID 排在第一個租戶之後）在範圍結束前一小時收到一批，讓兩個租戶的列在表的排序與 projection 的排序裡都共用邊界的 granule。
  - 合計 2,592,010 列。每個候選開始前 `OPTIMIZE TABLE ... FINAL`，每個月分區一個 part。

## 指令

在 repo 根目錄執行（建置主機上）：

```sh
cargo test --release --locked -p lab-dataset --test export_scale received_at_lookup_candidates -- --ignored --nocapture
```

量測的做法（測試的模組說明與程式）：

- **帳號**：每種情境一個匯出帳號，只有兩張表的 `SELECT` 與兩條 row policy，寫法與 `datasets/lab/README.md`「讀取帳號」的範例相同，只點名自己的租戶。產生資料後先確認每個帳號看到的遙測列數與完成紀錄數正好是自己租戶的。
- **候選**：依序是不加、`ALTER TABLE trustiot.telemetry ADD INDEX lookup_received_at received_at TYPE minmax GRANULARITY 1` 加 `MATERIALIZE INDEX`、`ADD PROJECTION lookup_received_at (SELECT * ORDER BY tenant_id, gateway_id, received_at)` 加 `MATERIALIZE PROJECTION`，都帶 `mutations_sync = 2`。量完一個就移除，再加下一個。
- **每個候選、每種情境**：
  - 以指令匯出 30 天，列數必須是 432,000，檔案與不加時的逐位元組相同。
  - 從 `system.query_log` 依種類取這次匯出的查詢次數、回傳列數、`read_rows`，以及有幾句用到 projection（`projections` 欄）。
  - 印出讀最多列的那個視窗的 `EXPLAIN indexes = 1, projections = 1`，用匯出帳號執行。
  - 只用接收時間（加租戶與閘道器）數範圍中間一小時的列，必須是 600。
- **每個候選各一次**：
  - 租戶帳號（`trustiot_tenant_reader`，與 telemetry-query 建的相同）經 `trustiot.tenant_telemetry` 讀最後兩天，條件是 `gateway_id IN` 自己的與相鄰租戶的閘道器、接收時間的範圍，以及 `throwIf(tenant_id != <自己的租戶>)`。別的租戶的列必須先被 definer 的 row policy 去掉，條件才不會拋錯。不加與 skip index 時斷言回傳 28,800 列；projection 時記錄結果。
  - 以 ingest I1 的方式（`DELETE FROM ... SETTINGS lightweight_deletes_sync = 2`）刪一批。
- **可行性探測**：寫 Rust 量測之前，先以一個 shell 腳本對同一個映像做過探測（[probe-script.txt](data/2026-10-31-852-telemetry-received-at-lookup/probe-script.txt)，在建置主機上執行，不進 repo 的程式）。兩個租戶各 432,000 列，看了以下幾件事：
  - 兩個候選在 row policy 帳號下是否被用上。
  - projection 的 merge tree 設定預設值。
  - `lightweight_mutation_projection_mode` 的 `throw` 與 `rebuild`。
  - 只存 `_part_offset` 的 projection。

## 結果

原始資料：

- [lookup-report.json](data/2026-10-31-852-telemetry-received-at-lookup/lookup-report.json)：量測印出的報告。
- [lookup-run.log](data/2026-10-31-852-telemetry-received-at-lookup/lookup-run.log)：完整輸出，含每個情境讀最多的視窗的 `EXPLAIN`。
- [probe.log](data/2026-10-31-852-telemetry-received-at-lookup/probe.log)：探測的輸出。

### 匯出的讀取量（`read_rows`）

| 情境 | 項目 | 不加 | skip index | projection |
| --- | --- | ---: | ---: | ---: |
| 依序上傳 | 讀取視窗（16 句） | 2,670,592 | 2,670,592 | 565,248 |
| 依序上傳 | 讀最多的視窗（回傳 28,670 列） | 245,760 | 245,760 | 40,960 |
| 依序上傳 | 整次匯出 | 4,488,792 | 4,488,792 | 2,383,448 |
| 四台閘道器 | 讀取視窗 | 10,387,456 | 10,387,456 | 573,440 |
| 四台閘道器 | 讀最多的視窗 | 991,232 | 991,232 | 40,960 |
| 四台閘道器 | 整次匯出 | 14,824,896 | 14,824,896 | 2,422,208 |
| 時鐘錯 | 讀取視窗 | 2,732,484 | 2,650,564 | 560,394 |
| 時鐘錯 | 整次匯出 | 4,361,402 | 4,279,482 | 2,189,312 |

- **整次匯出**：除了讀取視窗，還有普查、重複讀值、規劃與完成紀錄。projection 只用在讀取視窗（依序上傳時 44 句裡 16 句用到；四台閘道器時 18 句，規劃與普查也有一部分用到）。
- **與 #837 的數字不能直接比**：依序上傳的讀取視窗（267 萬列）比 Laysi/trustiot-v2#837 報告的 217 萬列多，因為資料在 part 裡的配置不同。這裡每個月分區 `OPTIMIZE FINAL` 成一個 part，而且表裡有三個租戶、六台閘道器；#837 是 5 個未合併的 part、只有一台閘道器。要比的是同一份資料上候選之間的差。
- **四台閘道器讀約四倍**：讀取視窗的主鍵只用得到 `tenant_id` 與 `ts`（`EXPLAIN` 的 PrimaryKey 是 `tenant_id`、`ts`，generic exclusion search）。閘道器不在主鍵裡，所以讀到這個租戶每台閘道器在那段讀值時間的 granule。
- **skip index 對匯出沒有效果**：依序上傳時，視窗的讀值時間範圍已經把 granule 縮到與接收時間相同的那些。它只在時鐘錯的情境少讀 3.0%（2,732,484 → 2,650,564）。

### 只用接收時間讀一小時（600 列）

| 情境 | 不加 | skip index | projection |
| --- | ---: | ---: | ---: |
| 依序上傳 | 417,792 | 163,840 | 8,192 |
| 四台閘道器 | 1,646,592 | 663,552 | 8,192 |
| 時鐘錯 | 410,810 | 163,840 | 8,192 |

這種讀法沒有讀值時間的條件，skip index 讓它少讀約 2.5 倍。探測裡兩天範圍的讀取也一樣：不加 393,216 列，skip index 204,800 列，與加上讀值時間條件時的 204,800 列相同。

### 儲存與套用

| 候選 | 整張表的 part（2,592,010 列） | 候選本身 | `MATERIALIZE`（x86_64 / arm64） |
| --- | ---: | ---: | ---: |
| 不加 | 21,818,699 bytes | — | — |
| skip index | 21,820,340 bytes | 3,500 bytes（壓縮後加 marks） | 0.3 s / 1.1 s |
| projection | 43,667,960 bytes | 21,851,364 bytes | 2.0 s / 1.0 s |

`SELECT *` 的 projection 等於多存一份資料。探測的兩個租戶也是約一倍；只存 `_part_offset` 的 projection 反而更大（4.4 MB，完整 projection 3.7 MB）。

### 輕量 DELETE

- 不加、skip index：成功。
- projection：被拒絕，`Code: 344 ... DELETE query is not allowed for table trustiot.telemetry because as it has projections and setting lightweight_mutation_projection_mode is set to THROW`（`SUPPORT_IS_DISABLED`）。
- 探測：`ALTER TABLE ... MODIFY SETTING lightweight_mutation_projection_mode = 'rebuild'` 之後可以刪，projection 也還被用上，但每次刪除都要重建碰到的 part 的 projection。
- 探測讀到的 merge tree 預設值：`lightweight_mutation_projection_mode = throw`、`deduplicate_merge_projection_mode = throw`、`materialize_projections_on_insert = 1`。

### row policy 與租戶隔離

- **不加與 skip index**：匯出帳號的 `EXPLAIN` 都有獨立的 `Row level filter`（`tenant_id = <租戶>`），呼叫者的條件在 `Prewhere filter`。
- **projection 被用上時**：`EXPLAIN` 沒有 `Row level filter`，policy 的條件被接在呼叫者條件的後面，一起放進 PREWHERE，例如：

  ```text
  Prewhere filter column:  tenant_id = '…5ca1e' AND gateway_id = 'scale-gateway' AND received_at < … AND ts >= … AND tenant_id = '…5ca1e'
  ```

  - 依序上傳的情境見 [lookup-run.log](data/2026-10-31-852-telemetry-received-at-lookup/lookup-run.log) 的 `EXPLAIN-BEGIN projection, in-order`。
  - 探測裡的帳號 `ra` 也是這樣。
- **經租戶 view 的 `throwIf` 讀取**：三個候選都回傳 28,800 列、沒有拋錯。三次都沒有用到 projection（`used_projections` 是空的），所以這只證明「projection 沒被用上時隔離不變」，不能證明 projection 被用上時 policy 仍先於呼叫者的條件。
- **匯出帳號的 policy 有效**：三個候選、三種情境的匯出都只讀到自己租戶的列：列數 432,000，檔案逐位元組相同，產生資料後的計數也正確。

### 其他依接收時間讀遙測表的程式

| 位置 | 條件 | 評估 |
| --- | --- | --- |
| `services/ingest/src/telemetry/store.rs`：`LEFTOVER_TENANTS`、`DELETE_ROWS`、`BATCH_ROWS` | `(device_id, point) IN`、`ts BETWEEN`、閘道器、串流、序號、`received_at >=`（對帳窗口） | 主鍵已縮到這批的 granule，接收時間只選 partition；projection 會讓 `DELETE_ROWS` 失敗 |
| `services/ingest/src/rollup.rs` 對帳 | 租戶、`(device_id, point) IN`、讀值時間的桶、閘道器、串流、`toDate(received_at) >=` | 同上 |
| `0010_telemetry_rollups.sql` 的兩個 backfill view | 只有 `toDate(received_at) BETWEEN` | skip index 可以在月分區內剪 granule；只在部署彙總或修補時跑 |
| 租戶 view（`tenant_telemetry`） | 可查期間（讀值時間）與租戶自己的條件 | 產品畫面依讀值時間查 |
| `lab-dataset export` | 每個視窗有讀值時間的範圍 | 見上 |

## 結論

決定：**不改遙測表**。projection 會讓 I1 的輕量 DELETE 失敗，多存一倍，而且讀取時 row policy 不再是獨立的 filter；skip index 對匯出與其他現有的讀取幾乎沒有效果。理由與重新評估的條件在 `datasets/lab/README.md`。

| 驗收條件 | 結果 |
| --- | --- |
| 做法有決定並寫在 `datasets/lab/README.md`「匯出」；改 schema 時也寫在 `services/ingest/README.md`，含對寫入、儲存與 merge 的影響 | 通過。決定是不改，寫在「依接收時間找列的索引（評估過，不加）」：數字、理由、其他讀取的評估、重新評估的條件。沒改 schema，`services/ingest/README.md` 的遙測表一節仍加了一條說明與連結，免得之後的人重複評估。 |
| 改 schema 時：migration 在拋棄式 ClickHouse 上套用、既有資料 `MATERIALIZE` 的測試通過；30 天資料匯出的 `read_rows` 與改之前比較記在 `docs/test-reports/` | 不適用（沒有改 schema）。兩個候選仍在拋棄式 ClickHouse 上加進表並 `MATERIALIZE`；30 天匯出的 `read_rows` 對照在上面的表。 |
| 不改時：理由與重新評估的條件寫在 `datasets/lab/README.md`「匯出」 | 通過。 |

待測（之後重新評估時才需要）：

- projection 被用上時，經租戶 view 的 `throwIf` 讀取是否仍不拋錯。這次的 view 讀取沒有用到 projection。
- 正式環境大小（數億列）時 `MATERIALIZE` 的時間。這次只量了 259 萬列。
