import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { DEVICES_PATH, deviceNewPath, deviceSettingsPath } from "../../app/paths.ts";
import { FakeDeviceServer } from "../../test/fake-devices.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";

afterEach(() => {
  cleanup();
});

type Role = "viewer" | "editor" | "admin";

/** Two models, one device on a gateway; the tenant's gateways are tg-0001 (online) and tg-0002 (offline). */
function tenant() {
  const server = new FakeDeviceServer();
  const meter = server.addModel("power-meter");
  const sensor = server.addModel("th-transmitter");
  server.assets.addModelWithPoints("chiller", [{ key: "running", function_code: 1, address: 0, data_type: "bool" }]);
  const device = server.addDevice({
    name: "line-1-meter",
    modelId: meter.model_id,
    hardwareId: "tg-0001",
    host: "192.0.2.10",
    transport: "rtu_over_tcp",
    port: 4196,
    unitId: 2,
  });
  return { server, meter, sensor, device };
}

function open(server: FakeDeviceServer, path: string, role: Role = "admin") {
  return renderAt(path, {
    devices: server.api(),
    assets: server.assetsApi(),
    identity: signedInBrowser({ role }).browser,
  });
}

function type(label: string, value: string) {
  fireEvent.change(screen.getByLabelText(label), { target: { value } });
}

async function fillNewDevice(server: FakeDeviceServer, modelId: string) {
  await screen.findByRole("heading", { level: 1, name: "Add a device" });
  // The selects fill in once the gateways and models are loaded.
  await waitFor(() => expect((screen.getByLabelText("Gateway") as HTMLSelectElement).disabled).toBe(false));
  await waitFor(() => expect((screen.getByLabelText("Model") as HTMLSelectElement).disabled).toBe(false));
  await act(async () => {
    type("Name", "line-2-meter");
    type("Gateway", "tg-0001");
    type("Model", modelId);
    type("Host", "192.0.2.30");
  });
  expect(server.devices.size).toBe(1);
}

