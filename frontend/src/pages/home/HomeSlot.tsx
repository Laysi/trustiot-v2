import { createContext, type ReactNode, useContext } from "react";

/**
 * What sits above the home page's greeting. The setup checklist goes there, but its API is still
 * pending (`src/api/pending/setup.ts`), and the production build must not contain a pending module
 * (`check:dist`), so `HomePage` knows only this slot: empty in the production build, filled by the
 * UI preview's setup provider (`src/test/fake-setup.ts`) with the checklist. When the endpoints
 * exist, `HomePage` renders `SetupChecklist` itself and this file goes.
 */
const HomeTopSlotContext = createContext<ReactNode>(null);

export function HomeTopSlotProvider({ slot, children }: { slot: ReactNode; children: ReactNode }) {
  return <HomeTopSlotContext.Provider value={slot}>{children}</HomeTopSlotContext.Provider>;
}

export function HomeTopSlot() {
  return useContext(HomeTopSlotContext);
}
