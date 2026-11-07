import { describe, expect, test } from "bun:test";

import { SPECS } from "../../dashboard/fixtures.ts";
import type { MessageId } from "../../i18n/messages.ts";
import { type MessageValues, translate } from "../../i18n/translate.ts";
import {
  analyzeStretch,
  compareLastWeek,
  componentQuestions,
  explainSource,
  instantText,
  wallText,
  weekRanges,
  whyNoData,
  whyOffline,
} from "./questions.ts";

const en = (id: MessageId, values?: MessageValues) => translate("en", id, values);
const zh = (id: MessageId, values?: MessageValues) => translate("zh-TW", id, values);

describe("weekRanges", () => {
  test("a Sunday ends its own week: this week is Monday to that Sunday, last week the one before", () => {
    // 2026-11-01 is the design's Sunday (docs/design/home-ai/design.md: last week is 10/19 to 10/25).
    expect(weekRanges(Date.UTC(2026, 10, 1, 10, 0), "UTC")).toEqual({
      thisWeek: { from: "2026-10-26", to: "2026-11-01" },
      lastWeek: { from: "2026-10-19", to: "2026-10-25" },
    });
  });

  test("on a Monday this week is that one day", () => {
    expect(weekRanges(Date.UTC(2026, 10, 2, 0, 5), "UTC")).toEqual({
      thisWeek: { from: "2026-11-02", to: "2026-11-02" },
      lastWeek: { from: "2026-10-26", to: "2026-11-01" },
    });
  });

  test("the day is the one in the user's time zone, not in UTC", () => {
    // 2026-11-01 17:00 UTC is already Monday 2026-11-02 in Taipei (UTC+8) and still Sunday in Los Angeles.
    const instant = Date.UTC(2026, 10, 1, 17, 0);
    expect(weekRanges(instant, "Asia/Taipei").thisWeek).toEqual({ from: "2026-11-02", to: "2026-11-02" });
    expect(weekRanges(instant, "America/Los_Angeles").thisWeek).toEqual({ from: "2026-10-26", to: "2026-11-01" });
  });

  test("a week that crosses a month and a year", () => {
    expect(weekRanges(Date.UTC(2027, 0, 2, 12, 0), "UTC")).toEqual({
      thisWeek: { from: "2026-12-28", to: "2027-01-02" },
      lastWeek: { from: "2026-12-21", to: "2026-12-27" },
    });
  });
});

describe("the times in a question", () => {
  test("an instant is written on the wall clock of the zone", () => {
    const instant = Date.UTC(2026, 10, 1, 9, 2);
    expect(instantText(instant, "UTC")).toBe("2026-11-01 09:02");
    expect(instantText(instant, "Asia/Taipei")).toBe("2026-11-01 17:02");
  });

  test("midnight is 00:00, not 24:00", () => {
    expect(instantText(Date.UTC(2026, 10, 1, 16, 0), "Asia/Taipei")).toBe("2026-11-02 00:00");
  });

  test("a chart's wall-clock x is read as its own fields; whole days drop the time", () => {
    const wall = Date.UTC(2026, 10, 1, 15, 10);
    expect(wallText(wall)).toBe("2026-11-01 15:10");
    expect(wallText(wall, true)).toBe("2026-11-01");
  });
});

describe("the device's questions", () => {
  const now = Date.UTC(2026, 10, 1, 10, 0);

  test("compare with last week names the device, both weeks and the zone", () => {
    expect(compareLastWeek(en, { name: "Compressor A2", now, timeZone: "UTC" })).toEqual({
      label: "Compare with last week",
      text: "Compare with last week: how do Compressor A2's energy use, running time and anomalies this week (2026-10-26 – 2026-11-01) differ from last week (2026-10-19 – 2026-10-25)? Dates are in UTC.",
    });
    expect(compareLastWeek(zh, { name: "空壓機 A2", now, timeZone: "Asia/Taipei" }).text).toBe(
      "跟上週比：空壓機 A2 本週（2026-10-26 – 2026-11-01）的用電、運轉時間與異常，跟上週（2026-10-19 – 2026-10-25）有什麼不同？日期以 Asia/Taipei 計。",
    );
  });

  test("why offline names the device and when its last reading was", () => {
    expect(
      whyOffline(en, { name: "Compressor A2", lastSeen: Date.UTC(2026, 10, 1, 9, 2), timeZone: "Asia/Taipei" }),
    ).toEqual({
      label: "Why is it offline?",
      text: "Why is it offline: Compressor A2 has had no new readings since 2026-11-01 17:02 (Asia/Taipei). What could be the cause?",
    });
  });

  test("why no data names the device", () => {
    expect(whyNoData(en, { name: "Compressor A2" }).text).toBe(
      "Why is there no data: Compressor A2 has no readings yet. What could be the cause?",
    );
  });

  test("a question is the same every time it is made", () => {
    const make = () => compareLastWeek(en, { name: "Boiler B1", now, timeZone: "UTC" });
    expect(make()).toEqual(make());
  });

  test("a stretch of a device's data point, and of a component, with its edges and zone", () => {
    const edges = { from: "2026-11-01 15:10", to: "2026-11-01 16:20", timeZone: "UTC" };
    expect(
      analyzeStretch(en, { subject: { kind: "point", device: "Boiler B1", point: "Exhaust temperature" }, ...edges }),
    ).toEqual({
      label: "Analyze this stretch",
      text: "Analyze this stretch: what happened to Boiler B1's “Exhaust temperature” from 2026-11-01 15:10 to 2026-11-01 16:20 (UTC)? Was there an anomaly or anything else worth noting, and what caused it?",
    });
    expect(analyzeStretch(en, { subject: { kind: "component", title: "Power by line" }, ...edges }).text).toContain(
      "what happened to “Power by line” from 2026-11-01 15:10",
    );
  });
});

describe("「Ask AI to explain」 in a source panel", () => {
  test("names the dashboard and what the panel shows, in both languages", () => {
    const source = { dashboard: "Energy overview", title: "Line power" };
    expect(explainSource(en, source)).toEqual({
      label: "Ask AI to explain",
      text: "Explain where “Line power” on the dashboard “Energy overview” comes from: which data it reads, how it is queried and how its numbers are worked out.",
    });
    expect(explainSource(zh, source)).toEqual({
      label: "請 AI 解釋",
      text: "請解釋儀表板「Energy overview」裡「Line power」的資料來源：它讀了哪些資料、怎麼查的、數字是怎麼得來的？",
    });
  });
});

describe("a component's questions", () => {
  test("each kind offers two or three, each naming the component by its title", () => {
    // One spec of each of the five kinds a card is drawn for.
    const specs = [SPECS.timeSeries, SPECS.bar, SPECS.kpi, SPECS.table, SPECS.status];
    expect(specs.map((spec) => [spec.kind, componentQuestions(en, spec).length])).toEqual([
      ["time_series", 3],
      ["bar", 3],
      ["kpi", 2],
      ["table", 2],
      ["status", 2],
    ]);
    for (const spec of specs) {
      for (const question of componentQuestions(en, spec)) {
        expect(question.text).toContain(`“${spec.title}”`);
        expect(question.label).not.toBe("");
      }
    }
  });

  test("a kind with no template gets no questions, not an error", () => {
    // `custom_page` is not a platform component, so it never reaches a card; if one did, the row is empty.
    expect(componentQuestions(en, { kind: "custom_page" } as never)).toEqual([]);
  });

  test("the labels of one component differ, so each button is told apart", () => {
    const labels = componentQuestions(en, SPECS.timeSeries).map((question) => question.label);
    expect(new Set(labels).size).toBe(labels.length);
  });
});