describe("adding a device", () => {
  test("creates the device with the form's values and the list shows it with a notice", async () => {
    const { server, sensor } = tenant();
    const { router } = open(server, `${DEVICES_PATH}/new`);
    await fillNewDevice(server, sensor.model_id);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Create the device" }));
    });
    await waitFor(() => expect(router.state.location.pathname).toBe(DEVICES_PATH));
    expect(server.assets.writes.filter((write) => write.operation === "createDevice")).toEqual([
      {
        operation: "createDevice",
        method: "POST",
        path: "/api/assets/v1/devices",
        body: {
          name: "line-2-meter",
          hardware_id: "tg-0001",
          model_id: sensor.model_id,
          transport: "modbus_tcp",
          host: "192.0.2.30",
          port: 502,
          unit_id: 1,
          poll_interval_ms: 60_000,
        },
      },
    ]);
    expect(await screen.findByText(/The device line-2-meter was added/)).toBeDefined();
    expect((await screen.findAllByRole("link", { name: "line-2-meter" })).length).toBeGreaterThan(0);
    // Dismissing the notice removes it.
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    });
    expect(screen.queryByText(/The device line-2-meter was added/)).toBeNull();
  });

  test("the address can choose the gateway beforehand", async () => {
    const { server } = tenant();
    open(server, deviceNewPath("tg-0002"));
    await screen.findByRole("heading", { level: 1, name: "Add a device" });
    await waitFor(() => expect((screen.getByLabelText("Gateway") as HTMLSelectElement).value).toBe("tg-0002"));
    const options = within(screen.getByLabelText("Gateway")).getAllByRole("option");
    expect(options.map((option) => option.textContent)).toEqual([
      "Choose a gateway",
      "tg-0001 (online)",
      "tg-0002 (offline)",
    ]);
  });

  test("a single bound gateway is chosen for the user", async () => {
    const { server } = tenant();
    server.assets.gateways.splice(1);
    open(server, deviceNewPath());
    await waitFor(() => expect((screen.getByLabelText("Gateway") as HTMLSelectElement).value).toBe("tg-0001"));
  });

  test("polling intervals below the plan's shortest are listed but disabled, and the default is the shortest allowed", async () => {
    const { server } = tenant();
    open(server, deviceNewPath());
    await screen.findByRole("heading", { level: 1, name: "Add a device" });
    await waitFor(() => expect(screen.getByText(/Your plan \(Starter\) allows polling/)).toBeDefined());
    const poll = screen.getByLabelText("Polling interval") as HTMLSelectElement;
    const options = within(poll).getAllByRole("option") as HTMLOptionElement[];
    expect(options.map((option) => [option.textContent, option.disabled])).toEqual([
      ["1 second (needs a higher plan)", true],
      ["10 seconds (needs a higher plan)", true],
      ["30 seconds (needs a higher plan)", true],
      ["1 minute", false],
      ["5 minutes", false],
      ["15 minutes", false],
      ["1 hour", false],
    ]);
    expect(poll.value).toBe("60000");
  });

  test("a plan that allows 10 seconds enables it", async () => {
    const { server } = tenant();
    server.assets.planLimits = { plan: "business", minPollIntervalMs: 10_000 };
    open(server, deviceNewPath());
    await waitFor(() => expect(screen.getByText(/Your plan \(Business\) allows polling/)).toBeDefined());
    const options = within(screen.getByLabelText("Polling interval")).getAllByRole("option") as HTMLOptionElement[];
    expect(options.map((option) => option.disabled)).toEqual([true, false, false, false, false, false, false]);
  });

  test("an empty form is not sent: each input says what is missing, in a summary that takes the focus", async () => {
    const { server } = tenant();
    open(server, deviceNewPath());
    await screen.findByRole("heading", { level: 1, name: "Add a device" });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Create the device" }));
    });
    const summary = await screen.findByRole("alert");
    expect(document.activeElement).toBe(summary);
    expect(summary.textContent).toContain("things need fixing");
    expect(screen.getByLabelText("Name").getAttribute("aria-invalid")).toBe("true");
    expect(screen.getByLabelText("Host").getAttribute("aria-invalid")).toBe("true");
    expect(screen.getByText("Choose a gateway.")).toBeDefined();
    expect(server.assets.writes.filter((write) => write.operation === "createDevice")).toEqual([]);
    // A line of the summary leads to its input.
    await act(async () => {
      fireEvent.click(within(summary).getByRole("button", { name: /^Host:/ }));
    });
    expect(document.activeElement).toBe(screen.getByLabelText("Host"));
    // Typing in an input answers its problem.
    await act(async () => {
      type("Host", "192.0.2.30");
    });
    expect(screen.getByLabelText("Host").getAttribute("aria-invalid")).toBeNull();
  });

  test("the service's field errors show under their inputs, with the hints kept, and in the summary", async () => {
    const { server, sensor } = tenant();
    server.assets.failNext(
      "createDevice",
      new Response(
        JSON.stringify({
          code: "validation_failed",
          message_key: "error.validation_failed",
          field_errors: [
            { field: "/host", message_key: "device.host_invalid", details: { max: 253 } },
            { field: "/unit_id", message_key: "device.unit_id_invalid", details: { min: 1, max: 247 } },
            {
              field: "/poll_interval_ms",
              message_key: "device.poll_interval_invalid",
              details: { min: 60_000, max: 86_400_000 },
            },
          ],
        }),
        { status: 422, headers: { "content-type": "application/json" } },
      ),
    );
    open(server, deviceNewPath());
    await fillNewDevice(server, sensor.model_id);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Create the device" }));
    });
    const summary = await screen.findByRole("alert");
    expect(summary.textContent).toContain("3 things need fixing");
    expect(summary.textContent).toContain("Host: Enter an IP address or a domain name of at most 253 bytes.");
    for (const label of ["Host", "Unit ID", "Polling interval"]) {
      expect(screen.getByLabelText(label).getAttribute("aria-invalid")).toBe("true");
    }
    // The error is added to the input's description; the hint stays.
    const host = screen.getByLabelText("Host");
    const described = (host.getAttribute("aria-describedby") ?? "")
      .split(" ")
      .map((id) => document.getElementById(id)?.textContent);
    expect(described).toEqual([
      "The IP address or domain name of the device or converter.",
      expect.stringContaining("Enter an IP address or a domain name of at most 253 bytes."),
    ]);
    expect(screen.getByLabelText("Name").getAttribute("aria-invalid")).toBeNull();
    // The inputs keep what the user typed.
    expect((host as HTMLInputElement).value).toBe("192.0.2.30");
    expect(server.devices.size).toBe(1);
  });

  test("a taken name (a 409 without a field error) shows under the name", async () => {
    const { server, sensor } = tenant();
    open(server, deviceNewPath());
    await fillNewDevice(server, sensor.model_id);
    await act(async () => {
      type("Name", "line-1-meter");
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Create the device" }));
    });
    await waitFor(() =>
      expect(screen.getAllByText("Your organization already has a device with this name.").length).toBeGreaterThan(0),
    );
    expect(screen.getByLabelText("Name").getAttribute("aria-invalid")).toBe("true");
  });

  test("a refusal that is not about an input shows as a message and keeps the form", async () => {
    const { server, sensor } = tenant();
    server.assets.failNext(
      "createDevice",
      Response.json({ code: "unavailable", message_key: "error.unavailable" }, { status: 503 }),
    );
    open(server, deviceNewPath());
    await fillNewDevice(server, sensor.model_id);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Create the device" }));
    });
    expect(await screen.findByText(fakeErrorText())).toBeDefined();
    expect((screen.getByLabelText("Host") as HTMLInputElement).value).toBe("192.0.2.30");
  });

  test("a tenant without bound gateways or models is told so at the selects", async () => {
    const server = new FakeDeviceServer();
    server.assets.gateways.splice(0);
    open(server, deviceNewPath());
    expect(await screen.findByText(/has no bound gateway yet/)).toBeDefined();
    const create = screen.getByRole("link", { name: "Create a model" });
    expect(create.getAttribute("href")).toBe("/models/new");
  });

  test("a viewer is told why they cannot add a device, with the way back", async () => {
    const { server } = tenant();
    open(server, deviceNewPath(), "viewer");
    expect(await screen.findByRole("heading", { level: 1, name: "You can't add devices" })).toBeDefined();
    expect(screen.getByText(/Your role in Acme Factory is viewer/)).toBeDefined();
    expect(screen.queryByRole("form")).toBeNull();
    expect(screen.getByRole("link", { name: "Back to the devices" }).getAttribute("href")).toBe("/devices");
  });
});

