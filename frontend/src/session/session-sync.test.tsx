import { afterEach, describe, expect, test } from "bun:test";
import { focusManager, type QueryClient, useMutation } from "@tanstack/react-query";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { type FormEvent, useEffect, useRef, useState } from "react";
import type { RouteObject } from "react-router";

import {
  type ApiClient,
  createApiClient,
  EXPECTED_NO_TENANT,
  EXPECTED_TENANT_HEADER,
  EXPECTED_USER_HEADER,
} from "../api/client.ts";
import { ApiError, parseErrorBody } from "../api/errors.ts";
import { type EventStream, openEventStream, type StreamFetch, type StreamState } from "../api/sse.ts";
import { Layout } from "../app/Layout.tsx";
import { useI18n } from "../i18n/useI18n.ts";
import { apiError, type FakeBrowser, type FakeIdentity } from "../test/fake-identity.ts";
import { renderAt, signedInBrowser } from "../test/render.tsx";
import { memorySessionChannels } from "../test/session-channels.ts";
import { useSessionBound } from "./PinSession.tsx";
import { shownSession, useStreamSession } from "./SessionProvider.tsx";
import { TenantGate } from "./TenantGate.tsx";

// Laysi/trustiot-v2#678: every tab of a browser shares the session cookie, and with it the
// session's user and current tenant. Each test renders two tabs of one browser: the same cookie jar (one
// `FakeBrowser`) against the same server, each tab with its own query cache, router and end of the
// session channel.

afterEach(() => {
  cleanup();
  focusManager.setFocused(undefined);
});

type Tab = ReturnType<typeof renderAt>;

function tabView(tab: Tab) {
  return within(tab.container);
}

function shownOrganization(tab: Tab): string {
  return tabView(tab).getByRole("button", { name: /^Organization:/ }).textContent ?? "";
}

async function switchTenantIn(tab: Tab, name: RegExp) {
  const trigger = await tabView(tab).findByRole("button", { name: /^Organization:/ });
  await act(async () => {
    fireEvent.keyDown(trigger, { key: "Enter" });
  });
  const menu = await screen.findByRole("menu");
  await act(async () => {
    fireEvent.click(within(menu).getByRole("menuitemradio", { name }));
  });
}

async function signOutIn(tab: Tab) {
  await act(async () => {
    fireEvent.keyDown(tabView(tab).getByRole("button", { name: "Account menu" }), { key: "Enter" });
  });
  const menu = await screen.findByRole("menu");
  await act(async () => {
    fireEvent.click(within(menu).getByRole("menuitem", { name: "Sign out" }));
  });
}

/**
 * The ingress's cookie check (identity's ext_authz) in front of one tenant write: a request that
 * names a user other than the session's, or a tenant other than its current one, is refused with
 * 409 `session_changed` before it reaches the service; `writes` lists what reached it, with the
 * user and the tenant it went to.
 */
function checkedWrites(server: FakeIdentity, browser: FakeBrowser) {
  const writes: { user: string; tenant: string | null; expected: [string | null, string | null]; body: unknown }[] = [];
  const refused: [string | null, string | null][] = [];
  const fetch = async (request: Request): Promise<Response> => {
    const session = browser.cookie === null ? undefined : server.sessions.get(browser.cookie);
    if (!session || session.ended) {
      return Response.json({ code: "unauthenticated", message_key: "error.unauthenticated" }, { status: 401 });
    }
    const user = request.headers.get(EXPECTED_USER_HEADER);
    const tenant = request.headers.get(EXPECTED_TENANT_HEADER);
    if (
      (user !== null && user !== session.userId) ||
      (tenant !== null && tenant !== (session.activeTenant ?? EXPECTED_NO_TENANT))
    ) {
      refused.push([user, tenant]);
      return Response.json({ code: "session_changed", message_key: "error.session_changed" }, { status: 409 });
    }
    writes.push({
      user: session.userId,
      tenant: session.activeTenant,
      expected: [user, tenant],
      body: await request.json(),
    });
    return Response.json({}, { status: 201 });
  };
  return { fetch, writes, refused };
}

