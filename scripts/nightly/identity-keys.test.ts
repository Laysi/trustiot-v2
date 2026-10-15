import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { COMMAND_TIMEOUT_MS, SLOW_TEST_MS, throwIfTimedOut } from "../../deploy/chart/tests/timing.ts";
import { pinnedTool } from "./exec.ts";
import {
  accessProblems,
  accessQuestions,
  BOUND_UID_ANNOTATION,
  identityKeyObjectsOf,
  PUBLIC_KEYS_KEY,
  publishedAt,
  publishedKeysOf,
  readerPods,
  SECRET_SUMMARY_TEMPLATE,
  secretSummaryProblems,
  SIGNING_KEYS_KEY,
} from "./identity-keys.ts";
import type { AccessAnswer, IdentityKeyObjects } from "./identity-keys.ts";
import { parseManifests } from "./manifests.ts";
import { discoverShapes, valuesArgs } from "./smoke.ts";

const REPO_ROOT = join(import.meta.dir, "..", "..");
const CHART_DIR = join(REPO_ROOT, "deploy", "chart");
const source = (...path: string[]) => readFileSync(join(REPO_ROOT, ...path), "utf8");
// A thumbprint-shaped key id and a public key: not secret, made up for the test.
const KID = "Gk7c0wqR8sJmP2vYxL5nTb3hZ9eWfA1uD4iKoN6yQrE";
const X = "jxSgv4kffdHZ1jN2sf_IVpE4VGbJdheBqhCAAqI89Q8";
const RELEASE = "trustiot";
const OBJECTS: IdentityKeyObjects = {
  deployment: `${RELEASE}-identity`,
  serviceAccount: `${RELEASE}-identity`,
  secret: `${RELEASE}-identity-signing-keys`,
  configMap: `${RELEASE}-internal-identity-keys`,
};

describe("the names the check reads are the ones the code writes", () => {
  test("the Secret and ConfigMap keys and the binding annotation", () => {
    const keys = source("crates", "identity-keys", "src", "lib.rs");
    expect(keys).toContain(`pub const SIGNING_KEYS_FILE: &str = "${SIGNING_KEYS_KEY}";`);
    expect(keys).toContain(`pub const JWKS_FILE: &str = "${PUBLIC_KEYS_KEY}";`);
    expect(source("tools", "migrate", "src", "passwords.rs")).toContain(`pub const UID_ANNOTATION: &str = "${BOUND_UID_ANNOTATION}";`);
  });

  for (const shape of discoverShapes(CHART_DIR)) {
    test(`values-${shape.name}.yaml: the identity Deployment names its Secret, ConfigMap and ServiceAccount`, async () => {
      const helm = await pinnedTool("helm", REPO_ROOT);
      const cmd = [helm, "template", "trustiot", CHART_DIR, "--namespace", `nightly-${shape.name}`, ...valuesArgs(CHART_DIR, shape), "--set", "services.identity.enabled=true"];
      const rendered = Bun.spawnSync({ cmd, stdout: "pipe", stderr: "pipe", timeout: COMMAND_TIMEOUT_MS });
      throwIfTimedOut(cmd, rendered);
      expect(rendered.exitCode, rendered.stderr.toString()).toBe(0);
      expect(identityKeyObjectsOf(parseManifests(rendered.stdout.toString()))).toEqual(OBJECTS);
    }, SLOW_TEST_MS);
  }

  test("a release without the init container fails the check", () => {
    const deployment = { kind: "Deployment", metadata: { name: "trustiot-identity", labels: { "app.kubernetes.io/component": "identity" } }, spec: { template: { spec: {} } } };
    expect(() => identityKeyObjectsOf([deployment])).toThrow("has no signing-keys init container");
    expect(() => identityKeyObjectsOf([])).toThrow("one identity Deployment, found 0");
  });
});

