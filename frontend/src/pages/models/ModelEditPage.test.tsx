import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import type { DeviceModelPoint } from "../../api/devices.ts";
import { devicesOfModelPath, MODELS_PATH, modelPath, modelsNewPath } from "../../app/paths.ts";
import { captureDownloads } from "../../test/downloads.ts";
import { FakeDeviceServer } from "../../test/fake-devices.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";

declare const happyDOM: { setViewport(viewport: { width: number; height: number }): void };

beforeEach(() => {
  happyDOM.setViewport({ width: 1280, height: 800 });
});

afterEach(() => {
  cleanup();
  happyDOM.setViewport({ width: 1280, height: 800 });
});

type Role = "viewer" | "editor" | "admin";

const POINTS: DeviceModelPoint[] = [
  { key: "voltage", function_code: 3, address: 0, data_type: "f32", description: "Average phase voltage (V)" },
  { key: "current", function_code: 3, address: 2, data_type: "f32" },
  { key: "energy_kwh", function_code: 3, address: 4, data_type: "u32", byte_order: "cdab", scale: 0.01, offset: 0 },
  { key: "serial", function_code: 4, address: 100, data_type: "string", length: 8 },
  { key: "running", function_code: 1, address: 0, data_type: "bool" },
];

function tenant() {
  const server = new FakeDeviceServer();
  const model = server.assets.addModelWithPoints("power-meter", POINTS);
  server.addDevice({ name: "line-1-meter", modelId: model.model_id, hardwareId: "tg-0001" });
  server.addDevice({ name: "line-2-meter", modelId: model.model_id, hardwareId: "tg-0001" });
  return { server, model };
}

function open(server: FakeDeviceServer, path: string, role: Role = "admin") {
  return renderAt(path, {
    devices: server.api(),
    assets: server.assetsApi(),
    identity: signedInBrowser({ role }).browser,
  });
}

async function openModel(server: FakeDeviceServer, modelId: string, role: Role = "admin") {
  const rendered = open(server, modelPath(modelId), role);
  await screen.findByRole("heading", { level: 1, name: "power-meter" });
  await screen.findByLabelText("Key of data point 1");
  return rendered;
}

function field(label: string): HTMLInputElement {
  return screen.getByLabelText(label) as HTMLInputElement;
}

function edit(label: string, value: string) {
  fireEvent.change(screen.getByLabelText(label), { target: { value } });
}

function saves(server: FakeDeviceServer) {
  return server.assets.writes.filter((write) => write.operation === "updateModel");
}

/** The body of the last update the service received. */
function lastSave(server: FakeDeviceServer): { revision: number; points: DeviceModelPoint[] } {
  const write = saves(server).at(-1);
  if (!write) throw new Error("no update was sent");
  return write.body as { revision: number; points: DeviceModelPoint[] };
}

async function save() {
  await act(async () => {
    fireEvent.click(screen.getByRole("button", { name: "Save the model" }));
  });
}

