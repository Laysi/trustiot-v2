import { afterEach, describe, expect, test } from "bun:test";
import { act, cleanup, fireEvent, screen, waitFor, within } from "@testing-library/react";

import { MODELS_PATH, modelsImportPath } from "../../app/paths.ts";
import { FakeDeviceServer } from "../../test/fake-devices.ts";
import { renderAt, signedInBrowser } from "../../test/render.tsx";
import { MAX_IMPORT_BYTES } from "./modelForm.ts";

afterEach(() => {
  cleanup();
});

const DOCUMENT = {
  format: "trustiot.device_model",
  format_version: 1,
  name: "power-meter",
  points: [
    { key: "voltage", function_code: 3, address: 0, data_type: "f32", byte_order: "abcd" },
    { key: "running", function_code: 1, address: 0, data_type: "bool" },
  ],
};

function open(server: FakeDeviceServer, path = modelsImportPath(), role: "viewer" | "editor" | "admin" = "admin") {
  return renderAt(path, {
    devices: server.api(),
    assets: server.assetsApi(),
    identity: signedInBrowser({ role }).browser,
  });
}

async function choose(text: string, name = "power-meter.json") {
  const input = (await screen.findByLabelText("Model file")) as HTMLInputElement;
  const file = new File([text], name, { type: "application/json" });
  await act(async () => {
    fireEvent.change(input, { target: { files: [file] } });
  });
}

async function dialog() {
  return await screen.findByRole("alertdialog");
}

describe("importing a model", () => {
  test("`?import` opens the dialog; a file is checked and previewed, then imported as a new model", async () => {
    const server = new FakeDeviceServer();
    const { router } = open(server);
    const box = await dialog();
    expect(within(box).getByText("Import a device model")).toBeDefined();
    expect((within(box).getByRole("button", { name: "Import" }) as HTMLButtonElement).disabled).toBe(true);
    await choose(JSON.stringify(DOCUMENT));
    expect(await within(box).findByText(/^power-meter\.json · /)).toBeDefined();
    const preview = within(box).getByText("Model name").closest("dl") as HTMLElement;
    expect(preview.textContent).toBe("Model namepower-meterData points2Format version1");
    expect((within(box).getByLabelText("Name after the import") as HTMLInputElement).value).toBe("power-meter");

    await act(async () => {
      fireEvent.click(within(box).getByRole("button", { name: "Import" }));
    });
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/models\/[0-9a-f-]{36}$/));
    expect(server.assets.writes.find((write) => write.operation === "importModel")?.body).toEqual(DOCUMENT);
    expect([...server.models.values()].map((model) => model.name)).toEqual(["power-meter"]);
    expect(await screen.findByText("The model power-meter was imported.")).toBeDefined();
    expect(await screen.findByLabelText("Key of data point 2")).toBeDefined();
  });

  test("a name the organization already has can be changed in the dialog, then the import goes through", async () => {
    const server = new FakeDeviceServer();
    server.addModel("power-meter");
    const { router } = open(server);
    const box = await dialog();
    await choose(JSON.stringify(DOCUMENT));
    await act(async () => {
      fireEvent.click(within(box).getByRole("button", { name: "Import" }));
    });
    const name = within(box).getByLabelText("Name after the import");
    await waitFor(() => expect(name.getAttribute("aria-invalid")).toBe("true"));
    expect(within(box).getByText("Your organization already has a device model with this name.")).toBeDefined();
    expect(server.models.size).toBe(1);
    await act(async () => {
      fireEvent.change(name, { target: { value: "power-meter-v2" } });
    });
    expect(name.getAttribute("aria-invalid")).toBeNull();
    await act(async () => {
      fireEvent.click(within(box).getByRole("button", { name: "Import" }));
    });
    await waitFor(() => expect(router.state.location.pathname).toMatch(/^\/models\//));
    expect(server.assets.writes.at(-1)?.body).toMatchObject({ name: "power-meter-v2" });
    expect([...server.models.values()].map((model) => model.name).sort()).toEqual(["power-meter", "power-meter-v2"]);
  });

  test("what the service refuses is listed line by line, in its own words", async () => {
    const server = new FakeDeviceServer();
    server.assets.failNext(
      "importModel",
      new Response(
        JSON.stringify({
          code: "validation_failed",
          message_key: "error.validation_failed",
          field_errors: [
            { field: "/points/1/address", message_key: "device_model.points_overlap" },
            { field: "/points/0/key", message_key: "device_model.point_key_invalid", details: { max: 64 } },
          ],
        }),
        { status: 422, headers: { "content-type": "application/json" } },
      ),
    );
    open(server);
    const box = await dialog();
    await choose(JSON.stringify(DOCUMENT));
    const importButton = await within(box).findByRole("button", { name: "Import" });
    await act(async () => {
      fireEvent.click(importButton);
    });
    expect(await within(box).findByText("The service refused the file (2)")).toBeDefined();
    const lines = within(box)
      .getAllByRole("listitem")
      .map((item) => item.textContent);
    expect(lines).toEqual([
      "Data point 2: This data point reads addresses that an earlier data point with the same function code already reads.",
      "Data point 1: Use 1 to 64 ASCII letters, digits, periods, underscores or hyphens, starting with a letter or digit.",
    ]);
    expect(server.models.size).toBe(0);
  });

  test.each([
    ["a file that is not JSON", "not json at all", "This file isn't valid JSON."],
    [
      "JSON that is not an exported model",
      JSON.stringify({ hello: "world" }),
      "This file isn't a device model exported from TrustIoT.",
    ],
    [
      "a format version this release does not read",
      JSON.stringify({ ...DOCUMENT, format_version: 2 }),
      "This device model file's format version isn't supported.",
    ],
    [
      "data points of the wrong shape",
      JSON.stringify({ ...DOCUMENT, points: [{ key: 1 }] }),
      "This file's name or data points don't have the shape of an exported model.",
    ],
  ])("%s is refused in the browser before anything is sent", async (_label, text, message) => {
    const server = new FakeDeviceServer();
    open(server);
    const box = await dialog();
    await choose(text);
    expect((await within(box).findByRole("alert")).textContent).toBe(message);
    expect((within(box).getByRole("button", { name: "Import" }) as HTMLButtonElement).disabled).toBe(true);
    expect(server.assets.writes).toEqual([]);
  });

  test("a file far larger than a model is refused without being read", async () => {
    const server = new FakeDeviceServer();
    open(server);
    const box = await dialog();
    await choose("x".repeat(MAX_IMPORT_BYTES + 1));
    expect((await within(box).findByRole("alert")).textContent).toBe(
      "This file is larger than 1 MB, which is far more than a model needs.",
    );
  });

  test("the Import JSON button opens it and Cancel closes it, leaving the list", async () => {
    const server = new FakeDeviceServer();
    server.addModel("power-meter");
    const { router } = open(server, MODELS_PATH);
    // Found before the act: React does not render the page while an act scope waits for it.
    const importButton = await screen.findByRole("button", { name: "Import JSON" });
    await act(async () => {
      fireEvent.click(importButton);
    });
    const box = await dialog();
    expect(router.state.location.search).toBe("?import=");
    await act(async () => {
      fireEvent.click(within(box).getByRole("button", { name: "Cancel" }));
    });
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(router.state.location.search).toBe("");
  });
});
