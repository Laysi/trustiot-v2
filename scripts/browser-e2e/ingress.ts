// The stand-in for the cluster's ingress in the browser end-to-end test, following the routes in
// services/identity/README.md (ext_authz) and deploy/chart: the browser talks to one origin, and
// - `/auth/login` goes to the identity service as it is (a public path, no cookie check);
// - `/api/identity` is checked first: the identity service's `/internal/ext-authz<path>` gets the
//   request's method, Host, Cookie, Origin and Sec-Fetch-Site, anything but 200 goes back to the
//   browser as it is, and a 200 carries `x-internal-identity`, which is passed on to the service
//   (whatever the client sent under that name is dropped);
// - any other `/api`, `/auth` or `/internal` path is a 404;
// - everything else goes to `services/web`, whose headers (the CSP among them) are kept.
// Every `/api` and `/auth` request is logged with its status, so a run shows what the browser got.

export interface IngressOptions {
  /** The identity service's API origin, such as `http://127.0.0.1:28180`. */
  identity: string;
  /** The `services/web` origin. */
  web: string;
  /** The listening port; 0 picks a free one. */
  port?: number;
  /** The Host the services see: the route table's host, which carries no port. */
  host?: string;
}

export interface Ingress {
  port: number;
  /** `METHOD path status`, in the order the responses were made. */
  log(): readonly string[];
  close(): Promise<void>;
}

/** Headers the proxy sets itself or must not copy from one hop to the next. */
const DROPPED = new Set(["connection", "keep-alive", "transfer-encoding", "content-length", "host", "x-internal-identity"]);

const COOKIE_PATHS = ["/api/identity"];

/** A response may have no body at all (204, 205 and 304 must not carry one, and a `Response` refuses to). */
async function bodyOf(response: Response): Promise<ArrayBuffer | null> {
  return [204, 205, 304].includes(response.status) ? null : await response.arrayBuffer();
}

export function startIngress(options: IngressOptions): Ingress {
  const host = options.host ?? "localhost";
  const lines: string[] = [];
  const server = Bun.serve({
    port: options.port ?? 0,
    hostname: "127.0.0.1",
    async fetch(request) {
      const url = new URL(request.url);
      const forwarded = new Headers();
      for (const [name, value] of request.headers) {
        if (!DROPPED.has(name.toLowerCase())) forwarded.set(name, value);
      }
      forwarded.set("host", host);

      const respond = (response: Response): Response => {
        if (url.pathname.startsWith("/api") || url.pathname.startsWith("/auth")) {
          lines.push(`${request.method} ${url.pathname} ${response.status}`);
        }
        return response;
      };
      const pass = async (target: string): Promise<Response> => {
        const body = request.method === "GET" || request.method === "HEAD" ? undefined : await request.arrayBuffer();
        const upstream = await fetch(target, { method: request.method, headers: forwarded, body, redirect: "manual" });
        const headers = new Headers(upstream.headers);
        // fetch has already decoded the body, so these no longer describe it.
        headers.delete("content-length");
        headers.delete("content-encoding");
        return new Response(await bodyOf(upstream), { status: upstream.status, headers });
      };

      try {
        if (url.pathname === "/auth/login") {
          return respond(await pass(options.identity + url.pathname + url.search));
        }
        if (COOKIE_PATHS.some((prefix) => url.pathname === prefix || url.pathname.startsWith(`${prefix}/`))) {
          const check = await fetch(`${options.identity}/internal/ext-authz${url.pathname}${url.search}`, {
            method: request.method,
            headers: forwarded,
          });
          if (check.status !== 200) {
            const headers = new Headers(check.headers);
            headers.delete("content-length");
            headers.delete("content-encoding");
            return respond(new Response(await bodyOf(check), { status: check.status, headers }));
          }
          forwarded.set("x-internal-identity", check.headers.get("x-internal-identity") ?? "");
          return respond(await pass(options.identity + url.pathname + url.search));
        }
        if (["/api", "/auth", "/internal"].some((prefix) => url.pathname === prefix || url.pathname.startsWith(`${prefix}/`))) {
          return respond(new Response("not found", { status: 404 }));
        }
        return respond(await pass(options.web + url.pathname + url.search));
      } catch (error) {
        return respond(new Response(`the ingress stand-in could not reach a service: ${String(error)}`, { status: 502 }));
      }
    },
  });
  return {
    port: server.port ?? 0,
    log: () => lines,
    close: async () => {
      await server.stop(true);
    },
  };
}
