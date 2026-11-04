/*
 * The dashboard page of the chat-dashboards design set: KPI tiles, the temperature line chart
 * (hover, legend toggles, drag to zoom, time ranges), the bar chart, the sortable table, the source
 * panel and the re-query. TI.mountDashboard(root, {variant}) fills `root`; variant "draft" is the
 * smaller version shown beside the conversation on the draft page (no title block, no bar chart,
 * no humidity tile).
 */
(function () {
  "use strict";
  var h = TI.h, icon = TI.icon, D = TI.data;

  function sparkPath(vals) {
    var lo = Math.min.apply(null, vals), hi = Math.max.apply(null, vals), rg = Math.max(0.0001, hi - lo);
    var pts = vals.map(function (x, j) { return [j / (vals.length - 1) * 96, 26 - (x - lo) / rg * 24]; });
    return { d: pts.map(function (p, j) { return (j ? "L" : "M") + p[0].toFixed(1) + " " + p[1].toFixed(1); }).join(""), endY: pts[pts.length - 1][1].toFixed(1) };
  }

  var BAR_DATA = [["冷卻水塔 1 號", 186], ["冷卻水塔 2 號", 214], ["冷卻水塔 3 號", 97], ["冷卻水泵 A", 342], ["冷卻水泵 B", 318], ["補水泵", 41]];
  var ROWS = [
    { name: "冷卻水塔 1 號", gw: "GW-0042", reading: "出水 28.1 °C", time: "14:30:12", t: 143012, status: "normal" },
    { name: "冷卻水塔 2 號", gw: "GW-0042", reading: "出水 32.6 °C", time: "14:30:08", t: 143008, status: "warning" },
    { name: "冷卻水塔 3 號", gw: "GW-0043", reading: "出水 25.9 °C", time: "12:10:44", t: 121044, status: "offline" },
    { name: "冷卻水泵 A", gw: "GW-0042", reading: "電流 42.7 A", time: "14:30:10", t: 143010, status: "normal" },
    { name: "冷卻水泵 B", gw: "GW-0042", reading: "電流 71.3 A", time: "14:30:09", t: 143009, status: "critical" },
    { name: "補水泵", gw: "GW-0043", reading: "電流 3.2 A", time: "14:30:05", t: 143005, status: "normal" }
  ];
  var RANK = { critical: 3, warning: 2, offline: 1, normal: 0 };
  var STATUS = {
    normal: { cls: "status--normal", icon: "circle-check", text: "正常" },
    warning: { cls: "status--warning", icon: "triangle-alert", text: "警告" },
    critical: { cls: "status--critical", icon: "octagon-alert", text: "嚴重" },
    offline: { cls: "status--offline", icon: "cloud-off", text: "離線" }
  };
  function statusBadge(key) {
    var s = STATUS[key];
    return h("span", { class: "status status--plain " + s.cls }, icon(s.icon), s.text);
  }

  function mountDashboard(root, opts) {
    var draft = opts && opts.variant === "draft";
    var state = { range: "24h", refreshing: false, dataTime: "2026-11-01 14:32:05（UTC+8）", source: null, sort: { key: "status", dir: "descending" }, bar: null };
    var timer = null;
    var triggers = [];       // every "show source" button: {key, el}
    var refetchEls = [];
    var popEl = null, popTrigger = null;
    root.style.position = "relative";

    // ---- source panel ----
    function closePop(refocus) {
      if (popEl) { popEl.remove(); popEl = null; }
      state.source = null;
      triggers.forEach(function (t) { t.el.setAttribute("aria-expanded", "false"); });
      if (refocus && popTrigger) { try { popTrigger.focus({ preventScroll: true }); } catch (err) { /* detached */ } }
    }
    function openSource(key, btn) {
      if (state.source === key) { closePop(true); return; }
      closePop(false);
      var r = root.getBoundingClientRect(), b = btn.getBoundingClientRect();
      var w = Math.min(384, r.width - 32);
      var left = b.left - r.left;
      if (left + w > r.width - 16) left = b.right - r.left - w;
      left = Math.max(16, Math.min(left, r.width - w - 16));
      popTrigger = btn;
      state.source = key;
      triggers.forEach(function (t) { t.el.setAttribute("aria-expanded", String(t.key === key)); });
      popEl = TI.sourcePanel(D.dashboardSource(key, D.dashboard(state.range), state.dataTime), {
        onClose: function () { closePop(true); }, aiHref: "conversation.html", cls: "popover--abs"
      });
      popEl.style.top = Math.round(b.bottom - r.top + 8) + "px";
      popEl.style.left = Math.round(left) + "px";
      popEl.style.width = Math.round(w) + "px";
      root.appendChild(popEl);
      var close = popEl.querySelector("[data-close]");
      if (close) { try { close.focus({ preventScroll: true }); } catch (err) { /* ignore */ } }
    }
    function sourceBtn(key, cls, kids, attrs) {
      var el = h("button", Object.assign({ type: "button", class: cls, "aria-haspopup": "dialog", "aria-expanded": "false", "data-source": key, onclick: function () { openSource(key, el); } }, attrs || {}), kids);
      triggers.push({ key: key, el: el });
      return el;
    }

    // ---- title block (page variant) ----
    var dataTimeEl = h("span", { class: "num", text: "資料時間 " + state.dataTime });
    var refreshBtn = null, refreshSpinner = null, chartSpinner = null, rangeSpinner = null;
    function titleBlock() {
      refreshBtn = h("button", { type: "button", class: "btn btn--outline", onclick: refresh }, icon("refresh"), "更新資料");
      return h("div", { class: "dash-title" },
        h("nav", { class: "crumbs", "aria-label": "位置" }, h("a", { href: "list.html", text: "已存的儀表板" }), icon("chevron-right", "icon--sm"), h("span", { "aria-current": "page", text: "冷卻水塔監控" })),
        h("div", { class: "dash-title__row" },
          h("div", { class: "dash-title__text" },
            h("div", { class: "row-wrap", style: { gap: "8px 12px" } },
              h("h1", { class: "h1", text: "冷卻水塔監控" }),
              h("span", { class: "tag tag--ok" }, icon("circle-check", "icon--sm"), "已發布・第 4 版"),
              h("a", { class: "chip-link", href: "draft.html" }, icon("pencil", "icon--sm"), "有 1 份未發布的草稿")),
            h("p", { class: "muted" }, dataTimeEl, "・每次開啟都會重新查詢・示範使用者 A 於 2026-10-30 16:05 發布")),
          h("div", { class: "btn-row" },
            refreshBtn,
            h("button", { type: "button", class: "btn btn--outline" }, icon("history"), "版本紀錄"),
            h("a", { class: "btn btn--default", href: "draft.html" }, icon("message"), "在對話中編輯"))));
    }
    var tag = function (el) { refetchEls.push(el); return el; };

    // ---- range row ----
    var rangeText = h("span", { class: "num muted" });
    var segBtns = ["1h", "24h", "7d", "30d"].map(function (k) {
      return h("button", { type: "button", class: "btn btn--sm", "aria-pressed": String(state.range === k), "data-range": k, text: D.RANGES[k].label, onclick: function () { setRange(k); } });
    });
    rangeSpinner = h("span", { class: "spinner", role: "status", hidden: true }, icon("loader", "spin"), "正在更新…");
    var rangeRow = h("div", { class: "row-wrap" }, h("div", { class: "seg", role: "group", "aria-label": "時間範圍" }, segBtns), rangeText, rangeSpinner);

    // ---- KPI tiles ----
    var d24 = D.dashboard("24h");
    var last = function (arr) { return arr.slice(arr.length - 12); };
    var outSeries = last(d24.vals[0]).map(function (x, j) { return (x + last(d24.vals[1])[j]) / 2; });
    var retSeries = outSeries.map(function (x, j) { return x + 4.7 + Math.sin(j * 1.3) * 0.15; });
    var humSeries = [72, 72, 71, 71, 71, 70, 70, 69, 69, 69, 68, 68];
    var tileDefs = [
      { key: "out", label: "出水溫度・平均", value: "30.4", unit: "°C", delta: "比 1 小時前 +0.8 °C", trend: "up", sp: sparkPath(outSeries) },
      { key: "ret", label: "回水溫度・平均", value: "35.2", unit: "°C", delta: "比 1 小時前 +0.5 °C", trend: "up", sp: sparkPath(retSeries) },
      { key: "hum", label: "平均濕度", value: "68", unit: "%RH", delta: "比昨天同時段 -4 %RH", trend: "down", sp: sparkPath(humSeries) },
      { key: "abn", label: "異常設備數", value: "2", unit: "台", delta: "1 台警告、1 台嚴重；另有 1 台離線", critical: true }
    ];
    if (draft) tileDefs = tileDefs.filter(function (x) { return x.key !== "hum"; });
    var tiles = h("div", { class: "tile-grid" }, tileDefs.map(function (x) {
      return tag(h("article", { class: "tile", "aria-label": x.label },
        h("div", { class: "tile__top" },
          h("span", { class: "tile__label", text: x.label }),
          x.critical ? h("span", { class: "status status--critical" }, icon("octagon-alert"), "嚴重") : null),
        h("div", { class: "tile__main" },
          sourceBtn(x.key, "source", [h("span", { class: "tile__value", text: x.value }), h("span", { class: "tile__unit", text: x.unit }), h("span", { class: "source__hint" }, icon("database", "icon--sm"))], { "aria-label": x.label + "：" + x.value + " " + x.unit + "，查看資料來源" }),
          x.sp ? h("svg:svg", { class: "spark", viewBox: "0 0 96 28", "aria-hidden": "true" }, h("svg:path", { class: "spark__line", d: x.sp.d }), h("svg:circle", { class: "spark__end", cx: 96, cy: x.sp.endY, r: 4 })) : null),
        h("span", { class: "delta" }, x.trend === "up" ? icon("trending-up") : x.trend === "down" ? icon("trending-down") : null, x.delta)));
    }));

    // ---- line chart card ----
    var chartHost = h("div", { class: "card__content" });
    var resetBtn = h("button", { type: "button", class: "btn btn--ghost btn--sm", disabled: true, onclick: function () { ts.resetZoom(); } }, icon("zoom-out"), "重設縮放");
    chartSpinner = h("span", { class: "spinner", hidden: true }, icon("loader", "spin"), "正在更新…");
    var chartDesc = h("p", { class: "card__description" });
    var tsCard = h("section", { class: "card card--compact", "aria-labelledby": "dash-ts-title" },
      h("div", { class: "card__header card__header--chart" },
        h("h2", { class: "card__title", id: "dash-ts-title", text: "冷卻水塔出水溫度" }),
        chartDesc,
        h("div", { class: "card__action toolbar" }, chartSpinner, resetBtn, sourceBtn("chart", "btn btn--ghost btn--sm", [icon("database"), "資料來源"]))),
      chartHost);

    function xTicks(v, times) {
      var tStart = times[v[0]], tEnd = times[v[1]], spanMin = (tEnd - tStart) / 60000;
      var cands = [5, 10, 15, 30, 60, 120, 180, 360, 720, 1440, 2880, 4320, 7200, 10080];
      var c = cands[cands.length - 1];
      for (var ci = 0; ci < cands.length; ci++) { if (spanMin / cands[ci] <= 7) { c = cands[ci]; break; } }
      var out = [], lmStart = tStart / 60000 + 480;
      for (var lm = Math.ceil(lmStart / c) * c; lm <= tEnd / 60000 + 480; lm += c) {
        var t = (lm - 480) * 60000;
        var pct = (t - tStart) / Math.max(1, tEnd - tStart) * 100;
        if (pct < 3 || pct > 97) continue;
        out.push({ pct: pct, text: (c >= 1440 || lm % 1440 === 0) ? D.md(t) : D.hm(t) });
      }
      return out;
    }
    var d0 = D.dashboard(state.range);
    var ts = TI.timeSeries(chartHost, {
      series: D.TOWERS, times: d0.times, vals: d0.vals, yMin: 22, yMax: 36, yTicks: [36, 34, 32, 30, 28, 26, 24, 22],
      yLabel: function (v) { return v + "°"; },
      thresholds: [{ v: 34, kind: "critical", label: "嚴重 34°" }, { v: 32, kind: "warning", label: "警告 32°" }],
      zones: [{ from: 34, to: 36, kind: "critical" }, { from: 32, to: 34, kind: "warning" }],
      height: 288, yW: 36, top: 8, bottom: 28, right: 8, unit: " °C", zoom: true,
      legendLabel: "序列",
      ariaLabel: "冷卻水塔 1 到 3 號的出水溫度。可以用方向鍵逐一讀取每個區間，Escape 取消。",
      timeLabel: function (t) { return D.md(t) + " " + D.hm(t); },
      xTicks: xTicks,
      readoutText: function (zoomed, v) {
        var times = D.dashboard(state.range).times;
        return zoomed
          ? "顯示 " + D.md(times[v[0]]) + " " + D.hm(times[v[0]]) + " – " + D.md(times[v[1]]) + " " + D.hm(times[v[1]]) + "；按「重設縮放」回到全段。"
          : "把游標移到圖表上讀取每個區間；在圖表上左右拖曳可以放大一段時間。";
      },
      onViewChange: function (zoomed) { resetBtn.disabled = !zoomed; }
    });
    refetchEls.push(chartHost.querySelector(".chart__area"));

    // ---- bar chart card (page variant) ----
    var barCard = null;
    if (!draft) {
      var barTicks = [400, 300, 200, 100, 0];
      var barTipBox = h("div", { "aria-hidden": "true" });
      var showBar = function (j) {
        TI.replace(barTipBox, j === null ? [] : [h("div", { class: "tooltip", style: { top: "-20px", left: ((j + 0.5) / BAR_DATA.length * 100).toFixed(2) + "%", transform: "translateX(-50%)" } },
          h("div", { class: "tooltip__time", text: BAR_DATA[j][0] }),
          h("div", { class: "tooltip__row s1" }, h("span", { class: "key key--box" }), h("span", { class: "tooltip__value", text: BAR_DATA[j][1] + " kWh" }), h("span", { class: "tooltip__name", text: "今日累計" })))]);
      };
      var nf = new Intl.NumberFormat("zh-TW");
      var barCols = BAR_DATA.map(function (x, j) {
        var pct = (x[1] / 400 * 100).toFixed(2);
        return h("li", { class: "barcol", tabindex: "0", "aria-label": x[0] + " " + nf.format(x[1]) + " kWh", onpointerenter: function () { showBar(j); }, onpointerleave: function () { showBar(null); }, onfocus: function () { showBar(j); }, onblur: function () { showBar(null); } },
          h("span", { class: "barcol__value", "aria-hidden": "true", style: { bottom: "calc(" + pct + "% + 4px)" }, text: nf.format(x[1]) }),
          h("span", { class: "barcol__bar", "aria-hidden": "true", style: { height: pct + "%" } }));
      });
      var barArea = h("div", { class: "chart__area", style: { height: "232px" } },
        h("div", { class: "chart__yaxis", "aria-hidden": "true", style: { left: "0", top: "24px", bottom: "0", width: "36px" } },
          barTicks.map(function (v) { return h("span", { class: "chart__tick chart__tick--y", style: { top: ((400 - v) / 400 * 100).toFixed(2) + "%" }, text: nf.format(v) }); })),
        h("div", { style: { position: "absolute", left: "44px", right: "8px", top: "24px", bottom: "0" } },
          barTicks.map(function (v) { return h("span", { "aria-hidden": "true", style: { position: "absolute", left: "0", right: "0", top: ((400 - v) / 400 * 100).toFixed(2) + "%", height: "1px", background: v === 0 ? "var(--input)" : "var(--chart-grid)" } }); }),
          h("ul", { class: "bars", "aria-label": "各設備今日用電，單位 kWh" }, barCols),
          barTipBox));
      barCard = h("section", { class: "card card--compact", "aria-labelledby": "dash-bar-title", style: { gap: "16px" } },
        h("div", { class: "card__header card__header--chart" },
          h("h2", { class: "card__title", id: "dash-bar-title", text: "各設備今日用電" }),
          h("p", { class: "card__description", text: "2026-11-01 00:00 到 14:30 的累計，單位 kWh。" }),
          h("div", { class: "card__action toolbar" }, sourceBtn("bar", "btn btn--ghost btn--sm", [icon("database"), "資料來源"]))),
        h("div", { class: "card__content chart" },
          barArea,
          h("div", { class: "bar-cats", "aria-hidden": "true" }, BAR_DATA.map(function (x) { return h("span", { text: x[0] }); }))));
      refetchEls.push(barArea);
    }

    // ---- table card ----
    var thead = h("tr"), tbody = h("tbody");
    var COLS = [["name", "設備", true, ""], ["gw", "閘道器", false, ""], ["reading", "最新讀數", false, "r"], ["time", "讀取時間", true, "r"], ["status", "狀態", true, ""]];
    function renderTable() {
      var sk = state.sort.key, sd = state.sort.dir;
      TI.replace(thead, COLS.map(function (cd) {
        var on = cd[2] && sk === cd[0] ? sd : "none";
        var th = h("th", { scope: "col", class: cd[3], "aria-sort": cd[2] ? on : null });
        if (cd[2]) {
          th.appendChild(h("button", { type: "button", class: "sort", onclick: function () {
            var next = sk !== cd[0] ? "descending" : (sd === "descending" ? "ascending" : sd === "ascending" ? "none" : "descending");
            state.sort = { key: cd[0], dir: next };
            renderTable();
          } }, cd[1], icon(on === "ascending" ? "arrow-up" : on === "descending" ? "arrow-down" : "chevrons-up-down")));
        } else th.textContent = cd[1];
        return th;
      }));
      var sorted = ROWS.slice();
      if (sd !== "none") {
        sorted.sort(function (p, q) {
          var r = sk === "name" ? p.name.localeCompare(q.name, "zh-Hant-TW") : sk === "time" ? p.t - q.t : RANK[p.status] - RANK[q.status];
          return sd === "ascending" ? r : -r;
        });
      }
      TI.replace(tbody, sorted.map(function (r) {
        return h("tr", null,
          h("th", { scope: "row", text: r.name }),
          h("td", null, h("span", { class: "mono", text: r.gw })),
          h("td", { class: "r", text: r.reading }),
          h("td", { class: "r muted", text: r.time }),
          h("td", null, statusBadge(r.status)));
      }));
    }
    var tableWrap = h("div", { class: "table-wrap" }, h("table", { class: "table", style: { "min-width": "520px" } },
      h("caption", { class: "sr-only", text: "設備狀態清單，可依設備、讀取時間或狀態排序" }), h("thead", null, thead), tbody));
    refetchEls.push(tableWrap);
    var tableCard = h("section", { class: "card card--compact", "aria-labelledby": "dash-tb-title", style: { gap: "16px" } },
      h("div", { class: "card__header card__header--chart" },
        h("h2", { class: "card__title", id: "dash-tb-title", text: "設備狀態清單" }),
        h("p", { class: "card__description", text: "每台設備最新一筆讀數與依門檻判斷的狀態。" }),
        h("div", { class: "card__action toolbar" }, sourceBtn("table", "btn btn--ghost btn--sm", [icon("database"), "資料來源"]))),
      h("div", { class: "card__content" }, tableWrap));
    renderTable();

    // ---- behaviour ----
    function updateRange() {
      var d = D.dashboard(state.range);
      rangeText.textContent = D.fmtFull(d.times[0]) + " – " + D.fmtFull(d.times[d.R.n - 1]) + "（UTC+8）";
      chartDesc.textContent = "以 " + d.R.bucket + "為區間取平均，時間是你的時區（UTC+8）。門檻：警告 32 °C、嚴重 34 °C。";
    }
    function setRange(k) {
      state.range = k;
      segBtns.forEach(function (b) { b.setAttribute("aria-pressed", String(b.getAttribute("data-range") === k)); });
      closePop(false);
      ts.setData(D.dashboard(k));
      updateRange();
    }
    function refresh() {
      if (state.refreshing) return;
      state.refreshing = true;
      applyRefreshing();
      clearTimeout(timer);
      timer = setTimeout(function () {
        state.refreshing = false;
        state.dataTime = "2026-11-01 14:33:18（UTC+8）";
        dataTimeEl.textContent = "資料時間 " + state.dataTime;
        applyRefreshing();
      }, 1400);
    }
    function applyRefreshing() {
      var on = state.refreshing;
      refetchEls.forEach(function (el) { el.classList.toggle("refetch", on); });
      chartSpinner.hidden = !on; rangeSpinner.hidden = !on;
      if (refreshBtn) {
        refreshBtn.disabled = on;
        TI.replace(refreshBtn, on ? [icon("loader", "spin"), "更新中…"] : [icon("refresh"), "更新資料"]);
      }
    }
    document.addEventListener("keydown", function (e) { if (e.key === "Escape" && popEl && !popEl.contains(document.activeElement)) closePop(true); });

    updateRange();
    TI.replace(root, [
      draft ? null : titleBlock(),
      rangeRow, tiles, tsCard,
      h("div", { class: "card-grid" }, barCard, tableCard)
    ]);
    // the title block carries no source popover of its own; keep root positioned for the panel
    return { openSource: openSource, closePop: closePop, refresh: refresh, setRange: setRange, chart: ts };
  }

  TI.mountDashboard = mountDashboard;
})();