function fakeErrorText(): string {
  return "The service is temporarily unavailable. Try again later.";
}

describe("a device's settings", () => {
  test("show the fixed values as text, save the whole form with the revision it read, and say so", async () => {
    const { server, device } = tenant();
    open(server, deviceSettingsPath(device.device_id));
    await screen.findByRole("heading", { level: 1, name: "line-1-meter" });
    await waitFor(() => expect((screen.getByLabelText("Host") as HTMLInputElement).value).toBe("192.0.2.10"));
    // The name, gateway and model cannot change: they are text, not inputs.
    expect(screen.queryByLabelText("Name")).toBeNull();
    expect(screen.getAllByText("Can't be changed after the device is created.")).toHaveLength(3);
    expect(screen.getByText("tg-0001")).toBeDefined();
    expect((screen.getByRole("radio", { name: /RTU over TCP/ }) as HTMLInputElement).checked).toBe(true);
    expect((screen.getByLabelText("Port") as HTMLInputElement).value).toBe("4196");
    expect((screen.getByLabelText("Unit ID") as HTMLInputElement).value).toBe("2");

    await act(async () => {
      type("Host", "192.0.2.99");
      type("Polling interval", "300000");
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Save changes" }));
    });
    expect(await screen.findByText("Saved. The device is at revision 2.")).toBeDefined();
    expect(server.assets.writes.filter((write) => write.operation === "updateDevice")).toEqual([
      {
        operation: "updateDevice",
        method: "PUT",
        path: `/api/assets/v1/devices/${device.device_id}`,
        // The update replaces every setting, so all of them are sent, not only the ones that changed.
        body: {
          revision: 1,
          transport: "rtu_over_tcp",
          host: "192.0.2.99",
          port: 4196,
          unit_id: 2,
          poll_interval_ms: 300_000,
        },
      },
    ]);
    expect(server.devices.get(device.device_id)?.host).toBe("192.0.2.99");
    // A second save names the new revision.
    await act(async () => {
      type("Port", "502");
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Save changes" }));
    });
    expect(await screen.findByText("Saved. The device is at revision 3.")).toBeDefined();
    expect(server.assets.writes.at(-1)?.body).toMatchObject({ revision: 2, port: 502 });
  });

  test("a polling interval outside the list stays selectable, marked as current", async () => {
    const { server, device } = tenant();
    server.assets.bumpDevice(device.device_id, { poll_interval_ms: 45_000 });
    server.assets.bumpDevice(device.device_id, {});
    open(server, deviceSettingsPath(device.device_id));
    await waitFor(() => expect((screen.getByLabelText("Polling interval") as HTMLSelectElement).value).toBe("45000"));
    const labels = within(screen.getByLabelText("Polling interval"))
      .getAllByRole("option")
      .map((option) => option.textContent);
    expect(labels).toContain("45 seconds (current)");
  });

  test("a device someone else changed keeps the user's edit, offers to reload, and lists the edit beside the latest settings", async () => {
    const { server, device } = tenant();
    open(server, deviceSettingsPath(device.device_id));
    await waitFor(() => expect((screen.getByLabelText("Host") as HTMLInputElement).value).toBe("192.0.2.10"));
    await act(async () => {
      type("Host", "192.0.2.77");
    });
    server.assets.bumpDevice(device.device_id, { host: "192.0.2.50", port: 4197 });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Save changes" }));
    });
    expect(await screen.findByText("Someone else changed this device (it is now at revision 2)")).toBeDefined();
    // Nothing was lost: the input still holds the user's edit.
    expect((screen.getByLabelText("Host") as HTMLInputElement).value).toBe("192.0.2.77");
    expect(server.devices.get(device.device_id)?.host).toBe("192.0.2.50");

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Reload the latest settings" }));
    });
    await waitFor(() => expect((screen.getByLabelText("Host") as HTMLInputElement).value).toBe("192.0.2.50"));
    expect((screen.getByLabelText("Port") as HTMLInputElement).value).toBe("4197");
    expect(screen.getByText("Your changes")).toBeDefined();
    expect(screen.getByText("Host: 192.0.2.77")).toBeDefined();

    // Taking the edit back and saving names the latest revision.
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Use my changes" }));
    });
    expect((screen.getByLabelText("Host") as HTMLInputElement).value).toBe("192.0.2.77");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Save changes" }));
    });
    expect(await screen.findByText("Saved. The device is at revision 3.")).toBeDefined();
    expect(server.assets.writes.at(-1)?.body).toMatchObject({ revision: 2, host: "192.0.2.77" });
  });

  test("a device whose gateway was unbound locks every input and leaves only deleting", async () => {
    const { server, device } = tenant();
    server.assets.endedDevices.add(device.device_id);
    open(server, deviceSettingsPath(device.device_id));
    await waitFor(() => expect((screen.getByLabelText("Host") as HTMLInputElement).value).toBe("192.0.2.10"));
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Save changes" }));
    });
    expect(await screen.findByText("The gateway was unbound")).toBeDefined();
    for (const label of ["Host", "Port", "Unit ID", "Polling interval"]) {
      expect((screen.getByLabelText(label) as HTMLInputElement).disabled).toBe(true);
    }
    expect(screen.queryByRole("button", { name: "Save changes" })).toBeNull();
    expect(screen.getByRole("button", { name: "Delete the device" })).toBeDefined();
  });

  test("deleting asks first, starts on Cancel, and removes the device", async () => {
    const { server, device } = tenant();
    const { router } = open(server, deviceSettingsPath(device.device_id));
    await waitFor(() => expect((screen.getByLabelText("Host") as HTMLInputElement).value).toBe("192.0.2.10"));
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Delete the device" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText("Delete the device line-1-meter?")).toBeDefined();
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(server.devices.size).toBe(1);
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Delete the device" }));
    });
    await waitFor(() => expect(router.state.location.pathname).toBe(DEVICES_PATH));
    expect(server.devices.size).toBe(0);
    expect(await screen.findByText("The device line-1-meter was deleted.")).toBeDefined();
  });

  test("a refused delete shows in the dialog, which stays open", async () => {
    const { server, device } = tenant();
    server.assets.failNext("deleteDevice", Response.json(fakeErrorBody(), { status: 503 }));
    open(server, deviceSettingsPath(device.device_id));
    await waitFor(() => expect((screen.getByLabelText("Host") as HTMLInputElement).value).toBe("192.0.2.10"));
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Delete the device" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Delete the device" }));
    });
    expect(await within(dialog).findByText(fakeErrorText())).toBeDefined();
    expect(server.devices.size).toBe(1);
  });

  test("an unknown device says so, and a viewer is told they cannot change devices", async () => {
    const { server } = tenant();
    open(server, deviceSettingsPath("00000000-0000-4000-8000-0000000000ff"));
    expect(await screen.findByRole("heading", { level: 1, name: "Device not found" })).toBeDefined();
    cleanup();
    const { device } = tenant();
    open(server, deviceSettingsPath(device.device_id), "viewer");
    expect(await screen.findByRole("heading", { level: 1, name: "You can't change devices" })).toBeDefined();
  });
});

