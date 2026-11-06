import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, within } from "@testing-library/react";

import { createApiClient, EXPECTED_TENANT_HEADER } from "../../api/client.ts";
import { type DeviceApi, deviceApi } from "../../api/devices.ts";
import { FakeDeviceServer } from "../../test/fake-devices.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";
import { until } from "../../test/until.ts";

afterEach(() => {
  cleanup();
  window.localStorage.clear();
});
beforeEach(() => {
  window.localStorage.clear();
});

/** A tenant switch and several lookups on a loaded machine: a generous bound for the whole test. */
const SLOW_TEST_MS = 90_000;

const box = () => screen.getByRole("textbox", { name: "Ask TrustIoT" }) as HTMLTextAreaElement;
const optionNames = () =>
  [...document.querySelectorAll('[role="option"]')].map(
    (option) => option.querySelector("span.truncate")?.textContent ?? "",
  );

async function typeAt(text = "@") {
  const input = await screen.findByRole("textbox", { name: "Ask TrustIoT" });
  act(() => {
    fireEvent.change(input, { target: { value: text } });
    (input as HTMLTextAreaElement).setSelectionRange(text.length, text.length);
  });
}

/** Opens the tenant menu from the keyboard, as Radix's trigger expects, and picks a tenant. */
async function switchTo(name: RegExp) {
  const trigger = await screen.findByRole("button", { name: /^Switch organization/ });
  await act(async () => {
    fireEvent.keyDown(trigger, { key: "Enter" });
  });
  const menu = await screen.findByRole("menu");
  await act(async () => {
    fireEvent.click(within(menu).getByRole("menuitemradio", { name }));
  });
}

/**
 * Two organizations' plants behind one device API, which answers each request from the tenant
 * the request names (the expected-tenant header the page pins), as the services do. `holdFirst`
 * keeps the first answer of a tenant's device list until it is released.
 */
function twoPlants(ids: { acme: string; beta: string }) {
  const servers = new Map<string, FakeDeviceServer>();
  const acme = new FakeDeviceServer();
  acme.addDevice({ name: "Acme Press 1", modelName: "Acme press", points: [{ key: "force", data_type: "f32" }] });
  const beta = new FakeDeviceServer();
  beta.addDevice({ name: "Beta Mixer 1", modelName: "Beta mixer", points: [{ key: "torque", data_type: "f32" }] });
  servers.set(ids.acme, acme);
  servers.set(ids.beta, beta);
  const asked: { tenant: string | null; path: string }[] = [];
  const gates = new Map<string, Promise<void>>();
  const client = createApiClient({
    fetch: async (request) => {
      const tenant = request.headers.get(EXPECTED_TENANT_HEADER);
      asked.push({ tenant, path: new URL(request.url).pathname });
      const gate = tenant === null ? undefined : gates.get(tenant);
      if (gate && request.url.endsWith("/devices")) await gate;
      const server = tenant === null ? undefined : servers.get(tenant);
      return server ? server.fetch(request) : new Response("{}", { status: 404 });
    },
  });
  const api: DeviceApi = deviceApi(client);
  return {
    api,
    acme,
    beta,
    asked,
    holdDevicesOf(tenant: string) {
      let release = () => {};
      gates.set(
        tenant,
        new Promise<void>((resolve) => {
          release = resolve;
        }),
      );
      return release;
    },
  };
}

function signedIn() {
  const fake = signedInBrowser({ tenants: ["Acme Factory", "Beta Plant"] });
  const [acme, beta] = fake.tenants;
  return { browser: fake.browser, acme: acme?.id ?? "", beta: beta?.id ?? "" };
}

describe("the production composer", () => {
  test(
    "lists the tenant's devices, models and ranges after an @, and offers no templates until a service lists them",
    async () => {
      const { browser, acme, beta } = signedIn();
      const plants = twoPlants({ acme, beta });
      renderAt("/", { identity: browser, devices: plants.api });
      await typeAt();
      await until(() => optionNames().includes("Acme Press 1"), "the devices");
      expect(optionNames()).toContain("Last week");
      expect(screen.queryByRole("button", { name: /Question templates/ })).toBeNull();
      expect(screen.getByRole("button", { name: /^Mention a device/ })).toBeTruthy();
    },
    SLOW_TEST_MS,
  );
});

