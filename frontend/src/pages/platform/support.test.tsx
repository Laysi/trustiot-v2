import { afterEach, describe, expect, test } from "bun:test";
import { cleanup, render } from "@testing-library/react";

import type { LoggedEvent } from "../../api/pending/platform.ts";
import { translate } from "../../i18n/translate.ts";
import { describeEvent } from "./audit/eventSummary.ts";
import { isFinal, isFull } from "./format.ts";
import { typedMatches } from "./ui/confirm-by-typing.tsx";
import { Meter } from "./ui/meter.tsx";
import { formatWait } from "./Waiting.tsx";

afterEach(() => {
  cleanup();
});

const t = (id: Parameters<typeof translate>[1], values?: Parameters<typeof translate>[2]) =>
  translate("en", id, values);

function event(type: string, payload: unknown): LoggedEvent {
  return {
    eventId: "e",
    type,
    version: 1,
    producer: "x",
    tenantId: null,
    aggregateType: "a",
    aggregateId: "b",
    aggregateSeq: 1,
    occurredAt: "2026-11-01T10:00:00Z",
    recordedAt: "2026-11-01T10:00:01Z",
    actor: null,
    payload,
  };
}

describe("event summaries", () => {
  const nameOf = (id: string) => (id === "u1" ? "pm@example.test" : id);

  test("a type that is a property name of every object is an unknown type, not a crash", () => {
    for (const type of ["constructor", "toString", "__proto__", "hasOwnProperty"]) {
      const described = describeEvent(event(type, {}), t, nameOf);
      expect(described.known).toBe(false);
      expect(described.title).toBe(type);
    }
  });

  test("a known type has a name and a one-line summary read from the payload", () => {
    expect(
      describeEvent(
        event("identity.member_role_changed", { user_id: "u1", previous_role: "viewer", role: "admin" }),
        t,
        nameOf,
      ),
    ).toEqual({
      known: true,
      title: "Member role",
      summary: "pm@example.test viewer → admin",
    });
    expect(
      describeEvent(
        event("identity.member_role_changed", { user_id: "u1", previous_role: null, role: "editor" }),
        t,
        nameOf,
      ).summary,
    ).toBe("pm@example.test joined as editor");
    expect(
      describeEvent(
        event("identity.tenant_state_changed", { state: "active", reason: null, state_version: 3 }),
        t,
        nameOf,
      ).summary,
    ).toBe("Resumed, version 3");
  });

  test("a payload that lacks what the summary reads never fails the row", () => {
    expect(describeEvent(event("identity.tenant_state_changed", "not an object"), t, nameOf).summary).toBe(
      "Suspended (—), version —",
    );
    expect(describeEvent(event("identity.quota_changed", null), t, nameOf).summary).toBe("");
  });

  test("an unknown type is listed by its code", () => {
    expect(describeEvent(event("assets.something_new", {}), t, nameOf)).toEqual({
      known: false,
      title: "assets.something_new",
      summary: "Open it to read the payload as written.",
    });
  });
});

describe("small pieces", () => {
  test("the waiting time counts seconds, then minutes", () => {
    expect(formatWait(14_000, t)).toBe("14 s");
    expect(formatWait(134_000, t)).toBe("2 min 14 s");
    expect(formatWait(-5, t)).toBe("0 s");
  });

  test("a suspension is final once the contract ended or the tenant is deleted; a limit is full at its number", () => {
    expect(isFinal({ state: "suspended", reason: "contract_terminated" })).toBe(true);
    expect(isFinal({ state: "deleted", reason: "contract_terminated" })).toBe(true);
    expect(isFinal({ state: "suspended", reason: "non_payment" })).toBe(false);
    expect(isFull(5, 5)).toBe(true);
    expect(isFull(5, "unlimited")).toBe(false);
    expect(isFull(0, 0)).toBe(true);
  });

  test("the meter is amber from 80%, red past the limit, and full for a limit of 0", () => {
    const tone = (value: number, max: number) => {
      const { container } = render(<Meter value={value} max={max} label="x" />);
      const tone = container.querySelector("progress")?.getAttribute("data-tone");
      cleanup();
      return tone;
    };
    expect(tone(3, 5)).toBe("primary");
    expect(tone(4, 5)).toBe("warning");
    expect(tone(6, 5)).toBe("destructive");
    expect(tone(0, 0)).toBe("warning");
  });

  test("a typed confirmation matches exactly, spaces around it aside", () => {
    expect(typedMatches(" Acme ", "Acme")).toBe(true);
    expect(typedMatches("acme", "Acme")).toBe(false);
    expect(typedMatches("", "Acme")).toBe(false);
  });
});