describe("the Secret, without its data", () => {
  test("the go-template prints the uid, the bound uid and the key names, never a value", () => {
    expect(SECRET_SUMMARY_TEMPLATE).toContain(".metadata.uid");
    expect(SECRET_SUMMARY_TEMPLATE).toContain(BOUND_UID_ANNOTATION);
    expect(SECRET_SUMMARY_TEMPLATE).toContain("{{range $key, $value := .data}}{{$key}},{{end}}");
    expect(SECRET_SUMMARY_TEMPLATE).not.toContain("{{$value");
  });

  test("only signing-keys.json, bound to itself", () => {
    expect(secretSummaryProblems("s", `u1|u1|${SIGNING_KEYS_KEY},`)).toEqual([]);
    expect(secretSummaryProblems("s", `u1|u1|${SIGNING_KEYS_KEY},other,`)).toEqual([`s: holds the keys [${SIGNING_KEYS_KEY}, other], expected only ${SIGNING_KEYS_KEY}`]);
    expect(secretSummaryProblems("s", "u1|u1|")).toEqual([`s: holds the keys [], expected only ${SIGNING_KEYS_KEY}`]);
    expect(secretSummaryProblems("s", `u1||${SIGNING_KEYS_KEY},`)).toEqual([`s: not bound to itself (${BOUND_UID_ANNOTATION} is not its uid)`]);
    expect(secretSummaryProblems("s", `u1|u0|${SIGNING_KEYS_KEY},`)).toHaveLength(1);
    expect(secretSummaryProblems("s", "garbage")).toEqual(["s: unexpected summary"]);
  });
});

describe("the published public keys", () => {
  const configMap = (data: Record<string, string>) => ({ data });
  const set = (...keys: Record<string, string>[]) => JSON.stringify({ keys });
  const key = { kty: "OKP", crv: "Ed25519", kid: KID, x: X };

  test("a JWK Set of Ed25519 public keys named by their thumbprints", () => {
    expect(publishedKeysOf("c", configMap({ [PUBLIC_KEYS_KEY]: set(key) }))).toEqual({ kids: [KID], problems: [] });
    expect(publishedKeysOf("c", configMap({ [PUBLIC_KEYS_KEY]: set(key, { ...key, kid: KID.replace("G", "H"), alg: "EdDSA", use: "sig" }) })).problems).toEqual([]);
  });

  test("a private member, another key type, a made-up key id, other data or no keys are problems", () => {
    expect(publishedKeysOf("c", configMap({ [PUBLIC_KEYS_KEY]: set({ ...key, d: "private" }) })).problems).toEqual([`c: key ${KID} has members beyond the public ones: d`]);
    expect(publishedKeysOf("c", configMap({ [PUBLIC_KEYS_KEY]: set({ ...key, crv: "X25519" }) })).problems).toEqual([`c: key ${KID} is not an Ed25519 public key`]);
    expect(publishedKeysOf("c", configMap({ [PUBLIC_KEYS_KEY]: set({ ...key, kid: "nightly-1" }) })).problems).toEqual(["c: key id nightly-1 is not a thumbprint"]);
    expect(publishedKeysOf("c", configMap({ [PUBLIC_KEYS_KEY]: set(key), extra: "x" })).problems).toEqual([`c: holds the keys [${PUBLIC_KEYS_KEY}, extra], expected only ${PUBLIC_KEYS_KEY}`]);
    expect(publishedKeysOf("c", configMap({ [PUBLIC_KEYS_KEY]: set() })).problems).toEqual([`c: ${PUBLIC_KEYS_KEY} lists no key`]);
    expect(publishedKeysOf("c", configMap({ [PUBLIC_KEYS_KEY]: "not json" })).problems).toEqual([`c: ${PUBLIC_KEYS_KEY} is not JSON`]);
    expect(publishedKeysOf("c", {}).problems).toContain(`c: ${PUBLIC_KEYS_KEY} is not JSON`);
  });
});

