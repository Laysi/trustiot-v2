/**
 * The image test's check requests: one valid dataset and constant, and pages that each break one
 * rule (frontend/sandbox/README.md, 「檢查規則」), with the problem kind the checker must answer.
 * The pages use only the part of `window.tq` that every runtime version has: `tq.on`,
 * `tq.dataset`, `tq.text` without a format, `tq.mark` and `tq.decor`.
 */

/** A check request the contract accepts (`contract::api::dashboard::check::CheckRequest`). */
export function checkRequest(page: string): Record<string, unknown> {
  return {
    runtime: 1,
    page,
    spec: {
      datasets: [
        {
          name: "sales",
          title: "Sales per month",
          description: "Sales of the three months, for the image test.",
          source: "sql",
          max_rows: 3,
        },
      ],
      computations: [],
      constants: [{ name: "title", title: "Title", value: "Monthly sales" }],
    },
    datasets: [
      {
        dataset: "sales",
        columns: [
          { name: "month", type: "text" },
          { name: "amount", type: "number" },
        ],
        rows: [
          ["Jan", 120],
          ["Feb", 80],
          ["Mar", 150],
        ],
        row_count: 3,
        meta: {
          duration_ms: 5,
          queried_at: "2026-10-26T01:02:03.456Z",
          source: {
            kind: "sql",
            queryable_from: {
              tenant_telemetry: "2026-07-28T00:00:00Z",
              tenant_telemetry_1h: "2025-10-26T00:00:00Z",
              tenant_telemetry_1m: "2026-07-28T00:00:00Z",
            },
          },
        },
      },
    ],
    locale: "en",
    tz: "Asia/Taipei",
  };
}

/** The body of a page that draws the title, a list of the rows and a bar per row. */
function drawing(extra = ""): string {
  return `<h1 id="title"></h1>
<ul id="list"></ul>
<svg id="chart" width="300" height="200"></svg>
<script>
  tq.on("data", () => {
    tq.text(document.getElementById("title"), "k|title");
    const sales = tq.dataset("sales");
    const list = document.getElementById("list");
    list.replaceChildren();
    for (const row of sales.rows) {
      const item = document.createElement("li");
      const month = document.createElement("span");
      const amount = document.createElement("span");
      tq.text(month, "c|sales|month|" + row.$row);
      tq.text(amount, "c|sales|amount|" + row.$row);
      item.append(month, " ", amount);
      list.append(item);
    }
    const svg = document.getElementById("chart");
    svg.replaceChildren();
    for (const row of sales.rows) {
      const bar = document.createElementNS("http://www.w3.org/2000/svg", "rect");
      bar.setAttribute("x", String(10 + row.$row * 40));
      bar.setAttribute("width", "30");
      bar.setAttribute("y", String(190 - row.amount));
      bar.setAttribute("height", String(row.amount));
      tq.mark(bar, "c|sales|amount|" + row.$row, { chart: "sales", channel: "height" });
      svg.append(bar);
    }
    ${extra}
  });
</script>`;
}

export interface Case {
  name: string;
  page: string;
  /** The verdict's problem kinds, in any order; empty for a page that passes. */
  expect: string[];
}

export const CASES: Case[] = [
  { name: "correct page", page: drawing(), expect: [] },
  {
    name: "literal number",
    page: drawing(`document.getElementById("list").append(Object.assign(document.createElement("p"), { textContent: "Total 350" }));`),
    expect: ["literal_number"],
  },
  {
    name: "wrong row",
    // The second amount names the first row's reference but shows its own value.
    page: drawing(`const shown = document.querySelectorAll("#list li span")[3]; shown.setAttribute("data-tq", "c|sales|amount|0");`),
    expect: ["mismatch"],
  },
  {
    name: "wrong bar height",
    page: drawing(`document.querySelectorAll("#chart rect")[1].setAttribute("height", "120");`),
    expect: ["mark_geometry"],
  },
  {
    name: "unmapped mark",
    page: drawing(`tq.mark(document.querySelectorAll("#chart rect")[2], "c|sales|amount|7", { chart: "sales", channel: "height" });`),
    expect: ["unmapped"],
  },
  {
    name: "unmarked shape",
    page: drawing(`document.getElementById("chart").append(document.createElementNS("http://www.w3.org/2000/svg", "circle"));`),
    expect: ["unmapped"],
  },
  {
    name: "synchronous script error",
    page: `${drawing()}<script>throw new Error("broken page");</script>`,
    expect: ["script_error"],
  },
  {
    name: "asynchronous script error",
    page: drawing(`setTimeout(() => { throw new Error("late failure"); }, 0);`),
    expect: ["script_error"],
  },
  {
    name: "navigation",
    page: drawing(`location.href = "/sandbox/v1/sandbox.html?again";`),
    expect: ["navigated"],
  },
  {
    name: "request for another file",
    page: drawing(`const extra = document.createElement("script"); extra.src = "/sandbox/check.js"; document.head.append(extra);`),
    expect: ["requested"],
  },
  {
    name: "prototype tampering",
    // The page shows a wrong amount and makes every text read in its own world look right; the
    // checker's isolated world reads the real text.
    page: drawing(`
      const amount = document.querySelectorAll("#list li span")[1];
      amount.firstChild.data = "999";
      const real = Object.getOwnPropertyDescriptor(Node.prototype, "textContent");
      Object.defineProperty(Node.prototype, "textContent", {
        configurable: true,
        get() { const text = real.get.call(this); return text === "999" ? "120" : text; },
        set(value) { real.set.call(this, value); },
      });
      Element.prototype.getAttribute = () => null;
      Document.prototype.querySelectorAll = () => [];`),
    expect: ["mismatch"],
  },
  {
    name: "shadow root",
    page: drawing(`const host = document.createElement("div"); document.getElementById("list").after(host); host.attachShadow({ mode: "closed" }).innerHTML = "<b>hidden</b>";`),
    expect: ["unsupported_content"],
  },
  {
    name: "page that keeps changing",
    page: drawing(`setInterval(() => { document.getElementById("title").dataset.tick = String(performance.now()); }, 20);`),
    expect: ["timeout"],
  },
  {
    name: "page without a render function",
    page: `<p id="empty"></p>`,
    expect: ["not_rendered"],
  },
];
