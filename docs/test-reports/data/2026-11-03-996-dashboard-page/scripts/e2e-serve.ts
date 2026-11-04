// Serves .preview-996/dist with the product's headers and sends /api/assistant/ to the real API
// of the Rust harness with the viewer's internal identity token (what the ingress would add).
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

const root = join(import.meta.dir, "dist");
const dir = process.env.E2E_DIR ?? "";
const port = Number(process.env.PORT ?? 28902);
const CSP =
  "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'";
const REPORT_ONLY = "require-trusted-types-for 'script'; trusted-types 'none'";
const read = (name: string) => readFileSync(join(dir, name), "utf8").trim();
Bun.serve({
  port,
  hostname: "127.0.0.1", // the container reaches it as host.docker.internal; it adds an identity token, so it must not listen on other interfaces
  async fetch(request) {
    const url = new URL(request.url);
    if (url.pathname.startsWith("/api/assistant/")) {
      // Only the headers the API needs: the browser's own (cookies, origin) are not the ingress's.
      const headers = new Headers();
      const type = request.headers.get("content-type");
      if (type) headers.set("content-type", type);
      headers.set("x-internal-identity", read("token"));
      const upstream = await fetch(`${read("api")}${url.pathname}${url.search}`, {
        method: request.method,
        headers,
        body: request.method === "GET" || request.method === "HEAD" ? undefined : await request.arrayBuffer(),
      });
      return new Response(upstream.body, { status: upstream.status, headers: upstream.headers });
    }
    const file = join(root, url.pathname);
    const target = url.pathname !== "/" && existsSync(file) && !file.endsWith("/") ? file : join(root, "index.html");
    return new Response(Bun.file(target), {
      headers: { "content-security-policy": CSP, "content-security-policy-report-only": REPORT_ONLY },
    });
  },
});
console.log(`listening on ${port}`);