/** Signs `browser` in as `email` without any tab, as a sign-in in another tab does. */
async function signInElsewhere(server: FakeIdentity, browser: FakeBrowser, email: string) {
  await browser.sendCode(email);
  const sent = server.outbox.at(-1);
  if (sent?.email !== email) throw new Error(`no code was sent to ${email}`);
  await browser.verifyCode({ email, code: sent.code, redirectTo: "/" });
}

/**
 * A tenant page with a form whose unsent text a remount loses, and one write; with `between`, a
 * second write after waiting for it, as a mutation of several steps makes.
 */
function NotePage({ client, between }: { client: ApiClient; between?: (() => Promise<void>) | undefined }) {
  const { errorMessage } = useI18n();
  const [note, setNote] = useState("");
  // Bound to the page, as useDeviceApi and useIdentityApi bind theirs.
  const page = useSessionBound(client);
  const send = useMutation({
    mutationFn: async (title: string) => {
      const post = async (body: { title: string }) => {
        const { response, error } = await page.POST("/api/assistant/v1/conversations", { body });
        if (!response.ok) throw new ApiError(response.status, parseErrorBody(error));
      };
      await post({ title });
      if (between) {
        await between();
        await post({ title: `${title}, step 2` });
      }
    },
  });
  const submit = (event: FormEvent) => {
    event.preventDefault();
    send.mutate(note);
  };
  return (
    <form onSubmit={submit}>
      <label>
        Note
        <input value={note} onChange={(event) => setNote(event.target.value)} />
      </label>
      <button type="submit">Send note</button>
      {send.isError ? <p role="alert">{errorMessage(send.error)}</p> : null}
    </form>
  );
}

function noteRoutes(client: ApiClient, between?: () => Promise<void>): RouteObject[] {
  return [
    {
      path: "/",
      element: <Layout />,
      children: [
        {
          element: <TenantGate />,
          children: [{ index: true, element: <NotePage client={client} between={between} /> }],
        },
      ],
    },
  ];
}

/** A tenant page that opens a stream of the tenant's data when asked, as a live chart would. */
function StreamPage({ fetch }: { fetch: StreamFetch }) {
  const session = useStreamSession();
  const [state, setState] = useState<StreamState | "idle">("idle");
  const stream = useRef<EventStream | null>(null);
  useEffect(() => () => stream.current?.close(), []);
  const open = () => {
    stream.current = openEventStream("/api/telemetry/v1/live", {
      fetch,
      session,
      onEvent: () => {},
      onState: setState,
    });
  };
  return (
    <>
      <button type="button" onClick={open}>
        Open stream
      </button>
      <p>Stream: {state}</p>
    </>
  );
}

function twoTenants() {
  const fake = signedInBrowser({ tenants: ["Acme Factory", "Beta Plant"] });
  const [acme, beta] = fake.tenants;
  if (!acme || !beta) throw new Error("two tenants expected");
  return { ...fake, acme, beta };
}

function loadSessionCount(server: FakeIdentity): number {
  return server.operations().filter((operation) => operation === "loadSession").length;
}

