// Stand-in for the ingress: ext_authz for /api/identity, /auth/login straight to identity, rest to web.
const IDENTITY = "http://127.0.0.1:28180";
const WEB = "http://127.0.0.1:28280";
const hop = new Set(["connection", "keep-alive", "transfer-encoding", "content-length", "host", "x-internal-identity"]);

Bun.serve({
  port: 28300,
  hostname: "127.0.0.1",
  async fetch(req) {
    const url = new URL(req.url);
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
      return new Response("not found", { status: 404 });
    } else {
      target = WEB + url.pathname + url.search;
    }
    const body = req.method === "GET" || req.method === "HEAD" ? undefined : await req.arrayBuffer();
    const res = await fetch(target, { method: req.method, headers, body, redirect: "manual" });
    const out = new Headers(res.headers);
    out.delete("content-length");
    out.delete("content-encoding");
    return new Response(await res.arrayBuffer(), { status: res.status, headers: out });
  },
});
console.log("proxy up");
