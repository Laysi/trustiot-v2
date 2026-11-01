import type { QueryClient } from "@tanstack/react-query";
import { render } from "@testing-library/react";
import { createMemoryRouter, type RouteObject } from "react-router";

import type { DeviceApi } from "../api/devices.ts";
import type { IdentityApi } from "../api/identity.ts";
import { App, type AppDependencies } from "../app/App.tsx";
import { LOGIN_PATH, loginPath } from "../app/paths.ts";
import { createQueryClient } from "../app/query-client.ts";
import { ROUTES } from "../app/routes.tsx";
import { DEFAULT_PREFERENCES, type Preferences, type PreferencesStore } from "../preferences/preferences.ts";
import type { SessionChannel } from "../session/session-channel.ts";
import { FakeDeviceServer } from "./fake-devices.ts";
import { FakeIdentity, type FakeTenant, type FakeUser } from "./fake-identity.ts";

/** Preferences held in memory; `saved` lists every save. */
export function memoryPreferences(
  initial: Partial<Preferences> = {},
): PreferencesStore & { saved: Preferences[]; current(): Preferences } {
  let current: Preferences = { ...DEFAULT_PREFERENCES, timeZone: "UTC", ...initial };
  const saved: Preferences[] = [];
  return {
    saved,
    current: () => current,
    load: () => current,
    save: (preferences) => {
      current = preferences;
      saved.push(preferences);
    },
  };
}

/** A media query that never matches dark and never changes. */
export function lightMedia(): (query: string) => MediaQueryList {
  return (query) =>
    ({
      matches: false,
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
    }) as unknown as MediaQueryList;
}

/** An identity service with one user, a member (admin) of one tenant. */
export function fakeWithUser(
  options: { tenants?: readonly (string | null)[]; preferences?: Partial<Preferences>; platformAdmin?: boolean } = {},
): { server: FakeIdentity; user: FakeUser; tenants: FakeTenant[] } {
  const server = new FakeIdentity();
  const tenants = (options.tenants ?? ["Acme Factory"]).map((name) => server.addTenant(name));
  const user = server.addUser("alice@example.com", {
    memberships: tenants.map((tenant) => [tenant, "admin"] as const),
    ...(options.preferences ? { preferences: options.preferences } : {}),
    ...(options.platformAdmin ? { platformAdmin: true } : {}),
  });
  return { server, user, tenants };
}

/** A browser signed in to the only tenant of `fakeWithUser()`'s user, and that server. */
export function signedInBrowser(options: Parameters<typeof fakeWithUser>[0] = {}) {
  const fake = fakeWithUser(options);
  const browser = fake.server.browser(fake.server.createSession(fake.user.email, fake.tenants[0] ?? null));
  return { ...fake, browser };
}

/**
 * Renders the app's providers and a memory router at `path`, as a reload of that URL would. The
 * query client is the app's (a 401 sends the user to sign in), without retries; the identity
 * service is a signed-in fake unless the test passes one.
 */
export function renderAt(
  path: string,
  options: {
    routes?: RouteObject[];
    preferences?: PreferencesStore;
    identity?: IdentityApi;
    /** The assets and telemetry endpoints; an empty `FakeDeviceServer` unless the test passes one. */
    devices?: DeviceApi;
    languages?: readonly string[];
    browserZone?: string;
    queryClient?: QueryClient;
    /** This tab's end of the browser's session channel (`memorySessionChannels`); none by default. */
    sessionChannel?: SessionChannel;
  } = {},
) {
  const router = createMemoryRouter(options.routes ?? ROUTES, { initialEntries: [path] });
  const queryClient =
    options.queryClient ??
    createQueryClient({
      onUnauthenticated: () => {
        const { pathname, search, hash } = router.state.location;
        if (pathname === LOGIN_PATH) return;
        void router.navigate(loginPath(`${pathname}${search}${hash}`), { replace: true });
      },
    });
  queryClient.setDefaultOptions({ queries: { retry: false }, mutations: { retry: false } });
  const deps: AppDependencies = {
    queryClient,
    preferences: options.preferences ?? memoryPreferences({ language: "en" }),
    identity: options.identity ?? signedInBrowser().browser,
    devices: options.devices ?? new FakeDeviceServer().api(),
    languages: options.languages ?? ["en-US"],
    browserZone: options.browserZone ?? "UTC",
    matchMedia: lightMedia(),
    sessionChannel: options.sessionChannel,
  };
  return { router, deps, queryClient, ...render(<App router={router} deps={deps} />) };
}
