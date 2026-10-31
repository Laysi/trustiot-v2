# 展示用閘道器連續 24 小時的資料連續性（2026-10-31）

對應 issue：Laysi/trustiot-v2#794（父 issue Laysi/trustiot-v2#458）。這份報告接續 [Laysi/trustiot-v2#793 的報告](2026-10-30-793-demo-bind-device.md)：展示用閘道器 `tg-lab-0001` 在 2026-10-30 16:08 UTC 開始收資料，17:18:16 UTC 換上 Laysi/trustiot-v2#805 的正式修正（agent 重啟一次）。

結果先講：

- 24 小時窗口內，兩個資料點（`temperature`、`humidity`）各有 1440 列，正好是每分鐘一筆的預期值；時間戳沒有重複，最大間隔 60.014 秒，沒有超過 90 秒的間隔。
- 窗口內雲端連線被重設 3 次（18:46:19、01:16:04、11:07:24），agent 每次都在 1 秒內重新連上，沒有造成缺漏。agent 在窗口內沒有重啟，叢集的 pod 也沒有重啟。

## 環境

| 項目 | 內容 |
| --- | --- |
| 展示環境 | lab 的 Mac mini M1 上的 k3s，Flux 部署，chart `0.0.0-rc.12`；HelmRelease 從 2026-10-30 15:42 UTC 起 Ready，窗口內沒有重新部署 |
| 閘道器 | lab 的閘道器（硬體 ID `tg-lab-0001`），agent 0.1.0，systemd 單元含 PR Laysi/trustiot-v2#806 的修正 |
| 設備 | lab 的 RS485 轉換器上的溫濕度變送器（型號 `ebyte-eid041-temp-humidity`、設備 `lab-temp-humidity-1`），設定的資料點是 `temperature` 與 `humidity`，`poll_interval_ms` 60000；轉換器的位址不公開 |
| 窗口 | 2026-10-30 17:19:00 UTC 起、2026-10-31 17:19:00 UTC 止（含起點、不含終點）；每分鐘一筆，預期每個資料點 1440 列 |
| 查詢時間 | 2026-10-31 17:29:30 UTC（ClickHouse）；閘道器與叢集的唯讀檢查在 17:23 UTC 前後 |

時間都是 UTC。主機名稱、位址、email 與各種 ID 不寫進報告。

## 指令

ClickHouse 的做法同 Laysi/trustiot-v2#793 的報告：管理帳號被 `deny_by_default` row policy 擋住（看得到 0 列），所以在 ClickHouse pod 內建立臨時使用者，只允許本機連線，隨機密碼不印出；設 SETTINGS PROFILE `trustiot_tenant_reader` 與展示租戶的常數 `SQL_tenant_id`，授與角色 `trustiot_tenant_reader`，查完立刻刪除（17:29:30 建立、17:29:31 刪除）。

```sql
-- 1. 窗口內每個資料點的列數、時間戳、頭尾與相鄰兩筆的間隔
WITH
    toDateTime64('2026-10-30 17:19:00', 3, 'UTC') AS w_start,
    toDateTime64('2026-10-31 17:19:00', 3, 'UTC') AS w_end
SELECT
    point,
    count() AS n,
    uniqExact(ts) AS distinct_ts,
    min(ts) AS first,
    max(ts) AS last,
    dateDiff('millisecond', w_start, min(ts)) AS lead_in_ms,
    dateDiff('millisecond', max(ts), w_end) AS tail_ms,
    maxIf(gap_ms, rn > 1) AS max_gap_ms,
    countIf(rn > 1 AND gap_ms > 90000) AS gaps_over_90s,
    countIf(rn > 1 AND gap_ms < 30000) AS gaps_under_30s
FROM
(
    SELECT
        point,
        ts,
        row_number() OVER w AS rn,
        dateDiff('millisecond', lagInFrame(ts) OVER w, ts) AS gap_ms
    FROM trustiot.tenant_telemetry
    WHERE ts >= w_start AND ts < w_end
    WINDOW w AS (PARTITION BY point ORDER BY ts ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)
)
GROUP BY point
ORDER BY point;

-- 2. 窗口之前的那一段（16:08 開始收資料到換上修正）
SELECT point, count() AS n, min(ts) AS first, max(ts) AS last
FROM trustiot.tenant_telemetry
WHERE ts >= toDateTime64('2026-10-30 16:00:00', 3, 'UTC')
  AND ts < toDateTime64('2026-10-30 17:19:00', 3, 'UTC')
GROUP BY point
ORDER BY point;

-- 3. 窗口內所有超過 90 秒的間隔與前後兩筆（連續時沒有任何列）
SELECT point, prev_ts, ts, gap_ms
FROM
(
    SELECT
        point,
        ts,
        lagInFrame(ts) OVER w AS prev_ts,
        row_number() OVER w AS rn,
        dateDiff('millisecond', lagInFrame(ts) OVER w, ts) AS gap_ms
    FROM trustiot.tenant_telemetry
    WHERE ts >= toDateTime64('2026-10-30 17:19:00', 3, 'UTC')
      AND ts < toDateTime64('2026-10-31 17:19:00', 3, 'UTC')
    WINDOW w AS (PARTITION BY point ORDER BY ts ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)
)
WHERE rn > 1 AND gap_ms > 90000
ORDER BY point, ts;
```

