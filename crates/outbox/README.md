# outbox

跨服務寫入的事件框架（[決策 4](../../docs/architecture/decisions.md)）：產生端與業務資料在同一筆交易寫進 outbox，消費端依序、冪等地處理，可以從指定位置重播，接受最終一致。事件的格式（`EventEnvelope`）與演進規則在 [`crates/contract`](../contract/README.md)，本 crate 負責寫入、投遞、去重與重播。

| 模組 | 內容 |
| --- | --- |
| `schema` | 產生端與消費端的資料表（`PRODUCER_DDL`、`CONSUMER_DDL`、`install_*`） |
| `publish`、`publish_as`、`write` | 在呼叫端的交易內寫入事件（`publish_as` 另記誰做了變更） |
| `Consumer`、`Handler` | 讀取產生端的事件、依序交給 handler、記錄進度 |
| `replay_from` | 把訂閱移到指定位置重新讀取 |
| `notify` | 通知模式：只存比目前更新的版本 |
| `lag` | 落後量指標 `outbox.consumer.lag`、`outbox.consumer.lag.age` |

範例服務 `src/bin/outbox-example.rs` 在同一個行程裡當產生端與消費端，整合測試以它做為真的行程來測當機與指標。

## 用法

### 資料表

所有 SQL 都不寫 schema 名稱，資料表落在連線 `search_path` 的第一個 schema，也就是該服務自己的 schema。因此每個服務的資料庫角色要設定 `search_path`（例如 `ALTER ROLE assets SET search_path = assets`）。migration Job 每次部署都執行 `install_producer`（產生事件的服務）與 `install_consumer`（消費事件的服務）；兩者都可重複執行，之後的變更只能以新的冪等語句往後加，不能改既有語句。產生端在 `install_producer` 之後還要各執行一次 service-kit 的兩個範本：[`sql/outbox-event.sql`](../service-kit/sql/outbox-event.sql)（`outbox_event` 的 RLS）與 [`sql/outbox-aggregate.sql`](../service-kit/sql/outbox-aggregate.sql)（`outbox_aggregate` 的租戶欄位與 RLS，見[聚合的租戶隔離](#聚合的租戶隔離laysitrustiot-v24132026-10-15)），migration Job 都會做。

| 資料表 | 位置 | 內容 |
| --- | --- | --- |
| `outbox_event` | 產生端 | 事件紀錄，只增不改；`tenant_id` 給 RLS 用 |
| `outbox_aggregate` | 產生端 | 每個聚合最後一筆事件的序號與排序交易；`tenant_id` 是聚合所屬的租戶（全平台的聚合為 NULL），給 RLS 用 |
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
- 人經 API 做的變更用 `publish_as`，多帶一個 `contract::event::Actor`：`Actor::User { user_id }`（請求的使用者，`UserIdentity::user_id()`）或服務自己的工作 `Actor::Service { service }`。它寫進信封的 `actor`，身分服務的事件紀錄（操作紀錄，K2）以它記下「誰」（[services/identity](../../services/identity/README.md) 的「事件層與操作紀錄」，Laysi/trustiot-v2#130）。`publish` 不帶 actor。
- 每個事件屬於一個聚合（`Aggregate`：種類加 ID，例如 `tenant` 加租戶 ID）。同一聚合的事件依寫入順序送達，不同聚合之間不保證順序。寫入時會鎖住該聚合在 `outbox_aggregate` 的列直到交易結束，同一聚合的寫入因此排隊；`REPEATABLE READ` 以上的隔離層級則會以序列化錯誤失敗，由呼叫端重試整筆交易。
- `Published::aggregate_seq` 是事件在聚合內的序號（1、2、3…），通知模式拿它當版本。
- 聚合屬於它的事件的租戶，或屬於平台（全平台事件）：第一筆事件決定。之後別的租戶（或平台角色）為它寫事件，資料庫以 SQLSTATE 42501 拒絕，該聚合的序號不變（見[聚合的租戶隔離](#聚合的租戶隔離laysitrustiot-v24132026-10-15)）。所以聚合 ID 要是服務自己產生、全平台唯一的值（例如 UUID），不能是租戶自己選的值（例如租戶輸入的序號）；要用這種值時，在前面加上租戶 ID。

### 消費端

```rust
struct TenantSync;

impl Handler for TenantSync {
    async fn handle(&self, tx: &mut Transaction<'static, Postgres>, delivery: &Delivery) -> Result<(), HandlerError> {
        // tx 已在 delivery.tenant() 的租戶裡（service-kit 設定）；RLS 只讓它讀寫這個租戶的列
        let event: TenantSuspended = delivery.envelope.decode()?;
        sqlx::query("...").execute(&mut **tx).await?;
        Ok(())
    }
}

// 啟動時：平台角色的 pool 讀事件，服務角色的背景 pool 處理事件（決策 29）
let platform = PlatformDb::connect_lazy(platform_role_options, PgPoolOptions::new().max_connections(2))?;
let (tenant_db, background) = TenantDb::connect_lazy_with_background(
    service_role_options,
    PgPoolOptions::new().max_connections(10),
    ServiceIdentity::new(ServiceName::Assets),
)?;
let config = ConsumerConfig::new("assets.tenant-status", "identity")?;
let consumer = Consumer::new(&platform, background, config, TenantSync);
let stop = context.shutdown().stopping_token();
context.spawn(async move {
    if let Err(error) = consumer.run(stop).await {
        tracing::error!(error = %error, "outbox consumer stopped");
    }
});
```

- 訂閱名稱在消費端服務內唯一、永遠不改；改名等於新訂閱，從頭（或 `StartFrom::Latest`）開始。一個訂閱只讀一個產生端，換產生端會被拒絕（`SourceMismatch`）。
- `Consumer::new(&PlatformDb, BackgroundTenantDb, ...)`：讀事件用消費端服務的**平台角色**，處理事件與消費端自己的表用**服務角色**（見[與租戶隔離的配合](#與租戶隔離決策-29的配合)）。兩個 pool 都由 service-kit 建立，所以一定驗證伺服器憑證並檢查每個連線。
- 每個事件一筆服務角色的交易，開在事件的租戶裡：設定租戶 → `begin` → 取得訂閱的鎖 → 查 inbox → `handle` → 記錄事件 ID → 推進位置 → commit。handler 的寫入與「已處理」同時生效：commit 前當機或出錯都不留下任何東西，事件之後會再送一次。`begin` 是可選的掛勾，給 `statement_timeout` 這類交易設定用；租戶不由 handler 設定。
- handler 只透過 `tx` 寫資料庫。資料庫以外的副作用（呼叫、通知）可能執行多次，要自己冪等。
- handler 不得結束 `tx`（`COMMIT`、`ROLLBACK`），也不得改它的租戶或角色（`trustiot.tenant_id`、`SET ROLE`）。`begin` 回來後（在取得訂閱的鎖之前，所以也涵蓋 inbox 判定為重複、不會呼叫 `handle` 的投遞）與 `handle` 回來後，消費端各在同一筆交易查一次：交易仍是開始時那一筆（呼叫 handler 前以 `set_config('outbox.delivery', <UUID>, true)` 設的交易層級標記還在；交易結束標記就消失。不用 `transaction_timestamp()`，因為兩筆交易的時間可能相同），租戶仍是事件的租戶。不符時回滾還開著的部分，記一筆 ERROR，把事件記進死信（`handler ended the delivery's transaction` 或 `handler changed the delivery's tenant`），不重試，因為每次重試都會再犯。handler 自己 `COMMIT` 之前的寫入在消費端拿回控制權時已經 commit，無法撤回；這個檢查保證的是事件不會被處理第二次，並留下紀錄。`begin` 破壞交易時 `handle` 不會被呼叫，所以不會有 `handle` 的寫入在交易外逐句 commit。切換租戶後在回來前又切回原租戶的寫法查不出來，依決策 29 由 `tools/sql-check` 擋設定租戶的 SQL。
- `run` 在 `Shutdown::stopping` 之後停止，正在處理的事件會先做完；建立 `Consumer` 前要先初始化遙測（`service_kit::run` 已經做了），lag 指標才會接到 OTLP。查詢失敗會退避重試，handler 或輪詢 panic 也會被攔下（見[失敗處理](#失敗處理)），所以只有 `stopping` 會讓處理停止。lag 由另一個 task 量測，不依賴處理迴圈（見 [Lag 指標](#lag-指標)）。

## 投遞機制（2026-10-10 定案）

**消費端以自己的資料庫角色直接輪詢產生端 schema 的 `outbox_event`**，不經訊息佇列，也不由產生端轉送。所有服務共用同一個 Postgres，消費端的平台角色只被授權讀這一張表（見[資料庫權限](#資料庫權限)），讀到的事件在消費端服務角色的交易裡處理並記錄進度。

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
- lag 指標會反映這種停滯：已 commit 但讀不到的事件也計入。長寫入交易自己的事件在它 commit 前不算。影響範圍與監控見[長寫入交易與 prepared transaction](#長寫入交易與-prepared-transaction)。

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
| 不論回傳什麼，`begin` 或 `handle` 結束了交易或改了租戶（見[消費端](#消費端)） | 同上，記進死信；handler 自己 `COMMIT` 之前的寫入已經 commit |
| `begin` 或 `handle` panic | 消費端攔下 panic，回滾，記一筆 ERROR，以同樣的退避重試（原因 `handler panicked`）；同一事件在同一副本連續第 `MAX_HANDLER_PANICS`（3）次 panic 時記進死信（`handler panicked repeatedly`），之後的事件照常處理。panic 前 handler 已結束交易或改了租戶時，照上一列直接記進死信 |

- 原因（`reason`）會記進日誌與死信表，不能含事件內容或個資；底層錯誤只記 SQLSTATE，因為錯誤訊息可能引用資料值（M2）。`ConsumerError` 與 `PublishError` 的文字同樣只有 SQLSTATE 或失敗種類，原始錯誤仍可由 `source()` 取得。
- 每一條不 commit 的路徑（上表各列，以及另一個副本持有或移動了訂閱、消費端自己的查詢失敗）都先**明確回滾**投遞交易、等回滾完成，才記死信或回報結果，見[明確回滾](#明確回滾laysitrustiot-v23502026-10-12)。
- 一直重試的事件會讓訂閱停住，lag 指標隨之上升；死信事件可在修正後以 `replay_from` 重新處理。

### Handler panic（Laysi/trustiot-v2#333，2026-10-12）

原本 handler panic 一次，panic 就經 `tokio::join!` 帶走整個 `run`，該副本從此不再處理，lag 指標也停在最後一次的值（Laysi/trustiot-v2#271 獨立驗證的 F-5）。現在的處理與 service-kit 的週期工作（Laysi/trustiot-v2#284）同一個做法：

- **怎麼攔**：在原地以 `catch_unwind`（`AssertUnwindSafe`）poll handler 的 future，不另開 task。handler 借用投遞交易（`&mut Transaction`），panic 後消費端要拿回這筆交易檢查並回滾；放進 task 就得把交易移進去，panic 時交易在 task 裡被 drop，消費端無從處理。原地攔截也不改變取消語意（投遞被 drop 時 handler 一起 drop），handler 也留在 `outbox.deliver` span 裡。`AssertUnwindSafe` 成立的理由：panic 後 handler 的 future 直接丟棄；交易經檢查後回滾；消費端自己的狀態（panic 次數）在容許 poison 的鎖後面；handler 物件本身的狀態由 handler 負責（之後還會再被呼叫，例如被 poison 的 `Mutex` 要自己處理）。
- **交易**：panic 時 handler 可能還有送出、尚未讀完結果的陳述。sqlx 在下一個陳述前會讀完並丟掉這些結果，連線仍可使用。消費端先在同一筆交易做與正常回傳時相同的檢查（交易仍是原本那筆、租戶沒變；交易已 abort 時檢查失敗，就當作單純的 panic）。為了讓這個檢查也涵蓋 `begin`，交易層級的標記改在呼叫 `begin` 之前設定（原本與查 inbox 合在同一個查詢）；`begin` 回傳錯誤時也做同樣的檢查。之後**明確回滾**而不是等 drop：回滾完成後交易與訂閱的 advisory lock 都已結束，之後記死信的交易不會因為鎖還在而得到 `Busy`。Laysi/trustiot-v2#350 把這個做法推廣到所有不 commit 的路徑（見下節）。
- **連線**：回滾後連線照一般路徑還給 pool。歸還時 service-kit 檢查 session 沒有殘留租戶（有就關掉連線），sqlx 再 ping 一次，沒有回應就關掉；回滾失敗時 sqlx 也會關掉連線，不會把還在交易中的連線放回 pool。測試以每個 pool 只有 1 條連線重現「陳述還在執行時 panic」，下一個事件在同一個 backend、沒有租戶、沒有前一次的寫入。
- **事件**：panic 可能來自暫時狀況，所以先視為可重試，與 `HandlerError::retry` 同樣退避；但多半是程式或資料造成、每次都會發生，一直重試會讓整個訂閱（所有租戶）永遠停在這個事件。所以同一事件連續第 3 次 panic 就記進死信。次數記在副本的記憶體：一次嘗試沒有 panic 就結束（`begin` 回錯誤、`handle` 回傳）或事件進死信時歸零，重啟也歸零；多個副本輪流處理時各自計數，所以最多約為副本數乘以 3 次，仍有上限。修正後以 `replay_from` 重新處理。
- **日誌**：每次 panic 記一行 ERROR（`outbox handler panicked; the event is retried`，進死信時 `too many panics in a row for this event`），帶訂閱與事件 ID，不含 panic 訊息；panic hook 另記的那行仍含訊息與位置，所以 panic 訊息不能放資料。`panic = "abort"` 的建置不受影響，行程仍會結束。
- handler 以外的 panic（消費端自己的程式）也由 `process` 每一輪攔下，記 ERROR 後退避重試；該輪的交易隨之 drop 而回滾，下一輪從資料庫記錄的位置重讀。

### 明確回滾（Laysi/trustiot-v2#350，2026-10-12）

- **問題**：原本一般的失敗路徑以 drop 投遞交易回滾。sqlx 的 drop 只把 `ROLLBACK` 排進那條連線，等 pool 在另一個 task 收回連線時才送出；在那之前，交易與訂閱的 advisory lock 都還在。緊接著記死信的交易（或重試後立刻開始的下一輪）可能先嘗試取鎖而得到 `Busy`，事件下一輪會再交給 handler 一次。至少一次的保證與 inbox 的去重不受影響，只是 handler 多被呼叫一次。
- **現在**：投遞交易只有兩種結束方式。成功時 commit；其他所有路徑（handler 回錯誤、`begin` 或 `handle` 破壞交易、panic、另一個副本持有或移動了訂閱、消費端自己的查詢失敗）都以 `rollback().await` 明確回滾，等伺服器回覆後，才記死信或回報 `Retry`、`Busy`、錯誤。伺服器回覆 `ROLLBACK` 時交易已結束、鎖已釋放，所以下一筆交易拿得到鎖，除非真的有別的副本持有它。
  - 回滾失敗只記一筆 WARN：sqlx 在連線歸還時會再送一次 `ROLLBACK`，再失敗 pool 就關掉連線，伺服器隨之結束交易；不會 commit。
  - commit 失敗不需要回滾：不論 `COMMIT` 成功與否，Postgres 都會結束交易。
  - 投遞的 future 本身被 drop 時（例如 `run` 的 future 被丟掉），仍是 drop 時回滾；這時也不會有接著執行的死信交易。
- **`begin` 的檢查**：`begin` 回 `Ok` 後，消費端在取得訂閱的鎖之前，立刻做與 `handle` 之後相同的檢查（交易仍是原本那筆、租戶沒變）。原本只有 `begin` 回錯誤或 panic 時才查，所以 `begin` 自己 `COMMIT`、而事件已在 inbox（重複投遞，不呼叫 `handle`）時查不出來：之後的取鎖、查 inbox、推進位置都在交易外逐句執行，事件被算成「已跳過」。現在這種投遞與其他路徑一樣記進死信（原因相同），不重試。第一次投遞時也提早到 `handle` 之前發現，`handle` 不會在交易外寫入。代價是每個事件多一個只讀的查詢（不取得交易 ID）。
- **測試**（`tests/failure_paths.rs`）：
  - 以 pool 的 `before_acquire` 掛勾，在消費端下一次從 pool 取連線時（記死信的交易，或重試後的下一輪），從另一條連線查 `pg_locks`，確認失敗那次投遞的 backend 已沒有交易（`virtualxid`）與 advisory lock。查詢時刻意卡住測試的單執行緒 runtime，pool 送出 drop 後 `ROLLBACK` 的 task 這時不可能執行，所以結果與時序無關：只 drop 的實作一定會被看到交易還開著。
  - 同一個測試也驗證 handler 的呼叫次數剛好是預期值（永久失敗 1 次，重試 2 次），而且沒有 `Busy`。
  - `begin` 自己 `COMMIT` 或改租戶後回 `Ok`：重複投遞與第一次投遞都進死信，`handle` 不會在被破壞的交易裡執行。

## Lag 指標

`Consumer::run` 每 `lag_interval`（預設 15 秒）量一次，經 OpenTelemetry 的全域 meter 匯出；service-kit 設定 `telemetry.otlp_endpoint` 後送到 `<url>/v1/metrics`，匯出間隔是 `OTEL_METRIC_EXPORT_INTERVAL`（預設 60 秒）。

| 指標 | 單位 | 意義 |
| --- | --- | --- |
| `outbox.consumer.lag` | `{event}` | 訂閱位置之後、已 commit 的事件數，最多數到 10 000（`LAG_COUNT_CAP`），超過仍讀到 10 000 |
| `outbox.consumer.lag.age` | `s` | 其中最舊一筆寫入至今的秒數；沒有等待中的事件時為 0 |

屬性：`outbox.subscription`（訂閱名稱）、`outbox.source`（產生端 schema）。`Consumer::lag()` 與 `measure_lag()` 在程式內讀同樣的值。

**count 到上限後以 age 判斷**：`outbox.consumer.lag` 只數到 10 000 就停，積壓再多也讀到 10 000，看不出還在惡化或已在排空；`outbox.consumer.lag.age` 沒有上限，會隨最舊的等待事件變老而繼續上升。所以告警條件用 age，不要用 lag 的門檻（超過 10 000 後它不會再增加）；lag 只當積壓規模的參考，讀到 10 000 就表示「至少這麼多」。Laysi/trustiot-v2#54 的獨立驗證中，積壓累積到以 SQL 量得 25 687 個事件時，函式庫的 lag 指標停在上限 10 000，age 達 1 113 秒（[報告](../../docs/test-reports/2026-10-11-54-verify.md)第 3 節）。

指標不會停在舊值（Laysi/trustiot-v2#333）：

- `run` 把量測放在另一個 tokio task（`monitor_lag`），不依賴處理迴圈。處理卡住（handler 一直重試、卡在等待、某個事件一直失敗）時量測照常進行，lag 與 age 持續上升。量測迴圈本身也攔 panic，失敗只記 WARN。
- 一筆量測只在 `CURRENT_FOR_INTERVALS`（3）個 `lag_interval` 內有效（預設 45 秒）。量測卡住（查詢不回來、執行緒被 handler 卡住）或一直失敗，超過這個時間就不再回報，`Consumer::lag()` 回 `None`。
- `monitor_lag` 不論因為什麼結束（`stopping`、`run` 的 future 被 drop、task 被 abort），都清掉最後一筆量測，指標不再回報這個訂閱。

所以「消費端停了」只有兩種樣子：lag 與 age 上升，或這個訂閱的序列不再出現。建議的告警：age 持續超過數分鐘（handler 一直失敗，或有長時間交易），以及某個訂閱的序列數分鐘沒有資料（量測停了或卡住；建立訂閱時一直失敗的 `run` 也從未回報）。沒有改成讓 readiness 失敗：每個副本跑同樣的訂閱，readiness 失敗會讓整個服務退出 Service 的 endpoints，背景工作的問題會變成 API 中斷，理由與 service-kit 週期工作的 panic 處理相同。

## 與租戶隔離（決策 29）的配合

決策 29 的 RLS 與 `SET LOCAL` 由 Laysi/trustiot-v2#53 實作（service-kit 的 `tenant` 模組；`outbox_event` 的 policy 範本是 [`crates/service-kit/sql/outbox-event.sql`](../service-kit/sql/outbox-event.sql)，說明見 [service-kit README 的 outbox 一節](../service-kit/README.md#outboxcratesoutbox)）。消費端照它讀取與處理（Laysi/trustiot-v2#271，2026-10-11）：

- **產生端**：`publish` 只在呼叫端的交易裡執行兩個語句，不另開交易、不改設定。租戶交易用 `outbox::publish(tx.transaction(), ..., Some(tx.tenant_id()))`（`TenantTransaction::transaction()` 交出同一筆交易，租戶設定不變）；全平台事件由產生端平台角色的 `PlatformTransaction::transaction()` 寫。`outbox_event.tenant_id` 來自信封；`tenant_isolation` policy 的 `WITH CHECK` 讓服務角色只寫得進目前租戶的事件，`platform_events` 讓全平台事件（`tenant_id` 為 NULL）只由產生端的平台角色寫（2026-10-10 決定），restrictive 的 `platform_events_only` 讓平台角色只寫得進全平台事件，即使它的 session 自己設定了租戶（Laysi/trustiot-v2#360，2026-10-12）。插入不使用 `RETURNING`，所以不需要 SELECT policy。
- **`outbox_aggregate`**：聚合的序號與排序交易決定該聚合之後事件的序號與順序，所以是租戶資料（2026-10-15 起，Laysi/trustiot-v2#413）：表有 `tenant_id` 與 RLS，服務角色只讀寫目前租戶的聚合，平台角色只讀寫全平台的聚合，即使它的 session 自己設定了租戶。做法與理由見[聚合的租戶隔離](#聚合的租戶隔離laysitrustiot-v24132026-10-15)。
- **消費端讀取**：投遞本來就跨租戶，`Consumer` 以消費端服務的**平台角色**讀產生端的 `outbox_event`（範本的 `consumer_read`：`FOR SELECT TO <消費端平台角色> USING (true)`）。`Consumer::new` 以 `PlatformDb::reader(PlatformActor::service(<服務>), "outbox-consumer-read")` 建立讀取端：actor 與 purpose 在建立時以 INFO 記一次（`platform database reader`），之後每秒的輪詢只記 DEBUG；讀取都是 `READ ONLY` 交易，資料庫拒絕任何寫入。讀取（`fetch`、lag、`StartFrom::Latest` 的 horizon）都在這種交易裡，與處理事件的交易分開。
- **讀取端設錯時不會靜靜讀不到**：`ensure_subscription` 檢查 `outbox_event` 開了 RLS 時，有一條 `USING (true)` 的 permissive SELECT policy 套用到讀取端的角色、而且沒有 restrictive policy；沒有就回 `ConsumerError::CannotReadAllTenants`。否則讀取端角色看不到所有租戶時只會漏讀事件，lag 也是 0，看起來一切正常：例如 policy 只開放全平台事件（`USING (tenant_id IS NULL)`）。讀取端角色有 `SELECT`、卻只有租戶條件（例如用服務角色讀）時，service-kit 先拒絕它的連線（`UnsafeSession::PlatformTenantOnly`，Laysi/trustiot-v2#366），消費端回 `ConsumerError::UnsafeSession`。反過來，把跨租戶讀取開給服務角色時，service-kit 拒絕服務角色的每個連線（`UnsafeSession::PermissivePolicy`），消費端回 `ConsumerError::UnsafeSession`。這些都可由 migration 修正，`is_retryable()` 為真，`run` 會退避重試。
- **消費端處理**：每個事件一筆**服務角色**的交易，由 service-kit 開在事件的租戶裡：`BackgroundTenantDb::begin(delivery.tenant(), "outbox-delivery")`，第一個陳述就是設定租戶，handler 拿到的 `tx` 是同一筆交易。RLS 讓 handler 只讀得到、寫得進該租戶的列；寫入別的租戶被資料庫以 SQLSTATE 42501 拒絕，依一般資料庫錯誤重試（事件留在原位、lag 上升），不會寫進去。
- **全平台事件**（`tenant_id` 為 NULL）的處理（2026-10-11 決定）：在服務角色**沒有租戶**的交易裡（`BackgroundTenantDb::begin_without_tenant`），handler 看不到也寫不進任何租戶的列，只能寫服務自己不帶租戶資料的表。不給 handler 平台角色：平台角色能跨租戶寫開放的表，而 handler 是每個事件都會執行的程式。需要依全平台事件改每個租戶的資料時，handler 在同一筆交易記下待辦，再由排程工作以 `PlatformDb` 找出租戶、逐一以 `BackgroundTenantDb::begin` 處理。
- **信封的租戶以 log 列為準**：產生端的 RLS 檢查的是 `outbox_event.tenant_id` 欄，不是信封 JSON。兩者不同時（只有直接寫 SQL 的產生端做得出來），消費端不在信封的租戶執行，直接記進死信（`envelope tenant differs from the log row`），所以 `Delivery::tenant()` 一定是產生端 RLS 檢查過的租戶。
- 消費端自己的 `outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version` 不帶租戶資料、不需要 RLS；處理事件時它們在同一筆租戶交易裡寫，死信與訂閱建立、位置查詢在沒有租戶的服務角色交易裡寫。

### 交易邊界與保證（2026-10-11）

| 交易 | 角色 | 內容 |
| --- | --- | --- |
| 讀取 | 平台角色，`READ ONLY` | 取讀取上限、讀一批事件；commit 後才處理 |
| 處理一個事件 | 服務角色，事件的租戶（全平台事件無租戶） | 設定租戶 → 設定交易層級的標記 → `begin` → 檢查交易與租戶沒被 `begin` 改掉 → 訂閱的 advisory lock → 位置未被移動的檢查 → 查 inbox → `handle` → 檢查交易與租戶沒被 handler 改掉 → 寫 inbox → 從原位置推進位置 → commit；任何一步沒走到 commit，都先明確回滾、等它完成 |
| 死信 | 服務角色，無租戶 | 訂閱的 advisory lock → 位置檢查 → 寫死信 → 推進位置 → commit |

原本的保證都不變，因為改的只是每筆交易用哪個 pool、開在哪個租戶，交易的切法與每筆交易裡的語句順序都沒變：

- **至少一次**：讀取交易只讀不寫，讀完就結束，事件是否「已處理」只由處理交易決定。handler 的寫入、inbox、位置在同一筆處理交易 commit；commit 前當機、出錯或被 RLS 拒絕，三者一起回滾，下一輪從同一位置再讀到同一事件。
- **只處理一次**：inbox 與位置跟 handler 的寫入在同一筆交易；位置只從開始處理時的位置推進，被別的副本或 `replay_from` 移動過就整筆回滾。設定租戶不影響這些檢查：inbox、訂閱表不帶租戶資料，RLS 不過濾它們。
- **有序、不漏讀**：讀取上限（`pg_snapshot_xmin`）是叢集層級的值，與用哪個角色讀無關；排序與上限的推論見[順序與不漏讀](#順序與不漏讀)。處理交易在 handler 第一次寫入前都只有讀取與 `set_config`（設定租戶不會取得交易 ID），所以不會比原本更早拖住其他訂閱的讀取上限。
- **handler 不守規則時**（Laysi/trustiot-v2#271 的獨立驗證 F-1 到 F-3，2026-10-11 修正）：handler 改了交易的租戶或自己結束交易，事件進死信（見[消費端](#消費端)）；handler 以 `set_config(..., false)` 設定 session 層級的租戶時，service-kit 在連線歸還 pool 時關掉這條連線，之後沒有租戶的交易（全平台事件、死信、位置查詢）不會在那個租戶裡執行。
- 測試：既有的投遞、當機、重播、通知模式、lag 測試全部改以這套角色執行（平台角色在 RLS 下讀、服務角色在租戶交易處理），併發測試的事件分屬兩個租戶；`tests/tenancy.rs` 另外驗證跨租戶讀取、handler 只能寫事件的租戶、角色設錯時被拒、信封租戶不符時進死信。

## 資料庫權限

服務的角色不是資料表的擁有者（決策 29），migration 角色建立資料表後授權。每個服務有服務角色（例 `identity_app`）與平台角色（例 `identity_platform`），見 [service-kit 的角色](../service-kit/README.md#角色與-policy-範本)。所有測試都以這組權限執行（`tests/support/mod.rs` 的 `Database::setup`；`tests/delivery.rs` 的 `services_need_only_the_documented_grants` 另外驗證拒絕的部分）：

```sql
-- 產生端（例：identity）：install_producer 之後執行 service-kit 的 sql/outbox-event.sql，
-- 以 example_app = identity_app、example_platform = identity_platform、
-- example_reader = 每個消費端的平台角色（例 assets_platform）；它啟用 RLS、建立四條 policy，
-- 並 GRANT INSERT ON outbox_event TO identity_app, identity_platform、
-- GRANT SELECT ON outbox_event TO assets_platform。
-- 接著執行 sql/outbox-aggregate.sql（同樣的 example_app、example_platform）：它加上
-- outbox_aggregate.tenant_id、啟用 RLS、建立三條 policy，
-- 並 GRANT SELECT, INSERT, UPDATE ON outbox_aggregate TO identity_app, identity_platform
GRANT USAGE ON SCHEMA identity TO identity_app, identity_platform;

-- 消費端（例：assets 讀 identity 的事件）
GRANT USAGE ON SCHEMA identity TO assets_platform;   -- 平台角色只讀得到 outbox_event
GRANT USAGE ON SCHEMA assets TO assets_app;
GRANT SELECT, INSERT, UPDATE, DELETE
    ON assets.outbox_subscription, assets.outbox_inbox, assets.outbox_dead_letter, assets.outbox_version
    TO assets_app;
ALTER ROLE assets_app SET search_path = assets;      -- 消費端的表在服務角色的 search_path 第一個 schema
```

消費端的平台角色看不到產生端的其他資料表，也不能寫產生端的事件（讀取交易是 `READ ONLY`，也沒有寫入權限）；消費端的服務角色完全碰不到產生端的 schema。例外是服務消費自己的事件時：消費端的平台角色就是產生端的平台角色，有 `outbox_event` 的 INSERT 權限，但 `platform_events_only` 只讓它寫全平台事件（`tenant_id IS NULL`），即使它的 session 自己設定了租戶也寫不進租戶事件。

### 聚合的租戶隔離（Laysi/trustiot-v2#413，2026-10-15）

**問題**：`outbox_aggregate` 記錄每個聚合最後一筆事件的序號（`last_seq`）與排序交易（`last_tx`），產生端兩個角色原本都能直接 `SELECT`、`INSERT`、`UPDATE`，表沒有 RLS。身分服務成為產生端（Laysi/trustiot-v2#65）之後，聚合是 `tenant` 加租戶 ID，所以 `identity_app` 能列出每個租戶的 ID，也能改別的租戶聚合的 `last_seq`、`last_tx`，影響那個租戶之後事件的序號（通知模式的版本）與排序。

**做法：租戶欄位加 RLS**（service-kit 的 [`sql/outbox-aggregate.sql`](../service-kit/sql/outbox-aggregate.sql)）：

- `outbox_aggregate.tenant_id uuid`：聚合所屬的租戶，全平台的聚合（全平台事件的聚合）為 NULL。`publish` 寫入時帶信封的租戶，與 `outbox_event.tenant_id` 同一個值；只有第一筆事件寫進去，之後的事件不改它。
- RLS 與 `outbox_event` 同一套：`tenant_isolation`（所有角色，`USING` 與 `WITH CHECK` 一字不差是決策 29 的租戶條件，連線檢查認得它）讓服務角色只讀寫目前租戶的聚合，沒有租戶時一列都看不到；`platform_aggregates`（`TO` 產生端平台角色，`tenant_id IS NULL`，FOR ALL，因為 upsert 要讀回、插入、更新）讓平台角色讀寫全平台的聚合；restrictive 的 `platform_aggregates_only` 讓平台角色即使自己設定了租戶，也碰不到任何租戶的聚合（同 Laysi/trustiot-v2#360 的理由）。權限是 `SELECT, INSERT, UPDATE`，沒有 `DELETE`（序號不能重來）、不給 `TRUNCATE`。
- **主鍵不變**，仍是 `(aggregate_type, aggregate_id)`，不加租戶：一個聚合只有一個序號、一個列鎖。`outbox_event` 的 `UNIQUE (aggregate_type, aggregate_id, aggregate_seq)`、消費端 `outbox_version` 的鍵、通知模式的版本都以聚合種類加 ID 識別聚合。別的租戶為同一個聚合寫事件時，`publish` 的 upsert 撞到既有的列，Postgres 以 `UPDATE` policy 的 `USING` 檢查那一列，不符就以 42501 拒絕（不是略過），所以不會開出第二個序號，也不會推進原本的序號。
- **聚合內有序不變**：`publish` 的語句只多了 `tenant_id` 一欄，同一聚合的寫入仍以該列的鎖排隊，`last_tx` 照舊帶著排序交易往前；[順序與不漏讀](#順序與不漏讀)的推論不受影響（測試 `an_aggregate_keeps_its_order_when_transaction_ids_are_inverted` 照舊通過）。
- **欄位與它的 policy 在同一筆交易出現**，所以欄位放在範本裡，不放進 `PRODUCER_DDL`：service-kit 對「能用、有 `tenant_id` 欄、但沒開 RLS」的表會拒絕角色的每個連線（`UnprotectedTable`），欄位若先在 `install_producer` 的交易 commit，範本之前（或範本失敗時）服務角色就連不上。代價是只跑 `install_producer`、不跑範本的產生端，`publish` 會因為沒有這個欄位失敗；migration Job 一定兩者都跑，測試的 `Database::setup` 也是。

**升級**（已經跑過舊版 Job 的資料庫）：範本是新檔案，Job 在下一次執行時套用它一次（記在 `trustiot_migrate.applied`，component `<服務>.outbox`），既有的 `outbox-event.sql` 沒改，所以「已套用的檔案不再修改」的規則照舊成立。範本在同一筆交易裡：

1. 加上 `tenant_id`，把每個既有聚合設成它**最後一筆事件**的租戶（以 `outbox_event` 的 `UNIQUE (aggregate_type, aggregate_id, aggregate_seq)` 對到 `last_seq`；執行範本的 owner 角色不受 RLS 限制）。`publish` 一定在同一筆交易寫聚合與事件，所以每個聚合都找得到最後一筆事件；找不到的（只有手動寫入才會有）維持 NULL，成為全平台的聚合。
2. 回填之後才設預設值 `NULLIF(current_setting('trustiot.tenant_id', true), '')::uuid`：還在跑舊版程式的 pod（滾動更新期間）寫新聚合時不帶 `tenant_id`，拿到交易的租戶，正好是 RLS 接受的值，所以不會失敗；既有的列不受預設值影響。
3. 啟用 RLS、建立三條 policy、授權。

升級期間（範本那筆交易持有 `outbox_aggregate` 的鎖）寫事件的交易會等它結束。`tools/migrate/tests/outbox_and_tenant_roles.rs` 把 schema 改回舊版的樣子、放進舊版寫的事件後再跑 Job，確認只套用這個檔案、每個聚合拿到最後一筆事件的租戶、下一次執行沒有變更。

**不採用的做法**：

| 做法 | 不採用的原因 |
| --- | --- |
| `SECURITY DEFINER` 函式推進序號，服務角色不直接讀寫這張表 | service-kit 的連線檢查拒絕能執行別的角色擁有的 `SECURITY DEFINER` 函式的連線（決策 29 的實作細節，Laysi/trustiot-v2#381），要放寬就是改決策 29 的規則；tools/migrate 的 migration 規則也不允許建這種函式。函式以 owner 的權限執行，不受任何 RLS 限制，寫錯一處就是跨租戶；它能依據的租戶也只有 `trustiot.tenant_id`，而服務角色自己就改得了這個設定（決策 29 的威脅模型），保證不會比 RLS 強。`aggregate_version` 也得再寫一個函式，跨租戶負向測試框架也測不到函式 |
| 租戶欄位加進主鍵（每個租戶各自的聚合空間） | 兩個租戶的同一個聚合種類加 ID 會各有一個從 1 開始的序號，撞上 `outbox_event` 的 `UNIQUE (aggregate_type, aggregate_id, aggregate_seq)` 與消費端以種類加 ID 為鍵的 `outbox_version`；要改這些就改了「聚合」的意義與通知模式的版本。聚合本來就只屬於一個擁有者 |
| 不要 `outbox_aggregate`，序號改由 `outbox_event` 算（`max(aggregate_seq)` 加 advisory lock） | 改了聚合內排序的機制（列鎖換成 advisory lock、`REPEATABLE READ` 下的失敗由序列化錯誤變成唯一鍵衝突），超出這個 issue 的範圍（不改順序保證）；服務角色也得拿到讀自己租戶事件的權限 |

**限制**：

- 唯一鍵看得出列存在：別的租戶為一個已存在的聚合寫事件時得到 42501，而不是成功，所以知道種類加 ID 的人能確認「這個聚合在別的租戶存在」，看不到序號與租戶。`outbox_event` 的唯一鍵與每張表的主鍵本來就有同樣的性質；聚合 ID 照上面的規則由服務產生，租戶選不了它。
- 那次被拒的 upsert 在檢查前會先鎖住既有的列，所以它會等那個聚合進行中的寫入結束，而它自己的交易在回滾前一直持有那個鎖。這只會讓同一聚合的寫入多等一下，不會改序號；而且只發生在服務程式為別的租戶的聚合 ID 寫事件時（程式錯誤）。
- 表的擁有者（migration 角色）與 superuser 不受 RLS 限制，測試以 superuser 寫的事件不檢查聚合的租戶。

### 長寫入交易與 prepared transaction

讀取上限是整個 Postgres 叢集最舊的「有交易 ID」的交易（見[順序與不漏讀](#順序與不漏讀)），不分 schema、不分服務，所以影響範圍是**所有訂閱**，不只是讀這個產生端的那些：

- 任何 schema 的長寫入交易（沒有 outbox 的服務、手動維護、長時間的 migration 或批次寫入）都會讓所有訂閱停在它之前；交易結束後恢復。
- `PREPARE TRANSACTION` 之後沒人 `COMMIT PREPARED` 或 `ROLLBACK PREPARED` 的交易（`pg_prepared_xacts`）也一樣：它沒有連線，不會因為連線結束而消失，在 Postgres 崩潰復原後仍在，會一直卡住所有訂閱，直到有人收尾。`max_prepared_transactions` 預設為 0（不能 prepare），要開的環境才需要這項監控。
- 唯讀交易沒有交易 ID，不受影響。
- 停滯期間事件照常寫入、commit，只是不投遞；結束後從原位置接著處理，不漏事件。

實測（[Laysi/trustiot-v2#271 的混沌報告](../../docs/test-reports/2026-10-12-271-chaos.md)第 4.4 節，沿用 Laysi/trustiot-v2#54 的探針）：一個 30 秒的長寫入交易（在另一個 schema）期間，已處理數固定在 173，事件從 197 增加到 531、lag 從 20 增加到 354，交易結束後約 15 秒內恢復；同樣 30 秒的長唯讀交易期間處理持續增加、lag 降到 0。Laysi/trustiot-v2#54 的報告另量到 lag 指標 332 個事件、age 29 秒（[報告](../../docs/test-reports/2026-10-11-54-verify.md)第 3.3 節）。兩份報告的混沌各有 20 個 15–35 秒的長寫入交易與一個跨 Postgres 崩潰的 prepared transaction，事件都沒有遺失。

上線後的監控建議：

1. **age 告警**（見 [Lag 指標](#lag-指標)）：age 持續超過數分鐘就告警。不論停滯的原因是長交易、prepared transaction、handler 一直失敗還是消費端處理太慢，age 都會上升；它只告訴你有訂閱落後，不會指出原因。
2. **長寫入交易**：監控 `pg_stat_activity` 中 `backend_xid IS NOT NULL`（已取得交易 ID）且 `xact_start` 距今超過門檻的連線，並看它的 `datname`、`usename`、`application_name`、`state` 與 `query` 找出來源。狀態是 `idle in transaction` 的連線最危險（沒人在做事卻一直擋住），可另設 `idle_in_transaction_session_timeout`。
3. **未收尾的 prepared transaction**：監控 `pg_prepared_xacts` 的筆數與最舊的 `prepared` 時間；預期是 0，任何一筆超過數分鐘都要處理（`COMMIT PREPARED` 或 `ROLLBACK PREPARED`，依其來源決定）。
4. 第 2、3 項是資料庫層的監控，由資料庫監控（例如 Postgres exporter 或同等的查詢）提供，不在 outbox 的指標裡。

## 限制與後續

- 保留期限：`outbox_event` 與 `outbox_inbox` 目前只增不刪。2026-10-21 決定（Laysi/trustiot-v2#130，[決策 34](../../docs/architecture/decisions.md)）：長期保存的操作紀錄是身分服務的事件紀錄（`identity.event_log`），不是各服務的 outbox，所以 outbox 只是投遞的緩衝，可以刪。刪除規則：一個事件要等**讀這個產生端的每個訂閱**（各消費端 `outbox_subscription` 中 `source_schema` 是它的列）的位置都超過它，再過一段重播用的保留期，才刪 `outbox_event` 的列；`outbox_inbox` 的列在它的訂閱位置超過它、過了同樣的保留期之後刪。還沒有訂閱的新消費端從頭讀（`StartFrom::Beginning`）時只讀得到保留期內的事件，所以要先建訂閱、再刪。刪除工作（排程、保留期長度、監控）在 Laysi/trustiot-v2#521 實作，在那之前照舊只增不刪。
- 讀取上限受整個叢集最舊的寫入交易影響，見[順序與不漏讀](#順序與不漏讀)；影響範圍與監控見[長寫入交易與 prepared transaction](#長寫入交易與-prepared-transaction)。
- 每個事件一筆交易，適合控制面的事件量；大量資料（遙測）不走 outbox（決策 8）。

## 測試

需要 podman 或 docker：每個測試起一個用完即刪、以測試時產生的 CA 啟用 TLS 的 `postgres:18.6-alpine` 容器，消費端以 service-kit 的 pool（`verify-full`、連線檢查）連線。沒有容器執行環境時在本機略過並印出 `skipped:`；設了 `CI` 時改為失敗。容器經 service-kit 測試的同一份啟動邏輯（`crates/service-kit/tests/support/start.rs`，以 `#[path]` 引入）啟動：等待有上限、發佈的連接埠失效時換容器、失敗訊息附容器狀態與日誌（`cargo test -p outbox --test postgres_start` 測這些）。不使用 `TEST_DATABASE_URL`：測試要以 superuser 建角色，並需要知道伺服器的 CA。每個測試的 `Database::setup` 建立產生端與消費端的 schema、決策 29 的四個角色、`outbox_event` 的 RLS（service-kit 的範本）與上面的授權。

| 指令 | 內容 |
| --- | --- |
| `cargo test -p outbox --lib` | 聚合、位置、設定的檢查；lag 量測過期或量測迴圈結束後指標不再回報 |
| `cargo test -p outbox --test producer` | 回滾不留事件、聚合序號、同一信封只能寫一次 |
| `cargo test -p outbox --test delivery` | 交易 ID 反轉時仍依序、晚 commit 的事件不漏、並行寫入（兩個租戶）加兩個副本每個事件只處理一次、重複投遞跳過、重播後狀態相同、重試與死信、`begin` 在同一交易、未寫入的 handler 不拖住其他訂閱、收尾時只做完進行中的事件、lag、`StartFrom::Latest`、最小權限（產生端以租戶交易與平台角色寫事件） |
| `cargo test -p outbox --test tenancy` | 平台角色在 RLS 下讀到兩個租戶與全平台的事件、每個事件在自己的租戶處理且看不到別的租戶；handler 寫別的租戶被拒（42501）；服務角色被開放跨租戶讀取時連線被拒、讀取端只有租戶條件時連線被拒（`PlatformTenantOnly`）、讀取端的 policy 看不到所有租戶時停下（`CannotReadAllTenants`）；信封租戶與 log 列不符時進死信；產生端的服務角色在租戶交易裡看不到、也改不了別的租戶與全平台的聚合（讀取、`aggregate_version`、直接 `UPDATE`、插入、搬移、`publish` 都被拒或讀不到，42501），平台角色只碰得到全平台的聚合，被拒之後每個聚合都沒變，原租戶的序號接著往下（Laysi/trustiot-v2#413） |
| `cargo test -p outbox --test verify_271` | Laysi/trustiot-v2#271 的獨立驗證（[報告](../../docs/test-reports/2026-10-11-271-verify.md)）：handler 以角色、RLS 手法離開租戶都失敗；改租戶、自己 `COMMIT` 的投遞進死信；session 層級的租戶不會帶進下一個交易；`PlatformReader` 的唯讀解除不了；怪異的租戶 ID、角色設錯、設定錯誤、三個副本加並行寫入、資料庫重啟；`f5_`：handler panic 之後 `run` 繼續處理，lag 持續更新（Laysi/trustiot-v2#333 修正） |
| `cargo test -p outbox --test handler_panic` | Laysi/trustiot-v2#333：`begin`、`handle` panic 一次時回滾並重試、之後只處理一次；一直 panic 的事件第 3 次進死信、之後的事件照常處理；`run` 遇到 panic 不停、lag 持續更新、停止後不再回報；`begin` 或 `handle` 自己 `COMMIT` 後 panic（或 `begin` 回錯誤）時直接進死信、不重試；陳述還在執行時 panic，同一條連線之後乾淨可用（無租戶、無殘留寫入）；處理卡住時 lag 與 age 上升、`run` 被 drop 後不再回報；量測卡住（log 被鎖）超過 3 個間隔後不再回報、解鎖後恢復 |
| `cargo test -p outbox --test failure_paths` | Laysi/trustiot-v2#350：`begin`、`handle` 回永久或可重試的錯誤時，消費端下一次取連線前，失敗那次投遞的交易與 advisory lock 已經結束（查 `pg_locks`），handler 的呼叫次數剛好、沒有 `Busy`；`begin` 自己 `COMMIT` 或改租戶後回 `Ok`，重複投遞與第一次投遞都進死信，`handle` 不在被破壞的交易裡執行 |
| `cargo test -p outbox --test postgres_start` | Laysi/trustiot-v2#374：測試用 Postgres 容器的啟動：伺服器起不來時的失敗訊息寫明在等什麼、等多久、最後的錯誤、容器狀態與日誌尾端（不含環境變數）並移除容器；發佈的連接埠主機端連不上時換新容器（含測試自己指定主機連接埠、之後重啟的情況） |
| `cargo test -p outbox --test notify` | 舊讀數不覆蓋新狀態（含副本競爭）、晚到與重播收斂 |
| `cargo test -p outbox --test crash` | 以 SIGKILL 結束 `outbox-example`：commit 後投遞前、handler 寫入後 commit 前，重啟後各處理一次，且在事件的租戶處理 |
| `cargo test -p outbox --test metrics` | lag 經 OTLP/HTTP 匯出到假的接收端，解碼後讀到數值 |
