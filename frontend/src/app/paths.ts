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
 * `raw` as a path of this SPA, or `null` when it is not one: same origin, not reserved, not the
 * sign-in page. `raw` comes from the address bar or from text no one checked (an answer of the
 * assistant), so anything else, such as `//other.example`, `/\\other.example` or `https://...`,
 * is refused. The answer is the parsed path, which is checked again: parsing resolves `.`, `..`
 * and `%2e` segments and turns `\\` into `/`, so `/.//other.example` or `/x/../\\other.example`
 * passes the first check but comes out as `//other.example`.
 */
export function spaPath(raw: string | null | undefined): string | null {
  if (!raw || !isRootRelative(raw)) {
    return null;
  }
  let url: URL;
  try {
    url = new URL(raw, BASE);
  } catch {
    return null;
  }
  const path = `${url.pathname}${url.search}${url.hash}`;
  const routed = routedPathname(url.pathname);
  if (
    url.origin !== BASE ||
    !isRootRelative(path) ||
    routed === null ||
    isReservedPath(url.pathname) ||
    isLoginPath(url.pathname) ||
    isReservedPath(routed) ||
    isLoginPath(routed)
  ) {
    return null;
  }
  return path;
}

/**
 * `pathname` as the router matches it: each segment percent-decoded, so `/%6Cogin` is the sign-in
 * page and `/%61pi/x` is under /api. `null` when a segment does not decode, or decodes to a slash,
 * a backslash or a control character, which would change what the path names.
 */
function routedPathname(pathname: string): string | null {
  const segments: string[] = [];
  for (const segment of pathname.split("/")) {
    let decoded: string;
    try {
      decoded = decodeURIComponent(segment);
    } catch {
      return null;
    }
    if (decoded.includes("/") || decoded.includes("\\") || hasControlCharacter(decoded)) {
      return null;
    }
    segments.push(decoded);
  }
  return segments.join("/");
}

/**
 * Where to go after signing in: `raw` when it is a path of this SPA (`spaPath`), otherwise the
 * home page. `next` comes from the address bar, so anything else would be an open redirect.
 */
export function safeNextPath(raw: string | null | undefined): string {
  return spaPath(raw) ?? "/";
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

/** The device form for a new device; `?gateway=<hardware ID>` chooses its gateway beforehand. */
export function deviceNewPath(gateway?: string): string {
  return gateway === undefined ? `${DEVICES_PATH}/new` : `${DEVICES_PATH}/new?gateway=${encodeURIComponent(gateway)}`;
}

/** The settings form of one device. */
export function deviceSettingsPath(deviceId: string): string {
  return `${devicePath(deviceId)}/settings`;
}

/** The tenant's device models; one model's page is `/models/<model ID>`, a new one `/models/new` (pages/models/). */
export const MODELS_PATH = "/models";

/** The form for a new device model. */
export function modelsNewPath(): string {
  return `${MODELS_PATH}/new`;
}

/** One device model's page. */
export function modelPath(modelId: string): string {
  return `${MODELS_PATH}/${encodeURIComponent(modelId)}`;
}

/** The models list with the import dialog open. */
export function modelsImportPath(): string {
  return `${MODELS_PATH}?import`;
}

/** The devices that use one model: the device list filtered by it. */
export function devicesOfModelPath(modelId: string): string {
  return `${DEVICES_PATH}?model=${encodeURIComponent(modelId)}`;
}

/** The tenant's dashboards; one dashboard's page is `/dashboards/<dashboard ID>` (pages/dashboards/). */
export const DASHBOARDS_PATH = "/dashboards";

/** One dashboard's page: its latest version, or `version` (`?version=<n>`, a deep link to an older one). */
export function dashboardPath(dashboardId: string, version?: number): string {
  const path = `${DASHBOARDS_PATH}/${encodeURIComponent(dashboardId)}`;
  return version === undefined ? path : `${path}?version=${version}`;
}

/**
 * The user's conversation history; `?tab=archived` and `?tab=deleted` are its other tabs. One
 * conversation's page is `/conversations/<conversation ID>` (pages/conversations/).
 */
export const CONVERSATIONS_PATH = "/conversations";

/** The tabs of the conversation history. */
export type ConversationsTab = "active" | "archived" | "deleted";

/** The conversation history on `tab`; the "In use" tab has no parameter. */
export function conversationsPath(tab: ConversationsTab = "active"): string {
  return tab === "active" ? CONVERSATIONS_PATH : `${CONVERSATIONS_PATH}?tab=${tab}`;
}

/** One conversation's page. */
export function conversationPath(conversationId: string): string {
  return `${CONVERSATIONS_PATH}/${encodeURIComponent(conversationId)}`;
}

/**
 * Whether `pathname` is one conversation's page, not the history. The router matches a path in
 * any case and with a trailing slash, so this does too.
 */
export function isConversationPath(pathname: string): boolean {
  const parts = pathname
    .toLowerCase()
    .split("/")
    .filter((part) => part !== "");
  return parts.length === 2 && parts[0] === CONVERSATIONS_PATH.slice(1);
}

/** The tenant's gateways (`?status=offline` filters, `#<hardware ID>` opens one, `?bind` opens the binding form; pages/gateways/). */
export const GATEWAYS_PATH = "/gateways";

/** The gateway page with the binding form open (administrators; others see the list). */
export function gatewayBindPath(): string {
  return `${GATEWAYS_PATH}?bind`;
}

/**
 * A conversation another member shared, read only: `/shared-conversations/<conversation ID>`
 * (pages/conversations/sharing/). It is the owner's conversation ID, which is not a conversation
 * of the reader's, so it has a path of its own.
 */
export const SHARED_CONVERSATIONS_PATH = "/shared-conversations";

/** One shared conversation's page. */
export function sharedConversationPath(conversationId: string): string {
  return `${SHARED_CONVERSATIONS_PATH}/${encodeURIComponent(conversationId)}`;
}

/** The conversation history's "Shared with me" tab. */
export function sharedConversationsPath(): string {
  return `${CONVERSATIONS_PATH}?tab=shared`;
}

/** Where a question typed in the address bar is put into the composer (`?q=`, `useQuestionFromAddress`). */
export const QUESTION_PARAM = "q";

/**
 * The conversation home with `question` already in its composer, for the entries on other pages
 * that start a conversation (「在對話中編輯」, 「在對話中新增」): the user reads it and sends it.
 */
export function newConversationPath(question?: string): string {
  return question ? `/?${QUESTION_PARAM}=${encodeURIComponent(question)}` : "/";
}
