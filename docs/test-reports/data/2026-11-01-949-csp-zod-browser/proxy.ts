// Stand-in for the ingress, for a one-off browser run (not part of the product): ext_authz for
// /api/identity, /auth/login straight to identity, the rest to web. A control port lets the
// driver rewrite chosen responses, to reach the paths a healthy backend never takes:
//   PUT    /rules  a JSON array of rules (replaces the old ones)
//   GET    /hits   what each served rule answered
//   DELETE /hits   clears that log
//   PUT    /web    the base URL of the web service the page comes from (empty = the usual one)
// A rule is { name, method, path ("*" at the end = prefix), bodyIncludes?, times?, and either
//   respond: { status, type?, body }  answered by the stand-in alone, or
//   patch: { set?, drop? }            the real 2xx JSON answer with top-level members changed }.
const IDENTITY = "http://127.0.0.1:28180";
const WEB = "http://127.0.0.1:28280";
let web = WEB;
const hop = new Set(["connection", "keep-alive", "transfer-encoding", "content-length", "host", "x-internal-identity"]);

interface Rule {
  name: string;
  method: string;
  path: string;
  bodyIncludes?: string;
  times?: number;
  respond?: { status: number; type?: string; body: string };
  patch?: { set?: Record<string, unknown>; drop?: string[] };
}
let rules: Rule[] = [];
let hits: { rule: string; method: string; path: string; status: number }[] = [];

function find(method: string, path: string, body: string): Rule | undefined {
  return rules.find(
    (rule) =>
      rule.method === method &&
      (rule.path.endsWith("*") ? path.startsWith(rule.path.slice(0, -1)) : rule.path === path) &&
      (rule.bodyIncludes === undefined || body.includes(rule.bodyIncludes)) &&
      (rule.times === undefined || rule.times > 0),
  );
}

function used(rule: Rule, method: string, path: string, status: number) {
  if (rule.times !== undefined) rule.times -= 1;
  hits.push({ rule: rule.name, method, path, status });
}

Bun.serve({
  port: 28301,
  hostname: "127.0.0.1",
  async fetch(req) {
    const url = new URL(req.url);
    if (req.method === "PUT" && url.pathname === "/rules") {
      rules = (await req.json()) as Rule[];
      return new Response(String(rules.length));
    }
    if (req.method === "GET" && url.pathname === "/hits") return Response.json(hits);
    if (req.method === "DELETE" && url.pathname === "/hits") {
      hits = [];
      return new Response(null, { status: 204 });
    }
    if (req.method === "PUT" && url.pathname === "/web") {
      web = (await req.text()) || WEB;
      return new Response(web);
    }
    return new Response("not found", { status: 404 });
  },
});

Bun.serve({
  port: 28300,
  hostname: "127.0.0.1",
  async fetch(req) {
    const url = new URL(req.url);
    const body = req.method === "GET" || req.method === "HEAD" ? undefined : await req.arrayBuffer();
    const rule = find(req.method, url.pathname, body ? new TextDecoder().decode(body) : "");
    if (rule?.respond) {
      used(rule, req.method, url.pathname, rule.respond.status);
      return new Response(rule.respond.body, {
        status: rule.respond.status,
        headers: { "content-type": rule.respond.type ?? "application/json" },
      });
    }
    const headers = new Headers();
    for (const [k, v] of req.headers) if (!hop.has(k.toLowerCase())) headers.set(k, v);
    headers.set("host", "localhost");
    let target: string;
    if (url.pathname === "/auth/login") {
      target = IDENTITY + url.pathname + url.search;
    } else if (url.pathname.startsWith("/api/identity")) {
      const check = await fetch(IDENTITY + "/internal/ext-authz" + url.pathname + url.search, {
        method: req.method,
        headers,
      });
      if (check.status !== 200) {
        const out = new Headers(check.headers);
        out.delete("content-length");
        return new Response(await check.arrayBuffer(), { status: check.status, headers: out });
      }
      headers.set("x-internal-identity", check.headers.get("x-internal-identity") ?? "");
      target = IDENTITY + url.pathname + url.search;
    } else if (
      url.pathname.startsWith("/api") ||
      url.pathname.startsWith("/auth") ||
      url.pathname.startsWith("/internal")
    ) {
      return new Response("not found", { status: 404, headers: { "content-type": "text/plain" } });
    } else {
      target = web + url.pathname + url.search;
    }
    const res = await fetch(target, { method: req.method, headers, body, redirect: "manual" });
    const out = new Headers(res.headers);
    out.delete("content-length");
    out.delete("content-encoding");
    let payload: BodyInit = await res.arrayBuffer();
    if (rule?.patch && res.status >= 200 && res.status < 300) {
      const json = JSON.parse(new TextDecoder().decode(payload as ArrayBuffer)) as Record<string, unknown>;
      for (const key of rule.patch.drop ?? []) delete json[key];
      Object.assign(json, rule.patch.set ?? {});
      payload = JSON.stringify(json);
      used(rule, req.method, url.pathname, res.status);
    }
    return new Response(payload, { status: res.status, headers: out });
  },
});
console.log("proxy up");
