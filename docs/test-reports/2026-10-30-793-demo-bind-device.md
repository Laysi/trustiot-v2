# 展示環境綁定閘道器、建立型號與設備並開始收資料（2026-10-30）

對應 issue：Laysi/trustiot-v2#793（父 issue Laysi/trustiot-v2#458）。這份報告接續 [Laysi/trustiot-v2#795 的報告](2026-10-30-795-demo-data-path.md)：那時展示用閘道器 `tg-lab-0001` 已經連上 broker，但還沒綁定，`online` 被拒（0x87）。

結果先講：

- 平台管理員以 API 建立展示租戶 TrustIoT Demo，綁定 `tg-lab-0001`，建立溫濕度變送器的型號與設備，沒有手動改資料庫。
- 綁定後 certs 立即把閘道器加回 broker 的 gateway 角色，閘道器第一次重連就被接受，gateway-ops 顯示在線，並記下設定版本 2 的套用回報。
- 一開始沒有遙測：agent 的 systemd 沙箱缺 `AF_NETLINK`，位址守門員讀不到介面，每次 Modbus 連線都被拒。已開 Laysi/trustiot-v2#805，修正為 PR Laysi/trustiot-v2#806（`95c24084`），並裝到閘道器上。
- 兩個資料點在 ClickHouse 各有 80 列，開始收資料的時間是 2026-10-30 16:08 UTC。

## 環境

| 項目 | 內容 |
| --- | --- |
| 展示環境 | lab 的 Mac mini M1 上的 k3s，Flux 部署，chart `0.0.0-rc.12`（同 Laysi/trustiot-v2#795 的報告） |
| 前置 PR | Laysi/trustiot-v2#804（`fa39b216`）：在展示環境的 app 主機加 cookie 路由 `/api/assets` 與 `/api/gateway-ops` |
| 閘道器 | lab 的閘道器（硬體 ID `tg-lab-0001`），agent 版本 0.1.0 |
| 設備 | lab 的 RS485 轉換器上的溫濕度變送器，RTU over TCP；轉換器的位址不公開 |
| 操作者 | 平台管理員，以 `trustiot-workstation login`（email 驗證碼）登入 |
| 租戶 | 展示租戶 TrustIoT Demo |
| 修正 | PR Laysi/trustiot-v2#806，合併為 `95c24084` |

時間都是 2026-10-30 的 UTC。主機名稱、位址、email 與各種 ID 不寫進報告。

## 指令

操作腳本放在 repo 外，因為它內含轉換器的位址。它先以 dry run 執行（只做 GET），再以 `--apply` 執行；每一步都先查現況，已完成就回報 already done（可重跑）。

```sh
# 平台管理員登入（email 驗證碼）
trustiot-workstation login
# dry run：只 GET。第一次在第 2 步停下，因為還沒有租戶
<operator script>
# 實際執行（15:46:15–15:46:17）
<operator script> --apply
# 重跑 dry run，確認每一步都 already done，並讀 gateway-ops
<operator script>
# 閘道器上（唯讀）
sudo journalctl -u trustiot-agent --since "<綁定時間>"
```

ClickHouse 的每個資料點查詢（17:27）：協調者的管理帳號被 `deny_by_default` row policy 擋住（看得到 0 列），所以在 ClickHouse pod 內建立臨時使用者，只允許本機連線，隨機密碼不印出；設 SETTINGS PROFILE `trustiot_tenant_reader` 與展示租戶的常數 `SQL_tenant_id`，授與角色 `trustiot_tenant_reader`，查完立刻刪除。

```sql
SELECT point, count() AS n, min(ts) AS first, max(ts) AS last,
       round(argMin(value_float, ts), 1) AS first_value,
       round(argMax(value_float, ts), 1) AS last_value,
       round(min(value_float), 1) AS lo, round(max(value_float), 1) AS hi
FROM trustiot.tenant_telemetry
GROUP BY point
ORDER BY point
```

## 結果

### 前置：路由

PR Laysi/trustiot-v2#804 合併後，Flux 套用成功。HTTPRoute `trustiot-ingress-app` 有 `/api/assets` 與 `/api/gateway-ops` 兩條規則，identity Deployment 已滾動，它的 `TRUSTIOT_INGRESS_ROUTES` 列出 `/api/identity`、`/api/certs`、`/api/assets`、`/api/gateway-ops`。

### 以 API 綁定與建立（15:46:15–15:46:17）

| 步驟 | 請求 | 回應 |
| --- | --- | --- |
| 目前的 session | `GET /api/identity/v1/sessions/current` | 200，`platform_admin` 為 true，沒有租戶 |
| 建立租戶 | `POST /api/identity/v1/tenants`，名稱 `TrustIoT Demo` | 201 |
| 加為 admin | `PUT /api/identity/v1/platform/tenants/{tenant}/members`，role `admin` | 201，`previous_role` 為 null |
| 切換租戶 | `PUT /api/identity/v1/sessions/current/tenant`；`GET /api/assets/v1/tenant` | 200；200。active tenant 為 TrustIoT Demo，role admin |
| 查閘道器 | `GET /api/assets/v1/platform/gateways/tg-lab-0001` | 200，09:41:47 已登錄，binding 為 null |
| 綁定 | `PUT /api/assets/v1/platform/gateways/tg-lab-0001/binding` | 201，`bound_at` 15:46:16.345 |
| 型號 | `GET /api/assets/v1/device-models`；`POST` 型號 `ebyte-eid041-temp-humidity` | 200（空）；201，revision 1 |
| 設備 | `GET /api/assets/v1/devices`；`POST` 設備 `lab-temp-humidity-1` | 200（空）；201 |
| gateway-ops | `GET /api/gateway-ops/v1/gateways` | 200，建立設備當下 presence unknown，`desired_config_version` 1 |

