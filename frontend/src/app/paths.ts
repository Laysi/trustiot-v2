/**
 * Paths on the app host that are not the SPA's. The ingress sends /api and /auth to the services
 * (the cookie routes, decision 27) and never forwards /internal; /static holds the build's assets
 * and the root files come from public/ or the web service (services/web). A client route there
 * would never load from a deep link, so routes.test.ts keeps every route out of them.
 */
export const RESERVED_PREFIXES: readonly string[] = ["/api", "/auth", "/internal", "/static"];
export const RESERVED_PATHS: readonly string[] = ["/healthz", "/readyz", "/index.html", "/robots.txt", "/favicon.svg"];

export function isReservedPath(path: string): boolean {
  return (
    RESERVED_PATHS.includes(path) ||
    RESERVED_PREFIXES.some((prefix) => path === prefix || path.startsWith(`${prefix}/`))
  );
}

/** The sign-in page's path. */
export const LOGIN_PATH = "/login";

/** Placeholder origin to resolve a relative path against; never contacted. */
const BASE = "https://spa.invalid";

function hasControlCharacter(text: string): boolean {
  for (let index = 0; index < text.length; index += 1) {
    const code = text.charCodeAt(index);
    if (code < 0x20 || code === 0x7f) return true;
  }
  return false;
}

/**
 * Where to go after signing in: `raw` when it is a path of this SPA (same origin, not reserved),
 * otherwise the home page. `next` comes from the address bar, so anything else, such as
 * `//other.example`, `/\other.example` or `https://...`, would be an open redirect.
 */
export function safeNextPath(raw: string | null | undefined): string {
  if (!raw?.startsWith("/") || raw.startsWith("//") || raw.startsWith("/\\") || hasControlCharacter(raw)) {
    return "/";
  }
  let url: URL;
  try {
    url = new URL(raw, BASE);
  } catch {
    return "/";
  }
  if (url.origin !== BASE || isReservedPath(url.pathname) || url.pathname === LOGIN_PATH) {
    return "/";
  }
  return `${url.pathname}${url.search}${url.hash}`;
}

/** The sign-in page, returning to `current` afterwards. */
export function loginPath(current: string): string {
  const next = safeNextPath(current);
  return next === "/" ? LOGIN_PATH : `${LOGIN_PATH}?next=${encodeURIComponent(next)}`;
}