describe("tabs of one browser (Laysi/trustiot-v2#678)", () => {
  test("after tab B switches tenants, tab A shows the new tenant and says so, and drops the old tenant's data", async () => {
    const { server, browser, user, acme, beta } = twoTenants();
    const channels = memorySessionChannels();
    const tabA = renderAt("/", { identity: browser, sessionChannel: channels.tab() });
    const tabB = renderAt("/", { identity: browser, sessionChannel: channels.tab() });
    await waitFor(() => expect(shownOrganization(tabA)).toContain("Acme Factory"));
    await waitFor(() => expect(shownOrganization(tabB)).toContain("Acme Factory"));
    tabA.queryClient.setQueryData(["devices", acme.id], ["a device of Acme"]);
    const readsBefore = loadSessionCount(server);

    await switchTenantIn(tabB, /Beta Plant/);

    await waitFor(() => expect(shownOrganization(tabA)).toContain("Beta Plant"));
    expect(loadSessionCount(server)).toBeGreaterThan(readsBefore);
    expect(
      await tabView(tabA).findByText(
        "The organization was switched to Beta Plant in another tab, so this tab now shows Beta Plant too. Anything you hadn't sent wasn't sent; check before you continue.",
      ),
    ).toBeDefined();
    expect(tabA.queryClient.getQueryData(["devices", acme.id])).toBeUndefined();
    expect(shownSession(tabA.queryClient)).toEqual({ user: user.id, tenant: beta.id });
    // Tab B switched itself: no notice there.
    expect(tabView(tabB).queryByRole("status")).toBeNull();

    await act(async () => {
      fireEvent.click(tabView(tabA).getByRole("button", { name: "Dismiss" }));
    });
    expect(tabView(tabA).queryByText(/in another tab/)).toBeNull();
  });

  test("a tab that never heard of the switch cannot write into the new tenant: the write is refused, the tab re-reads the session and starts afresh", async () => {
    const { server, browser, user, acme, beta } = twoTenants();
    let tabACache: QueryClient | undefined;
    const check = checkedWrites(server, browser);
    const client = createApiClient({
      fetch: check.fetch,
      shownSession: () => (tabACache ? shownSession(tabACache) : undefined),
    });
    // Tab A has no channel: like a browser without BroadcastChannel, or a tab frozen in the background.
    const tabA = renderAt("/", { identity: browser, routes: noteRoutes(client) });
    tabACache = tabA.queryClient;
    await waitFor(() => expect(shownOrganization(tabA)).toContain("Acme Factory"));
    const note = () => tabView(tabA).getByLabelText("Note") as HTMLInputElement;
    await act(async () => {
      fireEvent.change(note(), { target: { value: "Stop pump 7" } });
    });

    // Tab B moves the session to Beta Plant; tab A still shows Acme Factory and the typed note.
    await browser.switchTenant(beta.id);
    expect(shownOrganization(tabA)).toContain("Acme Factory");

    await act(async () => {
      fireEvent.click(tabView(tabA).getByRole("button", { name: "Send note" }));
    });

    await waitFor(() => expect(shownOrganization(tabA)).toContain("Beta Plant"));
    expect(check.writes).toEqual([]);
    expect(check.refused).toEqual([[user.id, acme.id]]);
    expect(await tabView(tabA).findByText(/^The organization was switched to Beta Plant in another tab/)).toBeDefined();
    // The page of the previous tenant is gone, and with it the note that was not sent.
    await waitFor(() => expect(note().value).toBe(""));

    // Sent from the page of the tenant the session is now in, it goes through, to that tenant.
    await act(async () => {
      fireEvent.change(note(), { target: { value: "Check pump 2" } });
    });
    await act(async () => {
      fireEvent.click(tabView(tabA).getByRole("button", { name: "Send note" }));
    });
    await waitFor(() => expect(check.writes).toHaveLength(1));
    expect(check.writes[0]).toEqual({
      user: user.id,
      tenant: beta.id,
      expected: [user.id, beta.id],
      body: { title: "Check pump 2" },
    });
  });

  test("a tab of the previous user cannot write as the next one, even in the same tenant: the write is refused and the tab starts afresh as the new user", async () => {
    const { server, browser, user, acme } = twoTenants();
    const bob = server.addUser("bob@example.com", { memberships: [[acme, "admin"]] });
    // Bob's latest session was in Acme Factory, so his new one starts there: only the user changes.
    server.createSession(bob.email, acme);
    let tabACache: QueryClient | undefined;
    const check = checkedWrites(server, browser);
    const client = createApiClient({
      fetch: check.fetch,
      shownSession: () => (tabACache ? shownSession(tabACache) : undefined),
    });
    // Tab A has no channel, as in the test above.
    const tabA = renderAt("/", { identity: browser, routes: noteRoutes(client) });
    tabACache = tabA.queryClient;
    await waitFor(() => expect(shownOrganization(tabA)).toContain("Acme Factory"));
    const note = () => tabView(tabA).getByLabelText("Note") as HTMLInputElement;
    await act(async () => {
      fireEvent.change(note(), { target: { value: "Stop pump 7" } });
    });

    // Another tab signs this browser in as Bob; tab A still shows Alice in Acme Factory.
    await signInElsewhere(server, browser, bob.email);
    const bobSession = browser.cookie === null ? undefined : server.sessions.get(browser.cookie);
    expect(bobSession).toEqual({ userId: bob.id, activeTenant: acme.id, ended: false });

    await act(async () => {
      fireEvent.click(tabView(tabA).getByRole("button", { name: "Send note" }));
    });

    expect(
      await tabView(tabA).findByText(
        "Another account signed in on this browser in another tab, so this tab now shows that account. Nothing of the previous account is shown here anymore.",
      ),
    ).toBeDefined();
    expect(check.writes).toEqual([]);
    expect(check.refused).toEqual([[user.id, acme.id]]);
    await waitFor(() => expect(shownSession(tabA.queryClient)).toEqual({ user: bob.id, tenant: acme.id }));
    // Alice's page is gone, and with it her note that was not sent.
    await waitFor(() => expect(note().value).toBe(""));

    // Sent from Bob's page, it goes through, as Bob.
    await act(async () => {
      fireEvent.change(note(), { target: { value: "Check pump 2" } });
    });
    await act(async () => {
      fireEvent.click(tabView(tabA).getByRole("button", { name: "Send note" }));
    });
    await waitFor(() => expect(check.writes).toHaveLength(1));
    expect(check.writes[0]).toEqual({
      user: bob.id,
      tenant: acme.id,
      expected: [bob.id, acme.id],
      body: { title: "Check pump 2" },
    });
  });

  test("a later step of a mutation started in the previous tenant is refused, even after the tab shows the new one: nothing is written into it", async () => {
    const { server, browser, user, acme, beta } = twoTenants();
    const channels = memorySessionChannels();
    let tabACache: QueryClient | undefined;
    const check = checkedWrites(server, browser);
    const client = createApiClient({
      fetch: check.fetch,
      shownSession: () => (tabACache ? shownSession(tabACache) : undefined),
    });
    let release = () => {};
    const between = new Promise<void>((resolve) => {
      release = resolve;
    });
    let waiting = false;
    const tabA = renderAt("/", {
      identity: browser,
      sessionChannel: channels.tab(),
      routes: noteRoutes(client, () => {
        waiting = true;
        return between;
      }),
    });
    tabACache = tabA.queryClient;
    const tabB = renderAt("/", { identity: browser, sessionChannel: channels.tab() });
    await waitFor(() => expect(shownOrganization(tabA)).toContain("Acme Factory"));
    await waitFor(() => expect(shownOrganization(tabB)).toContain("Acme Factory"));
    await act(async () => {
      fireEvent.change(tabView(tabA).getByLabelText("Note"), { target: { value: "Stop pump 7" } });
    });
    await act(async () => {
      fireEvent.click(tabView(tabA).getByRole("button", { name: "Send note" }));
    });
    // The first step went to Acme Factory; the second waits.
    await waitFor(() => expect(waiting).toBe(true));
    expect(check.writes.map((write) => [write.tenant, write.body])).toEqual([[acme.id, { title: "Stop pump 7" }]]);

    // Tab B moves the session to Beta Plant, and tab A hears of it and shows Beta Plant.
    await switchTenantIn(tabB, /Beta Plant/);
    await waitFor(() => expect(shownSession(tabA.queryClient)).toEqual({ user: user.id, tenant: beta.id }));
    await waitFor(() => expect(shownOrganization(tabA)).toContain("Beta Plant"));

    // The second step still names Acme Factory, which its page was made for: refused.
    release();
    await waitFor(() => expect(check.refused).toEqual([[user.id, acme.id]]));
    expect(check.writes.map((write) => write.tenant)).toEqual([acme.id]);
    expect(check.writes.filter((write) => write.tenant === beta.id)).toEqual([]);
  });

  test("a tenant switch refused because another tab signed in as someone else reads the session again and shows the new account", async () => {
    const { server, browser, acme } = twoTenants();
    const bob = server.addUser("bob@example.com", { memberships: [[acme, "admin"]] });
    server.createSession(bob.email, acme);
    // Tab A has no channel: it does not hear of the sign-in.
    const tabA = renderAt("/", { identity: browser });
    await waitFor(() => expect(shownOrganization(tabA)).toContain("Acme Factory"));

    await signInElsewhere(server, browser, bob.email);
    // The cookie check refuses the switch: it names Alice, and the session is Bob's.
    server.failNext("switchTenant", apiError(409, "session_changed", "error.session_changed"));
    await switchTenantIn(tabA, /Beta Plant/);

    expect(
      await tabView(tabA).findByText(
        "Another account signed in on this browser in another tab, so this tab now shows that account. Nothing of the previous account is shown here anymore.",
      ),
    ).toBeDefined();
    await waitFor(() => expect(shownSession(tabA.queryClient)).toEqual({ user: bob.id, tenant: acme.id }));
    expect(server.sessions.get(browser.cookie ?? "")?.activeTenant).toBe(acme.id);
  });

  test("after tab B signs out, tab A no longer shows the user's data and asks to sign in", async () => {
    const { browser, acme } = twoTenants();
    const channels = memorySessionChannels();
    const tabA = renderAt("/", { identity: browser, sessionChannel: channels.tab() });
    const tabB = renderAt("/", { identity: browser, sessionChannel: channels.tab() });
    await waitFor(() => expect(shownOrganization(tabA)).toContain("Acme Factory"));
    await waitFor(() => expect(shownOrganization(tabB)).toContain("Acme Factory"));
    tabA.queryClient.setQueryData(["devices", acme.id], ["a device of Acme"]);

    await signOutIn(tabB);

    await waitFor(() => expect(tabA.router.state.location.pathname).toBe("/login"));
    expect(tabA.queryClient.getQueryData(["devices", acme.id])).toBeUndefined();
    expect(tabA.queryClient.getQueryData(["preferences"])).toBeUndefined();
    expect(tabA.queryClient.getQueryData(["session"])).toBeNull();
    expect(tabView(tabA).queryByText("Acme Factory")).toBeNull();
    expect(tabView(tabA).queryByRole("button", { name: /^Organization:/ })).toBeNull();
  });

  test("after tab B signs in as someone else, tab A forgets the previous user and shows the new one", async () => {
    const { server, browser, acme } = twoTenants();
    const gamma = server.addTenant("Gamma Works");
    const bob = server.addUser("bob@example.com", { memberships: [[gamma, "admin"]], preferences: { theme: "dark" } });
    // Bob's latest session was in Gamma Works, so his new one starts there.
    server.createSession(bob.email, gamma);
    const channels = memorySessionChannels();
    const tabA = renderAt("/", { identity: browser, sessionChannel: channels.tab() });
    await waitFor(() => expect(shownOrganization(tabA)).toContain("Acme Factory"));
    tabA.queryClient.setQueryData(["devices", acme.id], ["a device of Acme"]);
    const tabB = renderAt("/login", { identity: browser, sessionChannel: channels.tab() });

    const b = tabView(tabB);
    await act(async () => {
      fireEvent.change(await b.findByLabelText("Email"), { target: { value: bob.email } });
    });
    await act(async () => {
      fireEvent.click(b.getByRole("button", { name: "Send code" }));
    });
    const code = await waitFor(() => {
      const sent = server.outbox.at(-1);
      if (sent?.email !== bob.email) throw new Error("waiting for the code sent to bob");
      return sent.code;
    });
    await act(async () => {
      fireEvent.change(await b.findByLabelText("Sign-in code"), { target: { value: code } });
    });
    await act(async () => {
      fireEvent.click(b.getByRole("button", { name: "Sign in" }));
    });

    // Bob has one tenant: the header names it, with no menu.
    expect(await tabView(tabA).findByText("Gamma Works")).toBeDefined();
    expect(
      await tabView(tabA).findByText(
        "Another account signed in on this browser in another tab, so this tab now shows that account. Nothing of the previous account is shown here anymore.",
      ),
    ).toBeDefined();
    expect(tabA.queryClient.getQueryData(["devices", acme.id])).toBeUndefined();
    expect(tabView(tabA).queryByText("Acme Factory")).toBeNull();
    await waitFor(() =>
      expect(tabA.queryClient.getQueryData<{ user: { email: string } }>(["session"])?.user.email).toBe(bob.email),
    );
    // The preferences are read again, as Bob's: none of Alice's stay on screen.
    await waitFor(() => expect(tabA.queryClient.getQueryData<{ theme: string }>(["preferences"])?.theme).toBe("dark"));
  });

  test("a tab that missed the announcement reads the session when it becomes visible again", async () => {
    const { server, browser, beta } = twoTenants();
    // No channel: the tab learns only from the focus.
    const tabA = renderAt("/", { identity: browser });
    await waitFor(() => expect(shownOrganization(tabA)).toContain("Acme Factory"));
    focusManager.setFocused(false);
    await browser.switchTenant(beta.id);
    const readsBefore = loadSessionCount(server);

    await act(async () => {
      focusManager.setFocused(true);
    });

    await waitFor(() => expect(shownOrganization(tabA)).toContain("Beta Plant"));
    expect(loadSessionCount(server)).toBeGreaterThan(readsBefore);
    expect(await tabView(tabA).findByText(/^The organization was switched to Beta Plant in another tab/)).toBeDefined();
  });

  test("a stream opened from a tab that shows the previous tenant is refused, and the tab re-reads the session", async () => {
    const { server, browser, user, acme, beta } = twoTenants();
    const named: [string | undefined, string | undefined][] = [];
    // The cookie check in front of the stream: 409 for another user or tenant than the session's.
    const fetch: StreamFetch = async (_url, init) => {
      const headers = init.headers as Record<string, string>;
      const expectedUser = headers[EXPECTED_USER_HEADER];
      const expectedTenant = headers[EXPECTED_TENANT_HEADER];
      named.push([expectedUser, expectedTenant]);
      const session = browser.cookie === null ? undefined : server.sessions.get(browser.cookie);
      const refused =
        (expectedUser !== undefined && expectedUser !== session?.userId) ||
        (expectedTenant !== undefined && expectedTenant !== (session?.activeTenant ?? EXPECTED_NO_TENANT));
      return { status: refused ? 409 : 503, headers: new Headers(), body: null };
    };
    const routes: RouteObject[] = [
      {
        path: "/",
        element: <Layout />,
        children: [{ element: <TenantGate />, children: [{ index: true, element: <StreamPage fetch={fetch} /> }] }],
      },
    ];
    // No channel: the tab learns only from the refusal.
    const tabA = renderAt("/", { identity: browser, routes });
    await waitFor(() => expect(shownOrganization(tabA)).toContain("Acme Factory"));
    await browser.switchTenant(beta.id);
    const readsBefore = loadSessionCount(server);

    await act(async () => {
      fireEvent.click(tabView(tabA).getByRole("button", { name: "Open stream" }));
    });

    await waitFor(() => expect(shownOrganization(tabA)).toContain("Beta Plant"));
    expect(named[0]).toEqual([user.id, acme.id]);
    expect(loadSessionCount(server)).toBeGreaterThan(readsBefore);
    expect(await tabView(tabA).findByText(/^The organization was switched to Beta Plant in another tab/)).toBeDefined();
    // The page of the previous tenant, and its refused stream, are gone.
    expect(await tabView(tabA).findByText("Stream: idle")).toBeDefined();
  });
});
