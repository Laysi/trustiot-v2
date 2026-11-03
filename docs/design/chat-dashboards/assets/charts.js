/*
 * Time-series chart of the chat-dashboards design set: hover crosshair and tooltip, legend toggles,
 * arrow-key reading, drag-to-zoom (optional). The plot is an SVG in a 1000 x 1000 viewBox stretched
 * over the plot box, with strokes kept at their token width (vector-effect: non-scaling-stroke).
 * Depends on ui.js (TI.h, TI.icon). Every node is built with DOM APIs.
 */
(function () {
  "use strict";
  var h = TI.h;

  /*
   * cfg: {
   *   series: [{name, cls}], times: [ms], vals: [[number|null]], yMin, yMax, yTicks: [values, top to bottom],
   *   yLabel(v), thresholds: [{v, kind, label}], zones: [{from, to, kind}],
   *   height, yW, top, bottom, right, unit, timeLabel(t, i), xTicks(view, times) -> [{pct, text, align}],
   *   interactive (default true), zoom, sortTip, legendLabel, legendClass, ariaLabel, readoutText(zoomed, view),
   *   onViewChange(zoomed, view)
   * }
   */
  function timeSeries(container, cfg) {
    var interactive = cfg.interactive !== false;
    var S = cfg.series;
    var times = cfg.times, vals = cfg.vals;
    var state = { view: null, active: null, sel: null, off: {}, hover: null, kbd: false };
    var drag = null;
    var yW = cfg.yW, plotLeft = yW + 8, top = cfg.top, bottom = cfg.bottom, right = cfg.right;
    var n = function () { return times.length; };
    var viewRange = function () { return state.view || [0, n() - 1]; };
    var X = function (i, a, b) { return (i - a) / Math.max(1, b - a) * 1000; };
    var Y = function (v) { return (cfg.yMax - v) / (cfg.yMax - cfg.yMin) * 1000; };

    // ---- static structure ----
    var svg = h("svg:svg", { class: "chart__svg", viewBox: "0 0 1000 1000", preserveAspectRatio: "none", "aria-hidden": "true" });
    (cfg.zones || []).forEach(function (z) {
      var y0 = Y(z.to), y1 = Y(z.from);
      svg.appendChild(h("svg:rect", { class: "zone--" + z.kind, x: 0, y: y0.toFixed(2), width: 1000, height: (y1 - y0).toFixed(2) }));
    });
    var tvals = (cfg.thresholds || []).map(function (t) { return t.v; });
    cfg.yTicks.forEach(function (v) {
      if (v === cfg.yMin || tvals.indexOf(v) >= 0) return;
      svg.appendChild(h("svg:line", { class: "grid-line", x1: 0, x2: 1000, y1: Y(v).toFixed(2), y2: Y(v).toFixed(2) }));
    });
    svg.appendChild(h("svg:line", { class: "baseline", x1: 0, x2: 1000, y1: 1000, y2: 1000 }));
    (cfg.thresholds || []).forEach(function (t) {
      svg.appendChild(h("svg:line", { class: "threshold threshold--" + t.kind, x1: 0, x2: 1000, y1: Y(t.v).toFixed(2), y2: Y(t.v).toFixed(2) }));
    });
    var groups = S.map(function (s) {
      var path = h("svg:path", { class: "line", d: "M0 0" });
      var g = h("svg:g", { class: "series " + s.cls }, path);
      svg.appendChild(g);
      return { g: g, path: path };
    });
    var cross = h("svg:line", { class: "crosshair off", x1: 0, x2: 0, y1: 0, y2: 1000 });
    svg.appendChild(cross);

    var plot = h("div", {
      class: "chart__plot" + (interactive ? "" : " chart__plot--static"),
      style: { left: plotLeft + "px", right: right + "px", top: top + "px", bottom: bottom + "px" }
    }, svg);
    if (interactive) {
      plot.setAttribute("role", "slider");
      plot.setAttribute("tabindex", "0");
      plot.setAttribute("aria-label", cfg.ariaLabel);
      plot.setAttribute("aria-orientation", "horizontal");
    } else {
      plot.setAttribute("role", "img");
      plot.setAttribute("aria-label", cfg.ariaLabel);
    }
    (cfg.thresholds || []).forEach(function (t) {
      plot.appendChild(h("span", {
        class: "chart__threshold-label chart__threshold-label--" + t.kind, "aria-hidden": "true", text: t.label,
        style: { top: "calc(" + (Y(t.v) / 10).toFixed(3) + "% - 18px)" }
      }));
    });
    var dotsBox = h("div", { "aria-hidden": "true" });
    var selBox = h("div", { "aria-hidden": "true" });
    var tipBox = h("div", { "aria-hidden": "true" });
    plot.appendChild(dotsBox); plot.appendChild(selBox); plot.appendChild(tipBox);

    var yAxis = h("div", { class: "chart__yaxis", "aria-hidden": "true", style: { left: "0", top: top + "px", bottom: bottom + "px", width: yW + "px" } },
      cfg.yTicks.map(function (v) {
        return h("span", { class: "chart__tick chart__tick--y", style: { top: (Y(v) / 10).toFixed(3) + "%" }, text: cfg.yLabel ? cfg.yLabel(v) : String(v) });
      }));
    var xAxis = h("div", { class: "chart__xaxis", "aria-hidden": "true", style: { left: plotLeft + "px", right: right + "px", bottom: "0", height: "20px" } });
    var area = h("div", { class: "chart__area", style: { height: cfg.height + "px" } }, yAxis, xAxis, plot);

    var legendEl = h("div", { class: "legend" + (cfg.legendClass ? " " + cfg.legendClass : ""), role: "group", "aria-label": cfg.legendLabel || "序列，按一下隱藏或顯示" },
      S.map(function (s, k) {
        return h("button", {
          type: "button", class: "legend__item " + s.cls, "aria-pressed": "true",
          onclick: function () { state.off[k] = !state.off[k]; render(); },
          onpointerenter: function () { state.hover = k; render(); },
          onpointerleave: function () { state.hover = null; render(); },
          onfocus: function () { state.hover = k; render(); },
          onblur: function () { state.hover = null; render(); }
        }, h("span", { class: "key" }), s.name);
      }));
    var readout = cfg.readoutText ? h("p", { class: "readout", "aria-hidden": "true" }) : null;
    container.classList.add("chart");
    container.appendChild(legendEl);
    if (readout) container.appendChild(readout);
    container.appendChild(area);

    // ---- interaction ----
    function idxFrom(e) {
      var r = plot.getBoundingClientRect();
      var f = Math.max(0, Math.min(1, (e.clientX - r.left) / r.width));
      var v = viewRange();
      return Math.round(v[0] + f * (v[1] - v[0]));
    }
    if (interactive) {
      plot.addEventListener("pointermove", function (e) {
        var i = idxFrom(e);
        if (drag) {
          if (Math.abs(e.clientX - drag.x0) > 4) { state.sel = [drag.from, i]; state.active = null; render(); }
          return;
        }
        if (i !== state.active) { state.active = i; render(); }
      });
      if (cfg.zoom) {
        plot.addEventListener("pointerdown", function (e) {
          drag = { from: idxFrom(e), x0: e.clientX };
          try { plot.setPointerCapture(e.pointerId); } catch (err) { /* synthetic events have no active pointer */ }
        });
        plot.addEventListener("pointerup", function () {
          var s = state.sel;
          drag = null;
          if (s && Math.abs(s[1] - s[0]) >= 3) {
            state.view = [Math.min(s[0], s[1]), Math.max(s[0], s[1])]; state.sel = null; state.active = null;
            render(); changed();
          } else if (s) { state.sel = null; render(); }
        });
      }
      plot.addEventListener("pointerleave", function () { if (!drag && !state.kbd) { state.active = null; render(); } });
      plot.addEventListener("focus", function () { state.kbd = true; if (state.active === null) { state.active = viewRange()[1]; render(); } });
      plot.addEventListener("blur", function () { state.kbd = false; state.active = null; render(); });
      plot.addEventListener("keydown", function (e) {
        var v = viewRange(), a = v[0], b = v[1];
        var cur = state.active === null ? b : state.active;
        var pg = Math.max(1, Math.round((b - a) / 10));
        var map = { ArrowLeft: cur - 1, ArrowDown: cur - 1, ArrowRight: cur + 1, ArrowUp: cur + 1, PageDown: cur - pg, PageUp: cur + pg, Home: a, End: b };
        if (e.key === "Escape") { state.active = null; state.sel = null; drag = null; render(); return; }
        if (!(e.key in map)) return;
        e.preventDefault();
        state.active = Math.max(a, Math.min(b, map[e.key]));
        render();
      });
    }
    function changed() { if (cfg.onViewChange) cfg.onViewChange(!!state.view, viewRange()); }

    // ---- render ----
    function render() {
      var v = viewRange(), a = v[0], b = v[1];
      S.forEach(function (s, k) {
        var d = "", run = 0;
        for (var i = Math.max(0, a - 1); i <= Math.min(n() - 1, b + 1); i++) {
          var val = vals[k][i];
          if (val === null) { run = 0; continue; }
          d += (run ? "L" : "M") + X(i, a, b).toFixed(1) + " " + Y(val).toFixed(1);
          run++;
        }
        groups[k].path.setAttribute("d", d || "M0 0");
        groups[k].g.classList.toggle("off", !!state.off[k]);
        if (state.hover !== null && state.hover !== k) groups[k].g.setAttribute("data-dim", ""); else groups[k].g.removeAttribute("data-dim");
      });
      Array.prototype.forEach.call(legendEl.children, function (btn, k) { btn.setAttribute("aria-pressed", String(!state.off[k])); });

      // x ticks
      TI.replace(xAxis, (cfg.xTicks ? cfg.xTicks(v, times) : []).map(function (t) {
        var st = {};
        if (t.align === "left") st.left = "0"; else if (t.align === "right") st.right = "0";
        else { st.left = t.pct.toFixed(2) + "%"; }
        return h("span", { class: "chart__tick chart__tick--x", style: Object.assign(st, t.align === "left" || t.align === "right" ? { transform: "none" } : {}), text: t.text });
      }));

      var act = state.active !== null && state.active >= a && state.active <= b ? state.active : null;
      var rows = [], dots = [];
      if (act !== null) {
        S.forEach(function (s, k) {
          if (state.off[k]) return;
          var val = vals[k][act];
          if (val !== null) dots.push(h("span", { class: "chart__dot " + s.cls, style: { left: (X(act, a, b) / 10).toFixed(2) + "%", top: (Y(val) / 10).toFixed(2) + "%" } }));
          rows.push({ k: k, val: val });
        });
        if (cfg.sortTip) rows.sort(function (p, q) { return (q.val === null ? -1 : q.val) - (p.val === null ? -1 : p.val); });
      }
      TI.replace(dotsBox, dots);
      cross.classList.toggle("off", act === null);
      if (act !== null) { cross.setAttribute("x1", X(act, a, b).toFixed(1)); cross.setAttribute("x2", X(act, a, b).toFixed(1)); }

      var sel = state.sel;
      TI.replace(selBox, sel ? [h("span", {
        class: "chart__selection",
        style: { left: (X(Math.min(sel[0], sel[1]), a, b) / 10).toFixed(2) + "%", width: ((X(Math.max(sel[0], sel[1]), a, b) - X(Math.min(sel[0], sel[1]), a, b)) / 10).toFixed(2) + "%" }
      })] : []);

      var tipText = "";
      if (act !== null && !sel && rows.length) {
        var px = X(act, a, b) / 10;
        var timeText = cfg.timeLabel(times[act], act);
        var tipStyle = { top: "8px" };
        if (px < 62) tipStyle.left = "calc(" + px.toFixed(2) + "% + 12px)"; else tipStyle.right = "calc(" + (100 - px).toFixed(2) + "% + 12px)";
        TI.replace(tipBox, [h("div", { class: "tooltip", style: tipStyle },
          h("div", { class: "tooltip__time", text: timeText }),
          rows.map(function (r) {
            return h("div", { class: "tooltip__row " + S[r.k].cls },
              h("span", { class: "key" }),
              h("span", { class: "tooltip__value" + (r.val === null ? " tooltip__value--none" : ""), text: r.val === null ? "沒有讀數" : r.val.toFixed(1) + cfg.unit }),
              h("span", { class: "tooltip__name", text: S[r.k].name }));
          }))]);
        tipText = timeText + "：" + rows.map(function (r) { return S[r.k].name + " " + (r.val === null ? "沒有讀數" : r.val.toFixed(1) + cfg.unit); }).join("，");
      } else TI.replace(tipBox, []);

      if (interactive) {
        plot.setAttribute("aria-valuemin", String(a));
        plot.setAttribute("aria-valuemax", String(b));
        plot.setAttribute("aria-valuenow", String(act === null ? b : act));
        plot.setAttribute("aria-valuetext", tipText || "移到圖上或用方向鍵讀取數值");
      }
      if (readout) readout.textContent = cfg.readoutText(!!state.view, v);
    }
    render();

    return {
      resetZoom: function () { state.view = null; state.active = null; state.sel = null; render(); changed(); },
      setData: function (d) { times = d.times; vals = d.vals; state.view = null; state.active = null; state.sel = null; render(); changed(); },
      setRefetching: function (on) { area.classList.toggle("refetch", !!on); },
      isZoomed: function () { return !!state.view; },
      view: viewRange,
      // exposed for the review scripts
      plot: plot, legend: legendEl
    };
  }

  TI.timeSeries = timeSeries;
})();
