import { describe, expect, test } from "bun:test";
import { type QueryKey, QueryObserver } from "@tanstack/react-query";

import { ApiError } from "../api/errors.ts";
import {
  createQueryClient,
  HANDLES_UNAUTHENTICATED,
  MAX_RETRIES,
  removeQueriesOnceUnobserved,
} from "./query-client.ts";

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

describe("removeQueriesOnceUnobserved", () => {
  const client = () => createQueryClient({ onUnauthenticated: () => {} });
  const keys = (queryClient: ReturnType<typeof client>) =>
    queryClient
      .getQueryCache()
      .getAll()
      .map((query) => query.queryKey);
  /** Observes `queryKey` the way a mounted `useQuery` does; the answer is cached, so nothing is read. */
  const observe = (queryClient: ReturnType<typeof client>, queryKey: QueryKey) => {
    const observer = new QueryObserver(queryClient, { queryKey, queryFn: () => "read again", staleTime: Infinity });
    return { observer, leave: observer.subscribe(() => {}) };
  };

  test("what nothing observes goes at once, and nothing outside the key", () => {
    const queryClient = client();
    queryClient.setQueryData(["doc", "a"], "a");
    queryClient.setQueryData(["doc", "a", "page", 1], "a1");
    queryClient.setQueryData(["doc", "b"], "b");
    removeQueriesOnceUnobserved(queryClient, ["doc", "a"]);
    expect(keys(queryClient)).toEqual([["doc", "b"]]);
  });

  test("an observed query stays as it is while observed, and goes when its last observer leaves", () => {
    const queryClient = client();
    queryClient.setQueryData(["doc", "a"], "a");
    queryClient.setQueryData(["doc", "a", "page", 1], "a1");
    const page = observe(queryClient, ["doc", "a"]);
    const image = observe(queryClient, ["doc", "a", "page", 1]);
    const aside = observe(queryClient, ["doc", "a", "page", 1]);
    const cached = queryClient.getQueryCache().find({ queryKey: ["doc", "a"], exact: true });
    removeQueriesOnceUnobserved(queryClient, ["doc", "a"]);
    expect(keys(queryClient)).toHaveLength(2);
    // Another render of the page that still shows it reads the cached query: none is built, none is read.
    expect(page.observer.getOptimisticResult(queryClient.defaultQueryOptions(page.observer.options)).data).toBe("a");
    expect(queryClient.getQueryCache().find({ queryKey: ["doc", "a"], exact: true })).toBe(cached);
    expect(queryClient.isFetching()).toBe(0);
    image.leave();
    expect(keys(queryClient)).toHaveLength(2);
    aside.leave();
    expect(keys(queryClient)).toEqual([["doc", "a"]]);
    page.leave();
    expect(keys(queryClient)).toEqual([]);
  });

  test("an observer that moves to another of the key's queries keeps the one it moves to", () => {
    const queryClient = client();
    queryClient.setQueryData(["doc", "a", "page", 1], "a1");
    const image = observe(queryClient, ["doc", "a", "page", 1]);
    removeQueriesOnceUnobserved(queryClient, ["doc", "a"]);
    queryClient.setQueryData(["doc", "a", "page", 2], "a2");
    image.observer.setOptions({ queryKey: ["doc", "a", "page", 2], queryFn: () => "read again", staleTime: Infinity });
    expect(keys(queryClient)).toEqual([["doc", "a", "page", 2]]);
    image.leave();
    expect(keys(queryClient)).toEqual([]);
  });

  test("a query built under the key while it is observed, and never observed, goes once nothing is observed", async () => {
    const queryClient = client();
    queryClient.setQueryData(["doc", "a"], "a");
    const page = observe(queryClient, ["doc", "a"]);
    removeQueriesOnceUnobserved(queryClient, ["doc", "a"]);
    // A render that built a query and was never committed leaves it with no observer.
    queryClient.getQueryCache().build(queryClient, { queryKey: ["doc", "a", "page", 3] });
    await Promise.resolve();
    expect(keys(queryClient)).toEqual([
      ["doc", "a"],
      ["doc", "a", "page", 3],
    ]);
    page.leave();
    await Promise.resolve();
    expect(keys(queryClient)).toEqual([]);
  });

  test("once the key's queries are gone it stops watching", async () => {
    const queryClient = client();
    queryClient.setQueryData(["doc", "a"], "a");
    const page = observe(queryClient, ["doc", "a"]);
    removeQueriesOnceUnobserved(queryClient, ["doc", "a"]);
    page.leave();
    expect(keys(queryClient)).toEqual([]);
    await Promise.resolve();
    // Cached again later under the same key, it is kept as usual when its observer leaves.
    queryClient.setQueryData(["doc", "a"], "again");
    observe(queryClient, ["doc", "a"]).leave();
    expect(keys(queryClient)).toEqual([["doc", "a"]]);
  });
});
