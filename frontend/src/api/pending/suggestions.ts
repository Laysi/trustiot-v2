// Pending Laysi/trustiot-v2#158: replace with a module over the generated client when api/openapi.json has these endpoints.
import { createContext, createElement, type ReactNode, useContext } from "react";

/**
 * The questions the conversation home suggests on an empty conversation
 * (docs/design/home-ai/design.md, 「建議問題」). No endpoint serves them yet; this is the interface
 * the home page is built against, from that section and the issue:
 *
 * - The server fills the question templates (the same set as the composer's, J11) with this
 *   tenant's devices and a time range, and returns the question as text plus the parameters it
 *   was filled with. The text is what is sent when a suggestion is pressed: the page never
 *   composes a question from the parameters, and a template and its blanks never come from a
 *   model.
 * - Only this tenant's devices appear (the issue's acceptance criterion). The tenant is the
 *   session's, as for every tenant endpoint; the interface names it so the in-memory fake, which
 *   has no session, can answer for it, and the module over the generated client drops it.
 * - A tenant without a reading yet has no devices worth asking about: `has_data` is false and the
 *   page explains instead of suggesting. The server may still return questions that need no data
 *   (decision 6: product questions, when the assistant can answer them); the page shows what it
 *   gets, at most `MAX_SUGGESTIONS`.
 *
 * A failure is an `ApiError` (api/errors.ts); the page shows one calm line with a retry, and the
 * composer is not affected.
 */

/** What a suggestion is about; the page draws an icon for it. */
export const SUGGESTION_CATEGORIES = ["energy", "offline", "anomaly", "compare", "trend"] as const;
export type SuggestionCategory = (typeof SUGGESTION_CATEGORIES)[number];

/** The most suggestions the page shows (design: six on a wide screen, four on a phone). */
export const MAX_SUGGESTIONS = 6;

export interface Suggestion {
  /** Identifies the suggestion within one answer. */
  id: string;
  /** The question, blanks filled; sent as it is. */
  text: string;
  category: SuggestionCategory;
  /** The template the text came from. */
  template: string;
  /** The device the blank was filled with, when the template has one. */
  device_id?: string;
  /** The time range the blank was filled with (RFC 3339), when the template has one. */
  range?: { from: string; to: string };
}

export interface SuggestionList {
  /** Whether the tenant has stored a reading. */
  has_data: boolean;
  suggestions: Suggestion[];
}

export interface SuggestionsApi {
  /** The suggestions for the empty conversation of `tenant`'s user. */
  listSuggestions(tenant: string, signal?: AbortSignal): Promise<SuggestionList>;
}

const SuggestionsApiContext = createContext<SuggestionsApi | null>(null);

/** Gives the home page the suggestions endpoint: a fake server in the UI preview and in tests. */
export function SuggestionsApiProvider({ api, children }: { api: SuggestionsApi; children?: ReactNode }) {
  return createElement(SuggestionsApiContext.Provider, { value: api }, children);
}

/**
 * The suggestions API. Only the suggestions themselves call it, and the production build never
 * draws them (pages/home/suggestions-slot.tsx). When the endpoint exists this becomes
 * `useSessionBound` over a module on the generated client, like `useDeviceApi`.
 */
export function useSuggestionsApi(): SuggestionsApi {
  const value = useContext(SuggestionsApiContext);
  if (value === null) {
    throw new Error("useSuggestionsApi needs a SuggestionsApiProvider");
  }
  return value;
}
