import { describe, expect, test } from "bun:test";

import { startSettings } from "./start-settings.ts";

describe("the settings a conversation starts with", () => {
  test("carry the AI language preference and the resolved time zone", () => {
    expect(startSettings("zh-TW", "Asia/Taipei")).toEqual({ ai_language: "zh-TW", time_zone: "Asia/Taipei" });
    expect(startSettings("auto", "UTC")).toEqual({ ai_language: "auto", time_zone: "UTC" });
    expect(startSettings("en", "America/Argentina/Buenos_Aires")).toEqual({
      ai_language: "en",
      time_zone: "America/Argentina/Buenos_Aires",
    });
    expect(startSettings("en", "Etc/GMT+8")).toEqual({ ai_language: "en", time_zone: "Etc/GMT+8" });
  });

  test("leave out a zone the assistant would refuse, so the start does not fail on it", () => {
    for (const zone of ["", "Asia/", "/Asia", "Asia/Tai pei", "1Asia/Taipei", `Area/${"a".repeat(60)}`]) {
      expect(startSettings("en", zone)).toEqual({ ai_language: "en" });
    }
  });
});
