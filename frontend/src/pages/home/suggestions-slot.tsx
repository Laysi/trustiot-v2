import { createContext, type ReactNode, useContext } from "react";

/**
 * What sits under the home page's composer. The suggested questions go there, but their endpoint
 * is still pending (`src/api/pending/suggestions.ts`, Laysi/trustiot-v2#158), and the production
 * build must not contain a pending module (`check:dist`), so `HomePage` knows only this slot:
 * empty in the production build, filled by the UI preview's suggestions provider
 * (`src/test/fake-suggestions.ts`) with `SuggestedQuestions`. When the endpoint exists, `HomePage`
 * renders `SuggestedQuestions` itself and this file goes, as `HomeSlot.tsx` does for the checklist.
 */
const HomeSuggestionsSlotContext = createContext<ReactNode>(null);

export function HomeSuggestionsSlotProvider({ slot, children }: { slot: ReactNode; children: ReactNode }) {
  return <HomeSuggestionsSlotContext.Provider value={slot}>{children}</HomeSuggestionsSlotContext.Provider>;
}

export function HomeSuggestionsSlot() {
  return useContext(HomeSuggestionsSlotContext);
}
