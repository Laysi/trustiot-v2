/*
 * Fictional sample data of the chat-dashboards design set: a cooling-tower dashboard for the
 * demo organization 東區示範廠. Values are generated from fixed formulas, so every load shows the same
 * numbers. Times are shown in UTC+8.
 */
(function () {
  "use strict";

  var MIN = 60000;
  var pad = function (n) { return (n < 10 ? "0" : "") + n; };
  var local = function (t) { return new Date(t + 8 * 3600000); };
  var md = function (t) { var x = local(t); return pad(x.getUTCMonth() + 1) + "/" + pad(x.getUTCDate()); };
  var hm = function (t) { var x = local(t); return pad(x.getUTCHours()) + ":" + pad(x.getUTCMinutes()); };
  var fmtFull = function (t) { var x = local(t); return x.getUTCFullYear() + "-" + pad(x.getUTCMonth() + 1) + "-" + pad(x.getUTCDate()) + " " + hm(t); };

  // ---- dashboard page: three cooling towers, outlet temperature, four time ranges ----
  var RANGES = {
    "1h": { n: 61, step: MIN, label: "1 小時", bucket: "1 分鐘", interval: "1 MINUTE" },
    "24h": { n: 145, step: 10 * MIN, label: "24 小時", bucket: "10 分鐘", interval: "10 MINUTE" },
    "7d": { n: 169, step: 60 * MIN, label: "7 天", bucket: "1 小時", interval: "1 HOUR" },
    "30d": { n: 121, step: 360 * MIN, label: "30 天", bucket: "6 小時", interval: "6 HOUR" }
  };
  var TOWERS = [
    { name: "冷卻水塔 1 號", cls: "s1", base: 26.6, amp: 1.5, target: 28.1, ref: "end" },
    { name: "冷卻水塔 2 號", cls: "s2", base: 29.4, amp: 2.6, target: 32.6, ref: "end" },
    { name: "冷卻水塔 3 號", cls: "s3", base: 25.0, amp: 1.2, target: 25.9, ref: "off" }
  ];
  var cache = {};
  function dashboard(range) {
    if (cache[range]) return cache[range];
    var T_END = Date.UTC(2026, 10, 1, 6, 30), T_OFF = Date.UTC(2026, 10, 1, 4, 10);
    var R = RANGES[range];
    var noise = function (a, k) { var x = Math.sin((a % 100000) * 12.9898 + k * 78.233) * 43758.5453; return x - Math.floor(x) - 0.5; };
    var raw = function (s, k, t) {
      var hrs = t / 3600000 + 8, hh = hrs % 24, day = Math.floor(hrs / 24);
      return s.base + s.amp * Math.sin(2 * Math.PI * (hh - 9) / 24) + 0.7 * Math.sin(day * 1.7 + k) + noise(t / MIN, k) * 0.3;
    };
    var times = [];
    for (var i = 0; i < R.n; i++) times.push(T_END - (R.n - 1 - i) * R.step);
    var vals = TOWERS.map(function (s, k) {
      var ref = s.ref === "end" ? T_END : T_OFF;
      var off = s.target - raw(s, k, ref);
      return times.map(function (t) {
        if (k === 2 && t > T_OFF) return null; // tower 3 stopped reporting at 12:10
        return Math.round((raw(s, k, t) + off) * 10) / 10;
      });
    });
    cache[range] = { times: times, vals: vals, R: R };
    return cache[range];
  }

  var TOWER_LIST = "('冷卻水塔 1 號', '冷卻水塔 2 號', '冷卻水塔 3 號')";
  function dashboardSource(key, d, queried) {
    var R = d.R;
    var span = fmtFull(d.times[0]) + " – " + fmtFull(d.times[R.n - 1]);
    var five = "  AND time >= '2026-11-01 14:25:00'\n  AND time <  '2026-11-01 14:30:00'";
    var map = {
      out: { title: "冷卻水塔出水溫度・平均（5 分鐘）", desc: "冷卻水塔 1–3 號 temp_out 資料點最近一個完整 5 分鐘區間的平均。3 號從 12:10 起沒有讀數，不列入平均。", span: "2026-11-01 14:25:00 – 14:30:00", rows: "60 筆讀數", sql: "SELECT avg(value) AS mean,\n       count()    AS readings\nFROM readings\nWHERE device IN " + TOWER_LIST + "\n  AND point = 'temp_out'\n" + five },
      ret: { title: "冷卻水塔回水溫度・平均（5 分鐘）", desc: "冷卻水塔 1–3 號 temp_return 資料點最近一個完整 5 分鐘區間的平均。3 號從 12:10 起沒有讀數，不列入平均。", span: "2026-11-01 14:25:00 – 14:30:00", rows: "60 筆讀數", sql: "SELECT avg(value) AS mean,\n       count()    AS readings\nFROM readings\nWHERE device IN " + TOWER_LIST + "\n  AND point = 'temp_return'\n" + five },
      hum: { title: "冷卻水塔區濕度・平均（5 分鐘）", desc: "冷卻水塔區外氣溫濕度計 rh_ambient 資料點最近一個完整 5 分鐘區間的平均。", span: "2026-11-01 14:25:00 – 14:30:00", rows: "30 筆讀數", sql: "SELECT avg(value) AS mean,\n       count()    AS readings\nFROM readings\nWHERE device = '冷卻水塔區溫濕度計'\n  AND point = 'rh_ambient'\n" + five },
      abn: { title: "狀態不是正常的設備", desc: "這個儀表板的 6 台設備中，最新狀態是警告或嚴重的台數。離線代表沒有回報，另外計算，不算異常。", span: "每台設備最新一筆讀數", rows: "6 台設備", sql: "SELECT countIf(status IN ('warning', 'critical')) AS abnormal,\n       countIf(status = 'offline')              AS offline\nFROM latest_status\nWHERE device IN ('冷卻水塔 1 號', '冷卻水塔 2 號', '冷卻水塔 3 號',\n                 '冷卻水泵 A', '冷卻水泵 B', '補水泵')" },
      chart: { title: "冷卻水塔出水溫度（" + R.bucket + "平均）", desc: "三台冷卻水塔的 temp_out，以 " + R.bucket + "為區間取平均。警告 32 °C、嚴重 34 °C 是這個儀表板設定的門檻。", span: span, rows: "3 × " + R.n + " 個區間", sql: "SELECT device,\n       toStartOfInterval(time, INTERVAL " + R.interval + ") AS bucket,\n       avg(value) AS mean\nFROM readings\nWHERE device IN " + TOWER_LIST + "\n  AND point = 'temp_out'\n  AND time >= '" + fmtFull(d.times[0]) + ":00'\n  AND time <= '" + fmtFull(d.times[R.n - 1]) + ":00'\nGROUP BY device, bucket\nORDER BY bucket" },
      bar: { title: "各設備今日用電", desc: "每台設備電表 energy_kwh 今天 00:00 到 14:30 的累計（最後一筆減第一筆）。", span: "2026-11-01 00:00 – 14:30", rows: "6 台設備", sql: "SELECT device,\n       max(value) - min(value) AS kwh\nFROM readings\nWHERE point = 'energy_kwh'\n  AND time >= '2026-11-01 00:00:00'\n  AND time <  '2026-11-01 14:30:00'\nGROUP BY device\nORDER BY device" },
      table: { title: "設備狀態清單", desc: "每台設備最新一筆讀數，以及依這個儀表板的門檻判斷的狀態；超過 10 分鐘沒有讀數算離線。", span: "每台設備最新一筆讀數", rows: "6 筆", sql: "SELECT device, gateway, point, value, time, status\nFROM latest_status\nWHERE device IN ('冷卻水塔 1 號', '冷卻水塔 2 號', '冷卻水塔 3 號',\n                 '冷卻水泵 A', '冷卻水泵 B', '補水泵')\nORDER BY device" }
    };
    var p = map[key] || map.out;
    return { title: p.title, desc: p.desc, span: p.span, rows: p.rows, sql: p.sql, queried: queried };
  }

  // ---- conversation: fan power of the three towers, 80 two-hour averages from 10/26 ----
  function fanPower() {
    var hash = function (i, k) { var x = Math.sin(i * 12.9898 + k * 78.233) * 43758.5453; return x - Math.floor(x); };
    var out = [[], [], []];
    for (var i = 0; i < 80; i++) {
      var hh = i * 2, cyc = Math.sin(2 * Math.PI * ((hh % 24) - 8) / 24);
      out[0].push(Math.round((6.0 + 1.1 * cyc + (hash(i, 1) - 0.5) * 0.6) * 10) / 10);
      out[1].push(Math.round(((hh < 72 ? 6.1 : 8.8) + 1.2 * cyc + (hash(i, 2) - 0.5) * 0.7) * 10) / 10);
      out[2].push(Math.round((5.4 + 1.0 * cyc + (hash(i, 3) - 0.5) * 0.6) * 10) / 10);
    }
    return out;
  }
  function fanTimeLabel(i) {
    var hh = i * 2, d = 26 + Math.floor(hh / 24), hod = hh % 24;
    var date = d > 31 ? "11/" + pad(d - 31) : "10/" + d;
    return date + " " + pad(hod) + ":00";
  }
  var FAN_WHERE = "WHERE device IN " + TOWER_LIST + "\n  AND point = 'fan_power'\n  AND time >= '2026-10-26 00:00:00'\n  AND time <  '2026-11-01 14:00:00'";
  var QUERIED = "2026-11-01 14:32:05（UTC+8）";
  var conversationSources = {
    answer: { title: "冷卻水塔風扇功率・2 小時平均", desc: "冷卻水塔 1–3 號 fan_power 資料點每 2 小時的平均；用電量是平均功率乘以 2 小時的總和。", span: "2026-10-26 00:00 到 2026-11-01 14:00（UTC+8）", rows: "240 列（3 台 × 80 個區間）", sql: "SELECT device,\n       toStartOfInterval(time, INTERVAL 2 HOUR) AS bucket,\n       avg(value) AS kw\nFROM readings\n" + FAN_WHERE + "\nGROUP BY device, bucket\nORDER BY bucket" },
    energy: { title: "冷卻水塔 2 號・本週用電", desc: "2 號 fan_power 每 2 小時平均乘以 2 小時，加總成 kWh；上週同期用同樣的算法。", span: "2026-10-26 00:00 到 2026-11-01 14:00（UTC+8）", rows: "80 個區間", sql: "SELECT sum(kw) * 2 AS kwh\nFROM (\n  SELECT toStartOfInterval(time, INTERVAL 2 HOUR) AS bucket, avg(value) AS kw\n  FROM readings\n  WHERE device = '冷卻水塔 2 號' AND point = 'fan_power'\n    AND time >= '2026-10-26 00:00:00' AND time < '2026-11-01 14:00:00'\n  GROUP BY bucket\n)" },
    power: { title: "冷卻水塔 2 號・風扇平均功率（10/29 起）", desc: "2 號 fan_power 從 2026-10-29 00:00 起的平均。", span: "2026-10-29 00:00 到 2026-11-01 14:00（UTC+8）", rows: "5,184 筆讀數", sql: "SELECT avg(value) AS kw, count() AS readings\nFROM readings\nWHERE device = '冷卻水塔 2 號' AND point = 'fan_power'\n  AND time >= '2026-10-29 00:00:00' AND time < '2026-11-01 14:00:00'" },
    temp: { title: "冷卻水塔 2 號・出水溫度（5 分鐘平均）", desc: "2 號 temp_out 資料點最近一個完整 5 分鐘區間的平均；警告門檻 32 °C。", span: "2026-11-01 14:25 到 14:30（UTC+8）", rows: "20 筆讀數", sql: "SELECT avg(value) AS mean, count() AS readings\nFROM readings\nWHERE device = '冷卻水塔 2 號' AND point = 'temp_out'\n  AND time >= '2026-11-01 14:25:00' AND time < '2026-11-01 14:30:00'" }
  };
  Object.keys(conversationSources).forEach(function (k) { conversationSources[k].queried = QUERIED; });

  var fanVals = fanPower();
  // a polyline path over the 80 points for a fixed 0-12 kW axis (the pinned panel's small chart)
  function fanPath(k) {
    var d = "";
    for (var i = 0; i < 80; i++) d += (i ? "L" : "M") + (i / 79 * 1000).toFixed(1) + " " + (1000 - fanVals[k][i] / 12 * 1000).toFixed(1);
    return d;
  }

  TI.data = {
    md: md, hm: hm, fmtFull: fmtFull, RANGES: RANGES, TOWERS: TOWERS,
    dashboard: dashboard, dashboardSource: dashboardSource,
    fanVals: fanVals, fanTimeLabel: fanTimeLabel, fanPath: fanPath, conversationSources: conversationSources,
    QUERIED: QUERIED
  };
})();
