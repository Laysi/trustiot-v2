import { useMemo } from "react";

import type { StartSettings } from "../api/conversations.ts";
import type { LanguagePreference } from "./preferences.ts";
import { usePreferences } from "./PreferencesProvider.tsx";

/** The assistant's form of a time zone name (`CreateConversation.time_zone`): at most 64 bytes. */
const MAX_TIME_ZONE_LENGTH = 64;
const TIME_ZONE_FORM = /^[A-Za-z][A-Za-z0-9_+-]*(\/[A-Za-z][A-Za-z0-9_+-]*)*$/;

/**
 * The settings a new conversation starts with, from the user's AI language preference and the
 * time zone dates are shown in (`auto` already resolved to the browser's zone, which only the
 * browser can do). A zone the assistant would refuse (no browser names one so, but a refused
 * zone would fail the whole start) is left out: the conversation then starts without one.
 */
export function startSettings(aiLanguage: LanguagePreference, timeZone: string): StartSettings {
  const zoneFits = timeZone.length <= MAX_TIME_ZONE_LENGTH && TIME_ZONE_FORM.test(timeZone);
  return { ai_language: aiLanguage, ...(zoneFits ? { time_zone: timeZone } : {}) };
}

/** `startSettings` for the signed-in user's preferences, the same object while they do not change. */
export function useStartSettings(): StartSettings {
  const { preferences, timeZone } = usePreferences();
  const aiLanguage = preferences.aiLanguage;
  return useMemo(() => startSettings(aiLanguage, timeZone), [aiLanguage, timeZone]);
}
