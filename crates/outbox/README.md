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
- `run` 在 `Shutdown::stopping` 之後停止，正在處理的事件會先做完；建立 `Consumer` 前要先初始化遙測（`service_kit::run` 已經做了），lag 指標才會接到 OTLP。

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

決策 29 的 RLS 與 `SET LOCAL` 由 Laysi/trustiot-v2#53 實作（service-kit 的 `tenant` 模組；`outbox_event` 的 policy 範本是 [`crates/service-kit/sql/outbox-event.sql`](../service-kit/sql/outbox-event.sql)，說明見 [service-kit README 的 outbox 一節](../service-kit/README.md#outboxcratesoutbox)）。消費端照它讀取與處理（Laysi/trustiot-v2#271，2026-10-11）：

- **產生端**：`publish` 只在呼叫端的交易裡執行兩個語句，不另開交易、不改設定。租戶交易用 `outbox::publish(tx.transaction(), ..., Some(tx.tenant_id()))`（`TenantTransaction::transaction()` 交出同一筆交易，租戶設定不變）；全平台事件由產生端平台角色的 `PlatformTransaction::transaction()` 寫。`outbox_event.tenant_id` 來自信封；`tenant_isolation` policy 的 `WITH CHECK` 讓服務角色只寫得進目前租戶的事件，`platform_events` 讓全平台事件（`tenant_id` 為 NULL）只由產生端的平台角色寫（2026-10-10 決定）。插入不使用 `RETURNING`，所以不需要 SELECT policy。
- **`outbox_aggregate`**：只有聚合種類、ID 與序號，不是租戶資料；產生端兩個角色都需要讀寫它。
- **消費端讀取**：投遞本來就跨租戶，`Consumer` 以消費端服務的**平台角色**讀產生端的 `outbox_event`（範本的 `consumer_read`：`FOR SELECT TO <消費端平台角色> USING (true)`）。`Consumer::new` 以 `PlatformDb::reader(PlatformActor::service(<服務>), "outbox-consumer-read")` 建立讀取端：actor 與 purpose 在建立時以 INFO 記一次（`platform database reader`），之後每秒的輪詢只記 DEBUG；讀取都是 `READ ONLY` 交易，資料庫拒絕任何寫入。讀取（`fetch`、lag、`StartFrom::Latest` 的 horizon）都在這種交易裡，與處理事件的交易分開。
- **讀取端設錯時不會靜靜讀不到**：`ensure_subscription` 檢查 `outbox_event` 開了 RLS 時，有一條 `USING (true)` 的 permissive SELECT policy 套用到讀取端的角色、而且沒有 restrictive policy；沒有就回 `ConsumerError::CannotReadAllTenants`。否則用錯角色（例如服務角色）讀時只會看不到任何事件，lag 也是 0，看起來一切正常。反過來，把跨租戶讀取開給服務角色時，service-kit 拒絕服務角色的每個連線（`UnsafeSession::PermissivePolicy`），消費端回 `ConsumerError::UnsafeSession`。兩者都可由 migration 修正，`is_retryable()` 為真，`run` 會退避重試。
- **消費端處理**：每個事件一筆**服務角色**的交易，由 service-kit 開在事件的租戶裡：`BackgroundTenantDb::begin(delivery.tenant(), "outbox-delivery")`，第一個陳述就是設定租戶，handler 拿到的 `tx` 是同一筆交易。RLS 讓 handler 只讀得到、寫得進該租戶的列；寫入別的租戶被資料庫以 SQLSTATE 42501 拒絕，依一般資料庫錯誤重試（事件留在原位、lag 上升），不會寫進去。
- **全平台事件**（`tenant_id` 為 NULL）的處理（2026-10-11 決定）：在服務角色**沒有租戶**的交易裡（`BackgroundTenantDb::begin_without_tenant`），handler 看不到也寫不進任何租戶的列，只能寫服務自己不帶租戶資料的表。不給 handler 平台角色：平台角色能跨租戶寫開放的表，而 handler 是每個事件都會執行的程式。需要依全平台事件改每個租戶的資料時，handler 在同一筆交易記下待辦，再由排程工作以 `PlatformDb` 找出租戶、逐一以 `BackgroundTenantDb::begin` 處理。
- **信封的租戶以 log 列為準**：產生端的 RLS 檢查的是 `outbox_event.tenant_id` 欄，不是信封 JSON。兩者不同時（只有直接寫 SQL 的產生端做得出來），消費端不在信封的租戶執行，直接記進死信（`envelope tenant differs from the log row`），所以 `Delivery::tenant()` 一定是產生端 RLS 檢查過的租戶。
- 消費端自己的 `outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version` 不帶租戶資料、不需要 RLS；處理事件時它們在同一筆租戶交易裡寫，死信與訂閱建立、位置查詢在沒有租戶的服務角色交易裡寫。

