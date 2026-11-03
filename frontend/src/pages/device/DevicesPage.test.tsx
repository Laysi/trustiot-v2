import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { DEVICES_PATH, devicePath } from "../../app/paths.ts";
import { FakeDeviceServer, fakeError } from "../../test/fake-devices.ts";
import { renderAt } from "../../test/render.tsx";

afterEach(() => {
  cleanup();
});

/** Two models and four devices on two gateways, one device behind an RS485 converter. */
function fleet() {
  const server = new FakeDeviceServer();
  const meter = server.addModel("power-meter");
  const sensor = server.addModel("th-transmitter");
  server.addDevice({ name: "line-1-meter", modelId: meter.model_id, hardwareId: "tg-0001", host: "192.0.2.10" });
  server.addDevice({
    name: "oven-2-sensor",
    modelId: sensor.model_id,
    hardwareId: "tg-0001",
    transport: "rtu_over_tcp",
    host: "192.0.2.20",
    port: 4196,
    unitId: 3,
    pollIntervalMs: 300_000,
  });
  server.addDevice({
    name: "warehouse-meter",
    modelId: meter.model_id,
    hardwareId: "tg-0002",
    host: "meter-wh.factory.example",
    pollIntervalMs: 900_000,
  });
  return { server, meter, sensor };
}

/** The wide form of the list (the phone's list holds the same devices). */
function table(): HTMLElement {
  return screen.getByRole("table");
}

async function open(server: FakeDeviceServer, path = DEVICES_PATH) {
  const rendered = renderAt(path, { devices: server.api() });
  await screen.findByRole("heading", { level: 1, name: "Devices" });
  return rendered;
}

function rowNames(): string[] {
  return within(table())
    .getAllByRole("row")
    .slice(1)
    .map((row) => within(row).getByRole("link").textContent ?? "");
}