function fakeErrorBody() {
  return { code: "unavailable", message_key: "error.unavailable" };
}

describe("the device list's actions", () => {
  test("an administrator and an editor get the add button and a row menu; a viewer gets neither", async () => {
    for (const role of ["admin", "editor"] as const) {
      const { server } = tenant();
      open(server, DEVICES_PATH, role);
      const add = await screen.findByRole("link", { name: "Add a device" });
      expect(add.getAttribute("href")).toBe("/devices/new");
      expect(screen.getAllByRole("button", { name: "Actions for line-1-meter" }).length).toBeGreaterThan(0);
      cleanup();
    }
    const { server } = tenant();
    open(server, DEVICES_PATH, "viewer");
    await screen.findAllByRole("link", { name: "line-1-meter" });
    expect(screen.queryByRole("link", { name: "Add a device" })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Actions for/ })).toBeNull();
    expect(screen.queryByRole("columnheader", { name: "Actions" })).toBeNull();
  });

  test("the row menu edits the settings and deletes after confirming", async () => {
    const { server, device } = tenant();
    server.addDevice({ name: "line-2-meter", modelId: device.model_id, hardwareId: "tg-0001", host: "192.0.2.11" });
    const { router } = open(server, DEVICES_PATH);
    const trigger = (await screen.findAllByRole("button", { name: "Actions for line-1-meter" }))[0] as HTMLElement;
    await act(async () => {
      fireEvent.keyDown(trigger, { key: "Enter" });
    });
    const edit = within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Edit settings" });
    expect(edit.getAttribute("href")).toBe(deviceSettingsPath(device.device_id));

    // Delete from the menu: a dialog first, the list updates after, with a notice.
    await act(async () => {
      fireEvent.click(within(screen.getByRole("menu")).getByRole("menuitem", { name: "Delete…" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText("Delete the device line-1-meter?")).toBeDefined();
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Delete the device" }));
    });
    await waitFor(() => expect(server.devices.size).toBe(1));
    expect(await screen.findByText("The device line-1-meter was deleted.")).toBeDefined();
    await waitFor(() => expect(screen.queryAllByRole("link", { name: "line-1-meter" })).toHaveLength(0));
    expect(router.state.location.pathname).toBe(DEVICES_PATH);
  });
});