### 交易邊界與保證（2026-10-11）

| 交易 | 角色 | 內容 |
| --- | --- | --- |
| 讀取 | 平台角色，`READ ONLY` | 取讀取上限、讀一批事件；commit 後才處理 |
| 處理一個事件 | 服務角色，事件的租戶（全平台事件無租戶） | 設定租戶 → `begin` → 訂閱的 advisory lock → 位置未被移動的檢查 → 查 inbox → `handle` → 寫 inbox → 從原位置推進位置 → commit |
| 死信 | 服務角色，無租戶 | 訂閱的 advisory lock → 位置檢查 → 寫死信 → 推進位置 → commit |

原本的保證都不變，因為改的只是每筆交易用哪個 pool、開在哪個租戶，交易的切法與每筆交易裡的語句順序都沒變：

- **至少一次**：讀取交易只讀不寫，讀完就結束，事件是否「已處理」只由處理交易決定。handler 的寫入、inbox、位置在同一筆處理交易 commit；commit 前當機、出錯或被 RLS 拒絕，三者一起回滾，下一輪從同一位置再讀到同一事件。
- **只處理一次**：inbox 與位置跟 handler 的寫入在同一筆交易；位置只從開始處理時的位置推進，被別的副本或 `replay_from` 移動過就整筆回滾。設定租戶不影響這些檢查：inbox、訂閱表不帶租戶資料，RLS 不過濾它們。
- **有序、不漏讀**：讀取上限（`pg_snapshot_xmin`）是叢集層級的值，與用哪個角色讀無關；排序與上限的推論見[順序與不漏讀](#順序與不漏讀)。處理交易在 handler 第一次寫入前都只有讀取與 `set_config`（設定租戶不會取得交易 ID），所以不會比原本更早拖住其他訂閱的讀取上限。
- 測試：既有的投遞、當機、重播、通知模式、lag 測試全部改以這套角色執行（平台角色在 RLS 下讀、服務角色在租戶交易處理），併發測試的事件分屬兩個租戶；`tests/tenancy.rs` 另外驗證跨租戶讀取、handler 只能寫事件的租戶、角色設錯時被拒、信封租戶不符時進死信。

## 資料庫權限

服務的角色不是資料表的擁有者（決策 29），migration 角色建立資料表後授權。每個服務有服務角色（例 `identity_app`）與平台角色（例 `identity_platform`），見 [service-kit 的角色](../service-kit/README.md#角色與-policy-範本)。所有測試都以這組權限執行（`tests/support/mod.rs` 的 `Database::setup`；`tests/delivery.rs` 的 `services_need_only_the_documented_grants` 另外驗證拒絕的部分）：

```sql
-- 產生端（例：identity）：install_producer 之後執行 service-kit 的 sql/outbox-event.sql，
-- 以 example_app = identity_app、example_platform = identity_platform、
-- example_reader = 每個消費端的平台角色（例 assets_platform）；它啟用 RLS、建立三條 policy，
-- 並 GRANT INSERT ON outbox_event TO identity_app, identity_platform、
-- GRANT SELECT ON outbox_event TO assets_platform
GRANT USAGE ON SCHEMA identity TO identity_app, identity_platform;
GRANT SELECT, INSERT, UPDATE ON identity.outbox_aggregate TO identity_app, identity_platform;

-- 消費端（例：assets 讀 identity 的事件）
GRANT USAGE ON SCHEMA identity TO assets_platform;   -- 平台角色只讀得到 outbox_event
GRANT USAGE ON SCHEMA assets TO assets_app;
GRANT SELECT, INSERT, UPDATE, DELETE
    ON assets.outbox_subscription, assets.outbox_inbox, assets.outbox_dead_letter, assets.outbox_version
    TO assets_app;
ALTER ROLE assets_app SET search_path = assets;      -- 消費端的表在服務角色的 search_path 第一個 schema
```

消費端的平台角色看不到產生端的其他資料表，也不能寫產生端的事件（讀取交易是 `READ ONLY`，也沒有寫入權限）；消費端的服務角色完全碰不到產生端的 schema。

## 限制與後續

- 保留期限：`outbox_event` 與 `outbox_inbox` 目前只增不刪。事件層也是操作紀錄（[現場脈絡](../../docs/architecture/field-context.md) AI 記憶第 4 點），保留與查詢介面由 Laysi/trustiot-v2#130 一併決定；刪除事件前要先確認每個訂閱都已讀過。
- 讀取上限受整個叢集最舊的寫入交易影響，見[順序與不漏讀](#順序與不漏讀)。
- 每個事件一筆交易，適合控制面的事件量；大量資料（遙測）不走 outbox（決策 8）。

## 測試

需要 podman 或 docker：每個測試起一個用完即刪、以測試時產生的 CA 啟用 TLS 的 `postgres:18.6-alpine` 容器，消費端以 service-kit 的 pool（`verify-full`、連線檢查）連線。沒有容器執行環境時在本機略過並印出 `skipped:`；設了 `CI` 時改為失敗。不使用 `TEST_DATABASE_URL`：測試要以 superuser 建角色，並需要知道伺服器的 CA。每個測試的 `Database::setup` 建立產生端與消費端的 schema、決策 29 的四個角色、`outbox_event` 的 RLS（service-kit 的範本）與上面的授權。

| 指令 | 內容 |
| --- | --- |
| `cargo test -p outbox --lib` | 聚合、位置、設定的檢查 |
| `cargo test -p outbox --test producer` | 回滾不留事件、聚合序號、同一信封只能寫一次 |
| `cargo test -p outbox --test delivery` | 交易 ID 反轉時仍依序、晚 commit 的事件不漏、並行寫入（兩個租戶）加兩個副本每個事件只處理一次、重複投遞跳過、重播後狀態相同、重試與死信、`begin` 在同一交易、未寫入的 handler 不拖住其他訂閱、收尾時只做完進行中的事件、lag、`StartFrom::Latest`、最小權限（產生端以租戶交易與平台角色寫事件） |
| `cargo test -p outbox --test tenancy` | 平台角色在 RLS 下讀到兩個租戶與全平台的事件、每個事件在自己的租戶處理且看不到別的租戶；handler 寫別的租戶被拒（42501）；服務角色被開放跨租戶讀取時連線被拒、讀取端沒有 `consumer_read` 時停下；信封租戶與 log 列不符時進死信 |
| `cargo test -p outbox --test notify` | 舊讀數不覆蓋新狀態（含副本競爭）、晚到與重播收斂 |
| `cargo test -p outbox --test crash` | 以 SIGKILL 結束 `outbox-example`：commit 後投遞前、handler 寫入後 commit 前，重啟後各處理一次，且在事件的租戶處理 |
| `cargo test -p outbox --test metrics` | lag 經 OTLP/HTTP 匯出到假的接收端，解碼後讀到數值 |
