/**
 * Paths on the app host that are not the SPA's. The ingress sends /api and /auth to the services
 * (the cookie routes, decision 27) and never forwards /internal; /static holds the build's assets,
 * /sandbox the dashboard sandbox's files (frontend/sandbox/README.md), and the root files come
 * from public/ or the web service (services/web). A client route there would never load from a
 * deep link, so routes.test.ts keeps every route out of them.
 */
export const RESERVED_PREFIXES: readonly string[] = ["/api", "/auth", "/internal", "/static", "/sandbox"];
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

/** Starts with one slash: not `//host` or `/\\host`, which a browser reads as another host. */
function isRootRelative(path: string): boolean {
  return path.startsWith("/") && !path.startsWith("//") && !path.startsWith("/\\") && !hasControlCharacter(path);
}

/** The sign-in page, as the router matches it: in any case, with or without a trailing slash. */
function isLoginPath(pathname: string): boolean {
  return pathname.replace(/\/+$/, "").toLowerCase() === LOGIN_PATH;
}

/**
 * Where to go after signing in: `raw` when it is a path of this SPA (same origin, not reserved),
 * otherwise the home page. `next` comes from the address bar, so anything else, such as
 * `//other.example`, `/\\other.example` or `https://...`, would be an open redirect. The answer is
 * the parsed path, which is checked again: parsing resolves `.`, `..` and `%2e` segments and turns
 * `\\` into `/`, so `/.//other.example` or `/x/../\\other.example` passes the first check but
 * comes out as `//other.example`.
 */
export function safeNextPath(raw: string | null | undefined): string {
  if (!raw || !isRootRelative(raw)) {
    return "/";
  }
  let url: URL;
  try {
    url = new URL(raw, BASE);
  } catch {
    return "/";
  }
  const path = `${url.pathname}${url.search}${url.hash}`;
  if (url.origin !== BASE || !isRootRelative(path) || isReservedPath(url.pathname) || isLoginPath(url.pathname)) {
    return "/";
  }
  return path;
}

/** The sign-in page, returning to `current` afterwards. */
export function loginPath(current: string): string {
  const next = safeNextPath(current);
  return next === "/" ? LOGIN_PATH : `${LOGIN_PATH}?next=${encodeURIComponent(next)}`;
}

/** The personal settings page (preferences, signing out of every device). */
export const SETTINGS_PATH = "/settings";

/** The devices; one device's page is `/devices/<device ID>` (pages/device/DevicePage.tsx). */
export const DEVICES_PATH = "/devices";

/** One device's page. */
export function devicePath(deviceId: string): string {
  return `${DEVICES_PATH}/${encodeURIComponent(deviceId)}`;
}