型號的資料點：

| 資料點 | function code | 位址 | 型別 | 縮放 | 位元組順序 |
| --- | --- | --- | --- | --- | --- |
| temperature | 4 | 0 | i16 | 0.1 | abcd |
| humidity | 4 | 1 | u16 | 0.1 | abcd |

設備：transport `rtu_over_tcp`，host 與 port 為 lab 的 RS485 轉換器（位址不公開；port 8886），`unit_id` 1，`poll_interval_ms` 60000。整個過程沒有手動改資料庫。

### broker 與設定下發

| 時間 | 服務 | 事件 |
| --- | --- | --- |
| 15:46:16.614 | certs | `applied assets.gateway_bound` |
| 15:46:16.666 | ingest | `applied assets.gateway_bound` |
| 15:46:16.689 | certs | `changed a gateway's broker permissions`：action added，role `trustiot-gateway` |
| 15:46:18.557 | gateway-ops | `gateway configuration published`：version 2，trigger change |
| 15:46:24.883 | gateway-ops | `gateway presence changed`：state online |
| 15:46:25.009 | gateway-ops | `apply report recorded`：version 2，outcome applied，active_version 2 |

綁定前 agent 的 `online` 被拒（PUBACK 0x87，預期，見 Laysi/trustiot-v2#795）。綁定後 15:46:24 的第一次連線被接受，agent 日誌有 `applied the configuration version=2 previous=None devices=1 changes=1 added`。

重跑 dry run（每一步都回 already done）時，gateway-ops 顯示：

- `presence: online`，`presence_changed_at` 15:46:24.87，`last_seen_at` 15:47:24.97
- `agent_version` 0.1.0，`desired_config_version` 2，`applied_config_version` null

`applied_config_version` 來自心跳，而 agent 的心跳固定送 `config_version` 為 None（`edge/agent/src/heartbeat.rs`），所以它一直是 null，雖然套用回報已經送達。這是後續提案，尚未開 issue。

### 沒有遙測的 bug 與修正

- 現象：綁定後約 20 分鐘沒有遙測。agent 的 debug 日誌每次輪詢都是 `polled … readings=0 failed=2`。
- 排除設備：從閘道器對轉換器送一個原始的 Modbus RTU 請求（站號 1、FC4、位址 0、2 個暫存器），得到有效回應，溫度 28.8 °C、濕度 45.6 %RH。轉換器沒有問題。
- 根因：agent 的 systemd unit 的 `RestrictAddressFamilies` 沒有 `AF_NETLINK`，位址守門員的 `getifaddrs` 失敗（EAFNOSUPPORT，errno 97），每次 Modbus 連線在 TCP 之前就被拒。已開 Laysi/trustiot-v2#805。
- 暫時處置：在閘道器加 runtime drop-in 補上 `AF_NETLINK`。之後每次輪詢讀到 2 個值，雲端在 16:08:27 確認收下第一批（第一筆讀值的時間戳 16:08:17.659）。
- 正式修正：PR Laysi/trustiot-v2#806 合併為 `95c24084`。17:18:16 把合併後的 unit 裝到閘道器並移除 drop-in。
- 修正後：遙測的列數持續增加，17:17:57 到 17:21:26 之間從 142 增到 150（4 次輪詢 × 2 個資料點），agent 日誌沒有 WARN 或 ERROR。

### 每個資料點的 ClickHouse 查詢（17:27）

查詢在上面的「指令」一節。臨時使用者查完就刪除，`system.users` 已沒有它。

| 資料點 | 列數 | 最早 | 最晚 | 第一筆 | 最後一筆 | 範圍 |
| --- | --- | --- | --- | --- | --- | --- |
| humidity | 80 | 16:08:17.659 | 17:26:16.799 | 45.6 | 45.8 | 45.6–45.9 %RH |
| temperature | 80 | 16:08:17.659 | 17:26:16.799 | 28.8 | 28.8 | 28.8–28.9 °C |

### 開始收資料的日期

記在 Laysi/trustiot-v2#458 的現狀：2026-10-30 16:08 UTC。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 綁定之後 gateway-ops 看得到 `tg-lab-0001` 在線 | 通過：`GET /api/gateway-ops/v1/gateways` 回 `presence: online`，自 15:46:24 UTC 起，15:47:24 仍有 `last_seen_at` 更新 |
| 綁定與設備都以 API 建立，沒有手動改資料庫 | 通過：請求與回應在「以 API 綁定與建立」一節 |
| agent 回報設定套用成功；每個資料點在 ClickHouse 都有第一筆資料 | 通過：套用回報 `outcome: applied`、active_version 2；temperature 與 humidity 各 80 列，第一筆都是 16:08:17.659。限制：列表的 `applied_config_version` 是 null（心跳不帶設定版本），屬後續提案 |
| 開始收資料的日期寫進 Laysi/trustiot-v2#458 的現狀 | 通過：2026-10-30 16:08 UTC |
| 測試報告存進 `docs/test-reports/` | 通過：就是這份報告 |

後續：

- 24 小時連續資料與斷電重開：Laysi/trustiot-v2#794。重開機會讓 runtime drop-in 消失，所以該 issue 須在 Laysi/trustiot-v2#805 的修正裝上之後才有意義；17:18:16 起閘道器已是合併後的 unit。
- 心跳的 `config_version`：尚未開 issue。