describe("a model's page", () => {
  test("shows the revision, the devices that use it and the data points, with the address a manual would print", async () => {
    const { server, model } = tenant();
    await openModel(server, model.model_id);
    await waitFor(() => expect(screen.getByText(/Revision 1 · used by 2 devices · updated/)).toBeDefined());
    expect(screen.getByRole("link", { name: "Device models" }).getAttribute("href")).toBe(MODELS_PATH);
    expect(screen.getByText(/the 2 devices that use this model get a new configuration version/)).toBeDefined();
    expect(field("Key of data point 3").value).toBe("energy_kwh");
    expect(field("Address of data point 3").value).toBe("4");
    // Manual addresses: holding 40001+, input registers 30001+, coils 00001+.
    expect(screen.getAllByText("Manual: 40001")).toHaveLength(1);
    expect(screen.getByText("Manual: 40005")).toBeDefined();
    expect(screen.getByText("Manual: 30101")).toBeDefined();
    expect(screen.getByText("Manual: 00001")).toBeDefined();
    expect(field("Scale of data point 3").value).toBe("0.01");
    expect(field("Length of data point 4").value).toBe("8");
    // A string and a bool cannot be scaled; a bool has no byte order.
    expect(screen.getAllByText("Can't be scaled")).toHaveLength(2);
    expect(screen.queryByLabelText("Byte order of data point 5")).toBeNull();
    expect(screen.getByRole("link", { name: "See the devices" }).getAttribute("href")).toBe(
      devicesOfModelPath(model.model_id),
    );
  });

  test("saves every data point with the revision it read, leaving out what does not apply to the type", async () => {
    const { server, model } = tenant();
    await openModel(server, model.model_id);
    await act(async () => {
      edit("Scale of data point 1", "0.1");
      edit("Description of data point 2", "  Phase current (A)  ");
    });
    await save();
    expect(await screen.findByText("Saved. The model is at revision 2.")).toBeDefined();
    expect(saves(server)).toEqual([
      {
        operation: "updateModel",
        method: "PUT",
        path: `/api/assets/v1/device-models/${model.model_id}`,
        body: {
          revision: 1,
          points: [
            {
              key: "voltage",
              function_code: 3,
              address: 0,
              data_type: "f32",
              byte_order: "abcd",
              scale: 0.1,
              description: "Average phase voltage (V)",
            },
            {
              key: "current",
              function_code: 3,
              address: 2,
              data_type: "f32",
              byte_order: "abcd",
              description: "Phase current (A)",
            },
            {
              key: "energy_kwh",
              function_code: 3,
              address: 4,
              data_type: "u32",
              byte_order: "cdab",
              scale: 0.01,
              offset: 0,
            },
            { key: "serial", function_code: 4, address: 100, data_type: "string", byte_order: "abcd", length: 8 },
            { key: "running", function_code: 1, address: 0, data_type: "bool" },
          ],
        },
      },
    ]);
    expect(server.models.get(model.model_id)?.revision).toBe(2);
    // The saved page shows the new revision, and a second save names it.
    await waitFor(() => expect(screen.getByText(/Revision 2 · /)).toBeDefined());
    await act(async () => {
      edit("Key of data point 1", "voltage_avg");
    });
    await save();
    await screen.findByText("Saved. The model is at revision 3.");
    expect(lastSave(server).revision).toBe(2);
  });

  test("changing the data type changes what applies: a string needs a length, a bit has no scaling or byte order", async () => {
    const { server, model } = tenant();
    await openModel(server, model.model_id);
    await act(async () => {
      edit("Data type of data point 2", "string");
    });
    expect(field("Length of data point 2")).toBeDefined();
    expect(screen.queryByLabelText("Scale of data point 2")).toBeNull();
    const orders = within(screen.getByLabelText("Byte order of data point 2"))
      .getAllByRole("option")
      .map((option) => option.textContent);
    expect(orders).toEqual(["abcd", "badc"]);
    // Function code 2 reads bits: the data type becomes bool, the only one it offers.
    await act(async () => {
      edit("Function code of data point 1", "2");
    });
    expect((screen.getByLabelText("Data type of data point 1") as HTMLSelectElement).value).toBe("bool");
    expect(
      within(screen.getByLabelText("Data type of data point 1"))
        .getAllByRole("option")
        .map((option) => option.textContent),
    ).toEqual(["bool"]);
    expect(screen.queryByLabelText("Byte order of data point 1")).toBeNull();
    await act(async () => {
      edit("Length of data point 2", "4");
    });
    await save();
    await screen.findByText("Saved. The model is at revision 2.");
    const points = lastSave(server).points;
    expect(points[0]).toEqual({
      key: "voltage",
      function_code: 2,
      address: 0,
      data_type: "bool",
      description: "Average phase voltage (V)",
    });
    expect(points[1]).toEqual({
      key: "current",
      function_code: 3,
      address: 2,
      data_type: "string",
      byte_order: "abcd",
      length: 4,
    });
  });

  test("data points move up and down and are deleted; the order is what is saved", async () => {
    const { server, model } = tenant();
    await openModel(server, model.model_id);
    expect((screen.getByRole("button", { name: "Move data point 1 up" }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "Move data point 5 down" }) as HTMLButtonElement).disabled).toBe(true);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Move data point 2 up" }));
    });
    expect(field("Key of data point 1").value).toBe("current");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Delete data point 5 running" }));
    });
    expect(screen.queryByLabelText("Key of data point 5")).toBeNull();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Add a data point" }));
    });
    expect(field("Key of data point 5").value).toBe("");
    await act(async () => {
      edit("Key of data point 5", "frequency");
      edit("Address of data point 5", "6");
    });
    await save();
    await screen.findByText("Saved. The model is at revision 2.");
    const keys = lastSave(server).points.map((point) => point.key);
    expect(keys).toEqual(["current", "voltage", "energy_kwh", "serial", "frequency"]);
  });

  test("problems the page can see are listed by data point and marked on the row; nothing is sent", async () => {
    const { server, model } = tenant();
    await openModel(server, model.model_id);
    await act(async () => {
      edit("Key of data point 2", "voltage");
      edit("Length of data point 4", "");
      edit("Address of data point 5", "70000");
    });
    await save();
    const summary = await screen.findByRole("alert");
    expect(document.activeElement).toBe(summary);
    expect(summary.textContent).toContain("3 things need fixing");
    expect(summary.textContent).toContain(
      "Data point 2 (voltage): Another data point in this device model already uses this key.",
    );
    expect(summary.textContent).toContain("Data point 4 (serial): A string data point needs a length.");
    expect(summary.textContent).toContain("Data point 5 (running): Enter a whole number from 0 to 65,535.");
    expect(field("Key of data point 2").getAttribute("aria-invalid")).toBe("true");
    expect(field("Key of data point 1").getAttribute("aria-invalid")).toBeNull();
    expect(saves(server)).toEqual([]);
    // A line of the summary focuses its input; correcting the input clears its problem.
    await act(async () => {
      fireEvent.click(within(summary).getByRole("button", { name: /^Data point 4/ }));
    });
    expect(document.activeElement).toBe(field("Length of data point 4"));
    await act(async () => {
      edit("Length of data point 4", "8");
    });
    expect(field("Length of data point 4").getAttribute("aria-invalid")).toBeNull();
  });

  test("the service's errors name a data point and a field: the row is marked and the line says which", async () => {
    const { server, model } = tenant();
    server.assets.failNext(
      "updateModel",
      new Response(
        JSON.stringify({
          code: "validation_failed",
          message_key: "error.validation_failed",
          field_errors: [
            { field: "/points/2/address", message_key: "device_model.points_overlap" },
            { field: "/points/3/length", message_key: "device_model.length_required" },
          ],
        }),
        { status: 422, headers: { "content-type": "application/json" } },
      ),
    );
    await openModel(server, model.model_id);
    await save();
    const summary = await screen.findByRole("alert");
    expect(summary.textContent).toContain("2 things need fixing");
    expect(summary.textContent).toContain(
      "Data point 3 (energy_kwh): This data point reads addresses that an earlier data point with the same function code already reads.",
    );
    expect(summary.textContent).toContain("Data point 4 (serial): A string data point needs a length.");
    expect(field("Address of data point 3").getAttribute("aria-invalid")).toBe("true");
    expect(field("Length of data point 4").getAttribute("aria-invalid")).toBe("true");
    const row = field("Address of data point 3").closest("tr");
    expect(row?.getAttribute("aria-invalid")).toBe("true");
    expect(row?.textContent).toContain("reads addresses that an earlier data point");
    // The rows without a problem are not marked, and the form keeps every value.
    expect(field("Address of data point 1").getAttribute("aria-invalid")).toBeNull();
    expect(field("Key of data point 3").value).toBe("energy_kwh");
    expect(server.models.get(model.model_id)?.revision).toBe(1);
  });

  test("a model someone else changed asks to reload, and a reload brings the latest data points", async () => {
    const { server, model } = tenant();
    await openModel(server, model.model_id);
    await act(async () => {
      edit("Key of data point 1", "my_voltage");
    });
    server.assets.bumpModel(model.model_id, [{ key: "frequency", function_code: 3, address: 9, data_type: "u16" }]);
    await save();
    expect(await screen.findByText("Someone else changed this model (it is now at revision 2)")).toBeDefined();
    expect(field("Key of data point 1").value).toBe("my_voltage");
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Reload the latest version" }));
    });
    await waitFor(() => expect(field("Key of data point 1").value).toBe("frequency"));
    expect(screen.queryByLabelText("Key of data point 2")).toBeNull();
    expect(screen.queryByText(/Someone else changed this model/)).toBeNull();
    await act(async () => {
      edit("Address of data point 1", "10");
    });
    await save();
    await screen.findByText("Saved. The model is at revision 3.");
    expect(lastSave(server).revision).toBe(2);
  });

  test("a viewer sees the same page read-only, and can export", async () => {
    const { server, model } = tenant();
    const downloads = captureDownloads();
    try {
      await openModel(server, model.model_id, "viewer");
      expect(screen.getByText(/Your role is viewer: you can look at and export models/)).toBeDefined();
      expect(field("Key of data point 1").readOnly).toBe(true);
      expect((screen.getByLabelText("Function code of data point 1") as HTMLSelectElement).disabled).toBe(true);
      for (const name of ["Add a data point", "Save the model", "Delete the model"]) {
        expect(screen.queryByRole("button", { name })).toBeNull();
      }
      expect(screen.queryByRole("button", { name: /^Delete data point/ })).toBeNull();
      await act(async () => {
        fireEvent.click(screen.getByRole("button", { name: "Export JSON" }));
      });
      await waitFor(() => expect(downloads.files).toHaveLength(1));
      expect(downloads.files[0]?.name).toBe("power-meter.json");
      expect(JSON.parse(await (downloads.files[0]?.text ?? Promise.resolve(""))).name).toBe("power-meter");
    } finally {
      downloads.restore();
    }
  });

  test("an unknown model says so", async () => {
    const { server } = tenant();
    open(server, modelPath("00000000-0000-4000-8000-0000000000ff"));
    expect(await screen.findByRole("heading", { level: 1, name: "Model not found" })).toBeDefined();
    cleanup();
    open(server, "/models/not-an-id");
    expect(await screen.findByRole("heading", { level: 1, name: "Model not found" })).toBeDefined();
  });

  test("deleting a model devices use is refused in the dialog; deleting an unused one leaves for the list", async () => {
    const { server, model } = tenant();
    await openModel(server, model.model_id);
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Delete the model" }));
    });
    let dialog = await screen.findByRole("alertdialog");
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Delete the model" }));
    });
    expect(await within(dialog).findByText("It can't be deleted")).toBeDefined();
    cleanup();

    const spare = server.assets.addModelWithPoints("spare", []);
    const { router } = open(server, modelPath(spare.model_id));
    await screen.findByRole("heading", { level: 1, name: "spare" });
    expect(screen.getByText("This model has no data points yet.")).toBeDefined();
    expect(screen.getByText("No device uses this model yet, so saving only updates the model.")).toBeDefined();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Delete the model" }));
    });
    dialog = await screen.findByRole("alertdialog");
    await act(async () => {
      fireEvent.click(within(dialog).getByRole("button", { name: "Delete the model" }));
    });
    await waitFor(() => expect(router.state.location.pathname).toBe(MODELS_PATH));
    expect(await screen.findByText("The model spare was deleted.")).toBeDefined();
  });

  test("on a phone each data point is a card; a card with a problem is open", async () => {
    happyDOM.setViewport({ width: 390, height: 844 });
    const { server, model } = tenant();
    open(server, modelPath(model.model_id));
    await screen.findByRole("heading", { level: 1, name: "power-meter" });
    await screen.findByText("1. voltage · FC3 · address 0 · f32");
    expect(screen.queryByRole("table")).toBeNull();
    const cards = document.querySelectorAll("details");
    expect(cards).toHaveLength(5);
    expect([...cards].every((card) => !card.open)).toBe(true);
    await act(async () => {
      cards[1]?.setAttribute("open", "");
      edit("Key of data point 2", "voltage");
    });
    await save();
    await screen.findByRole("alert");
    const open2 = [...document.querySelectorAll("details")].filter((card) => card.open);
    expect(open2.map((card) => card.querySelector("summary")?.textContent)).toEqual([
      "2. voltage · FC3 · address 2 · f32",
    ]);
  });
});

