// Serves .preview-996/dist with the product's headers (services/web/src/headers.rs): the enforcing
// CSP and the Trusted Types report-only policy; any path that is not a file gets index.html.
import { existsSync } from "node:fs";
import { join } from "node:path";

const root = join(import.meta.dir, "dist");
const port = Number(process.env.PORT ?? 28901);
const CSP =
  "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'";
const REPORT_ONLY = "require-trusted-types-for 'script'; trusted-types 'none'";
Bun.serve({
  port,
  hostname: "127.0.0.1", // the container reaches it as host.docker.internal; it adds an identity token, so it must not listen on other interfaces
  fetch(request) {
    const path = new URL(request.url).pathname;
    const file = join(root, path);
    const target = path !== "/" && existsSync(file) && !file.endsWith("/") ? file : join(root, "index.html");
    return new Response(Bun.file(target), {
      headers: { "content-security-policy": CSP, "content-security-policy-report-only": REPORT_ONLY },
    });
  },
});
console.log(`listening on ${port}`);