describe("the device list", () => {
  test("lists the tenant's devices with model, gateway, connection and polling, and a device's name opens its page", async () => {
    const { server } = fleet();
    const { router } = await open(server);
    await within(await screen.findByRole("table")).findByRole("link", { name: "line-1-meter" });

    expect(rowNames()).toEqual(["line-1-meter", "oven-2-sensor", "warehouse-meter"]);
    const oven = within(table()).getByRole("row", { name: /oven-2-sensor/ });
    expect(within(oven).getByText("Model th-transmitter")).toBeDefined();
    expect(within(oven).getByText("tg-0001")).toBeDefined();
    expect(within(oven).getByText("Modbus RTU over TCP · 192.0.2.20:4196 · unit 3")).toBeDefined();
    expect(within(oven).getByText("Every 5 minutes")).toBeDefined();
    // No status column (health data is not there yet), no add button, no row actions.
    expect(within(table()).queryByRole("columnheader", { name: "Status" })).toBeNull();
    expect(within(screen.getByRole("main")).queryByRole("button")).toBeNull();
    expect(screen.getByText("3 of 3 devices")).toBeDefined();

    const link = within(table()).getByRole("link", { name: "warehouse-meter" });
    const [device] = [...server.devices.values()].filter((candidate) => candidate.name === "warehouse-meter");
    expect(link.getAttribute("href")).toBe(devicePath(device?.device_id ?? ""));
    await act(async () => {
      fireEvent.click(link);
    });
    await waitFor(() => expect(router.state.location.pathname).toBe(devicePath(device?.device_id ?? "")));
    expect(await screen.findByRole("heading", { level: 1, name: "warehouse-meter" })).toBeDefined();
  });

  test("the menu marks Devices on the list and on a device's page", async () => {
    const { server } = fleet();
    const [device] = [...server.devices.values()];
    for (const path of [DEVICES_PATH, devicePath(device?.device_id ?? "")]) {
      const { unmount } = renderAt(path, { devices: server.api() });
      const trigger = await screen.findByRole("button", { name: "Menu" });
      await act(async () => {
        fireEvent.keyDown(trigger, { key: "Enter" });
      });
      const menu = await screen.findByRole("menu");
      const marked = within(menu)
        .getAllByRole("menuitem")
        .filter((entry) => entry.getAttribute("aria-current") === "page");
      expect(marked.map((entry) => entry.textContent)).toEqual(["Devices"]);
      unmount();
      cleanup();
    }
  });

  test("search and the filters narrow the list and are kept in the address, so a reload shows the same list", async () => {
    const { server, sensor } = fleet();
    const { router, unmount } = await open(server);
    await within(await screen.findByRole("table")).findByRole("link", { name: "line-1-meter" });

    // Search by a host, then by a name.
    await act(async () => {
      fireEvent.change(screen.getByRole("searchbox", { name: "Search devices" }), { target: { value: "192.0.2.20" } });
    });
    expect(rowNames()).toEqual(["oven-2-sensor"]);
    expect(router.state.location.search).toBe("?q=192.0.2.20");
    await act(async () => {
      fireEvent.change(screen.getByRole("searchbox", { name: "Search devices" }), { target: { value: "METER" } });
    });
    expect(rowNames()).toEqual(["line-1-meter", "warehouse-meter"]);

    // Gateway and model, on top of the search.
    await act(async () => {
      fireEvent.change(screen.getByRole("combobox", { name: "Gateway" }), { target: { value: "tg-0002" } });
    });
    expect(rowNames()).toEqual(["warehouse-meter"]);
    expect(screen.getByText("1 of 3 devices")).toBeDefined();
    expect(router.state.location.search).toBe("?q=METER&gateway=tg-0002");

    unmount();
    cleanup();
    // The same address, as after a reload: the box, the selects, the chips and the rows agree.
    const modelUrl = `${DEVICES_PATH}?gateway=tg-0001&model=${sensor.model_id}`;
    const reloaded = renderAt(modelUrl, { devices: server.api() });
    await within(await screen.findByRole("table")).findByRole("link", { name: "oven-2-sensor" });
    expect(rowNames()).toEqual(["oven-2-sensor"]);
    expect((screen.getByRole("combobox", { name: "Gateway" }) as HTMLSelectElement).value).toBe("tg-0001");
    expect((screen.getByRole("combobox", { name: "Model" }) as HTMLSelectElement).value).toBe(sensor.model_id);
    expect(screen.getByRole("button", { name: "Remove the filter Model: th-transmitter" })).toBeDefined();

    // Removing a chip and clearing all go back to the whole list and out of the address.
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Remove the filter Model: th-transmitter" }));
    });
    expect(rowNames()).toEqual(["line-1-meter", "oven-2-sensor"]);
    expect(reloaded.router.state.location.search).toBe("?gateway=tg-0001");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Clear all" }));
    });
    expect(rowNames()).toHaveLength(3);
    expect(reloaded.router.state.location.search).toBe("");
  });

  test("going to another search address puts that search in the box", async () => {
    const { server } = fleet();
    const { router } = await open(server, `${DEVICES_PATH}?q=oven`);
    const box = await screen.findByRole("searchbox", { name: "Search devices" });
    expect((box as HTMLInputElement).value).toBe("oven");
    await act(async () => {
      await router.navigate(`${DEVICES_PATH}?q=warehouse`);
    });
    await waitFor(() => expect((box as HTMLInputElement).value).toBe("warehouse"));
    await act(async () => {
      await router.navigate(DEVICES_PATH);
    });
    await waitFor(() => expect((box as HTMLInputElement).value).toBe(""));
  });

  test("a filter that matches nothing says so and offers to clear it", async () => {
    const { server } = fleet();
    await open(server, `${DEVICES_PATH}?q=nothing-like-this`);
    expect(await screen.findByText("No device matches")).toBeDefined();
    expect(screen.queryByRole("table")).toBeNull();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Clear all" }));
    });
    expect(await screen.findByRole("table")).toBeDefined();
    expect((screen.getByRole("searchbox", { name: "Search devices" }) as HTMLInputElement).value).toBe("");
  });

  test("a tenant without devices is told the next steps, as text with no links to pages that do not exist", async () => {
    const server = new FakeDeviceServer();
    await open(server);
    expect(await screen.findByText("No devices yet")).toBeDefined();
    const steps = screen.getByRole("list", { name: "Steps to set up devices" });
    expect(
      within(steps)
        .getAllByRole("listitem")
        .map((item) => item.textContent),
    ).toEqual([
      "1Bind a gateway",
      "2Create a device model(next)A model describes which registers to read and how to convert them. You can import one from JSON exported from another organization.",
      "3Add a device",
    ]);
    expect(screen.queryByRole("searchbox")).toBeNull();
    expect(screen.queryByRole("table")).toBeNull();
    expect(within(screen.getByRole("main")).queryByRole("button")).toBeNull();
    expect(within(screen.getByRole("main")).queryByRole("link")).toBeNull();
  });

  test("once a model exists, the model step is done and adding a device is next", async () => {
    const server = new FakeDeviceServer();
    server.addModel("power-meter");
    await open(server);
    const steps = await screen.findByRole("list", { name: "Steps to set up devices" });
    const items = within(steps).getAllByRole("listitem");
    expect(items[1]?.textContent).toContain("(done)");
    expect(items[2]?.textContent).toBe("3Add a device(next)");
  });

  test("a failed model list is said, and the setup steps do not claim what it would have shown", async () => {
    const server = new FakeDeviceServer();
    server.failNext("models", fakeError(503, "unavailable", "error.unavailable"));
    await open(server);
    expect(await screen.findByText(/The model names couldn't be loaded/)).toBeDefined();
    const second = screen.getByText("Create a device model").closest("li");
    expect(second?.textContent).not.toMatch(/done|next/);
  });

  test("a failed model list leaves the devices listed, and Try again brings the model names", async () => {
    const { server } = fleet();
    server.failNext("models", fakeError(503, "unavailable", "error.unavailable"));
    await open(server);
    expect(await screen.findByText(/The model names couldn't be loaded/)).toBeDefined();
    expect(screen.getAllByText("warehouse-meter").length).toBeGreaterThan(0);
    expect(screen.queryByText("Model power-meter")).toBeNull();
    await act(async () => {
      fireEvent.click(screen.getByText("Try again"));
    });
    expect((await screen.findAllByText("Model power-meter")).length).toBeGreaterThan(0);
    expect(screen.queryByText(/The model names couldn't be loaded/)).toBeNull();
  });

  test("a failed load says why and Retry loads again", async () => {
    const { server } = fleet();
    server.failNext("devices", fakeError(503, "unavailable", "error.unavailable"));
    await open(server);
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("The devices couldn't be loaded");
    await act(async () => {
      fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    });
    await waitFor(() => expect(screen.getByRole("table")).toBeDefined());
    expect(rowNames()).toHaveLength(3);
  });
});