閘道器（唯讀）：

```sh
systemctl show trustiot-agent -p ActiveEnterTimestamp -p NRestarts -p ActiveState
journalctl -u trustiot-agent --since "2026-10-30 17:19:00 UTC" --until "2026-10-31 17:19:00 UTC" -o short-iso \
  | grep -E 'WARN|ERROR|connected to the broker|failed|reconnect'
```

agent 的日誌寫到 stdout，journald 一律記成 info，所以 `journalctl -p warning` 篩不出 agent 的 WARN，要直接搜尋日誌文字。

叢集（唯讀）：

```sh
kubectl -n trustiot get pods -o custom-columns=NAME:.metadata.name,START:.status.startTime,RESTARTS:.status.containerStatuses[*].restartCount,LAST_TERMINATED:.status.containerStatuses[*].lastState.terminated.finishedAt
kubectl get gitrepositories.source.toolkit.fluxcd.io,helmreleases.helm.toolkit.fluxcd.io -A
```

## 結果

### 窗口內的每個資料點

| 資料點 | 列數 | 不同的 ts | 第一筆 | 最後一筆 | 起點到第一筆 | 最後一筆到終點 | 最大間隔 | 超過 90 秒的間隔 | 小於 30 秒的間隔 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| humidity | 1440 | 1440 | 10-30 17:19:16.796 | 10-31 17:18:16.799 | 16.796 秒 | 43.201 秒 | 60.014 秒 | 0 | 0 |
| temperature | 1440 | 1440 | 10-30 17:19:16.796 | 10-31 17:18:16.799 | 16.796 秒 | 43.201 秒 | 60.014 秒 | 0 | 0 |

第三段查詢（超過 90 秒的間隔）沒有回傳任何列。

讀法：

- 1440 列且 1440 個不同的時間戳：每分鐘一筆、沒有重複寫入。
- 起點到第一筆 16.8 秒、最後一筆到終點 43.2 秒，兩者相加約 60 秒，也就是窗口兩端都沒有少掉一筆。
- 最大間隔 60.014 秒，沒有任何間隔超過 90 秒，也沒有小於 30 秒的間隔。

### 窗口之前（16:08–17:19）

| 資料點 | 列數 | 第一筆 | 最後一筆 |
| --- | --- | --- | --- |
| humidity | 72 | 10-30 16:08:17.659 | 10-30 17:18:16.806 |
| temperature | 72 | 10-30 16:08:17.659 | 10-30 17:18:16.806 |

這一段包含 17:18:16 換上修正時 agent 的重啟，只列出來、不算進 24 小時的窗口。窗口前最後一筆（17:18:16.806）到窗口第一筆（17:19:16.796）相隔約 60 秒，兩段之間沒有斷開。

### 連線與服務事件

| 時間 | 事件 |
| --- | --- |
| 2026-10-30 17:18:16 | agent 啟動（換上 Laysi/trustiot-v2#805 的修正）。到 2026-10-31 17:23 檢查時，systemd 記錄的重啟次數是 0，agent 從這個時間起持續執行 |
| 2026-10-30 18:46:19 | 雲端連線被重設：agent 記錄 WebSocket 連線沒有經過關閉交握就被重設；broker 同一秒記錄連線被對端關閉（叢集內轉送的一端讀到 EOF）。期中檢查時 Flux 停在 main `608766cc` |
| 2026-10-30 18:46:20 | agent 重新連上 broker（TLS 1.3） |
| 2026-10-30 19:27 | 期中唯讀檢查：閘道器的磁碟緩衝是空的 |
| 2026-10-31 01:16:04 | 雲端連線被重設，agent 記錄的錯誤與 18:46:19 相同 |
| 2026-10-31 01:16:05 | agent 重新連上 broker |
| 2026-10-31 11:07:24 | 雲端連線被重設，agent 記錄的錯誤相同；同一秒重新連上 broker |

叢集：窗口內所有 pod 都沒有重啟（broker 唯一的一次重啟在 2026-10-30 12:15:34，早於窗口）；HelmRelease 的 chart 一直是 `0.0.0-rc.12`。窗口內照排程執行的 CronJob 有 identity 的金鑰輪替（10-31 03:17）與 broker 憑證的工作（10-31 17:15），都沒有讓 pod 重啟。

三次重設的原因沒有再追。第一次的雙方日誌顯示，是 broker 前面叢集內轉送的那一端讀到 EOF，可能是外部通道的連線被重設；三次都在 1 秒內重新連上，資料點每分鐘一筆，所以都沒有造成缺漏。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 每個設定的資料點連續 24 小時沒有缺漏（查詢與結果存進 `docs/test-reports/`） | 通過：`temperature` 與 `humidity` 各 1440 列、1440 個不同的時間戳，最大間隔 60.014 秒，沒有超過 90 秒的間隔；窗口內三次雲端連線重設都沒有造成缺漏 |
| 斷電重開後自動連回並繼續送資料 | 已移出：2026-10-31 依驗證分級移到 Laysi/trustiot-v2#102 的最後一項 |
