import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { createApiClient } from "../../api/client.ts";
import { deviceApi } from "../../api/devices.ts";
import { devicesOfModelPath, MODELS_PATH, modelPath } from "../../app/paths.ts";
import { captureDownloads } from "../../test/downloads.ts";
import { FakeDeviceServer, fakeError } from "../../test/fake-devices.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";

afterEach(() => {
  cleanup();
});

type Role = "viewer" | "editor" | "admin";

/** Two models, three devices on the first, none on the second. */
function tenant() {
  const server = new FakeDeviceServer();
  const meter = server.assets.addModelWithPoints("power-meter", [
    { key: "voltage", function_code: 3, address: 0, data_type: "f32" },
    { key: "current", function_code: 3, address: 2, data_type: "f32" },
  ]);
  const chiller = server.assets.addModelWithPoints("chiller-controller", [
    { key: "running", function_code: 1, address: 0, data_type: "bool" },
  ]);
  for (const name of ["line-1-meter", "line-2-meter", "line-3-meter"]) {
    server.addDevice({ name, modelId: meter.model_id, hardwareId: "tg-0001", host: "192.0.2.10" });
  }
  return { server, meter, chiller };
}

function open(server: FakeDeviceServer, path = MODELS_PATH, role: Role = "admin") {
  return renderAt(path, {
    devices: server.api(),
    assets: server.assetsApi(),
    identity: signedInBrowser({ role }).browser,
  });
}

function table(): HTMLElement {
  return screen.getByRole("table");
}

