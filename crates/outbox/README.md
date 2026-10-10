# outbox

跨服務寫入的事件框架（[決策 4](../../docs/architecture/decisions.md)）：產生端與業務資料在同一筆交易寫進 outbox，消費端依序、冪等地處理，可以從指定位置重播，接受最終一致。事件的格式（`EventEnvelope`）與演進規則在 [`crates/contract`](../contract/README.md)，本 crate 負責寫入、投遞、去重與重播。

| 模組 | 內容 |
| --- | --- |
| `schema` | 產生端與消費端的資料表（`PRODUCER_DDL`、`CONSUMER_DDL`、`install_*`） |
| `publish`、`write` | 在呼叫端的交易內寫入事件 |
| `Consumer`、`Handler` | 讀取產生端的事件、依序交給 handler、記錄進度 |
| `replay_from` | 把訂閱移到指定位置重新讀取 |
| `notify` | 通知模式：只存比目前更新的版本 |
| `lag` | 落後量指標 `outbox.consumer.lag`、`outbox.consumer.lag.age` |

範例服務 `src/bin/outbox-example.rs` 在同一個行程裡當產生端與消費端，整合測試以它做為真的行程來測當機與指標。

## 用法

### 資料表

所有 SQL 都不寫 schema 名稱，資料表落在連線 `search_path` 的第一個 schema，也就是該服務自己的 schema。因此每個服務的資料庫角色要設定 `search_path`（例如 `ALTER ROLE assets SET search_path = assets`）。migration Job 每次部署都執行 `install_producer`（產生事件的服務）與 `install_consumer`（消費事件的服務）；兩者都可重複執行，之後的變更只能以新的冪等語句往後加，不能改既有語句。

| 資料表 | 位置 | 內容 |
| --- | --- | --- |
| `outbox_event` | 產生端 | 事件紀錄，只增不改；`tenant_id` 給 RLS 用 |
| `outbox_aggregate` | 產生端 | 每個聚合最後一筆事件的序號與排序交易 |
| `outbox_subscription` | 消費端 | 每個訂閱讀到的位置，與它讀的產生端 schema |
| `outbox_inbox` | 消費端 | 已處理的事件 ID（去重） |
| `outbox_dead_letter` | 消費端 | 永久失敗、已跳過的事件與原因 |
| `outbox_version` | 消費端 | 通知模式已存的聚合版本 |

### 產生端

```rust
let mut tx = pool.begin().await?;
sqlx::query("UPDATE tenant SET status = 'suspended' WHERE id = $1").bind(id).execute(&mut *tx).await?;
outbox::publish(&mut tx, &Aggregate::new("tenant", id.to_string())?, &TenantSuspended { .. }, Some(tenant)).await?;
tx.commit().await?;
```

- `publish` 只接受交易（`&mut Transaction`），不能在交易外寫事件。交易回滾，事件與它的序號一起消失。
- 每個事件屬於一個聚合（`Aggregate`：種類加 ID，例如 `tenant` 加租戶 ID）。同一聚合的事件依寫入順序送達，不同聚合之間不保證順序。寫入時會鎖住該聚合在 `outbox_aggregate` 的列直到交易結束，同一聚合的寫入因此排隊；`REPEATABLE READ` 以上的隔離層級則會以序列化錯誤失敗，由呼叫端重試整筆交易。
- `Published::aggregate_seq` 是事件在聚合內的序號（1、2、3…），通知模式拿它當版本。

### 消費端

```rust
struct TenantSync;

impl Handler for TenantSync {
    async fn begin(&self, tx: &mut Transaction<'static, Postgres>, delivery: &Delivery) -> Result<(), HandlerError> {
        // 決策 29：交易開頭設定租戶（Laysi/trustiot-v2#53 提供的做法）
        Ok(())
    }

    async fn handle(&self, tx: &mut Transaction<'static, Postgres>, delivery: &Delivery) -> Result<(), HandlerError> {
        let event: TenantSuspended = delivery.envelope.decode()?;
        sqlx::query("...").execute(&mut **tx).await?;
        Ok(())
    }
}

let config = ConsumerConfig::new("assets.tenant-status", "identity")?;
let consumer = Consumer::new(pool.clone(), config, TenantSync);
let stop = context.shutdown().stopping_token();
context.spawn(async move {
    if let Err(error) = consumer.run(stop).await {
        tracing::error!(error = %error, "outbox consumer stopped");
    }
});
```