describe("no other tenant's names ever appear (Laysi/trustiot-v2#155, acceptance)", () => {
  test(
    "the list shows only the active tenant's items, and a switch while it is open leaves none of the old tenant's behind",
    async () => {
      const { browser, acme, beta } = signedIn();
      const plants = twoPlants({ acme, beta });
      renderAt("/", { identity: browser, devices: plants.api });
      await typeAt("@acme");
      await until(() => optionNames().includes("Acme Press 1"), "Acme's devices");
      await until(() => optionNames().includes("Acme Press 1 · force"), "Acme's data points");
      expect(document.body.textContent).not.toContain("Beta Mixer");
      expect(document.body.textContent).toContain("Only items of Acme Factory");
      // Every request named Acme: the page's tenant, never another.
      expect(plants.asked.length).toBeGreaterThan(0);
      expect(plants.asked.every((request) => request.tenant === acme)).toBe(true);
      const betaRequests = plants.beta.requests.length;

      await switchTo(/Beta Plant/);
      await until(() => document.title === "Beta Plant · TrustIoT", "the switch to Beta");
      // The page was remounted for the new tenant: the open list, the text and everything cached for Acme are gone.
      await until(() => optionNames().length === 0, "the list to close");
      expect(box().value).toBe("");
      expect(document.body.textContent).not.toContain("Acme Press");
      expect(plants.beta.requests.length).toBe(betaRequests);

      await typeAt("@beta");
      await until(() => optionNames().includes("Beta Mixer 1"), "Beta's devices");
      await until(() => optionNames().includes("Beta Mixer 1 · torque"), "Beta's data points");
      expect(document.body.textContent).not.toContain("Acme Press");
      expect(document.body.textContent).toContain("Only items of Beta Plant");
      expect(plants.asked.filter((request) => request.tenant === beta).length).toBeGreaterThan(0);
    },
    SLOW_TEST_MS,
  );

  test(
    "an answer of the old tenant that arrives after the switch is never shown",
    async () => {
      const { browser, acme, beta } = signedIn();
      const plants = twoPlants({ acme, beta });
      const release = plants.holdDevicesOf(acme);
      renderAt("/", { identity: browser, devices: plants.api });
      await typeAt();
      // Acme's device list is on its way and has not answered; only the time ranges are listed.
      await until(
        () => plants.asked.some((request) => request.tenant === acme && request.path.endsWith("/devices")),
        "Acme's device request",
      );
      expect(optionNames()).toContain("Last week");

      await switchTo(/Beta Plant/);
      await until(() => document.title === "Beta Plant · TrustIoT", "the switch to Beta");
      act(() => release());
      await typeAt();
      // Beta's list is listed after Acme's late answer was released, so a leak would be on screen by now.
      await until(() => optionNames().includes("Beta Mixer 1"), "Beta's devices");
      expect(document.body.textContent).not.toContain("Acme Press");
    },
    SLOW_TEST_MS,
  );

  test(
    "the recently used items of one tenant are not offered in another",
    async () => {
      const { browser, acme, beta } = signedIn();
      const plants = twoPlants({ acme, beta });
      renderAt("/", { identity: browser, devices: plants.api });
      const input = await screen.findByRole("textbox", { name: "Ask TrustIoT" });
      act(() => {
        fireEvent.change(input, { target: { value: "@acme" } });
        (input as HTMLTextAreaElement).setSelectionRange(5, 5);
      });
      await until(() => optionNames().includes("Acme Press 1"), "Acme's devices");
      act(() => {
        fireEvent.keyDown(box(), { key: "Enter" });
      });
      expect(box().value).toBe("@Acme Press 1 ");
      await switchTo(/Beta Plant/);
      await until(() => document.title === "Beta Plant · TrustIoT", "the switch to Beta");
      await typeAt();
      await until(() => optionNames().includes("Beta Mixer 1"), "Beta's devices");
      expect(document.body.textContent).not.toContain("Recently used");
      expect(document.body.textContent).not.toContain("Acme Press");
    },
    SLOW_TEST_MS,
  );
});