describe("the model list", () => {
  test("lists each model with its data points, the devices that use it, and when it changed", async () => {
    const { server, meter, chiller } = tenant();
    open(server);
    await within(await screen.findByRole("table")).findByRole("link", { name: "power-meter" });
    const rows = within(table()).getAllByRole("row").slice(1);
    // Name, data points, devices using it; then the date and the row menu.
    expect(
      rows.map((row) =>
        within(row)
          .getAllByRole("cell", { hidden: true })
          .slice(0, 2)
          .map((cell) => cell.textContent),
      ),
    ).toEqual([
      ["1", "0"],
      ["2", "3"],
    ]);
    expect(rows.map((row) => within(row).getByRole("rowheader").textContent)).toEqual([
      "chiller-controller",
      "power-meter",
    ]);
    const used = within(table()).getByRole("link", { name: "3 devices use power-meter" });
    expect(used.getAttribute("href")).toBe(devicesOfModelPath(meter.model_id));
    expect(within(table()).getByRole("link", { name: "chiller-controller" }).getAttribute("href")).toBe(
      modelPath(chiller.model_id),
    );
  });

  test("an administrator gets Import JSON, New model and a menu per row", async () => {
    const { server } = tenant();
    open(server);
    expect(await screen.findByRole("button", { name: "Import JSON" })).toBeDefined();
    expect(screen.getByRole("link", { name: "New model" }).getAttribute("href")).toBe("/models/new");
    expect(screen.getAllByRole("button", { name: "Actions for power-meter" }).length).toBeGreaterThan(0);
  });

  test("a viewer reads the list: no buttons, no row menus", async () => {
    const { server } = tenant();
    open(server, MODELS_PATH, "viewer");
    await within(await screen.findByRole("table")).findByRole("link", { name: "power-meter" });
    expect(screen.queryByRole("button", { name: "Import JSON" })).toBeNull();
    expect(screen.queryByRole("link", { name: "New model" })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Actions for/ })).toBeNull();
    // `?import` does nothing for a viewer.
    cleanup();
    open(server, `${MODELS_PATH}?import`, "viewer");
    await screen.findByRole("table");
    expect(screen.queryByRole("alertdialog")).toBeNull();
  });

  test("while it loads there is one status, then a failed load says why and Try again loads again", async () => {
    const { server } = tenant();
    server.failNext("models", fakeError(503, "unavailable", "error.unavailable"));
    // The model list is held back until the test lets it go, so the loading state can be looked at.
    let release: () => void = () => {};
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const client = createApiClient({
      fetch: async (request) => {
        if (request.url.endsWith("/device-models")) await gate;
        return server.fetch(request);
      },
    });
    renderAt(MODELS_PATH, {
      devices: deviceApi(client),
      assets: server.assetsApi(),
      identity: signedInBrowser().browser,
    });
    expect(await screen.findByText("Loading the models…")).toBeDefined();
    expect(screen.getAllByRole("status")).toHaveLength(1);
    release();
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("The models couldn't be loaded");
    await act(async () => {
      fireEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    });
    await within(await screen.findByRole("table")).findByRole("link", { name: "power-meter" });
  });

  test("a tenant without models is told the next step, with New model and Import JSON", async () => {
    const server = new FakeDeviceServer();
    open(server);
    expect(await screen.findByText("No device models yet")).toBeDefined();
    // Both are in the page header and in the empty state.
    for (const link of screen.getAllByRole("link", { name: "New model" }))
      expect(link.getAttribute("href")).toBe("/models/new");
    const imports = screen.getAllByRole("link", { name: "Import JSON" });
    expect(imports.length).toBeGreaterThan(0);
    for (const link of imports) expect(link.getAttribute("href")).toBe("/models?import");
    cleanup();
    open(server, MODELS_PATH, "viewer");
    expect(await screen.findByText("Ask an editor or an administrator to create one.")).toBeDefined();
    expect(screen.queryByRole("link", { name: "New model" })).toBeNull();
  });

  test("the row menu edits, exports and deletes", async () => {
    const { server, chiller } = tenant();
    const downloads = captureDownloads();
    try {
      open(server);
      const trigger = (
        await screen.findAllByRole("button", { name: "Actions for chiller-controller" })
      )[0] as HTMLElement;
      await act(async () => {
        fireEvent.keyDown(trigger, { key: "Enter" });
      });
      const menu = await screen.findByRole("menu");
      expect(within(menu).getByRole("menuitem", { name: "Edit" }).getAttribute("href")).toBe(
        modelPath(chiller.model_id),
      );
      await act(async () => {
        fireEvent.click(within(menu).getByRole("menuitem", { name: "Export JSON" }));
      });
      await waitFor(() => expect(downloads.files).toHaveLength(1));
      expect(downloads.files[0]?.name).toBe("chiller-controller.json");
      expect(JSON.parse(await (downloads.files[0]?.text ?? Promise.resolve("")))).toEqual({
        format: "trustiot.device_model",
        format_version: 1,
        name: "chiller-controller",
        points: [{ key: "running", function_code: 1, address: 0, data_type: "bool" }],
      });
    } finally {
      downloads.restore();
    }
  });

  test("deleting asks first and removes an unused model, with a notice", async () => {
    const { server } = tenant();
    open(server);
    const trigger = (
      await screen.findAllByRole("button", { name: "Actions for chiller-controller" })
    )[0] as HTMLElement;
    await act(async () => {
      fireEvent.keyDown(trigger, { key: "Enter" });
    });
    await act(async () => {
      fireEvent.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Delete…" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByText("Delete the model chiller-controller?")).toBeDefined();
    expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" }));
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Delete the model" }));
    });
    expect(await screen.findByText("The model chiller-controller was deleted.")).toBeDefined();
    expect(server.models.size).toBe(1);
    await waitFor(() => expect(screen.queryAllByRole("link", { name: "chiller-controller" })).toHaveLength(0));
  });

  test("a model that devices use cannot be deleted: the dialog says so and leads to those devices", async () => {
    const { server, meter } = tenant();
    open(server);
    const trigger = (await screen.findAllByRole("button", { name: "Actions for power-meter" }))[0] as HTMLElement;
    await act(async () => {
      fireEvent.keyDown(trigger, { key: "Enter" });
    });
    await act(async () => {
      fireEvent.click(within(await screen.findByRole("menu")).getByRole("menuitem", { name: "Delete…" }));
    });
    const dialog = await screen.findByRole("alertdialog");
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Delete the model" }));
    });
    expect(await within(dialog).findByText("It can't be deleted")).toBeDefined();
    const link = within(dialog).getByRole("link", { name: "See the devices that use power-meter" });
    expect(link.getAttribute("href")).toBe(devicesOfModelPath(meter.model_id));
    expect((within(dialog).getByRole("button", { name: "Delete the model" }) as HTMLButtonElement).disabled).toBe(true);
    expect(server.models.size).toBe(2);
    // Closing leaves the list as it was.
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Close" }));
    });
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
  });
});
