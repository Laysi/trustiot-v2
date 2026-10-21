import { describe, expect, test } from "bun:test";

import { checkDist, type DistFile } from "./check-dist.ts";

const PUBLIC = new Set(["favicon.svg", "robots.txt"]);

const INDEX = `<!doctype html>
<html lang="zh-TW">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <link rel="icon" href="/favicon.svg" type="image/svg+xml" />
    <title>TrustIoT</title>
    <script type="module" crossorigin src="/static/l8_nc_Bx.js"></script>
    <link rel="stylesheet" crossorigin href="/static/BO0X5c1C.css">
  </head>
  <body><noscript>JavaScript</noscript><div id="root"></div></body>
</html>`;

function build(overrides: Record<string, string | null> = {}): DistFile[] {
  const files: Record<string, string | null> = {
    "index.html": INDEX,
    "favicon.svg": '<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1"/></svg>',
    "robots.txt": "User-agent: *\nDisallow: /\n",
    "static/l8_nc_Bx.js": "console.log(1)",
    "static/BO0X5c1C.css": "body{background:url(/static/abc123.png)}",
    "static/abc123.png": "PNG",
    ...overrides,
  };
  return Object.entries(files)
    .filter((entry): entry is [string, string | null] => entry[1] !== undefined)
    .map(([path, content]) => ({ path, content }));
}

function withIndex(html: string): DistFile[] {
  return build({ "index.html": html });
}

describe("checkDist", () => {
  test("a build like Vite's passes", () => {
    expect(checkDist(build(), PUBLIC)).toEqual([]);
  });

  test("index.html is required", () => {
    expect(
      checkDist(
        build().filter((file) => file.path !== "index.html"),
        PUBLIC,
      ),
    ).toContain("index.html is missing");
  });

  test.each([
    ["an inline script", "<script>alert(1)</script>", "inline <script>"],
    ["a style element", "<style>body{}</style>", "<style> element"],
    ["a style attribute", '<div style="color:red"></div>', "style attribute"],
    ["an event handler attribute", '<img src="/static/abc123.png" onerror="x()">', "inline event handler"],
    ["a javascript: URL", '<a href="javascript:void(0)">x</a>', "javascript: URL"],
    ["a data: URI", '<img src="data:image/png;base64,AAAA">', "data: URI"],
  ])("index.html with %s fails", (_name, markup, problem) => {
    const problems = checkDist(withIndex(INDEX.replace('<div id="root"></div>', markup)), PUBLIC);
    expect(problems.some((line) => line.includes(problem))).toBe(true);
  });

  test("index.html may refer only to /static/ and the root files of public/", () => {
    const problems = checkDist(
      withIndex(
        INDEX.replace("/favicon.svg", "https://cdn.example.test/favicon.svg").replace(
          "/static/l8_nc_Bx.js",
          "/assets/main.js",
        ),
      ),
      PUBLIC,
    );
    expect(problems).toEqual([
      "index.html: https://cdn.example.test/favicon.svg is neither under /static/ nor a root file of public/",
      "index.html: /assets/main.js is neither under /static/ nor a root file of public/",
    ]);
  });

  test("CSS without data: URIs, expressions or other origins", () => {
    expect(
      checkDist(build({ "static/BO0X5c1C.css": "a{background:url(data:image/png;base64,AA)}" }), PUBLIC),
    ).toContain("static/BO0X5c1C.css: contains a data: URI (the CSP allows none)");
    expect(
      checkDist(build({ "static/BO0X5c1C.css": "a{background:url('https://x.example.test/a.png')}" }), PUBLIC),
    ).toContain("static/BO0X5c1C.css: url(https://x.example.test/a.png) is not on this origin");
    expect(checkDist(build({ "static/BO0X5c1C.css": "a{width:expression(1)}" }), PUBLIC)).toContain(
      "static/BO0X5c1C.css: contains a CSS expression()",
    );
  });

  test("an SVG with a script fails", () => {
    expect(
      checkDist(build({ "favicon.svg": '<svg xmlns="http://www.w3.org/2000/svg"><script>x()</script></svg>' }), PUBLIC),
    ).toContain("favicon.svg: has an inline <script> (the CSP allows only script files)");
  });

  test("source maps, unknown types and stray files fail", () => {
    const problems = checkDist(
      build({
        "static/l8_nc_Bx.js.map": "{}",
        "static/data.json": "{}",
        "notes.txt": "x",
        "static/nested/abc.js": "x",
        "static/main.app.js": "x",
        "other.html": "<p></p>",
      }),
      PUBLIC,
    );
    expect(problems).toEqual([
      "static/l8_nc_Bx.js.map: source maps are not shipped",
      "static/data.json: .json is not a file type the web service serves",
      "notes.txt: only index.html and the files of public/ belong at the root",
      "static/nested/abc.js: everything else belongs directly under static/ with a hash-only name",
      "static/main.app.js: everything else belongs directly under static/ with a hash-only name",
      "other.html: only index.html and the files of public/ belong at the root",
      "other.html: index.html is the only document",
    ]);
  });

  test("a symbolic link or other special file fails", () => {
    expect(checkDist(build({ "static/link.js": null }), PUBLIC)).toEqual(["static/link.js: not a regular file"]);
  });
});
