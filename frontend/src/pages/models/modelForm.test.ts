import { describe, expect, test } from "bun:test";

import { blankPoint, parseImportFile, pointDraft, pointOf } from "./modelForm.ts";

// The model editor has no input for a data point's unit yet (Laysi/trustiot-v2#126), and an
// update replaces every point: units must survive an edit and an import untouched
// (Laysi/trustiot-v2#982).
describe("units in the model editor", () => {
  test("an edited point keeps the unit it was read with, and a point without one gets none", () => {
    const draft = pointDraft(
      { key: "temperature", function_code: 3, address: 0, data_type: "i16", scale: 0.1, unit: "°C" },
      1,
    );
    expect(pointOf({ ...draft, description: "Air temperature" })).toEqual({
      key: "temperature",
      function_code: 3,
      address: 0,
      data_type: "i16",
      byte_order: "abcd",
      scale: 0.1,
      description: "Air temperature",
      unit: "°C",
    });
    const plain = pointOf(pointDraft({ key: "uptime", function_code: 4, address: 0, data_type: "u32" }, 2));
    expect("unit" in plain).toBe(false);
    expect("unit" in pointOf({ ...blankPoint(3), key: "new" })).toBe(false);
  });

  test("an imported file keeps its units, and a file from before units imports as it did", () => {
    const file = (points: unknown[]) =>
      JSON.stringify({ format: "trustiot.device_model", format_version: 1, name: "transmitter", points });
    const point = { key: "humidity", function_code: 3, address: 1, data_type: "u16" } as const;
    const withUnit = parseImportFile(file([{ ...point, unit: "%RH" }]));
    expect(withUnit.ok && withUnit.file.points[0]?.unit).toBe("%RH");
    const before = parseImportFile(file([point]));
    expect(before.ok && before.file.points[0]).toEqual(point);
  });
});
