import { createElement, type ReactNode } from "react";

import { ApiError } from "../api/errors.ts";
import {
  type Suggestion,
  type SuggestionList,
  type SuggestionsApi,
  SuggestionsApiProvider,
} from "../api/pending/suggestions.ts";
import { SuggestedQuestions } from "../pages/home/SuggestedQuestions.tsx";
import { HomeSuggestionsSlotProvider } from "../pages/home/suggestions-slot.tsx";

/**
 * The suggestions endpoint (`api/pending/suggestions.ts`) in memory. Like the server it fills the
 * question templates, here from the devices of the tenant it is asked for, and knows nothing of
 * any other tenant's: a test holds two tenants' devices in one fake and checks that each
 * tenant's home shows only its own. A tenant the fake was not given is refused (404), so a page
 * that asks for the wrong one fails loudly. Every call is recorded; `failNext` makes the next
 * call fail.
 */

export interface FakeSuggestionDevice {
  id: string;
  name: string;
}

export interface FakeSuggestionTenant {
  devices: FakeSuggestionDevice[];
  /** Whether the tenant stored a reading; without one it gets no device questions. */
  hasData: boolean;
}

type Language = "en" | "zh-TW";

/** The words of the templates, which the real server writes in the user's language. */
const WORDS = {
  en: {
    top: "Which device used the most energy today?",
    offline: "Which devices are offline?",
    compare: (device: string) => `${device} this week compared with last week`,
    trend: (device: string) => `${device}'s daily energy this month`,
    anomaly: (device: string) => `When was ${device} abnormal yesterday?`,
  },
  "zh-TW": {
    top: "今天哪台設備最耗電？",
    offline: "有哪些設備離線？",
    compare: (device: string) => `${device} 本週用電跟上週比`,
    trend: (device: string) => `${device} 這個月每天的用電`,
    anomaly: (device: string) => `${device} 昨天什麼時候異常？`,
  },
} as const;

/** The suggestions of a tenant: the templates that need no device, then one per device for the next. */
function fill(tenant: FakeSuggestionTenant, language: Language): Suggestion[] {
  if (!tenant.hasData) return [];
  const words = WORDS[language];
  const suggestions: Suggestion[] = [
    { id: "energy-top", category: "energy", template: "energy.top_device", text: words.top },
    { id: "offline-list", category: "offline", template: "offline.list", text: words.offline },
  ];
  const [first, second, third] = tenant.devices;
  if (first) {
    suggestions.push({
      id: `compare-${first.id}`,
      category: "compare",
      template: "energy.compare_last_week",
      text: words.compare(first.name),
      device_id: first.id,
    });
  }
  if (second) {
    suggestions.push({
      id: `anomaly-${second.id}`,
      category: "anomaly",
      template: "anomaly.yesterday",
      text: words.anomaly(second.name),
      device_id: second.id,
    });
  }
  if (third) {
    suggestions.push({
      id: `trend-${third.id}`,
      category: "trend",
      template: "energy.daily_trend",
      text: words.trend(third.name),
      device_id: third.id,
    });
  }
  return suggestions;
}

export class FakeSuggestionsServer {
  readonly tenants = new Map<string, FakeSuggestionTenant>();
  /** What a tenant the fake was not given gets, instead of a refusal (the UI preview's session has its own tenant). */
  fallback: FakeSuggestionTenant | undefined;
  /** Every call, in order: the tenant it was for. */
  readonly calls: string[] = [];
  private readonly failures: ApiError[] = [];

  constructor(private readonly language: Language = "en") {}

  addTenant(tenant: string, data: FakeSuggestionTenant): void {
    this.tenants.set(tenant, data);
  }

  failNext(error: ApiError = new ApiError(503, { code: "unavailable", message_key: "error.unavailable" })): void {
    this.failures.push(error);
  }

  api(): SuggestionsApi {
    return {
      listSuggestions: async (tenant): Promise<SuggestionList> => {
        this.calls.push(tenant);
        // An answer is never synchronous, as a request's is not.
        await Promise.resolve();
        const failure = this.failures.shift();
        if (failure !== undefined) throw failure;
        const data = this.tenants.get(tenant) ?? this.fallback;
        if (data === undefined) throw new ApiError(404, { code: "not_found", message_key: "error.not_found" });
        return { has_data: data.hasData, suggestions: fill(data, this.language) };
      },
    };
  }
}

/** The design's demo tenant (docs/design/home-ai/03-home-ready.html): six devices, readings arriving. */
function demoTenant(): FakeSuggestionTenant {
  return {
    hasData: true,
    devices: [
      { id: "00000000-0000-4000-8000-0000000000a2", name: "空壓機 A2" },
      { id: "00000000-0000-4000-8000-0000000000b1", name: "鍋爐 B1" },
      { id: "00000000-0000-4000-8000-0000000000c1", name: "主配電盤 P1" },
    ],
  };
}

/**
 * For the UI preview (Laysi/trustiot-v2#1065): "normal" is the design's tenant with six
 * suggestions, "empty" a new one that has no reading and is told what will appear. It also fills
 * the home page's suggestions slot, so the pills show only here.
 */
export const preview = {
  name: "Suggested questions",
  provider: (seed: "normal" | "empty") => {
    const server = new FakeSuggestionsServer("zh-TW");
    server.fallback = seed === "empty" ? { hasData: false, devices: [] } : demoTenant();
    const api = server.api();
    return function SuggestionsPreviewProvider({ children }: { children: ReactNode }) {
      return createElement(
        SuggestionsApiProvider,
        { api },
        createElement(HomeSuggestionsSlotProvider, { slot: createElement(SuggestedQuestions), children }),
      );
    };
  },
};
