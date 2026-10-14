// A stand-in for the Kubernetes API that the migration Job fills passwords through
// (tools/migrate/README.md, `TRUSTIOT__PASSWORDS__*`). In a cluster the Job generates every
// service login role's password into a Secret declared empty by the chart; a login role without a
// bound Secret is NOLOGIN. The local environment has no Kubernetes, so `up` runs the same Job
// binary, unchanged, against this server: the Job's code paths (generate, patch, set the role's
// password, LOGIN) are the ones that run in a cluster, and tools/migrate learned nothing about
// "local".
//
// It answers exactly what the Job asks, as the API server does: `GET` of a Secret and JSON merge
// `PATCH` of `metadata` (resourceVersion precondition, annotations, labels, ownerReferences and
// finalizers) and `data`. Plain HTTP on 127.0.0.1, which the Job accepts for tests and this use
// only; every request needs the bearer token the caller was given. The Secrets live in memory for
// one run: nothing is written to disk here, and the caller keeps the passwords it reads back in
// the instance's state file (mode 0600), like every other credential of the environment.
import { randomBytes, randomUUID, timingSafeEqual } from "node:crypto";

export interface StoredSecret {
  data: Map<string, Buffer>;
  annotations: Map<string, string>;
  labels: Map<string, string>;
  ownerReferences: unknown[];
  finalizers: string[];
  resourceVersion: number;
  uid: string;
}

export interface SecretApi {
  /** `http://127.0.0.1:<port>`, for `TRUSTIOT__PASSWORDS__API_URL`. */
  url: string;
  /** The bearer token the Job presents, for the file `TRUSTIOT__PASSWORDS__TOKEN_FILE` names. */
  token: string;
  /** The value of a key, as text; `undefined` when the Secret or the key does not exist. */
  value(secret: string, key: string): string | undefined;
  stop(): void;
}

/** Fields of a merge patch the Job sends; anything else is refused, as a strict API would. */
const METADATA_FIELDS = ["resourceVersion", "annotations", "labels", "ownerReferences", "finalizers"];

function status(code: number, reason: string): Response {
  return Response.json({ kind: "Status", apiVersion: "v1", status: "Failure", reason, code }, { status: code });
}

function render(namespace: string, name: string, secret: StoredSecret): Response {
  return Response.json({
    kind: "Secret",
    apiVersion: "v1",
    metadata: {
      name,
      namespace,
      resourceVersion: String(secret.resourceVersion),
      uid: secret.uid,
      annotations: Object.fromEntries(secret.annotations),
      labels: Object.fromEntries(secret.labels),
      ownerReferences: secret.ownerReferences,
      finalizers: secret.finalizers,
    },
    type: "Opaque",
    data: Object.fromEntries([...secret.data].map(([key, value]) => [key, value.toString("base64")])),
  });
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Applies a patch to `secret`; returns the HTTP status that refuses it, or undefined. */
function apply(secret: StoredSecret, patch: unknown): number | undefined {
  if (!isObject(patch) || Object.keys(patch).some((key) => key !== "metadata" && key !== "data")) return 400;
  const metadata = patch.metadata ?? {};
  if (!isObject(metadata) || Object.keys(metadata).some((key) => !METADATA_FIELDS.includes(key))) return 400;
  if ("resourceVersion" in metadata && metadata.resourceVersion !== String(secret.resourceVersion)) return 409;
  for (const [field, map] of [
    ["annotations", secret.annotations],
    ["labels", secret.labels],
  ] as const) {
    const changes = metadata[field];
    if (changes === undefined) continue;
    if (!isObject(changes)) return 400;
    for (const [key, value] of Object.entries(changes)) {
      if (value === null) map.delete(key);
      else if (typeof value === "string") map.set(key, value);
      else return 400;
    }
  }
  // The Job only ever clears these two.
  if (metadata.ownerReferences === null) secret.ownerReferences = [];
  else if (metadata.ownerReferences !== undefined) return 400;
  if (metadata.finalizers === null) secret.finalizers = [];
  else if (metadata.finalizers !== undefined) return 400;
  const data = patch.data;
  if (data !== undefined) {
    if (!isObject(data)) return 400;
    for (const [key, value] of Object.entries(data)) {
      if (value === null) secret.data.delete(key);
      else if (typeof value === "string" && /^[A-Za-z0-9+/]*={0,2}$/.test(value)) {
        secret.data.set(key, Buffer.from(value, "base64"));
      } else return 400;
    }
  }
  return undefined;
}

/**
 * Starts the API on a free loopback port with `names` declared as empty Secrets in `namespace`,
 * the way the chart declares them.
 */
export function startSecretApi(namespace: string, names: readonly string[]): SecretApi {
  const token = randomBytes(32).toString("hex");
  const expected = Buffer.from(`Bearer ${token}`);
  const secrets = new Map<string, StoredSecret>();
  let version = 0;
  for (const name of names) {
    secrets.set(name, {
      data: new Map(),
      annotations: new Map(),
      labels: new Map(),
      ownerReferences: [],
      finalizers: [],
      resourceVersion: ++version,
      uid: randomUUID(),
    });
  }
  const prefix = `/api/v1/namespaces/${namespace}/secrets/`;
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    async fetch(request) {
      const presented = Buffer.from(request.headers.get("authorization") ?? "");
      if (presented.length !== expected.length || !timingSafeEqual(presented, expected)) {
        return status(401, "Unauthorized");
      }
      const url = new URL(request.url);
      const name = url.pathname.startsWith(prefix) ? url.pathname.slice(prefix.length) : "";
      const secret = secrets.get(name);
      if (name === "" || name.includes("/") || !secret) return status(404, "NotFound");
      if (request.method === "GET" && url.search === "") return render(namespace, name, secret);
      if (request.method !== "PATCH") return status(405, "MethodNotAllowed");
      if (request.headers.get("content-type") !== "application/merge-patch+json") {
        return status(415, "UnsupportedMediaType");
      }
      let patch: unknown;
      try {
        patch = JSON.parse(await request.text());
      } catch {
        return status(400, "BadRequest");
      }
      const refused = apply(secret, patch);
      if (refused !== undefined) return status(refused, refused === 409 ? "Conflict" : "BadRequest");
      secret.resourceVersion = ++version;
      return render(namespace, name, secret);
    },
  });
  return {
    url: `http://127.0.0.1:${server.port}`,
    token,
    value: (secret, key) => secrets.get(secret)?.data.get(key)?.toString("utf8"),
    stop: () => void server.stop(true),
  };
}