describe("who may change the keys", () => {
  const accounts = ["default", "trustiot-identity", "trustiot-assets", "trustiot-migrate"];
  const answers = (allowed: (answer: Omit<AccessAnswer, "allowed">) => boolean): AccessAnswer[] =>
    accounts.flatMap((serviceAccount) => accessQuestions(OBJECTS).map((q) => ({ serviceAccount, ...q, allowed: allowed({ serviceAccount, ...q }) })));
  const intended = (a: Omit<AccessAnswer, "allowed">) =>
    a.serviceAccount === "trustiot-identity" && ["get", "patch"].includes(a.verb) && a.resource.includes("/");

  test("every ServiceAccount is asked about both objects and both kinds", () => {
    const questions = accessQuestions(OBJECTS).map((q) => `${q.verb} ${q.resource}`);
    for (const resource of [`secrets/${OBJECTS.secret}`, `configmaps/${OBJECTS.configMap}`]) {
      for (const verb of ["get", "patch", "update", "delete"]) expect(questions).toContain(`${verb} ${resource}`);
    }
    for (const resource of ["secrets", "configmaps"]) for (const verb of ["create", "list", "watch"]) expect(questions).toContain(`${verb} ${resource}`);
  });

  test("only the identity service's account may get and patch the two objects", () => {
    expect(accessProblems(OBJECTS, answers(intended))).toEqual([]);
    // Another account that may patch the ConfigMap could make every service accept its own key.
    const wider = answers((a) => intended(a) || (a.serviceAccount === "trustiot-migrate" && a.verb === "patch" && a.resource.startsWith("configmaps/")));
    expect(accessProblems(OBJECTS, wider)).toEqual([`trustiot-migrate may patch configmaps/${OBJECTS.configMap}`]);
    const listing = answers((a) => intended(a) || (a.serviceAccount === "trustiot-identity" && a.verb === "list" && a.resource === "secrets"));
    expect(accessProblems(OBJECTS, listing)).toEqual(["trustiot-identity may list secrets"]);
    const missing = answers((a) => intended(a) && a.verb !== "patch");
    expect(accessProblems(OBJECTS, missing)).toEqual([
      `trustiot-identity may not patch secrets/${OBJECTS.secret}`,
      `trustiot-identity may not patch configmaps/${OBJECTS.configMap}`,
    ]);
    expect(accessProblems(OBJECTS, [])).toEqual(["no answer for trustiot-identity"]);
  });
});

describe("how long the services took to become ready", () => {
  const pods = {
    items: [
      {
        metadata: { name: "trustiot-identity-5d9f-abcde", labels: { "app.kubernetes.io/component": "identity" } },
        status: {
          initContainerStatuses: [
            { name: "signing-keys", state: { terminated: { finishedAt: "2026-10-15T10:00:05Z" } } },
            { name: "wait-for-migrations", state: { terminated: { finishedAt: "2026-10-15T10:00:20Z" } } },
          ],
          containerStatuses: [{ name: "identity", state: { running: { startedAt: "2026-10-15T10:00:21Z" } } }],
          conditions: [
            { type: "Initialized", status: "True", lastTransitionTime: "2026-10-15T10:00:20Z" },
            { type: "Ready", status: "True", lastTransitionTime: "2026-10-15T10:01:30Z" },
          ],
        },
      },
      {
        metadata: { name: "trustiot-assets-7c8b-fghij", labels: { "app.kubernetes.io/component": "assets" } },
        status: {
          containerStatuses: [{ name: "assets", state: { running: { startedAt: "2026-10-15T10:00:22Z" } } }],
          conditions: [{ type: "Ready", status: "False", lastTransitionTime: "2026-10-15T10:00:22Z" }],
        },
      },
      {
        metadata: { name: "trustiot-smoke-echo-1-x", labels: { "app.kubernetes.io/component": "smoke-echo" } },
        status: { containerStatuses: [{ name: "smoke-echo", state: { running: { startedAt: "2026-10-15T10:00:22Z" } } }] },
      },
    ],
  };

  test("the keys were published when the identity pod's init container finished", () => {
    expect(publishedAt(pods, "trustiot-identity")).toBe(Date.parse("2026-10-15T10:00:05Z"));
    expect(publishedAt({ items: [] }, "trustiot-identity")).toBeUndefined();
  });

  test("the services built from the commit, with their start times and when they became ready", () => {
    // A pod that is not ready has no readyAt, which the check reports as a problem.
    expect(readerPods(pods, "trustiot")).toEqual([
      { pod: "trustiot-assets-7c8b-fghij", service: "assets", startedAt: Date.parse("2026-10-15T10:00:22Z") },
      {
        pod: "trustiot-identity-5d9f-abcde",
        service: "identity",
        startedAt: Date.parse("2026-10-15T10:00:21Z"),
        readyAt: Date.parse("2026-10-15T10:01:30Z"),
      },
    ]);
  });
});
