// The stand-in Kubernetes API (lib/secret-api.ts) answers what tools/migrate asks, and nothing
// else. Each case names the request the Job makes (tools/migrate/src/passwords.rs).
import { afterEach, beforeEach, describe, expect, test } from "bun:test";

import { startSecretApi, type SecretApi } from "../lib/secret-api.ts";

const NAMESPACE = "ns";
const NAME = "store";
let api: SecretApi;

beforeEach(() => {
  api = startSecretApi(NAMESPACE, [NAME]);
});

afterEach(() => {
  api.stop();
});

const path = (name = NAME) => `${api.url}/api/v1/namespaces/${NAMESPACE}/secrets/${name}`;
const auth = (token = api.token) => ({ authorization: `Bearer ${token}` });
const encode = (text: string) => Buffer.from(text).toString("base64");

async function get(name = NAME) {
  const response = await fetch(path(name), { headers: auth() });
  return { status: response.status, body: (await response.json()) as any };
}

async function patch(body: unknown, query = "?fieldManager=trustiot-migrate", type = "application/merge-patch+json") {
  const response = await fetch(`${path()}${query}`, {
    method: "PATCH",
    headers: { ...auth(), "content-type": type },
    body: JSON.stringify(body),
  });
  return { status: response.status, body: (await response.json()) as any };
}

describe("the stand-in Secret API", () => {
  test("listens on loopback only, and the URL is the form the Job accepts for tests", () => {
    expect(api.url).toMatch(/^http:\/\/127\.0\.0\.1:\d+$/);
  });

  test("declares each Secret empty, with a uid and a resourceVersion, as the chart does", async () => {
    const { status, body } = await get();
    expect(status).toBe(200);
    expect(body.kind).toBe("Secret");
    expect(body.metadata.name).toBe(NAME);
    expect(body.metadata.uid).toMatch(/^[0-9a-f-]{36}$/);
    expect(body.metadata.resourceVersion).toMatch(/^\d+$/);
    expect(body.data).toEqual({});
  });

  test("refuses a request without the token, or with another one", async () => {
    expect((await fetch(path())).status).toBe(401);
    expect((await fetch(path(), { headers: auth("not-the-token") })).status).toBe(401);
    expect((await fetch(path(), { headers: { authorization: api.token } })).status).toBe(401);
  });

  test("answers 404 for a Secret that was not declared and for other paths", async () => {
    expect((await get("other")).status).toBe(404);
    const other = await fetch(`${api.url}/api/v1/namespaces/elsewhere/secrets/${NAME}`, { headers: auth() });
    expect(other.status).toBe(404);
    expect((await fetch(`${api.url}/api/v1/secrets`, { headers: auth() })).status).toBe(404);
  });

  test("stores what a merge patch writes and records annotations, and the value reads back", async () => {
    const before = (await get()).body;
    const { status, body } = await patch({
      metadata: {
        resourceVersion: before.metadata.resourceVersion,
        annotations: { "trustiot.io/password-generation": "1", "trustiot.io/password-secret-uid": before.metadata.uid },
      },
      data: { identity_app: encode("generated-value") },
    });
    expect(status).toBe(200);
    expect(body.metadata.annotations["trustiot.io/password-generation"]).toBe("1");
    expect(body.metadata.resourceVersion).not.toBe(before.metadata.resourceVersion);
    expect(Buffer.from(body.data.identity_app, "base64").toString()).toBe("generated-value");
    expect(api.value(NAME, "identity_app")).toBe("generated-value");
    expect(api.value(NAME, "absent")).toBeUndefined();
    expect(api.value("other", "identity_app")).toBeUndefined();
  });

  test("a stale resourceVersion is a conflict and changes nothing", async () => {
    const before = (await get()).body;
    const { status } = await patch({ metadata: { resourceVersion: "0" }, data: { k: encode("v") } });
    expect(status).toBe(409);
    expect((await get()).body.data).toEqual({});
    expect((await get()).body.metadata.resourceVersion).toBe(before.metadata.resourceVersion);
  });

  test("null removes annotations, labels, ownerReferences, finalizers and keys", async () => {
    await patch({ metadata: { annotations: { a: "1" }, labels: { l: "1" } }, data: { k: encode("v") } });
    const { status, body } = await patch({
      metadata: { annotations: { a: null }, labels: { l: null }, ownerReferences: null, finalizers: null },
      data: { k: null },
    });
    expect(status).toBe(200);
    expect(body.metadata.annotations).toEqual({});
    expect(body.metadata.labels).toEqual({});
    expect(body.data).toEqual({});
  });

  test("refuses what the Job never sends: other fields, other content types, odd values", async () => {
    expect((await patch({ spec: {} })).status).toBe(400);
    expect((await patch({ metadata: { name: "x" } })).status).toBe(400);
    expect((await patch({ data: { k: "not base64!" } })).status).toBe(400);
    expect((await patch({ metadata: { annotations: { a: 1 } } })).status).toBe(400);
    expect((await patch({}, "?fieldManager=x", "application/json")).status).toBe(415);
    const post = await fetch(path(), { method: "POST", headers: auth(), body: "{}" });
    expect(post.status).toBe(405);
    const bad = await fetch(path(), { method: "PATCH", headers: { ...auth(), "content-type": "application/merge-patch+json" }, body: "{" });
    expect(bad.status).toBe(400);
  });

  test("stops answering after stop()", async () => {
    const { url } = api;
    api.stop();
    let failed = false;
    try {
      await fetch(`${url}/api/v1/namespaces/${NAMESPACE}/secrets/${NAME}`, { headers: auth() });
    } catch {
      failed = true;
    }
    expect(failed).toBe(true);
  });
});
