import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, render, screen } from "@testing-library/react";

import { ComponentExtrasProvider, useComponentExtras } from "./extensions.tsx";

afterEach(() => {
  cleanup();
});

function Slots() {
  const { deviceName, explainSource } = useComponentExtras();
  return (
    <p>
      {deviceName ? `name:${deviceName("d1")}` : "no name"} {explainSource ? "explain" : "no explain"}
    </p>
  );
}

const NAMES = { deviceName: (id: string) => `Device ${id}` };
const EXPLAIN = { explainSource: () => null };

describe("ComponentExtrasProvider", () => {
  test("without a provider no slot is set", () => {
    render(<Slots />);
    expect(screen.getByText("no name no explain")).toBeTruthy();
  });

  test("a provider inside another adds its slots to the outer one's, and wins for a slot both set", () => {
    render(
      <ComponentExtrasProvider extras={NAMES}>
        <ComponentExtrasProvider extras={EXPLAIN}>
          <Slots />
        </ComponentExtrasProvider>
      </ComponentExtrasProvider>,
    );
    expect(screen.getByText("name:Device d1 explain")).toBeTruthy();
    cleanup();
    render(
      <ComponentExtrasProvider extras={NAMES}>
        <ComponentExtrasProvider extras={{ deviceName: () => "inner" }}>
          <Slots />
        </ComponentExtrasProvider>
      </ComponentExtrasProvider>,
    );
    expect(screen.getByText("name:inner no explain")).toBeTruthy();
  });
});
