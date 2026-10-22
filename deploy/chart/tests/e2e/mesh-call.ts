// The call mesh.e2e.ts makes from inside a pod: a Bun script that `kubectl exec` runs in the
// caller's container, so the connection leaves with the caller's mesh identity. Kept apart from
// the test so mesh-call.test.ts can run the script without a cluster.

/**
 * What the script prints as its last line. `tokenEchoed`, only for a call that carried a token:
 * the callee answered with the request's headers (the identity double's echo) and the token
 * among them.
 */
export type CallResult = {
  outcome: "response" | "error" | "timeout";
  status?: number;
  tokenEchoed?: boolean;
  code?: string;
  message?: string;
  ms: number;
};

export type ScriptOptions = {
  /** A Host header other than the URL's. */
  host?: string;
  /** Whether to read a token from stdin and send it as `x-internal-identity`. */
  withToken: boolean;
  /** A JSON body: the call becomes a POST. */
  body?: unknown;
  /** How long the call may take before it counts as refused. */
  timeoutMs: number;
};

/**
 * The script. It reads the token from stdin, so the token is on no command line, and it prints
 * only whether the echo carried the token, never the token or the response.
 */
export function callScript(url: string, options: ScriptOptions): string {
  const post = options.body !== undefined;
  const headers = { ...(options.host ? { host: options.host } : {}), ...(post ? { "content-type": "application/json" } : {}) };
  return `const started = performance.now();
const ms = () => Math.round(performance.now() - started);
const token = ${options.withToken ? "(await Bun.stdin.text()).trim()" : "undefined"};
try {
  const response = await fetch(${JSON.stringify(url)}, {
    method: ${JSON.stringify(post ? "POST" : "GET")},
    headers: { ...${JSON.stringify(headers)}, ...(token ? { "x-internal-identity": token } : {}) },
    ${post ? `body: ${JSON.stringify(JSON.stringify(options.body))},` : ""}
    signal: AbortSignal.timeout(${Math.trunc(options.timeoutMs)}),
  });
  const text = await response.text();
  let tokenEchoed;
  if (token) {
    try {
      tokenEchoed = JSON.parse(text)?.headers?.["x-internal-identity"] === token;
    } catch {
      tokenEchoed = false;
    }
  }
  console.log(JSON.stringify({ outcome: "response", status: response.status, tokenEchoed, ms: ms() }));
} catch (error) {
  const timeout = error?.name === "TimeoutError" || error?.name === "AbortError";
  console.log(JSON.stringify({ outcome: timeout ? "timeout" : "error", code: error?.code, message: String(error?.message ?? error), ms: ms() }));
}`;
}

/** The script's result from its output, or undefined when it printed none. */
export function parseCallOutput(output: string): CallResult | undefined {
  const line = output
    .trim()
    .split("\n")
    .reverse()
    .find((l) => l.startsWith("{") && l.includes('"outcome"'));
  return line === undefined ? undefined : (JSON.parse(line) as CallResult);
}