- 訂閱名稱在消費端服務內唯一、永遠不改；改名等於新訂閱，從頭（或 `StartFrom::Latest`）開始。一個訂閱只讀一個產生端，換產生端會被拒絕（`SourceMismatch`）。
- 每個事件一筆交易：`begin` → 取得訂閱的鎖 → 查 inbox → `handle` → 記錄事件 ID → 推進位置 → commit。handler 的寫入與「已處理」同時生效：commit 前當機或出錯都不留下任何東西，事件之後會再送一次。
- handler 只透過 `tx` 寫資料庫。資料庫以外的副作用（呼叫、通知）可能執行多次，要自己冪等。
- `run` 在 `Shutdown::stopping` 之後停止，正在處理的事件會先做完；建立 `Consumer` 前要先初始化遙測（`service_kit::run` 已經做了），lag 指標才會接到 OTLP。

## 投遞機制（2026-10-10 定案）

**消費端以自己的資料庫角色直接輪詢產生端 schema 的 `outbox_event`**，不經訊息佇列，也不由產生端轉送。所有服務共用同一個 Postgres，消費端只被授權讀這一張表（見[資料庫權限](#資料庫權限)），讀到的事件在消費端自己的交易裡處理並記錄進度。

| 方案 | 優點 | 缺點 | 結論 |
| --- | --- | --- | --- |
| 輪詢（採用） | 只靠 Postgres；當機、重啟、斷線都不掉事件；重播只是把位置往回移 | 延遲上限是輪詢間隔（預設 1 秒）；閒置時每個訂閱每秒一個查詢 | 採用。事件是控制面的低頻事件，1 秒延遲可接受 |
| LISTEN/NOTIFY | 延遲低 | 通知不持久，斷線期間的通知會掉，仍要輪詢補；NOTIFY 在 commit 時取得全域鎖，所有寫事件的交易會互相排隊 | 不用。日後要降延遲，可以加成「喚醒輪詢」的提示，不影響保證 |
| 產生端轉送（SKIP LOCKED 佇列或推送） | 消費端不碰產生端的表 | 產生端要知道所有訂閱並為每個訂閱保存進度；多一個轉送元件與一次網路傳遞；重播要產生端配合 | 不用 |

`SKIP LOCKED` 的角色由交易層級的 advisory lock 取代：同一訂閱的多個副本以 `pg_try_advisory_xact_lock` 輪流，拿不到鎖的副本這一輪跳過（`Outcome::Busy`）。不用列鎖（`FOR UPDATE SKIP LOCKED`）是因為鎖列會立刻讓交易取得交易 ID，下一節的讀取上限會因此被 handler 的執行時間拖住。

### 順序與不漏讀

用遞增的 ID 排序會漏事件：ID 在寫入時配發，ID 11 的交易可能比 ID 10 先 commit，消費端讀到 11 就跳過了之後才出現的 10。本 crate 這樣排序：

1. 每筆事件記錄 `tx_order`：寫入它的交易 ID（`pg_current_xact_id()`），但不小於同一聚合上一筆事件的 `tx_order`（由 `outbox_aggregate.last_tx` 帶過來）。位置是 `(tx_order, id)`。
2. 消費端先取得讀取上限 `pg_snapshot_xmin(pg_current_snapshot())`，也就是仍在執行的最舊交易 ID，下一個查詢只讀 `tx_order` 小於它的事件。
3. 不漏讀：一筆事件的 `tx_order` 不小於它自己的交易 ID。它的交易還在執行時，讀取上限不會超過這個 ID，所以已讀的位置都在它之前；之後才開始的交易，ID 也不小於當時的上限。因此 commit 得晚的事件，位置一定排在已讀的位置之後。
4. 聚合內有序：同一聚合的寫入以 `outbox_aggregate` 的列鎖排隊，後寫的事件 `tx_order` 不小於前一筆、`id` 也比較大。只用交易 ID 排序時，先取得 ID、後寫入的交易會排到前面（測試 `an_aggregate_keeps_its_order_when_transaction_ids_are_inverted` 重現這個情況）。

代價：讀取上限是整個 Postgres 叢集最舊的「有交易 ID」的交易。任何服務的長時間寫入交易（含 handler 寫入後才做慢的事）都會讓所有訂閱停在它之前，直到它結束；唯讀交易（例如 pg_dump）沒有交易 ID，不受影響。所以：

- handler 先做慢的讀取（例如通知模式呼叫內部 API），再寫入；寫入到 commit 之間要短。
- 寫事件的交易不要包著外部呼叫。
- lag 指標會反映這種停滯：已 commit 但讀不到的事件也計入。

## 保證與去重

- **同交易**：事件與業務資料一起 commit 或一起回滾（測試 `a_rolled_back_transaction_leaves_no_event`）。
- **至少一次投遞**：事件 commit 後就在產生端的表裡，產生端當機不影響投遞（`an_event_committed_before_the_producer_is_killed_is_delivered_after_restart`）；消費端在 commit 前當機，事件會再送一次（`a_consumer_killed_mid_delivery_processes_the_event_once_after_restart`）。
- **每個事件 ID 只處理一次**：處理與記錄事件 ID、推進位置在同一筆交易；再次送達（重播、副本競爭）時查到 inbox 就跳過（`Progress::duplicates`）。推進位置時只從開始處理時的位置推進，位置被別人移動過就整筆回滾重讀。
- **同一聚合依序**；同一訂閱一次只處理一個事件。

## 重播

```rust
let mut tx = pool.begin().await?;
outbox::replay_from(&mut tx, "assets.tenant-status", Position::START, Replay::Reprocess).await?;
sqlx::query("DELETE FROM tenant_copy").execute(&mut *tx).await?; // 要重建的狀態，在 replay_from 之後
tx.commit().await?;
```

- `replay_from(tx, 訂閱, 位置, 模式)` 讓下一次輪詢從該位置之後開始讀；`Position::START` 是整份紀錄，`position.just_before()` 會包含該事件。它會等正在處理的事件做完，並持有訂閱的鎖到交易結束，所以要在交易裡先呼叫它，再動 handler 會寫的資料表；順序反過來時可能與處理中的事件互相等待而死結。
- `Replay::SkipProcessed`：已處理的事件仍會跳過，只有沒處理過的（含死信）會進 handler。用於安全地重讀一段範圍。
- `Replay::Reprocess`：忘記該位置之後的處理紀錄，handler 會再看到每一個事件。用於重建狀態，並在同一筆交易清掉要重建的狀態。
- 位置寫成 `<tx>:<id>`（`Position` 實作 `Display` 與 `FromStr`），可以從 `outbox_event` 或 `Delivery::position` 取得。

## 通知模式（B1）

事件只代表「這個聚合變了」，消費端讀產生端的目前狀態收斂，不依賴事件內容：

1. 產生端的內部 API 回傳目前狀態，與 `outbox::aggregate_version` 讀到的版本，兩者在同一個快照讀（單一查詢，或 `REPEATABLE READ` 交易）。版本比狀態晚讀，會把舊狀態標成新版本。
2. 消費端 handler 先呼叫 API（讀取，不佔交易 ID），再以 `notify::store_if_newer(tx, scope, 聚合, 版本)` 判斷：版本比已存的新才寫入，否則什麼都不做。這一列會鎖到交易結束，同時處理同一聚合的副本會排隊再比較。
3. 可選：`notify::stored_version` 不小於 `delivery.aggregate_seq` 時，已存的狀態已經包含這個事件，可以不呼叫 API。

晚到的事件、重播的舊事件、副本競爭都只會讀到目前狀態或被版本擋下，不會用舊狀態蓋掉新的（測試 `a_stale_reading_never_overwrites_a_newer_one`、`late_and_replayed_events_converge_on_the_current_state`）。

## 失敗處理

| handler 回傳 | 處理 |
| --- | --- |
| `HandlerError::retry(原因)`、`sqlx::Error`、`DecodeError::NewerVersion` | 回滾，以退避（輪詢間隔加倍，上限 `max_backoff`，預設 30 秒）重試同一個事件；後面的事件等它，順序不變。每次失敗記一筆 WARN |
| `HandlerError::permanent(原因)`、其他 `DecodeError`、無法解析的信封 | 回滾 handler 的寫入，另開交易把事件記進 `outbox_dead_letter` 並跳過，記一筆 ERROR |

- 原因（`reason`）會記進日誌與死信表，不能含事件內容或個資；底層錯誤只記 SQLSTATE，因為錯誤訊息可能引用資料值（M2）。`ConsumerError` 與 `PublishError` 的文字同樣只有 SQLSTATE 或失敗種類，原始錯誤仍可由 `source()` 取得。
- 一直重試的事件會讓訂閱停住，lag 指標隨之上升；死信事件可在修正後以 `replay_from` 重新處理。

## Lag 指標

`Consumer::run` 每 `lag_interval`（預設 15 秒）量一次，經 OpenTelemetry 的全域 meter 匯出；service-kit 設定 `telemetry.otlp_endpoint` 後送到 `<url>/v1/metrics`，匯出間隔是 `OTEL_METRIC_EXPORT_INTERVAL`（預設 60 秒）。

| 指標 | 單位 | 意義 |
| --- | --- | --- |
| `outbox.consumer.lag` | `{event}` | 訂閱位置之後、已 commit 的事件數，最多數到 10 000 |
| `outbox.consumer.lag.age` | `s` | 其中最舊一筆寫入至今的秒數；沒有等待中的事件時為 0 |

屬性：`outbox.subscription`（訂閱名稱）、`outbox.source`（產生端 schema）。`Consumer::lag()` 與 `measure_lag()` 在程式內讀同樣的值。建議的告警：age 持續超過數分鐘（handler 一直失敗，或有長時間交易）。

## 與租戶隔離（決策 29）的配合

決策 29 的 RLS 與 `SET LOCAL` 由 Laysi/trustiot-v2#53 實作。本 crate 不處理租戶，設計成在那樣的交易裡也能運作：

- **產生端**：`publish` 只在呼叫端的交易裡執行兩個語句，不另開交易、不改設定，所以在交易開頭設定租戶之後呼叫即可。`outbox_event.tenant_id` 來自信封（呼叫端傳入的租戶）；#53 為產生端角色加上 `WITH CHECK (tenant_id = 目前租戶)` 的 INSERT policy，寫錯租戶會被資料庫擋下。插入不使用 `RETURNING`，所以不需要 SELECT policy。全平台事件（`tenant_id` 為 NULL）是否允許、由哪個角色寫，由 #53 決定。
- **`outbox_aggregate`**：只有聚合種類、ID 與序號，不是租戶資料；產生端角色需要讀寫它。
- **消費端讀取**：投遞本來就跨租戶，消費端角色對產生端 `outbox_event` 需要一條不限租戶的 SELECT policy（例如 `USING (true)`，只給消費端角色）。讀取在消費端自己的唯讀交易裡進行，與 handler 的交易分開。
- **消費端處理**：每個事件一筆交易，`Handler::begin` 是交易裡的第一步，在本 crate 碰任何資料表之前，於此依 `Delivery::tenant()` 設定租戶；之後 handler 的讀寫都在該租戶之下。消費端自己的 `outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version` 不帶租戶資料，不需要 RLS。

## 資料庫權限

服務的角色不是資料表的擁有者（決策 29），migration 角色建立資料表後授權（`tests/delivery.rs` 的 `services_need_only_the_documented_grants` 以這組權限實際跑過一次）：

```sql
-- 產生端（例：identity）
GRANT USAGE ON SCHEMA identity TO identity;
GRANT SELECT, INSERT, UPDATE ON identity.outbox_aggregate TO identity;
GRANT INSERT ON identity.outbox_event TO identity;

-- 消費端（例：assets 讀 identity 的事件）
GRANT USAGE ON SCHEMA identity TO assets;
GRANT SELECT ON identity.outbox_event TO assets;   -- 產生端只開放這張表
GRANT USAGE ON SCHEMA assets TO assets;
GRANT SELECT, INSERT, UPDATE, DELETE
    ON assets.outbox_subscription, assets.outbox_inbox, assets.outbox_dead_letter, assets.outbox_version
    TO assets;
```

消費端角色看不到產生端的其他資料表，也不能寫產生端的事件。

## 限制與後續

- 保留期限：`outbox_event` 與 `outbox_inbox` 目前只增不刪。事件層也是操作紀錄（[現場脈絡](../../docs/architecture/field-context.md) AI 記憶第 4 點），保留與查詢介面由 Laysi/trustiot-v2#130 一併決定；刪除事件前要先確認每個訂閱都已讀過。
- 讀取上限受整個叢集最舊的寫入交易影響，見[順序與不漏讀](#順序與不漏讀)。
- 每個事件一筆交易，適合控制面的事件量；大量資料（遙測）不走 outbox（決策 8）。

## 測試

需要 `TEST_DATABASE_URL`（可建立 schema 與角色的 Postgres 帳號），或 podman／docker（每個測試起一個用完即刪的 `postgres:18.6-alpine` 容器）。都沒有時在本機略過並印出 `skipped:`；設了 `CI` 時改為失敗。

| 指令 | 內容 |
| --- | --- |
| `cargo test -p outbox --lib` | 聚合、位置、設定的檢查 |
| `cargo test -p outbox --test producer` | 回滾不留事件、聚合序號、同一信封只能寫一次 |
| `cargo test -p outbox --test delivery` | 交易 ID 反轉時仍依序、晚 commit 的事件不漏、並行寫入加兩個副本每個事件只處理一次、重複投遞跳過、重播後狀態相同、重試與死信、`begin` 在同一交易、未寫入的 handler 不拖住其他訂閱、收尾時只做完進行中的事件、lag、`StartFrom::Latest`、最小權限 |
| `cargo test -p outbox --test notify` | 舊讀數不覆蓋新狀態（含副本競爭）、晚到與重播收斂 |
| `cargo test -p outbox --test crash` | 以 SIGKILL 結束 `outbox-example`：commit 後投遞前、handler 寫入後 commit 前，重啟後各處理一次 |
| `cargo test -p outbox --test metrics` | lag 經 OTLP/HTTP 匯出到假的接收端，解碼後讀到數值 |
