/*
 * Shared helpers of the chat-dashboards design set: theme, DOM builder, icons, the shared shell
 * header (docs/design/app-shell.md once Laysi/trustiot-v2#1000 lands) and the design bar.
 * Loaded in <head>, so the theme is set before the first paint. Every node is built with DOM APIs
 * and textContent; no markup is assembled from strings.
 */
(function () {
  "use strict";

  var SVG_NS = "http://www.w3.org/2000/svg";
  var params = new URLSearchParams(location.search);

  // ---- theme: ?theme=dark|light, otherwise the operating system's setting ----
  function currentTheme() {
    var t = params.get("theme");
    if (t === "dark" || t === "light") return t;
    return matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  document.documentElement.setAttribute("data-theme", currentTheme());

  // ---- DOM builder: h("div", {class: "x", onclick: fn}, child, ...); "svg:path" makes an SVG node ----
  function h(tag, attrs) {
    var svg = tag.indexOf("svg:") === 0;
    var el = svg ? document.createElementNS(SVG_NS, tag.slice(4)) : document.createElement(tag);
    var keys = Object.keys(attrs || {});
    for (var i = 0; i < keys.length; i++) {
      var k = keys[i], v = attrs[k];
      if (v === null || v === undefined || v === false) continue;
      if (k === "text") el.textContent = v;
      else if (k.indexOf("on") === 0) el.addEventListener(k.slice(2), v);
      else if (k === "style" && typeof v === "object") {
        Object.keys(v).forEach(function (p) { el.style.setProperty(p, v[p]); });
      } else el.setAttribute(k, v === true ? "" : String(v));
    }
    append(el, Array.prototype.slice.call(arguments, 2));
    return el;
  }
  function append(el, kids) {
    kids.forEach(function (c) {
      if (c === null || c === undefined || c === false) return;
      if (Array.isArray(c)) append(el, c);
      else el.appendChild(typeof c === "object" ? c : document.createTextNode(String(c)));
    });
  }
  function replace(el, kids) {
    el.textContent = "";
    append(el, kids);
  }

  // ---- icons (Lucide shapes; a string is a path, an array is [tag, attrs]) ----
  var ICONS = {
    building: ["M6 22V4a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2v18Z", "M6 12H4a2 2 0 0 0-2 2v6a2 2 0 0 0 2 2h2", "M18 9h2a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-2", "M10 6h4", "M10 10h4", "M10 14h4", "M10 18h4"],
    "chevron-down": ["m6 9 6 6 6-6"],
    "chevron-right": ["m9 18 6-6-6-6"],
    menu: ["M4 12h16", "M4 6h16", "M4 18h16"],
    account: [["circle", { cx: 12, cy: 12, r: 10 }], ["circle", { cx: 12, cy: 10, r: 3 }], "M7 20.662V19a2 2 0 0 1 2-2h6a2 2 0 0 1 2 2v1.662"],
    "arrow-left": ["m12 19-7-7 7-7", "M19 12H5"],
    "arrow-right": ["M5 12h14", "m12 5 7 7-7 7"],
    plus: ["M12 5v14", "M5 12h14"],
    dashboard: [["rect", { width: 7, height: 9, x: 3, y: 3, rx: 1 }], ["rect", { width: 7, height: 5, x: 14, y: 3, rx: 1 }], ["rect", { width: 7, height: 9, x: 14, y: 12, rx: 1 }], ["rect", { width: 7, height: 5, x: 3, y: 16, rx: 1 }]],
    cpu: [["rect", { width: 16, height: 16, x: 4, y: 4, rx: 2 }], ["rect", { width: 6, height: 6, x: 9, y: 9, rx: 1 }], "M15 2v2", "M15 20v2", "M2 15h2", "M2 9h2", "M20 15h2", "M20 9h2", "M9 2v2", "M9 20v2"],
    package: ["M11 21.73a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73z", "M12 22V12", "m3.3 7 8.7 5 8.7-5", "m7.5 4.27 9 5.15"],
    router: [["rect", { width: 20, height: 8, x: 2, y: 14, rx: 2 }], "M6.01 18H6", "M10.01 18H10", "M15 10v4", "M17.84 7.17a4 4 0 0 0-5.66 0", "M20.66 4.34a8 8 0 0 0-11.31 0"],
    bell: ["M10.268 21a2 2 0 0 0 3.464 0", "M3.262 15.326A1 1 0 0 0 4 17h16a1 1 0 0 0 .74-1.673C19.41 13.956 18 12.499 18 8A6 6 0 0 0 6 8c0 4.499-1.411 5.956-2.738 7.326"],
    "chart-column": ["M3 3v16a2 2 0 0 0 2 2h16", "M18 17V9", "M13 17V5", "M8 17v-3"],
    factory: ["M2 20a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V8l-7 5V8l-7 5V4a2 2 0 0 0-2-2H4a2 2 0 0 0-2 2Z", "M17 18h1", "M12 18h1", "M7 18h1"],
    "book-open": ["M12 7v14", "M3 18a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h5a4 4 0 0 1 4 4 4 4 0 0 1 4-4h5a1 1 0 0 1 1 1v13a1 1 0 0 1-1 1h-6a3 3 0 0 0-3 3 3 3 0 0 0-3-3z"],
    users: ["M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2", ["circle", { cx: 9, cy: 7, r: 4 }], "M22 21v-2a4 4 0 0 0-3-3.87", "M16 3.13a4 4 0 0 1 0 7.75"],
    settings: ["M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z", ["circle", { cx: 12, cy: 12, r: 3 }]],
    "shield-check": ["M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z", "m9 12 2 2 4-4"],
    database: [["ellipse", { cx: 12, cy: 5, rx: 9, ry: 3 }], "M3 5V19A9 3 0 0 0 21 19V5", "M3 12A9 3 0 0 0 21 12"],
    x: ["M18 6 6 18", "m6 6 12 12"],
    copy: [["rect", { width: 14, height: 14, x: 8, y: 8, rx: 2, ry: 2 }], "M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"],
    sparkles: ["M9.937 15.5A2 2 0 0 0 8.5 14.063l-6.135-1.582a.5.5 0 0 1 0-.962L8.5 9.936A2 2 0 0 0 9.937 8.5l1.582-6.135a.5.5 0 0 1 .963 0L14.063 8.5A2 2 0 0 0 15.5 9.937l6.135 1.581a.5.5 0 0 1 0 .964L15.5 14.063a2 2 0 0 0-1.437 1.437l-1.582 6.135a.5.5 0 0 1-.963 0z", "M20 3v4", "M22 5h-4", "M4 17v2", "M5 18H3"],
    history: ["M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8", "M3 3v5h5", "M12 7v5l4 2"],
    pencil: ["M12 20h9", "M16.376 3.622a1 1 0 0 1 3.002 3.002L7.368 18.635a2 2 0 0 1-.855.506l-2.872.838a.5.5 0 0 1-.62-.62l.838-2.872a2 2 0 0 1 .506-.854z"],
    message: ["M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"],
    refresh: ["M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8", "M21 3v5h-5", "M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16", "M8 16H3v5"],
    loader: ["M21 12a9 9 0 1 1-6.219-8.56"],
    "zoom-out": [["circle", { cx: 11, cy: 11, r: 8 }], ["line", { x1: 21, x2: 16.65, y1: 21, y2: 16.65 }], ["line", { x1: 8, x2: 14, y1: 11, y2: 11 }]],
    "trending-up": ["M16 7h6v6", "m22 7-8.5 8.5-5-5L2 17"],
    "trending-down": ["M16 17h6v-6", "m22 17-8.5-8.5-5 5L2 7"],
    "octagon-alert": ["M12 16h.01", "M12 8v4", "M15.312 2a2 2 0 0 1 1.414.586l4.688 4.688A2 2 0 0 1 22 8.688v6.624a2 2 0 0 1-.586 1.414l-4.688 4.688a2 2 0 0 1-1.414.586H8.688a2 2 0 0 1-1.414-.586l-4.688-4.688A2 2 0 0 1 2 15.312V8.688a2 2 0 0 1 .586-1.414l4.688-4.688A2 2 0 0 1 8.688 2z"],
    "triangle-alert": ["m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3", "M12 9v4", "M12 17h.01"],
    "circle-check": [["circle", { cx: 12, cy: 12, r: 10 }], "m9 12 2 2 4-4"],
    "cloud-off": ["m2 2 20 20", "M5.782 5.782A7 7 0 0 0 9 19h8.5a4.5 4.5 0 0 0 1.307-.193", "M21.532 16.5A4.5 4.5 0 0 0 17.5 10h-1.79A7.008 7.008 0 0 0 10 5.07"],
    "chevrons-up-down": ["m7 15 5 5 5-5", "m7 9 5-5 5 5"],
    "arrow-down": ["M12 5v14", "m19 12-7 7-7-7"],
    "arrow-up": ["m5 12 7-7 7 7", "M12 19V5"],
    pin: ["M12 17v5", "M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z"],
    check: ["M20 6 9 17l-5-5"],
    "external-link": ["M15 3h6v6", "M10 14 21 3", "M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"],
    upload: ["M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4", "m17 8-5-5-5 5", "M12 3v12"],
    archive: [["rect", { width: 20, height: 5, x: 2, y: 3, rx: 1 }], "M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8", "M10 12h4"],
    "archive-restore": [["rect", { width: 20, height: 5, x: 2, y: 3, rx: 1 }], "M4 8v11a2 2 0 0 0 2 2h2", "M20 8v11a2 2 0 0 1-2 2h-2", "m9 15 3-3 3 3", "M12 12v9"],
    eye: ["M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0", ["circle", { cx: 12, cy: 12, r: 3 }]],
    search: [["circle", { cx: 11, cy: 11, r: 8 }], "m21 21-4.3-4.3"],
    "search-x": ["m13.5 8.5-5 5", "m8.5 8.5 5 5", ["circle", { cx: 11, cy: 11, r: 8 }], "m21 21-4.3-4.3"],
    hourglass: ["M5 22h14", "M5 2h14", "M17 22v-4.172a2 2 0 0 0-.586-1.414L12 12l-4.414 4.414A2 2 0 0 0 7 17.828V22", "M7 2v4.172a2 2 0 0 0 .586 1.414L12 12l4.414-4.414A2 2 0 0 0 17 6.172V2"],
    clock: [["circle", { cx: 12, cy: 12, r: 10 }], "M12 6v6l4 2"],
    code: ["m16 18 6-6-6-6", "m8 6-6 6 6 6"],
    zap: ["M4 14a1 1 0 0 1-.78-1.63l9.9-10.2a.5.5 0 0 1 .86.46l-1.92 6.02A1 1 0 0 0 13 10h7a1 1 0 0 1 .78 1.63l-9.9 10.2a.5.5 0 0 1-.86-.46l1.92-6.02A1 1 0 0 0 11 14z"],
    thermometer: ["M14 4v10.54a4 4 0 1 1-4 0V4a2 2 0 0 1 4 0Z"],
    "chart-line": ["M3 3v16a2 2 0 0 0 2 2h16", "M7 16l4-4 4 4 5-6"]
  };

  function icon(name, cls) {
    var spec = ICONS[name];
    var svg = h("svg:svg", { class: "icon" + (cls ? " " + cls : ""), viewBox: "0 0 24 24", "aria-hidden": "true" });
    (spec || []).forEach(function (s) {
      svg.appendChild(typeof s === "string" ? h("svg:path", { d: s }) : h("svg:" + s[0], s[1]));
    });
    return svg;
  }
  // <i data-icon="pin" data-icon-class="icon--sm"></i> becomes the SVG, keeping the element's own class.
  function hydrateIcons(root) {
    (root || document).querySelectorAll("[data-icon]").forEach(function (n) {
      var svg = icon(n.getAttribute("data-icon"), n.getAttribute("class") || "");
      n.replaceWith(svg);
    });
  }

  // ---- logo: the TrustIoT mark (frontend/public/favicon.svg), colored by the page's tokens ----
  var LOGO_PATHS = [
    "m54.74,110.37c1.09,1.05,2.58,9.21,3.36,11.32,24.58,65.79,137.14,109.53,177.49,35.1-.14,11-1.66,17.96-5.81,28.02-4.37,9.33-11.21,17.29-19.79,22.99-18.12,12.06-43.2,12-63.88,7.91-32.48-6.43-63.3-24.08-85.65-48.5-6.96-7.6-12.05-15.22-17.71-23.73,2.44-12.71,5.83-21.65,11.99-33.1Z",
    "m58.27,8.26c8.77-1.4,24.93.53,33.61,2.33,34.06,7.33,64.78,25.6,87.47,52.04,5.29,6.21,9.53,12.52,13.99,19.31-3.16,14.56-5.14,20.51-12.24,33.58-3.93-21.77-14.96-37.23-31.35-51.65-6.48-5.79-13.48-10.98-20.92-15.48-29.23-17.88-71.66-26.23-102.46-7.35C14.18,48.51,7.18,57.51,0,69.48c1.63-15.39,3.78-26.62,13.9-39.23,11.49-14.31,26.53-20.11,44.37-21.99Z",
    "m82.63,39.5c9.73,1.07,26.82,7.22,35.2,11.88-54.82,10.22-96.02,77.73-81.01,131.41,4.46,15.96,12.97,26.48,25.12,37.35,2.75,2.3,5.94,4.45,8.89,6.52-3.15-.37-6.28-.71-9.39-1.29-43.31-7.98-55.23-47.23-47.79-86.05,6.28-32.3,22.88-61.68,47.29-83.74,7.18-6.38,13.79-10.75,21.69-16.09Z",
    "m167.01.04c3.39-.17,7.85.3,11.18.97,14.95,3.15,28.08,12.02,36.58,24.71,11.59,17.52,12.24,42.81,7.7,62.6-10.13,44.21-32.8,73.92-69.82,98.28-3-.65-6.27-1.55-9.26-2.31-9.28-2.55-16.3-5.45-24.78-9.91,1.1-.21,2.21-.42,3.3-.64,26.32-5.4,46.92-24.87,61.42-46.6C211.02,85.62,212.62,29.36,167.01.04Z",
    "m115.91,92.2c12.11-1.2,22.92,7.64,24.13,19.75,1.22,12.11-7.61,22.92-19.72,24.15-12.14,1.23-22.97-7.61-24.19-19.75-1.22-12.14,7.64-22.96,19.78-24.16Z",
    "m49.52,193.74c2.18.76,5.16,2.22,7.67,3.11,10.7,3.78,18.83,3.96,29.87,2.49,8.84,6.55,19.2,11.29,29.33,15.47l-7.08,3.09c-8.51,3.35-16.02,5.39-24.86,7.28-15.46-7.11-26.09-16.87-34.94-31.43Z",
    "m150.92,1.17c2.59-.56,8.61,2.66,10.86,4.16,9.99,6.65,19.17,15.65,24.86,26.28-7.61-4.04-15.8-5.89-24.36-6.15-4.33-.12-8.38,0-12.63.84-10.33-6.87-17.34-10.38-28.55-15.21,11.34-5.3,17.59-7.29,29.83-9.92Z",
    "m29.18,48.51l.28.13c.34.85-2.31,10.33-2.59,13.54-.72,8.22-.7,13.1.73,21.23-7.33,11.14-10.04,17.52-15.06,29.74-.85-1.63-1.7-3.55-2.5-5.24-3.74-8.73-6.03-16.16-8.09-25.38,6.16-15.14,14.46-24.38,27.22-34.03Z",
    "m223.31,113.84c2.94,2.48,9.42,24.13,10.69,29.23-4.63,12.5-12.47,22.48-22.67,30.89-1.58,1.41-3.88,2.96-5.64,4.23,3.88-10.8,5.13-23.93,2.64-35.19,7.16-10.7,10.12-17.52,14.98-29.17Z"
  ];
  function logo(extraClass) {
    var svg = h("svg:svg", { viewBox: "0 0 235.59 226.66", "aria-hidden": "true" });
    LOGO_PATHS.forEach(function (d) { svg.appendChild(h("svg:path", { d: d })); });
    return h("span", { class: "logo" + (extraClass ? " " + extraClass : ""), "aria-hidden": "true" }, svg);
  }
  function hydrateLogos(root) {
    (root || document).querySelectorAll("[data-logo]").forEach(function (n) {
      var l = logo(n.getAttribute("data-logo"));
      n.replaceWith(l);
    });
  }

  // ---- shared shell header ----
  var ORGS = ["東區示範廠", "南區示範廠"];
  var org = ORGS[0];
  var MENU_GROUPS = [
    [{ id: "saved", label: "已存的儀表板", icon: "dashboard", href: "list.html" }],
    [
      { id: "devices", label: "設備", icon: "cpu", href: "../tenant-admin/devices--devices-normal.html" },
      { id: "models", label: "型號", icon: "package", href: "../tenant-admin/devices--models-normal.html" },
      { id: "gateways", label: "閘道器", icon: "router", href: "../tenant-admin/gateways--normal.html" }
    ],
    [
      { id: "alerts", label: "警報與通知", icon: "bell", href: "../ms4-ms5/alerts.html" },
      { id: "reports", label: "報表", icon: "chart-column", href: "../ms4-ms5/reports.html" },
      { id: "machines", label: "機台與生產紀錄", icon: "factory", href: "../ms4-ms5/machines.html" },
      { id: "documents", label: "文件庫", icon: "book-open", href: "../ms4-ms5/documents.html" }
    ],
    [
      { id: "members", label: "成員", icon: "users", href: "../tenant-admin/members--normal.html" },
      { id: "settings", label: "設定", icon: "settings", href: null }
    ],
    [{ id: "platform", label: "平台後台", icon: "shield-check", href: "../platform-admin/index.html", platformOnly: true }]
  ];

  var openMenus = [];
  function closeAllMenus(except) {
    openMenus.slice().forEach(function (m) { if (m !== except) m.close(); });
  }
  // A dropdown: a trigger button and a role="menu" panel; Escape and a click outside close it.
  function dropdown(trigger, panel) {
    var api = {
      open: function (byKeyboard) {
        closeAllMenus(api);
        panel.hidden = false;
        trigger.setAttribute("aria-expanded", "true");
        if (openMenus.indexOf(api) < 0) openMenus.push(api);
        if (byKeyboard) {
          var first = panel.querySelector('[role^="menuitem"]');
          if (first) first.focus();
        }
      },
      close: function (refocus) {
        panel.hidden = true;
        trigger.setAttribute("aria-expanded", "false");
        openMenus = openMenus.filter(function (m) { return m !== api; });
        if (refocus) trigger.focus();
      },
      toggle: function (byKeyboard) { if (panel.hidden) api.open(byKeyboard); else api.close(); }
    };
    trigger.addEventListener("click", function (e) { api.toggle(e.detail === 0); });
    panel.addEventListener("keydown", function (e) {
      var items = Array.prototype.slice.call(panel.querySelectorAll('[role^="menuitem"]'));
      var i = items.indexOf(document.activeElement);
      if (e.key === "ArrowDown") { e.preventDefault(); items[(i + 1) % items.length].focus(); }
      else if (e.key === "ArrowUp") { e.preventDefault(); items[(i - 1 + items.length) % items.length].focus(); }
    });
    return api;
  }
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape" && openMenus.length) {
      var m = openMenus[openMenus.length - 1];
      m.close(true);
    }
  });
  document.addEventListener("click", function (e) {
    openMenus.slice().forEach(function (m) {
      if (!m.wrap.contains(e.target)) m.close();
    });
  });

  function menuItem(it, current) {
    var attrs = { class: "menu__item", role: "menuitem" };
    var kids = [icon(it.icon), it.label];
    if (it.href) {
      attrs.href = it.href;
      if (it.id === current) attrs["aria-current"] = "page";
      return h("a", attrs, kids);
    }
    attrs.type = "button";
    attrs.onclick = function () { closeAllMenus(); };
    return h("button", attrs, kids);
  }

  function buildHeader(root) {
    var body = document.body;
    var secondary = body.hasAttribute("data-secondary");
    var current = body.getAttribute("data-current");
    var platform = params.get("platform") === "1";

    var brand = h("a", { class: "brand", href: "home.html", "aria-label": "TrustIoT，回到對話首頁" }, logo(), h("span", { class: "brand__name", text: "TrustIoT" }));

    var orgName = h("span", { class: "org__name", "data-org": "", text: org });
    var orgBtn = h("button", { type: "button", class: "btn btn--ghost btn--sm", "aria-haspopup": "menu", "aria-expanded": "false" }, icon("building"), orgName, icon("chevron-down"));
    var orgPanel = h("div", { class: "menu menu--left", role: "menu", "aria-label": "切換組織", hidden: true }, h("p", { class: "menu__label", text: "組織" }));
    var orgWrap = h("div", { class: "menu-wrap org" }, orgBtn, orgPanel);
    function syncOrg() {
      orgBtn.setAttribute("aria-label", "切換組織，目前是" + org);
      document.querySelectorAll("[data-org]").forEach(function (n) { n.textContent = org; });
      Array.prototype.forEach.call(orgPanel.querySelectorAll('[role="menuitemradio"]'), function (b) {
        var on = b.getAttribute("data-name") === org;
        b.setAttribute("aria-checked", String(on));
        var ind = b.querySelector(".menu__indicator");
        ind.textContent = "";
        if (on) ind.appendChild(icon("check"));
      });
    }
    var orgApi = dropdown(orgBtn, orgPanel);
    orgApi.wrap = orgWrap;
    ORGS.forEach(function (name) {
      orgPanel.appendChild(h("button", {
        type: "button", class: "menu__item menu__item--radio", role: "menuitemradio", "data-name": name, "aria-checked": "false",
        onclick: function () {
          org = name;
          syncOrg();
          orgApi.close(true);
          document.dispatchEvent(new CustomEvent("ti:org", { detail: { name: name } }));
        }
      }, h("span", { class: "menu__indicator" }), name));
    });

    var menuBtn = h("button", { type: "button", class: "btn btn--ghost btn--sm", "aria-haspopup": "menu", "aria-expanded": "false", "aria-label": "選單" }, icon("menu"), h("span", { class: "not-sm", text: "選單" }));
    var menuPanel = h("div", { class: "menu menu--right", role: "menu", "aria-label": "選單", hidden: true });
    MENU_GROUPS.forEach(function (group) {
      var items = group.filter(function (it) { return !it.platformOnly || platform; });
      if (!items.length) return;
      if (menuPanel.childNodes.length) menuPanel.appendChild(h("div", { class: "menu__separator", role: "separator" }));
      items.forEach(function (it) { menuPanel.appendChild(menuItem(it, current)); });
    });
    // On a phone the account has no button of its own: it moves into the menu.
    menuPanel.appendChild(h("div", { class: "menu__separator only-sm", role: "separator" }));
    menuPanel.appendChild(h("button", { type: "button", class: "menu__item menu-item only-sm", role: "menuitem", onclick: function () { closeAllMenus(); } }, icon("account"), "帳號"));
    var menuWrap = h("div", { class: "menu-wrap menu-wrap--end" }, menuBtn, menuPanel);
    var menuApi = dropdown(menuBtn, menuPanel);
    menuApi.wrap = menuWrap;

    var right = h("div", { class: "site-header__right" },
      secondary ? h("a", { class: "btn btn--outline btn--sm not-sm", href: "conversation.html" }, icon("arrow-left"), "回到對話") : null,
      h("a", { class: "btn btn--ghost btn--icon only-sm", href: "home.html", "aria-label": "新對話" }, icon("plus")),
      menuWrap,
      h("button", { type: "button", class: "btn btn--ghost btn--icon not-sm", "aria-label": "帳號" }, icon("account"))
    );

    root.appendChild(h("div", { class: "site-header__in" }, brand, orgWrap, right));
    syncOrg();
  }

  // ---- design bar: theme switch and per-page switches for reviewing the design ----
  var barItems = [];
  function addBarItem(item) { barItems.push(item); }
  function buildBar() {
    if (params.get("bar") === "0") return;
    var bar = h("div", { class: "design-bar", role: "group", "aria-label": "設計稿檢視選項" }, h("span", { class: "design-bar__label", text: "設計稿" }));
    barItems.forEach(function (it) { bar.appendChild(it); });
    var themeBtn = h("button", { type: "button", class: "btn btn--outline btn--sm" });
    function label() { themeBtn.textContent = document.documentElement.getAttribute("data-theme") === "dark" ? "切到淺色" : "切到深色"; }
    themeBtn.addEventListener("click", function () {
      var next = document.documentElement.getAttribute("data-theme") === "dark" ? "light" : "dark";
      document.documentElement.setAttribute("data-theme", next);
      var q = new URLSearchParams(location.search);
      q.set("theme", next);
      try { history.replaceState(null, "", "?" + q.toString() + location.hash); } catch (err) { /* file: URLs may refuse */ }
      label();
    });
    label();
    bar.appendChild(themeBtn);
    document.body.appendChild(bar);
  }

  document.addEventListener("DOMContentLoaded", function () {
    var slot = document.getElementById("site-header");
    if (slot) buildHeader(slot);
    hydrateLogos();
    hydrateIcons();
    // Pages register extra bar items with TI.addBarItem before DOMContentLoaded finishes (scripts at the end of body).
    setTimeout(function () { buildBar(); }, 0);
  });

  // ---- copy to clipboard with the wording of the source panel ----
  function copyText(text, done) {
    var ok = function () { done("已複製 SQL。"); };
    var fail = function () { done("無法複製，請手動選取。"); };
    try { navigator.clipboard.writeText(text).then(ok, fail); } catch (err) { fail(); }
  }

  // ---- source panel: where a number comes from (dataset, query time, range, rows, SQL) ----
  // src = {title, desc, span, rows, sql, queried}; opts.onClose, opts.aiHref
  function sourcePanel(src, opts) {
    var status = h("p", { class: "popover__status", role: "status" });
    var titleId = "src-title-" + Math.random().toString(36).slice(2, 8);
    var closeBtn = h("button", { type: "button", class: "btn btn--ghost btn--xs", "aria-label": "關閉資料來源", "data-close": "", onclick: opts.onClose }, icon("x"));
    var el = h("section", {
      class: "popover" + (opts.cls ? " " + opts.cls : ""), role: "dialog", "aria-labelledby": titleId,
      onkeydown: function (e) { if (e.key === "Escape") { e.stopPropagation(); opts.onClose(); } }
    },
      h("div", { class: "popover__head" }, h("span", { class: "popover__eyebrow" }, icon("database"), "資料來源"), closeBtn),
      h("h4", { class: "popover__title", id: titleId, text: src.title }),
      h("p", { class: "popover__desc", text: src.desc }),
      h("dl", { class: "facts" },
        h("dt", { text: "查詢時間" }), h("dd", { text: src.queried }),
        h("dt", { text: "資料範圍" }), h("dd", { text: src.span }),
        h("dt", { text: "筆數" }), h("dd", { text: src.rows })),
      h("div", { class: "sql" },
        h("pre", { tabindex: "0", "aria-label": "查詢的 SQL", text: src.sql }),
        h("button", { type: "button", class: "btn btn--ghost btn--xs", "aria-label": "複製 SQL", "data-copy": "", onclick: function () { copyText(src.sql, function (m) { status.textContent = m; }); } }, icon("copy"))),
      opts.aiHref
        ? h("div", { class: "popover__foot" }, status, h("a", { class: "btn btn--outline btn--sm", href: opts.aiHref }, icon("sparkles"), "請 AI 解釋"))
        : status
    );
    return el;
  }

  window.TI = {
    h: h, replace: replace, icon: icon, logo: logo, hydrateIcons: hydrateIcons, params: params,
    sourcePanel: sourcePanel, addBarItem: addBarItem, closeAllMenus: closeAllMenus, copyText: copyText
  };
})();