describe("a new model", () => {
  test("is created with the name and data points, and the list shows it with a notice", async () => {
    const server = new FakeDeviceServer();
    const { router } = open(server, modelsNewPath());
    await screen.findByRole("heading", { level: 1, name: "New device model" });
    await act(async () => {
      fireEvent.change(screen.getByLabelText("Name"), { target: { value: "th-transmitter" } });
      fireEvent.click(screen.getByRole("button", { name: "Add a data point" }));
    });
    await act(async () => {
      edit("Key of data point 1", "temperature");
      edit("Data type of data point 1", "i16");
      edit("Scale of data point 1", "0.1");
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Save the model" }));
    });
    await waitFor(() => expect(router.state.location.pathname).toBe(MODELS_PATH));
    expect(server.assets.writes.filter((write) => write.operation === "createModel")[0]?.body).toEqual({
      name: "th-transmitter",
      points: [{ key: "temperature", function_code: 3, address: 0, data_type: "i16", byte_order: "abcd", scale: 0.1 }],
    });
    expect(await screen.findByText("The model th-transmitter was created.")).toBeDefined();
    expect((await screen.findAllByRole("link", { name: "th-transmitter" })).length).toBeGreaterThan(0);
  });

  test("a bad or taken name shows under the name and nothing is lost", async () => {
    const { server } = tenant();
    open(server, modelsNewPath());
    await screen.findByRole("heading", { level: 1, name: "New device model" });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Save the model" }));
    });
    expect(screen.getByLabelText("Name").getAttribute("aria-invalid")).toBe("true");
    expect(screen.getAllByText(/This field is required/).length).toBeGreaterThan(0);
    await act(async () => {
      fireEvent.change(screen.getByLabelText("Name"), { target: { value: "Power Meter" } });
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Save the model" }));
    });
    expect(screen.getAllByText(/Use 1 to 40 lowercase letters or digits/).length).toBeGreaterThan(0);
    await act(async () => {
      fireEvent.change(screen.getByLabelText("Name"), { target: { value: "power-meter" } });
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Save the model" }));
    });
    await waitFor(() =>
      expect(
        screen.getAllByText("Your organization already has a device model with this name.").length,
      ).toBeGreaterThan(0),
    );
    expect(screen.getByLabelText("Name").getAttribute("aria-invalid")).toBe("true");
  });

  test("a viewer is told why they cannot add a model", async () => {
    const { server } = tenant();
    open(server, modelsNewPath(), "viewer");
    expect(await screen.findByRole("heading", { level: 1, name: "You can't add models" })).toBeDefined();
    expect(screen.getByRole("link", { name: "Back to the models" }).getAttribute("href")).toBe(MODELS_PATH);
  });
});
