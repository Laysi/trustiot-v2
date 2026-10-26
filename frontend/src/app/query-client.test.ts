import { describe, expect, test } from "bun:test";

import { ApiError } from "../api/errors.ts";
import { createQueryClient, HANDLES_UNAUTHENTICATED, MAX_RETRIES } from "./query-client.ts";

describe("createQueryClient", () => {
  test("an unauthenticated response sends the user to sign in; others do not", async () => {
    let signIns = 0;
    const client = createQueryClient({ onUnauthenticated: () => (signIns += 1) });
    await client
      .fetchQuery({ queryKey: ["a"], queryFn: () => Promise.reject(new ApiError(401, null)), retry: false })
      .catch(() => {});
    expect(signIns).toBe(1);
    await client
      .fetchQuery({ queryKey: ["b"], queryFn: () => Promise.reject(new ApiError(403, null)), retry: false })
      .catch(() => {});
    await client
      .fetchQuery({ queryKey: ["c"], queryFn: () => Promise.reject(new ApiError(503, null)), retry: false })
      .catch(() => {});
    expect(signIns).toBe(1);
  });

  test("a request that handles 401 itself (the sign-in form) does not send the user to sign in", async () => {
    let signIns = 0;
    const client = createQueryClient({ onUnauthenticated: () => (signIns += 1) });
    await client
      .getMutationCache()
      .build(client, {
        mutationFn: () =>
          Promise.reject(new ApiError(401, { code: "unauthenticated", message_key: "error.unauthenticated" })),
        meta: HANDLES_UNAUTHENTICATED,
      })
      .execute(undefined)
      .catch(() => {});
    await client
      .fetchQuery({
        queryKey: ["d"],
        queryFn: () => Promise.reject(new ApiError(401, null)),
        retry: false,
        meta: HANDLES_UNAUTHENTICATED,
      })
      .catch(() => {});
    expect(signIns).toBe(0);
    await client
      .getMutationCache()
      .build(client, { mutationFn: () => Promise.reject(new ApiError(401, null)) })
      .execute(undefined)
      .catch(() => {});
    expect(signIns).toBe(1);
  });

  test("retries only what is worth retrying", () => {
    const client = createQueryClient({ onUnauthenticated: () => {} });
    const retry = client.getDefaultOptions().queries?.retry as (failures: number, error: unknown) => boolean;
    expect(retry(0, new ApiError(503, null))).toBe(true);
    expect(retry(0, new ApiError(0, null))).toBe(true);
    expect(retry(MAX_RETRIES, new ApiError(503, null))).toBe(false);
    expect(retry(0, new ApiError(500, { code: "internal", message_key: "error.internal" }))).toBe(false);
    expect(retry(0, new ApiError(401, null))).toBe(false);
    expect(retry(0, new Error("bug"))).toBe(false);
  });
});
