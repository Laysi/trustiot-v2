import { createContext, type ReactNode, useContext } from "react";

/**
 * What the setup checklist's step "choose a first use" shows under its cards: the weekly report
 * (`pages/reports/WeeklyReportStep.tsx`). The reports exist only in the UI preview until their API
 * does (Laysi/trustiot-v2#173, `src/api/pending/reports.ts`), and the production build must not
 * contain a pending module (`check:dist`), so the checklist knows only this slot: empty in the
 * production build, filled by the UI preview's reports provider (`src/test/fake-reports.ts`). When
 * the reports API exists, `SetupStepList` renders `WeeklyReportStep` itself and this file goes, as
 * `HomeSlot.tsx` does for the checklist.
 */
const FirstUseSlotContext = createContext<ReactNode>(null);

export function FirstUseSlotProvider({ slot, children }: { slot: ReactNode; children?: ReactNode }) {
  return <FirstUseSlotContext.Provider value={slot}>{children}</FirstUseSlotContext.Provider>;
}

export function FirstUseSlot() {
  return useContext(FirstUseSlotContext);
}
